//! Web Push: VAPID key management, subscription CRUD, test notification.
//! The actual send transport lives behind [`Pusher`] so the reminder
//! scheduler's tests can swap in a recording fake (Task 7/9 of the 5C plan).

use crate::AppState;
use crate::error::ApiError;
use crate::session::CurrentUser;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use perkele_shared::push::{SubscribeRequest, UnsubscribeRequest, VapidResponse};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/push/vapid", get(vapid))
        .route("/api/push/subscribe", post(subscribe).delete(unsubscribe))
        .route("/api/push/test", post(test_notification))
}

/// Why a push could not be delivered. `Gone` = the subscription is dead
/// (404/410/unparseable endpoint) → prune the row; anything else is logged
/// (with its category) and not pruned.
#[derive(Debug)]
pub enum PushError {
    Gone,
    Other(String),
}

/// Push transport seam: the scheduler and routes talk to this, tests swap in
/// a recording fake. `impl Future` in the trait = static dispatch (RPITIT) —
/// callers are generic over `P: Pusher`, no boxing.
pub trait Pusher: Send + Sync {
    fn send(
        &self,
        endpoint: &str,
        keys_json: &str,
        payload: &str,
    ) -> impl std::future::Future<Output = Result<(), PushError>> + Send;
}

/// What the browser handed us in keys_json.
#[derive(serde::Deserialize)]
struct SubKeys {
    p256dh: String,
    auth: String,
}

/// The only hosts the server will ever POST a push to. A subscription endpoint
/// is attacker-supplied (any logged-in user can submit one), and the server
/// makes an outbound HTTPS request to it — without this list that is an SSRF
/// into the LAN (router admin, Proxmox, localhost services). Entries:
/// - a bare host matches exactly;
/// - `*.suffix` matches any subdomain of `suffix` (not `suffix` itself).
///
/// Chrome/Edge (FCM, incl. legacy android.googleapis.com), Firefox (Mozilla
/// autopush), legacy Edge/Windows (WNS) and Safari/iOS (Apple) cover every
/// browser the app supports. Add a service here if a new browser shows up.
const PUSH_HOST_ALLOWLIST: &[&str] = &[
    "fcm.googleapis.com",
    // Exact, not `*.googleapis.com`: that wildcard would also admit e.g.
    // storage.googleapis.com, where anyone can host a URL.
    "android.googleapis.com",
    "*.push.services.mozilla.com",
    "*.notify.windows.com",
    "web.push.apple.com",
    "*.push.apple.com",
];

/// Real endpoints are ~200-500 chars; anything past this is garbage.
const MAX_ENDPOINT_LEN: usize = 1024;

/// Devices kept per user; subscribing an 11th evicts the least recently
/// (re-)subscribed one, so a user can't grow the table without bound.
pub const MAX_SUBS_PER_USER: i64 = 10;

/// Upper bound on one push round-trip (connect + TLS + request + response).
/// The isahc client web-push builds by default never times out, so without
/// this a black-holed endpoint would stall the scheduler tick forever.
const PUSH_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn host_allowed(host: &str) -> bool {
    PUSH_HOST_ALLOWLIST.iter().any(|entry| {
        // `strip_prefix` returns Some(rest) only when the prefix is there, so
        // this one `match` splits "wildcard" from "exact" entries.
        match entry.strip_prefix("*.") {
            Some(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
            None => host == *entry,
        }
    })
}

/// Is `endpoint` something we are willing to send to? Checked at subscribe
/// time AND again right before every send (rows stored before this check
/// existed must not reach web-push, whose VAPID signer `unwrap()`s the
/// scheme and host and would panic on e.g. "x.example").
pub(crate) fn validate_endpoint(endpoint: &str) -> Result<(), &'static str> {
    if endpoint.is_empty() || endpoint.len() > MAX_ENDPOINT_LEN {
        return Err("bad length");
    }
    let uri: axum::http::Uri = endpoint.parse().map_err(|_| "unparseable")?;
    if !uri
        .scheme_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
    {
        return Err("not https");
    }
    let authority = uri.authority().ok_or("no host")?;
    if authority.as_str().contains('@') {
        return Err("userinfo");
    }
    if authority.port_u16().is_some_and(|p| p != 443) {
        return Err("non-default port");
    }
    let host = authority.host().to_ascii_lowercase();
    // IPv6 literals come back bracketed ("[::1]"); IPv4 parses as IpAddr.
    if host.starts_with('[') || host.parse::<std::net::IpAddr>().is_ok() {
        return Err("ip literal");
    }
    if !host_allowed(&host) {
        return Err("host not allowlisted");
    }
    Ok(())
}

