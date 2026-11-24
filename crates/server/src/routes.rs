//! HTTP handlers for setup and authentication.

use crate::AppState;
use crate::auth::{
    canonicalize_invite_code, generate_invite_code, hash_password, hash_token, verify_password,
};
use crate::error::ApiError;
use crate::session::{self, AdminUser, CurrentUser, SESSION_COOKIE};
use crate::throttle::Attempt;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_extra::extract::CookieJar;
use perkele_shared::auth::{
    CreateInviteRequest, InviteResponse, LoginRequest, MAX_PASSWORD_LEN, RedeemRequest, Role,
    SetupRequest, SetupStatus, UserView, validate_name, validate_password, validate_username,
};
use perkele_shared::client_report::ClientErrorReport;
use std::sync::{Arc, LazyLock};
use std::time::Instant;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use tokio::sync::Semaphore;

/// How long an invite code stays valid.
const INVITE_TTL: Duration = Duration::days(7);

/// A precomputed Argon2 hash of an arbitrary password. When a login names a
/// user that doesn't exist, we still verify the password against this so the
/// response takes the same time as a real check — otherwise an attacker could
/// tell which usernames exist by timing the responses.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password("perkele-timing-equalizer").expect("hash dummy password"));

/// Compute [`DUMMY_HASH`] now, at startup, on a blocking thread — otherwise the
/// first login for an unknown user would run a full Argon2 hash inline on an
/// async worker thread.
pub async fn prime_dummy_hash() {
    let _ = tokio::task::spawn_blocking(|| LazyLock::force(&DUMMY_HASH)).await;
}

/// How many Argon2 hashes/verifies may run at once: one per CPU core (at least
/// two). Each run takes ~20 MB of RAM and a core for a good fraction of a
/// second, so without a bound a burst of logins could queue up hundreds on the
/// blocking thread pool and starve the machine. Extra callers simply wait
/// their turn for a permit.
fn argon2_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .max(2)
}

/// `Arc` so a permit can be *owned* (`acquire_owned`) and moved into the
/// blocking closure: it is released when the hash actually finishes, even if
/// the request that started it has already gone away.
static ARGON2_SLOTS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(argon2_permits())));

/// The credential-accepting endpoints. These are the brute-force targets, so
/// `main` wraps this sub-router in a rate limiter. Kept separate from the rest
/// precisely so the limit doesn't throttle ordinary reads.
pub fn limited_router() -> Router<AppState> {
    Router::new()
        .route("/api/setup", post(setup))
        .route("/api/auth/login", post(login))
        .route("/api/auth/redeem", post(redeem))
}

/// Everything else: reads and authenticated actions that aren't password guesses.
pub fn general_router() -> Router<AppState> {
    Router::new()
        .route("/api/setup/status", get(setup_status))
        .route("/api/auth/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/family/members", get(list_members))
        .route("/api/family/invites", post(create_invite))
        // `.layer` on the method router applies to this one route only. It
        // caps what the `String` extractor will buffer, so an oversized body
        // is refused with 413 before serde ever sees it.
        .route(
            "/api/client-error",
            post(report_client_error).layer(DefaultBodyLimit::max(CLIENT_ERROR_MAX_BODY)),
        )
}

/// Largest client error report we accept, in bytes. Real ones (a panic
/// message + a route) are a few hundred bytes.
pub(crate) const CLIENT_ERROR_MAX_BODY: usize = 16 * 1024;
/// Reports one user may send back-to-back before throttling kicks in…
pub(crate) const CLIENT_ERROR_BURST: u32 = 5;
/// …after which they earn one more every this often (5/minute sustained).
const CLIENT_ERROR_REFILL: std::time::Duration = std::time::Duration::from_secs(12);

/// Per-user token bucket for `/api/client-error`.
///
/// Every accepted report becomes a `tracing::error!` and so a GlitchTip event.
/// Without a limit, one account — or a client stuck in a panic loop — could
/// flood the logs and burn the GlitchTip event quota, so the *next real* crash
/// gets dropped. Keyed by user id, which comes from the session, not the
/// request: a caller can't mint fresh keys, and the map is bounded by the
/// number of accounts, so it needs no cleanup.
pub struct ClientErrorLimiter(governor::DefaultKeyedRateLimiter<i64>);

impl Default for ClientErrorLimiter {
    fn default() -> Self {
        // `NonZeroU32::new(..).expect(..)` on a constant: governor's API takes
        // `NonZeroU32` so "a burst of 0" is unrepresentable. `Quota::with_period`
        // returns `None` only for a zero period.
        let burst = std::num::NonZeroU32::new(CLIENT_ERROR_BURST).expect("burst > 0");
        let quota = governor::Quota::with_period(CLIENT_ERROR_REFILL)
            .expect("refill period > 0")
            .allow_burst(burst);
        Self(governor::RateLimiter::keyed(quota))
    }
}

impl ClientErrorLimiter {
    /// Take one token from `user_id`'s bucket; `false` = over the limit.
    pub fn allow(&self, user_id: i64) -> bool {
        self.0.check_key(&user_id).is_ok()
    }
}

/// Make untrusted text safe to put in a log line: keep at most `max_chars`
/// characters, and turn control characters into visible escapes (`\n`,
/// `\u{1b}`). Otherwise a report containing "\n<timestamp> INFO …" would forge
/// an extra, legitimate-looking log line, and ANSI codes could repaint a
/// terminal tailing the logs. Escaping (vs. stripping) keeps a real panic
/// message's line breaks readable.
fn log_safe(text: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_chars));
    // `.chars()` walks Unicode scalar values, so `take` can't cut "ä" in half
    // the way slicing by byte index could.
    for c in text.chars().take(max_chars) {
        if c.is_control() {
            // `escape_default` yields the escape as several chars ('\\', 'n');
            // `extend` appends them all.
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

/// `POST /api/client-error` — the app's panic hook beacons browser-side errors
/// here, and we re-emit them with `tracing::error!` so they ride the same
/// tracing → Sentry/GlitchTip pipeline as server errors.
///
/// The body is parsed by hand instead of with the `Json` extractor:
/// `navigator.sendBeacon` sends `text/plain`, which the extractor would
/// reject with a 415 before we ever saw the report.
///
/// Input is bounded at three points: the route's body limit (size), the fixed
/// two-field struct with `deny_unknown_fields` (shape), and the per-user
/// limiter (rate). What reaches the log is then length-capped and escaped.
async fn report_client_error(
    State(state): State<AppState>,
    user: CurrentUser,
    body: String,
) -> Result<StatusCode, ApiError> {
    let report: ClientErrorReport = serde_json::from_str(&body)
        .map_err(|_| ApiError::BadRequest("Virheellinen virheraportti.".to_owned()))?;

    // Over the limit: still 204. A beacon can't act on an error, and a client
    // in a crash loop retrying on 429 would only make the flood worse.
    if !state.client_error_limiter.allow(user.user_id) {
        tracing::debug!(user_id = user.user_id, "client error report throttled");
        return Ok(StatusCode::NO_CONTENT);
    }

    let message = log_safe(&report.message, 2000);
    let url = log_safe(&report.url.unwrap_or_default(), 500);
    tracing::error!(user_id = user.user_id, url = %url, "client error: {message}");
    Ok(StatusCode::NO_CONTENT)
}

/// The full auth surface with no rate limiting — used by tests. Production wires
/// the rate limiter onto `limited_router` in `main`.
#[cfg(test)]
pub fn router() -> Router<AppState> {
    limited_router().merge(general_router())
}

/// How many families exist? Zero means the server needs first-run setup.
async fn count_families(db: &crate::db::Db) -> Result<i64, ApiError> {
    let n = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM families"#)
        .fetch_one(db)
        .await?;
    Ok(n)
}

/// `GET /api/setup/status` — the app calls this on load to decide whether to
/// show the setup screen or the login screen.
async fn setup_status(State(state): State<AppState>) -> Result<Json<SetupStatus>, ApiError> {
    let needs_setup = count_families(&state.db).await? == 0;
    Ok(Json(SetupStatus { needs_setup }))
}

/// `POST /api/setup` — create the first family and its admin. Refuses once any
/// family exists, so this can't be used to hijack an existing install.
///
/// Also requires the one-time setup token printed in the server log, so that
/// whoever reaches a fresh install first can't claim it unless they can also
/// read the server's logs.
async fn setup(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<SetupRequest>,
) -> Result<(CookieJar, Json<UserView>), ApiError> {
    // Token first: a caller without it learns nothing, not even which field
    // would have failed validation.
    if !state.setup_gate.accepts(&req.setup_token) {
        tracing::warn!("setup attempt with an invalid setup token");
        return Err(ApiError::SetupTokenRejected);
    }
    validate_name(&req.family_name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_username(&req.username).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_name(&req.display_name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_password(&req.password).map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    // Hash before opening the transaction; Argon2 is deliberately slow and we
    // don't want to hold a write lock while it runs.
    let pw_hash = hash_in_blocking(req.password).await?;
    let now = OffsetDateTime::now_utc();
    let admin = Role::Admin.as_str();

    // A transaction so we never end up with a family that has no admin.
    // `BEGIN IMMEDIATE` takes SQLite's write lock up front instead of on the
    // first INSERT. Two concurrent setups are then strictly serialized: the
    // second waits (busy_timeout), sees the first family, and gets a 409 —
    // rather than both passing the count check and one failing with a 500.
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    if count_families_tx(&mut tx).await? > 0 {
        return Err(ApiError::Conflict("Asennus on jo suoritettu.".to_owned()));
    }
    let family_id = sqlx::query!(
        "INSERT INTO families (name, created_at) VALUES (?, ?)",
        req.family_name,
        now,
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    let user_id = sqlx::query!(
        "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        family_id,
        req.username,
        req.display_name,
        pw_hash,
        admin,
        now,
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    tx.commit().await?;

    let token = session::create(&state.db, user_id).await?;
    let view = UserView {
        id: user_id,
        family_id,
        username: req.username,
        display_name: req.display_name,
        role: Role::Admin,
    };
    Ok((
        jar.add(session::cookie(token, state.cookie_secure)),
        Json(view),
    ))
}

/// `POST /api/auth/login`.
async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<LoginRequest>,
) -> Result<(CookieJar, Json<UserView>), ApiError> {
    // Per-account backoff (see `throttle`): while locked, answer 429 without
    // even looking the user up. Applied to unknown usernames too, so the
    // lockout itself doesn't reveal which accounts exist.
    //
    // `begin` checks AND reserves in one step, so concurrent requests can't
    // all slip past the check before any failure is recorded. The returned
    // `attempt` must be resolved below; if a `?` bails out first, dropping it
    // just releases the reservation.
    let attempt = match state.login_throttle.begin(&req.username, Instant::now()) {
        Ok(attempt) => attempt,
        Err(wait) => {
            tracing::warn!(username = ?log_name(&req.username), "login attempt while locked out");
            // Round up so the client never retries a moment too early.
            return Err(ApiError::TooManyRequests(wait.as_secs_f64().ceil() as u64));
        }
    };
    // An over-long password can't be anyone's (setup/redeem cap it), so fail
    // it without spending Argon2 time on it.
    if req.password.chars().count() > MAX_PASSWORD_LEN {
        return Err(login_failed(attempt, &req.username));
    }

    // Username is unique *within* a family. Self-hosted installs have one
    // family, so this is 0 or 1 row; multi-family SaaS (Phase 8) will scope by
    // family. More than one match is treated as no match.
    let mut rows = sqlx::query!(
        r#"SELECT
               id           AS "id!: i64",
               family_id    AS "family_id!: i64",
               username     AS "username!: String",
               display_name AS "display_name!: String",
               role         AS "role!: String",
               pw_hash      AS "pw_hash!: String"
           FROM users WHERE username = ?"#,
        req.username,
    )
    .fetch_all(&state.db)
    .await?;

    let user = if rows.len() == 1 { rows.pop() } else { None };
    let stored_hash = user
        .as_ref()
        .map(|u| u.pw_hash.clone())
        .unwrap_or_else(|| DUMMY_HASH.clone());

    let ok = verify_in_blocking(req.password, stored_hash).await?;
    let (true, Some(user)) = (ok, user) else {
        return Err(login_failed(attempt, &req.username));
    };
    attempt.succeeded();

    let role = Role::from_db(&user.role).ok_or(ApiError::Internal)?;
    let token = session::create(&state.db, user.id).await?;
    let view = UserView {
        id: user.id,
        family_id: user.family_id,
        username: user.username,
        display_name: user.display_name,
        role,
    };
    Ok((
        jar.add(session::cookie(token, state.cookie_secure)),
        Json(view),
    ))
}

/// `POST /api/auth/logout` — delete the session and clear the cookie.
async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    if let Some(c) = jar.get(SESSION_COOKIE) {
        session::destroy(&state.db, c.value()).await?;
    }
    Ok((
        jar.add(session::clearing_cookie(state.cookie_secure)),
        StatusCode::NO_CONTENT,
    ))
}

/// `GET /api/me` — the current member. Requires a valid session (enforced by
/// the `CurrentUser` extractor).
async fn me(user: CurrentUser) -> Json<UserView> {
    Json(user.to_view())
}

/// `GET /api/family/members` — everyone in the caller's family. Scoped to
/// `family_id`, so one family can never see another's members.
async fn list_members(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<UserView>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               id           AS "id!: i64",
               family_id    AS "family_id!: i64",
               username     AS "username!: String",
               display_name AS "display_name!: String",
               role         AS "role!: String"
           FROM users WHERE family_id = ? ORDER BY created_at"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    let members = rows
        .into_iter()
        .filter_map(|r| {
            Some(UserView {
                id: r.id,
                family_id: r.family_id,
                username: r.username,
                display_name: r.display_name,
                role: Role::from_db(&r.role)?,
            })
        })
        .collect();
    Ok(Json(members))
}

/// `POST /api/family/invites` (admin only) — mint a single-use invite code.
/// The `AdminUser` extractor makes this 403 for non-admins automatically.
async fn create_invite(
    admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<CreateInviteRequest>,
) -> Result<Json<InviteResponse>, ApiError> {
    let code = generate_invite_code();
    let code_hash = hash_token(&canonicalize_invite_code(&code));
    let now = OffsetDateTime::now_utc();
    let expires_at = now + INVITE_TTL;
    let role = req.role.as_str();

    sqlx::query!(
        "INSERT INTO invites (family_id, code_hash, role, expires_at, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        admin.0.family_id,
        code_hash,
        role,
        expires_at,
        admin.0.user_id,
        now,
    )
    .execute(&state.db)
    .await?;

    let expires_str = expires_at
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;
    Ok(Json(InviteResponse {
        code,
        expires_at: expires_str,
    }))
}

/// `POST /api/auth/redeem` — a new member joins using an invite code. Their
/// role comes from the invite, never from the request, and the invite is
/// consumed atomically with the account creation.
async fn redeem(
    State(state): State<AppState>,
    jar: CookieJar,
    Json(req): Json<RedeemRequest>,
) -> Result<(CookieJar, Json<UserView>), ApiError> {
    validate_username(&req.username).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_name(&req.display_name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_password(&req.password).map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    let code_hash = hash_token(&canonicalize_invite_code(&req.code));

    // Check the invite BEFORE hashing the password: Argon2 is the expensive
    // part, and a wrong/used/expired code shouldn't get to spend it.
    // The result is discarded; the transaction below re-reads it.
    find_usable_invite(&state.db, &code_hash).await?;

    // Hash outside the transaction so Argon2's runtime doesn't extend a write.
    let pw_hash = hash_in_blocking(req.password).await?;
    let now = OffsetDateTime::now_utc();

    // `BEGIN IMMEDIATE`: take the write lock first (see `setup`), then re-check
    // the invite — it may have been used or expired while we were hashing.
    let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
    let invite = find_usable_invite(&mut *tx, &code_hash).await?;
    let role = Role::from_db(&invite.role).ok_or(ApiError::Internal)?;
    let role_str = role.as_str();

    // Consume the invite first. `used_at IS NULL` in the WHERE makes the
    // update itself the arbiter: if another request consumed it in between,
    // zero rows change and this request loses cleanly with a 409.
    let consumed = sqlx::query!(
        "UPDATE invites SET used_at = ? WHERE id = ? AND used_at IS NULL",
        now,
        invite.id
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if consumed != 1 {
        return Err(ApiError::Conflict("Tämä kutsu on jo käytetty.".to_owned()));
    }

    let insert = sqlx::query!(
        "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
        invite.family_id,
        req.username,
        req.display_name,
        pw_hash,
        role_str,
        now,
    )
    .execute(&mut *tx)
    .await;

    let user_id = match insert {
        Ok(r) => r.last_insert_rowid(),
        // The (family_id, username) UNIQUE index rejects a duplicate name.
        // Returning drops `tx` uncommitted, which rolls back the consume too.
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
            return Err(ApiError::Conflict(
                "Käyttäjänimi on jo käytössä.".to_owned(),
            ));
        }
        Err(e) => return Err(e.into()),
    };
    tx.commit().await?;

    let token = session::create(&state.db, user_id).await?;
    let view = UserView {
        id: user_id,
        family_id: invite.family_id,
        username: req.username,
        display_name: req.display_name,
        role,
    };
    Ok((
        jar.add(session::cookie(token, state.cookie_secure)),
        Json(view),
    ))
}

// --- helpers -------------------------------------------------------------

/// A username as it may appear in logs: capped, and printed with `?` (Debug)
/// by callers so control characters are escaped — it's untrusted input.
fn log_name(username: &str) -> String {
    username.chars().take(64).collect()
}

/// Record a failed login and return the (generic) error to send.
fn login_failed(attempt: Attempt<'_>, username: &str) -> ApiError {
    let failures = attempt.failed(Instant::now());
    // Never log the password. The username is useful: error tracking sees
    // which accounts are being guessed at.
    tracing::warn!(username = ?log_name(username), failures, "login failed");
    ApiError::LoginFailed
}

/// The invite row fields `redeem` needs.
struct UsableInvite {
    id: i64,
    family_id: i64,
    role: String,
}

/// Look up an invite by code hash and check it can still be redeemed: exists
/// (400), unused (409), unexpired (409).
///
/// Generic over the executor so the same query runs against the pool (the
/// cheap pre-check) and inside the transaction (the authoritative re-check).
/// `impl sqlx::SqliteExecutor<'_>` accepts both `&Db` and `&mut *tx`.
async fn find_usable_invite(
    db: impl sqlx::SqliteExecutor<'_>,
    code_hash: &str,
) -> Result<UsableInvite, ApiError> {
    let invite = sqlx::query!(
        r#"SELECT
               id         AS "id!: i64",
               family_id  AS "family_id!: i64",
               role       AS "role!: String",
               expires_at AS "expires_at!: OffsetDateTime",
               used_at    AS "used_at?: OffsetDateTime"
           FROM invites WHERE code_hash = ?"#,
        code_hash,
    )
    .fetch_optional(db)
    .await?;

    let Some(invite) = invite else {
        return Err(ApiError::BadRequest("Virheellinen kutsukoodi.".to_owned()));
    };
    if invite.used_at.is_some() {
        return Err(ApiError::Conflict("Tämä kutsu on jo käytetty.".to_owned()));
    }
    if invite.expires_at < OffsetDateTime::now_utc() {
        return Err(ApiError::Conflict("Tämä kutsu on vanhentunut.".to_owned()));
    }
    Ok(UsableInvite {
        id: invite.id,
        family_id: invite.family_id,
        role: invite.role,
    })
}

/// Wait for an Argon2 slot (see [`ARGON2_SLOTS`]).
async fn argon2_permit() -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    // Only fails if the semaphore is closed, which we never do.
    ARGON2_SLOTS
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::Internal)
}

/// Run Argon2 hashing on a blocking thread so it doesn't stall the async
/// runtime (Argon2 is intentionally CPU-heavy), bounded by [`ARGON2_SLOTS`].
async fn hash_in_blocking(password: String) -> Result<String, ApiError> {
    let permit = argon2_permit().await?;
    tokio::task::spawn_blocking(move || {
        // Moving the permit in ties its release to the end of this closure.
        let _permit = permit;
        hash_password(&password)
    })
    .await
    .map_err(|_| ApiError::Internal)?
    .map_err(|_| ApiError::Internal)
}

async fn verify_in_blocking(password: String, stored_hash: String) -> Result<bool, ApiError> {
    let permit = argon2_permit().await?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        verify_password(&password, &stored_hash)
    })
    .await
    .map_err(|_| ApiError::Internal)
}

/// `count(*)` of families inside an open transaction (used by `setup` to make
/// the "already set up?" check part of the same atomic unit).
async fn count_families_tx(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<i64, ApiError> {
    let n = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM families"#)
        .fetch_one(&mut **tx)
        .await?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt; // for `oneshot`

    /// Build the auth router backed by a fresh in-memory database, returning the
    /// pool too so a test can seed data directly (e.g. a second family).
    async fn app_with_db() -> (Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        (router().with_state(state), db)
    }

    /// The common case: just the router.
    async fn app() -> Router {
        app_with_db().await.0
    }

    fn post(path: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// POST with a session cookie attached.
    fn post_auth(path: &str, body: Value, cookie: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// GET with a session cookie attached.
    fn get_auth(path: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    /// The `name=value` pair from a response's Set-Cookie, ready to send back.
    fn cookie_pair(resp: &axum::response::Response) -> String {
        session_cookie(resp).split(';').next().unwrap().to_owned()
    }

    /// Run first-run setup and return the admin's session cookie.
    async fn setup_admin(app: &Router) -> String {
        let resp = app
            .clone()
            .oneshot(post("/api/setup", valid_setup()))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// Insert a second family with one member directly, bypassing the API, to
    /// test that one family can't see another's data. Uses runtime queries so
    /// it needs no offline cache entry.
    async fn seed_other_family(db: &crate::db::Db) {
        let now = OffsetDateTime::now_utc();
        let fid = sqlx::query("INSERT INTO families (name, created_at) VALUES (?, ?)")
            .bind("Other")
            .bind(now)
            .execute(db)
            .await
            .unwrap()
            .last_insert_rowid();
        let hash = crate::auth::hash_password("password1").unwrap();
        sqlx::query(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(fid)
        .bind("outsider")
        .bind("Outsider")
        .bind(hash)
        .bind("member")
        .bind(now)
        .execute(db)
        .await
        .unwrap();
    }

    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Pull the session cookie value out of a Set-Cookie header.
    fn session_cookie(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn valid_setup() -> Value {
        json!({
            "family_name": "Virtanen",
            "username": "mikko",
            "display_name": "Mikko",
            "password": "hunter2!"
        })
    }

    #[tokio::test]
    async fn fresh_server_needs_setup() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, json!({ "needs_setup": true }));
    }

    #[tokio::test]
    async fn setup_creates_admin_and_logs_in() {
        let app = app().await;
        let resp = app
            .oneshot(post("/api/setup", valid_setup()))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // A session cookie is set, and the returned user is an admin.
        assert!(session_cookie(&resp).contains("perkele_session="));
        let body = json_body(resp).await;
        assert_eq!(body["role"], "admin");
        assert_eq!(body["username"], "mikko");
    }

    #[tokio::test]
    async fn setup_is_refused_once_a_family_exists() {
        let app = app().await;
        app.clone()
            .oneshot(post("/api/setup", valid_setup()))
            .await
            .unwrap();
        // Second attempt must be rejected.
        let resp = app
            .oneshot(post(
                "/api/setup",
                json!({
                    "family_name": "Intruder",
                    "username": "mallory",
                    "display_name": "Mallory",
                    "password": "password1"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn setup_rejects_weak_password() {
        let resp = app()
            .await
            .oneshot(post(
                "/api/setup",
                json!({
                    "family_name": "Virtanen",
                    "username": "mikko",
                    "display_name": "Mikko",
                    "password": "short"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn login_succeeds_then_me_returns_the_user() {
        let app = app().await;
        app.clone()
            .oneshot(post("/api/setup", valid_setup()))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/login",
                json!({ "username": "mikko", "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookie = session_cookie(&resp);
        let cookie_pair = cookie.split(';').next().unwrap().to_owned();

        // `/api/me` with the cookie returns the logged-in member.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header(header::COOKIE, &cookie_pair)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["username"], "mikko");
    }

    #[tokio::test]
    async fn login_with_wrong_password_is_unauthorized() {
        let app = app().await;
        app.clone()
            .oneshot(post("/api/setup", valid_setup()))
            .await
            .unwrap();
        let resp = app
            .oneshot(post(
                "/api/auth/login",
                json!({ "username": "mikko", "password": "wrongpass" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn me_without_session_is_unauthorized() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // --- 1D: invites, redeem, members, roles ----------------------------

    /// Create an invite of the given role and return the plaintext code.
    async fn mint_invite(app: &Router, admin_cookie: &str, role: &str) -> String {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/family/invites",
                json!({ "role": role }),
                admin_cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        json_body(resp).await["code"].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn admin_invites_and_member_redeems_into_same_family() {
        let app = app().await;
        let admin = setup_admin(&app).await;
        let code = mint_invite(&app, &admin, "member").await;

        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/redeem",
                json!({
                    "code": code,
                    "username": "matti",
                    "display_name": "Matti",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        // Role comes from the invite, family from the inviter.
        assert_eq!(body["role"], "member");
        assert_eq!(body["family_id"], 1);
        assert_eq!(body["username"], "matti");
    }

    #[tokio::test]
    async fn non_admin_cannot_create_invites() {
        let app = app().await;
        let admin = setup_admin(&app).await;
        let code = mint_invite(&app, &admin, "member").await;

        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/redeem",
                json!({
                    "code": code,
                    "username": "matti",
                    "display_name": "Matti",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        let member = cookie_pair(&resp);

        // A plain member trying to mint an invite is forbidden.
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/family/invites",
                json!({ "role": "kid" }),
                &member,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_invite_code_works_only_once() {
        let app = app().await;
        let admin = setup_admin(&app).await;
        let code = mint_invite(&app, &admin, "member").await;

        let redeem = |username: &'static str| {
            let app = app.clone();
            let code = code.clone();
            async move {
                app.oneshot(post(
                    "/api/auth/redeem",
                    json!({
                        "code": code,
                        "username": username,
                        "display_name": username,
                        "password": "hunter2!"
                    }),
                ))
                .await
                .unwrap()
            }
        };

        assert_eq!(redeem("matti").await.status(), StatusCode::OK);
        // Second use of the same code (different username) is rejected.
        let resp = redeem("liisa").await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn redeem_rejects_an_unknown_code() {
        let app = app().await;
        let _admin = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/redeem",
                json!({
                    "code": "ZZZZ-ZZZZ",
                    "username": "matti",
                    "display_name": "Matti",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn members_list_is_scoped_to_the_callers_family() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await; // family 1: "mikko"
        seed_other_family(&db).await; // family 2: "outsider"

        let resp = app
            .clone()
            .oneshot(get_auth("/api/family/members", &admin))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let usernames: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["username"].as_str().unwrap())
            .collect();

        assert!(usernames.contains(&"mikko"));
        // The other family's member must NOT be visible.
        assert!(!usernames.contains(&"outsider"));
    }

    #[tokio::test]
    async fn client_error_requires_auth() {
        let app = app().await;
        let resp = app
            .oneshot(post(
                "/api/client-error",
                json!({ "message": "boom", "url": "/ostoslista" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn client_error_report_is_accepted() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(post_auth(
                "/api/client-error",
                json!({ "message": "panicked at 'index out of bounds'", "url": "/ostoslista" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    /// sendBeacon can't set Content-Type to application/json (it sends
    /// text/plain), so the endpoint must accept the body regardless of the
    /// declared content type.
    #[tokio::test]
    async fn client_error_accepts_text_plain_beacon_body() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/client-error")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from(
                        json!({ "message": "beacon boom", "url": null }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn client_error_rejects_malformed_body() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/client-error")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .header(header::COOKIE, &cookie)
                    .body(Body::from("not json at all"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn client_error_rejects_oversized_body_before_parsing() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        // Valid JSON, just far bigger than any real crash report.
        let huge = "x".repeat(CLIENT_ERROR_MAX_BODY + 1);
        let resp = app
            .oneshot(post_auth(
                "/api/client-error",
                json!({ "message": huge, "url": null }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn client_error_rejects_unknown_fields() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(post_auth(
                "/api/client-error",
                json!({ "message": "boom", "url": null, "extra": [1, 2, 3] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A crash loop (or a hostile account) firing reports nonstop must never
    /// see an error — the beacon can't act on one — but only the first
    /// `CLIENT_ERROR_BURST` may reach the logs.
    #[tokio::test]
    async fn client_error_flood_is_accepted_but_throttled() {
        let app = app().await;
        let cookie = setup_admin(&app).await;
        for i in 0..(CLIENT_ERROR_BURST * 4) {
            let resp = app
                .clone()
                .oneshot(post_auth(
                    "/api/client-error",
                    json!({ "message": format!("boom {i}"), "url": null }),
                    &cookie,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT, "report {i}");
        }
    }

    #[test]
    fn client_error_limiter_is_per_user() {
        let limiter = ClientErrorLimiter::default();
        for _ in 0..CLIENT_ERROR_BURST {
            assert!(limiter.allow(1));
        }
        assert!(!limiter.allow(1), "user 1 is over the burst");
        // Another user's crash reports are unaffected by user 1's flood.
        assert!(limiter.allow(2));
    }

    #[test]
    fn log_safe_escapes_control_chars_and_caps_length() {
        // A newline + fake log line, and an ANSI colour code: both must come
        // out as visible escapes, not as real line breaks / terminal codes.
        let forged = "boom\n2026-11-24T10:00:00Z  INFO login ok user=admin\u{1b}[31m";
        let safe = log_safe(forged, 2000);
        assert!(!safe.chars().any(char::is_control), "{safe:?}");
        assert!(
            safe.contains(r"\n2026") && safe.contains(r"\u{1b}[31m"),
            "{safe}"
        );
        // Ordinary text, including non-ASCII, passes through untouched.
        assert_eq!(
            log_safe("Virhe ostoslistassa: ä ö", 2000),
            "Virhe ostoslistassa: ä ö"
        );
        // The cap counts characters of the input, so it can't split "ä".
        assert_eq!(log_safe("ääää", 2), "ää");
    }

    // --- setup token ------------------------------------------------------

    const TOKEN: &str = "TEST-SETU-PTOK-EN42";

    /// Like `app()`, but setup requires `TOKEN` (the production behaviour).
    async fn gated_app() -> Router {
        let db = crate::db::test_pool().await;
        let mut state = AppState::for_test(db);
        state.setup_gate = crate::auth::SetupGate::from_env_or_random(Some(TOKEN))
            .unwrap()
            .0;
        router().with_state(state)
    }

    fn setup_with_token(token: Option<&str>) -> Value {
        let mut body = valid_setup();
        if let Some(t) = token {
            body["setup_token"] = json!(t);
        }
        body
    }

    #[tokio::test]
    async fn setup_without_or_with_wrong_token_is_forbidden() {
        let app = gated_app().await;
        for body in [
            setup_with_token(None),
            setup_with_token(Some("WRONG-TOKEN-XXXX")),
        ] {
            let resp = app.clone().oneshot(post("/api/setup", body)).await.unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        // Nothing was created: the server still needs setup.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_body(resp).await, json!({ "needs_setup": true }));
    }

    #[tokio::test]
    async fn setup_with_the_token_succeeds_forgivingly_typed() {
        let app = gated_app().await;
        let resp = app
            .oneshot(post(
                "/api/setup",
                setup_with_token(Some("test setu ptok en42")),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // --- per-account login backoff ------------------------------------------

    async fn login_status(app: &Router, username: &str, password: &str) -> StatusCode {
        app.clone()
            .oneshot(post(
                "/api/auth/login",
                json!({ "username": username, "password": password }),
            ))
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn account_locks_after_repeated_failures_even_for_the_right_password() {
        let app = app().await;
        setup_admin(&app).await;
        for _ in 0..crate::throttle::FREE_FAILURES {
            assert_eq!(
                login_status(&app, "mikko", "wrongpass").await,
                StatusCode::UNAUTHORIZED
            );
        }
        // Locked: even the correct password isn't checked right now.
        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/login",
                json!({ "username": "mikko", "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_some());
        assert!(json_body(resp).await["error"].is_string());
    }

    #[tokio::test]
    async fn unknown_usernames_lock_out_the_same_way() {
        // Otherwise "never locks" would reveal that the account doesn't exist.
        let app = app().await;
        setup_admin(&app).await;
        for _ in 0..crate::throttle::FREE_FAILURES {
            assert_eq!(
                login_status(&app, "ghost", "wrongpass").await,
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            login_status(&app, "ghost", "wrongpass").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        // A different account is unaffected.
        assert_eq!(
            login_status(&app, "mikko", "hunter2!").await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn successful_login_resets_the_failure_count() {
        let app = app().await;
        setup_admin(&app).await;
        for _ in 0..crate::throttle::FREE_FAILURES - 1 {
            login_status(&app, "mikko", "wrongpass").await;
        }
        assert_eq!(
            login_status(&app, "mikko", "hunter2!").await,
            StatusCode::OK
        );
        // Four more failures are again below the threshold.
        for _ in 0..crate::throttle::FREE_FAILURES - 1 {
            assert_eq!(
                login_status(&app, "mikko", "wrongpass").await,
                StatusCode::UNAUTHORIZED
            );
        }
    }

    /// Many wrong logins fired *at once* must not all get their password
    /// checked: the throttle reserves a slot before verifying, so at most
    /// `FREE_FAILURES` verifications can be in flight for one username.
    /// `multi_thread` matters: on the default single-threaded test runtime the
    /// requests would interleave less and could hide the race.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_wrong_logins_cannot_bypass_the_backoff() {
        let app = app().await;
        setup_admin(&app).await;
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..40 {
            let app = app.clone();
            tasks.spawn(async move { login_status(&app, "mikko", "wrongpass").await });
        }
        let (mut unauthorized, mut too_many) = (0, 0);
        while let Some(status) = tasks.join_next().await {
            match status.unwrap() {
                StatusCode::UNAUTHORIZED => unauthorized += 1,
                StatusCode::TOO_MANY_REQUESTS => too_many += 1,
                other => panic!("unexpected status {other}"),
            }
        }
        assert!(
            unauthorized <= crate::throttle::FREE_FAILURES,
            "{unauthorized} passwords were checked (max {})",
            crate::throttle::FREE_FAILURES
        );
        assert_eq!(unauthorized + too_many, 40);
    }

    #[tokio::test]
    async fn overlong_login_password_fails_without_error() {
        let app = app().await;
        setup_admin(&app).await;
        let long = "x".repeat(MAX_PASSWORD_LEN + 1);
        assert_eq!(
            login_status(&app, "mikko", &long).await,
            StatusCode::UNAUTHORIZED
        );
    }

    // --- redeem ordering / races -------------------------------------------

    #[tokio::test]
    async fn redeem_rejects_an_expired_invite() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        let code = mint_invite(&app, &admin, "member").await;
        // Age the invite past its expiry directly in the DB.
        let past = OffsetDateTime::now_utc() - Duration::days(1);
        sqlx::query("UPDATE invites SET expires_at = ?")
            .bind(past)
            .execute(&db)
            .await
            .unwrap();
        let resp = app
            .oneshot(post(
                "/api/auth/redeem",
                json!({
                    "code": code,
                    "username": "matti",
                    "display_name": "Matti",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn duplicate_username_on_redeem_leaves_the_invite_unused() {
        // The consume-UPDATE runs before the user INSERT; a failed INSERT must
        // roll it back so the invitee can retry with another name.
        let app = app().await;
        let admin = setup_admin(&app).await;
        let code = mint_invite(&app, &admin, "member").await;
        let redeem = |username: &'static str| {
            post(
                "/api/auth/redeem",
                json!({
                    "code": code,
                    "username": username,
                    "display_name": username,
                    "password": "hunter2!"
                }),
            )
        };
        let resp = app.clone().oneshot(redeem("mikko")).await.unwrap(); // taken by admin
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = app.clone().oneshot(redeem("matti")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Two setups racing on a real multi-connection pool (the in-memory test
    /// pool has one connection, which would serialize them for us). Exactly
    /// one wins; the loser must get a clean 409, never a 500.
    // `flavor = "multi_thread"` gives the test real worker threads, so the two
    // requests genuinely run at the same time instead of taking turns.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_setups_yield_one_success_and_one_conflict() {
        let path = std::env::temp_dir().join(format!(
            "perkele-setup-race-{}-{}.db",
            std::process::id(),
            OffsetDateTime::now_utc().unix_timestamp_nanos()
        ));
        let db = crate::db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let app = router().with_state(AppState::for_test(db.clone()));

        let mut second = valid_setup();
        second["username"] = json!("liisa");
        // `tokio::join!` polls both futures concurrently and waits for both.
        let (a, b) = tokio::join!(
            app.clone().oneshot(post("/api/setup", valid_setup())),
            app.clone().oneshot(post("/api/setup", second)),
        );
        let mut statuses = [a.unwrap().status(), b.unwrap().status()];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);

        db.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn argon2_concurrency_is_at_least_two() {
        assert!(argon2_permits() >= 2);
    }
}