/// p256dh must be an uncompressed P-256 point (65 bytes, leading 0x04) and
/// auth a 16-byte secret, both base64url. Browsers send them unpadded;
/// tolerate padding anyway.
fn validate_keys(p256dh: &str, auth: &str) -> Result<(), &'static str> {
    let decode = |s: &str| URL_SAFE_NO_PAD.decode(s.trim_end_matches('='));
    match decode(p256dh) {
        Ok(k) if k.len() == 65 && k[0] == 0x04 => {}
        _ => return Err("bad p256dh"),
    }
    match decode(auth) {
        Ok(a) if a.len() == 16 => Ok(()),
        _ => Err("bad auth"),
    }
}

/// Send-time recheck of a stored row. Returns why it is unusable, if it is.
fn stored_sub_problem(endpoint: &str, keys_json: &str) -> Option<&'static str> {
    if let Err(why) = validate_endpoint(endpoint) {
        return Some(why);
    }
    match serde_json::from_str::<SubKeys>(keys_json) {
        Ok(k) => validate_keys(&k.p256dh, &k.auth).err(),
        Err(_) => Some("malformed keys_json"),
    }
}

/// The real transport. Built ONCE at startup and shared through `AppState`
/// (behind an `Arc`): the isahc client inside owns a background agent thread
/// and a connection pool, so one per request would be wasteful.
pub struct WebPushPusher {
    private_key: String,
    client: web_push::IsahcWebPushClient,
}

impl WebPushPusher {
    pub fn new(private_key_b64: String) -> Result<Self, ApiError> {
        // `Configurable` is the isahc trait that adds `.timeout()` etc. to the
        // builder; trait methods are only callable while the trait is in scope.
        use isahc::config::Configurable as _;
        use std::time::Duration;
        const PUSH_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
        const PUSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
        // Transport-level limits, so a stalled push service can't hold a
        // connection (and the curl agent's slot) open forever. The tokio
        // timeout in `send` stays as the outer bound.
        let http = isahc::HttpClient::builder()
            .timeout(PUSH_HTTP_TIMEOUT)
            .connect_timeout(PUSH_CONNECT_TIMEOUT)
            .build()
            .map_err(|e| {
                tracing::error!("push client init failed: {e}");
                ApiError::Internal
            })?;
        Ok(Self {
            private_key: private_key_b64,
            // web-push implements `From<isahc::HttpClient>` for its client.
            client: web_push::IsahcWebPushClient::from(http),
        })
    }
}

impl Pusher for WebPushPusher {
    async fn send(&self, endpoint: &str, keys_json: &str, payload: &str) -> Result<(), PushError> {
        use web_push::WebPushClient as _;
        use web_push::WebPushError;
        // Belt and braces: callers already filter, but this is the last stop
        // before web-push's panicking VAPID signer, so never trust the caller.
        validate_endpoint(endpoint).map_err(|_| PushError::Gone)?;
        // Fixed message: serde's error text can echo input, i.e. key material.
        let keys: SubKeys = serde_json::from_str(keys_json)
            .map_err(|_| PushError::Other("malformed keys_json".into()))?;
        let info = web_push::SubscriptionInfo::new(endpoint, &keys.p256dh, &keys.auth);
        let sig = web_push::VapidSignatureBuilder::from_base64(&self.private_key, &info)
            .and_then(|b| b.build())
            .map_err(|e| PushError::Other(e.to_string()))?;
        let mut msg = web_push::WebPushMessageBuilder::new(&info);
        msg.set_vapid_signature(sig);
        msg.set_payload(web_push::ContentEncoding::Aes128Gcm, payload.as_bytes());
        let msg = msg.build().map_err(|e| PushError::Other(e.to_string()))?;
        // `tokio::time::timeout` races the send against a timer; on expiry the
        // send future is dropped, which makes isahc abort the transfer.
        let Ok(result) = tokio::time::timeout(PUSH_SEND_TIMEOUT, self.client.send(msg)).await
        else {
            return Err(PushError::Other("timed out".into()));
        };
        match result {
            Ok(()) => Ok(()),
            // 404/410/unparseable endpoint = the subscription is dead for good.
            Err(WebPushError::EndpointNotValid(_))
            | Err(WebPushError::EndpointNotFound(_))
            | Err(WebPushError::InvalidUri) => Err(PushError::Gone),
            // Auth, rate-limit/5xx, payload-too-large etc. are transient or
            // config-related — log them with their category, never prune.
            Err(e) => Err(PushError::Other(format!("{}: {e}", e.short_description()))),
        }
    }
}

/// Push `payload` to every subscription one user has; prune dead endpoints.
/// Returns how many sends succeeded. Send failures are logged, never bubbled —
/// a reminder is not bank mail.
pub async fn send_to_user<P: Pusher>(
    db: &crate::db::Db,
    pusher: &P,
    family_id: i64,
    user_id: i64,
    payload: &str,
) -> Result<u32, ApiError> {
    let subs = sqlx::query!(
        r#"SELECT id AS "id!: i64", endpoint AS "endpoint!: String",
                  keys_json AS "keys_json!: String"
           FROM push_subscriptions WHERE family_id = ? AND user_id = ?"#,
        family_id,
        user_id,
    )
    .fetch_all(db)
    .await?;
    let mut sent = 0;
    for s in subs {
        // Rows from before subscribe-time validation existed may be junk
        // (or SSRF targets): drop them instead of sending. Logs carry the
        // row id only — the endpoint URL is itself a bearer capability.
        if let Some(why) = stored_sub_problem(&s.endpoint, &s.keys_json) {
            tracing::warn!("push: deleting invalid subscription id={}: {why}", s.id);
            delete_sub(db, s.id).await?;
            continue;
        }
        match pusher.send(&s.endpoint, &s.keys_json, payload).await {
            Ok(()) => sent += 1,
            Err(PushError::Gone) => delete_sub(db, s.id).await?,
            Err(PushError::Other(e)) => {
                tracing::warn!("push send failed (subscription id={}): {e}", s.id)
            }
        }
    }
    Ok(sent)
}

async fn delete_sub(db: &crate::db::Db, id: i64) -> Result<(), ApiError> {
    sqlx::query!("DELETE FROM push_subscriptions WHERE id = ?", id)
        .execute(db)
        .await?;
    Ok(())
}

/// Fan a new announcement out to every family member except the author.
/// Body preview is char-truncated so a long post doesn't blow the payload.
pub async fn push_announcement<P: Pusher>(
    db: &crate::db::Db,
    pusher: &P,
    family_id: i64,
    author_id: i64,
    author_name: &str,
    body: &str,
) -> Result<u32, ApiError> {
    // Announcements hidden → the post is still stored, just no push.
    if crate::feature::is_hidden(
        db,
        family_id,
        perkele_shared::feature::Feature::Announcements,
    )
    .await?
    {
        return Ok(0);
    }
    let recipients = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM users WHERE family_id = ? AND id != ?"#,
        family_id,
        author_id,
    )
    .fetch_all(db)
    .await?;
    let preview: String = body.chars().take(100).collect();
    let payload = serde_json::json!({
        "title": "Ilmoitustaulu",
        "body": format!("{author_name}: {preview}"),
    })
    .to_string();
    let mut sent = 0;
    for user_id in recipients {
        sent += send_to_user(db, pusher, family_id, user_id, &payload).await?;
    }
    Ok(sent)
}

async fn test_notification(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let payload =
        serde_json::json!({ "title": "Perhe-elämää", "body": "Testi-ilmoitus toimii 🎉" })
            .to_string();
    // `&*state.pusher`: `*` derefs the Arc to the WebPushPusher, `&` borrows
    // it — send_to_user wants `&P` where `P: Pusher`, not `&Arc<P>`.
    send_to_user(
        &state.db,
        &*state.pusher,
        user.family_id,
        user.user_id,
        &payload,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Subscribe this device. Status codes:
/// - 201: stored (new row, or the caller's own endpoint re-subscribed —
///   keys and timestamp refreshed);
/// - 400: endpoint/keys fail validation (not https, not a known push
///   service, IP literal, bad key lengths, …);
/// - 409: the endpoint already belongs to ANOTHER user. It is not re-homed:
///   otherwise anyone who learned an endpoint could redirect that device's
///   notifications. (On a shared browser, the old user unsubscribing — or
///   the dead-endpoint prune — frees it.)
async fn subscribe(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SubscribeRequest>,
) -> Result<StatusCode, ApiError> {
    let bad = || ApiError::BadRequest("Virheellinen push-tilaus.".into());
    validate_endpoint(&req.endpoint).map_err(|_| bad())?;
    validate_keys(&req.p256dh, &req.auth).map_err(|_| bad())?;
    let keys_json = serde_json::json!({ "p256dh": req.p256dh, "auth": req.auth }).to_string();
    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| ApiError::Internal)?;
    // The endpoint is the device identity: re-subscribing replaces the keys
    // and bumps created_at (so the per-user cap evicts stale devices first).
    // The `WHERE` on DO UPDATE makes the upsert a no-op (0 rows affected)
    // when the existing row is someone else's.
    let affected = sqlx::query!(
        "INSERT INTO push_subscriptions (user_id, family_id, endpoint, keys_json, created_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET family_id = excluded.family_id,
             keys_json = excluded.keys_json, created_at = excluded.created_at
         WHERE push_subscriptions.user_id = excluded.user_id",
        user.user_id,
        user.family_id,
        req.endpoint,
        keys_json,
        now,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if affected == 0 {
        return Err(ApiError::Conflict(
            "Tämä laite on jo toisen käyttäjän ilmoituksissa.".into(),
        ));
    }
    // Cap: keep the MAX_SUBS_PER_USER most recent rows. julianday() compares
    // instants, not strings — RFC 3339 with variable-length fractional
    // seconds does not sort correctly as text within the same second.
    sqlx::query!(
        "DELETE FROM push_subscriptions WHERE user_id = ? AND id NOT IN (
             SELECT id FROM push_subscriptions WHERE user_id = ?
             ORDER BY julianday(created_at) DESC, id DESC LIMIT ?)",
        user.user_id,
        user.user_id,
        MAX_SUBS_PER_USER,
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::CREATED)
}

async fn unsubscribe(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<UnsubscribeRequest>,
) -> Result<StatusCode, ApiError> {
    // Scoped to the caller so one user can't unsubscribe another's device.
    // Always 204 (idempotent, and doesn't reveal whose endpoint it was).
    sqlx::query!(
        "DELETE FROM push_subscriptions WHERE endpoint = ? AND family_id = ? AND user_id = ?",
        req.endpoint,
        user.family_id,
        user.user_id,
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Load the VAPID private key (base64url raw ES256 scalar), generating and
/// persisting one on first use. INSERT … ON CONFLICT DO NOTHING makes a
/// concurrent first-boot race harmless: everyone re-reads the winner's key.
pub async fn ensure_vapid_private_key(db: &crate::db::Db) -> Result<String, ApiError> {
    if let Some(v) = sqlx::query_scalar!(
        r#"SELECT value AS "value!: String" FROM config WHERE key = 'vapid_private_key'"#
    )
    .fetch_optional(db)
    .await?
    {
        return Ok(v);
    }
    let generated =
        URL_SAFE_NO_PAD.encode(jwt_simple::algorithms::ES256KeyPair::generate().to_bytes());
    sqlx::query!(
        "INSERT INTO config (key, value) VALUES ('vapid_private_key', ?)
         ON CONFLICT DO NOTHING",
        generated,
    )
    .execute(db)
    .await?;
    let v = sqlx::query_scalar!(
        r#"SELECT value AS "value!: String" FROM config WHERE key = 'vapid_private_key'"#
    )
    .fetch_one(db)
    .await?;
    Ok(v)
}

/// Uncompressed P-256 public point for the stored private key, base64url —
/// the `applicationServerKey` the browser needs.
pub fn public_key_b64(private_b64: &str) -> Result<String, ApiError> {
    let partial =
        web_push::VapidSignatureBuilder::from_base64_no_sub(private_b64).map_err(|e| {
            tracing::error!("bad VAPID key in config: {e}");
            ApiError::Internal
        })?;
    Ok(URL_SAFE_NO_PAD.encode(partial.get_public_key()))
}

// Public on purpose: the VAPID public key is not secret (it is the `k=` claim
// in every push's Authorization header), and the service worker needs to fetch
// it without a session to re-subscribe after a `pushsubscriptionchange`.
async fn vapid(State(state): State<AppState>) -> Result<Json<VapidResponse>, ApiError> {
    let private = ensure_vapid_private_key(&state.db).await?;
    Ok(Json(VapidResponse {
        public_key: public_key_b64(&private)?,
    }))
}

/// Test double shared by reminder + announcement tests: records sends
/// instead of pushing. Lives here (not in a test module) so both callers
/// can `use crate::push::test_support::FakePusher`.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{PushError, Pusher, URL_SAFE_NO_PAD, WebPushPusher};
    use base64::Engine as _;
    use std::sync::{Arc, OnceLock};

    /// One real pusher shared by every test's `AppState` (a `static` so the
    /// isahc agent thread is spawned once per test binary, not per test).
    /// `OnceLock::get_or_init` runs the closure on first use only. Its key is
    /// random and unrelated to any DB's VAPID key — tests never reach the
    /// network with it (only invalid endpoints are ever sent through it).
    pub fn shared_pusher() -> Arc<WebPushPusher> {
        static PUSHER: OnceLock<Arc<WebPushPusher>> = OnceLock::new();
        PUSHER
            .get_or_init(|| {
                let key = URL_SAFE_NO_PAD
                    .encode(jwt_simple::algorithms::ES256KeyPair::generate().to_bytes());
                Arc::new(WebPushPusher::new(key).expect("test push client"))
            })
            .clone()
    }

    /// Well-formed (but fake) browser keys: 65-byte uncompressed point
    /// (0x04 prefix) and a 16-byte auth secret, base64url.
    pub fn valid_keys() -> (String, String) {
        let mut p256dh = [7u8; 65];
        p256dh[0] = 0x04;
        (
            URL_SAFE_NO_PAD.encode(p256dh),
            URL_SAFE_NO_PAD.encode([9u8; 16]),
        )
    }

    /// A subscribe body for `endpoint` with valid-looking keys.
    pub fn sub_body(endpoint: &str) -> serde_json::Value {
        let (p256dh, auth) = valid_keys();
        serde_json::json!({ "endpoint": endpoint, "p256dh": p256dh, "auth": auth })
    }

    /// An allowlisted (FCM) endpoint for device `name`.
    pub fn fcm(name: &str) -> String {
        format!("https://fcm.googleapis.com/fcm/send/{name}")
    }

    pub struct FakePusher {
        /// (endpoint, payload) of every accepted send.
        pub sent: std::sync::Arc<tokio::sync::Mutex<Vec<(String, String)>>>,
        /// When true every send reports Gone (tests endpoint pruning).
        pub fail_gone: bool,
    }

    impl Pusher for FakePusher {
        async fn send(&self, endpoint: &str, _keys: &str, payload: &str) -> Result<(), PushError> {
            if self.fail_gone {
                return Err(PushError::Gone);
            }
            self.sent
                .lock()
                .await
                .push((endpoint.to_owned(), payload.to_owned()));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn push_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    async fn push_app() -> axum::Router {
        push_app_with_db().await.0
    }

    /// Admin mints a member invite, "matti" redeems it; returns matti's cookie.
    async fn join_member(app: &axum::Router, admin: &str) -> String {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/family/invites",
                admin,
                Some(json!({ "role": "member" })),
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "code": code, "username": "matti",
                                "display_name": "Matti", "password": "hunter2!" })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    fn req(method: &str, path: &str, cookie: &str, body: Option<Value>) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, cookie);
        match body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                b.body(Body::from(v.to_string())).unwrap()
            }
            None => b.body(Body::empty()).unwrap(),
        }
    }

    fn cookie_pair(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn setup_admin(app: &axum::Router) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/setup")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "family_name": "Virtanen", "username": "mikko",
                                "display_name": "Mikko", "password": "hunter2!" })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    use test_support::{fcm, sub_body};

    /// POST /api/push/subscribe as `cookie`; returns the status.
    async fn subscribe_as(app: &axum::Router, cookie: &str, body: Value) -> StatusCode {
        app.clone()
            .oneshot(req("POST", "/api/push/subscribe", cookie, Some(body)))
            .await
            .unwrap()
            .status()
    }

    async fn sub_owner(db: &crate::db::Db, endpoint: &str) -> Option<i64> {
        sqlx::query_scalar!(
            r#"SELECT user_id AS "user_id!: i64" FROM push_subscriptions WHERE endpoint = ?"#,
            endpoint
        )
        .fetch_optional(db)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn test_notification_route_is_safe_without_working_subs() {
        let app = push_app().await;
        let cookie = setup_admin(&app).await;
        // No subscriptions at all → nothing to send, still 204.
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/push/test", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // A bogus subscription is now refused up front (it used to be stored
        // and fail at send time)…
        let status = subscribe_as(&app, &cookie, sub_body("https://invalid.localhost/nope")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // …and the test route still answers 204.
        let resp = app
            .oneshot(req("POST", "/api/push/test", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[test]
    fn endpoint_validation_allowlist() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/abc",
            "https://android.googleapis.com/gcm/send/abc",
            "https://updates.push.services.mozilla.com/wpush/v2/abc",
            "https://db5p.notify.windows.com/w/?token=abc",
            "https://web.push.apple.com/QAbc",
            "https://api.push.apple.com/abc",
            "HTTPS://FCM.GOOGLEAPIS.COM/fcm/send/abc",
            "https://fcm.googleapis.com:443/fcm/send/abc",
        ] {
            assert_eq!(validate_endpoint(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            "x.example",
            "/x",
            "fcm.googleapis.com/fcm/send/abc",
            "http://fcm.googleapis.com/fcm/send/abc",
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://192.168.1.1/",
            "https://localhost/x",
            "https://push.example/abc",
            "https://googleapis.com/x",
            "https://evilgoogleapis.com/x",
            "https://fcm.googleapis.com.evil.example/x",
            "https://user:pw@fcm.googleapis.com/x",
            "https://fcm.googleapis.com:8443/x",
            "https://push.services.mozilla.com/x",
            // Other googleapis.com hosts are not push services.
            "https://storage.googleapis.com/bucket/x",
            "https://www.googleapis.com/x",
        ] {
            assert!(validate_endpoint(bad).is_err(), "{bad}");
        }
        let long = format!("https://fcm.googleapis.com/{}", "a".repeat(1100));
        assert!(validate_endpoint(&long).is_err());
    }

    #[tokio::test]
    async fn subscribe_rejects_bad_endpoints_and_keys() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        for bad in [
            "http://fcm.googleapis.com/fcm/send/a", // not https
            "https://10.0.0.1/x",                   // IP literal
            "https://localhost/x",                  // localhost
            "https://push.example/abc",             // not allowlisted
            "x.example",                            // scheme-less
            "/x",                                   // path only
        ] {
            let status = subscribe_as(&app, &cookie, sub_body(bad)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        // Keys of the wrong shape are refused too.
        let (p256dh, auth) = test_support::valid_keys();
        for (p, a) in [("pk", auth.as_str()), (p256dh.as_str(), "as"), ("", "")] {
            let body = json!({ "endpoint": fcm("k"), "p256dh": p, "auth": a });
            assert_eq!(
                subscribe_as(&app, &cookie, body).await,
                StatusCode::BAD_REQUEST
            );
        }
        let n = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM push_subscriptions"#)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        // Real-looking FCM and Mozilla endpoints are accepted.
        for ok in [
            fcm("dev1"),
            "https://updates.push.services.mozilla.com/wpush/v2/abc".to_owned(),
        ] {
            assert_eq!(
                subscribe_as(&app, &cookie, sub_body(&ok)).await,
                StatusCode::CREATED
            );
        }
    }

    #[tokio::test]
    async fn send_time_guard_drops_invalid_legacy_rows_without_panicking() {
        let (app, db) = push_app_with_db().await;
        let _cookie = setup_admin(&app).await; // user 1, family 1
        // Rows written before validation existed: the scheme-less ones made
        // web-push's VAPID signer panic; the LAN one was an SSRF target.
        let (p256dh, auth) = test_support::valid_keys();
        let keys = json!({ "p256dh": p256dh, "auth": auth }).to_string();
        for ep in ["x.example", "/x", "https://192.168.1.10/"] {
            sqlx::query!(
                "INSERT INTO push_subscriptions (user_id, family_id, endpoint, keys_json, created_at)
                 VALUES (1, 1, ?, ?, '2026-01-01T00:00:00Z')",
                ep,
                keys,
            )
            .execute(&db)
            .await
            .unwrap();
        }
        // The REAL transport: without the guard this panics in web-push.
        let pusher = test_support::shared_pusher();
        let sent = send_to_user(&db, &*pusher, 1, 1, "{}").await.unwrap();
        assert_eq!(sent, 0);
        let left = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM push_subscriptions"#)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0, "invalid rows are deleted");
        // And the transport itself refuses (as Gone) rather than panicking.
        assert!(matches!(
            pusher.send("x.example", &keys, "{}").await,
            Err(PushError::Gone)
        ));
    }

    #[tokio::test]
    async fn another_users_endpoint_cannot_be_taken_over() {
        let (app, db) = push_app_with_db().await;
        let admin = setup_admin(&app).await; // user 1
        let member = join_member(&app, &admin).await; // user 2
        let ep = fcm("admins-phone");
        assert_eq!(
            subscribe_as(&app, &admin, sub_body(&ep)).await,
            StatusCode::CREATED
        );
        // The member submits the same endpoint: refused, row stays the admin's.
        assert_eq!(
            subscribe_as(&app, &member, sub_body(&ep)).await,
            StatusCode::CONFLICT
        );
        assert_eq!(sub_owner(&db, &ep).await, Some(1));
        // The owner can still re-subscribe (upsert).
        assert_eq!(
            subscribe_as(&app, &admin, sub_body(&ep)).await,
            StatusCode::CREATED
        );
        assert_eq!(sub_owner(&db, &ep).await, Some(1));
    }

    #[tokio::test]
    async fn unsubscribe_cannot_remove_another_users_subscription() {
        let (app, db) = push_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let ep = fcm("admins-phone");
        subscribe_as(&app, &admin, sub_body(&ep)).await;
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                "/api/push/subscribe",
                &member,
                Some(json!({ "endpoint": ep })),
            ))
            .await
            .unwrap();
        // Idempotent 204 either way, but the admin's row survives.
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(sub_owner(&db, &ep).await, Some(1));
    }

    #[tokio::test]
    async fn subscriptions_are_capped_per_user() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        for i in 0..=MAX_SUBS_PER_USER {
            assert_eq!(
                subscribe_as(&app, &cookie, sub_body(&fcm(&format!("d{i}")))).await,
                StatusCode::CREATED
            );
        }
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM push_subscriptions WHERE user_id = 1"#
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(n, MAX_SUBS_PER_USER);
        // The oldest (d0) was evicted; the newest is kept.
        assert_eq!(sub_owner(&db, &fcm("d0")).await, None);
        assert_eq!(
            sub_owner(&db, &fcm(&format!("d{MAX_SUBS_PER_USER}"))).await,
            Some(1)
        );
    }

    #[tokio::test]
    async fn subscribe_upserts_and_unsubscribe_is_idempotent() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        let sub = sub_body(&fcm("abc"));
        for _ in 0..2 {
            // Same endpoint twice = one row (re-subscribes after permission churn).
            assert_eq!(
                subscribe_as(&app, &cookie, sub.clone()).await,
                StatusCode::CREATED
            );
        }
        assert_eq!(sub_owner(&db, &fcm("abc")).await, Some(1));
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    "/api/push/subscribe",
                    &cookie,
                    Some(json!({ "endpoint": fcm("abc") })),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }
        assert_eq!(sub_owner(&db, &fcm("abc")).await, None);
    }

    #[tokio::test]
    async fn push_announcement_targets_family_minus_author() {
        let (app, db) = push_app_with_db().await;
        let admin = setup_admin(&app).await;
        // A member joins and both subscribe one device each.
        let member = join_member(&app, &admin).await;
        subscribe_as(&app, &admin, sub_body(&fcm("admin"))).await;
        subscribe_as(&app, &member, sub_body(&fcm("member"))).await;

        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = test_support::FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };
        // Admin (user id 1) posts → only the member's endpoint is pushed.
        let n = push_announcement(
            &db,
            &pusher,
            1,
            1,
            "Mikko",
            "Muistakaa mummon synttärit lauantaina!",
        )
        .await
        .unwrap();
        assert_eq!(n, 1);
        let log = sent.lock().await.clone();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].0, fcm("member"));
        assert!(log[0].1.contains("Ilmoitustaulu"));
        assert!(log[0].1.contains("Mikko:"));
    }

    #[tokio::test]
    async fn push_announcement_is_skipped_while_announcements_hidden() {
        use perkele_shared::feature::Feature;
        let (app, db) = push_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        subscribe_as(&app, &member, sub_body(&fcm("member"))).await;
        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = test_support::FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };

        crate::feature::replace_hidden(&db, 1, &[Feature::Announcements])
            .await
            .unwrap();
        let n = push_announcement(&db, &pusher, 1, 1, "Mikko", "Hei")
            .await
            .unwrap();
        assert_eq!(n, 0);
        assert!(sent.lock().await.is_empty());

        // Shown again → pushes resume.
        crate::feature::replace_hidden(&db, 1, &[]).await.unwrap();
        let n = push_announcement(&db, &pusher, 1, 1, "Mikko", "Hei")
            .await
            .unwrap();
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn vapid_key_is_public_and_stable() {
        let app = push_app().await;
        // Public (no session needed) — the service worker fetches it to re-subscribe.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/push/vapid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let cookie = setup_admin(&app).await;
        let k1 = json_body(
            app.clone()
                .oneshot(req("GET", "/api/push/vapid", &cookie, None))
                .await
                .unwrap(),
        )
        .await["public_key"]
            .as_str()
            .unwrap()
            .to_owned();
        // Generated once, then stable.
        let k2 = json_body(
            app.oneshot(req("GET", "/api/push/vapid", &cookie, None))
                .await
                .unwrap(),
        )
        .await["public_key"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!k1.is_empty());
        assert_eq!(k1, k2);
    }
}
