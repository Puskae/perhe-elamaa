use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router, routing::any, routing::get};
use perkele_shared::{ApiHealth, ErrorResponse};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

mod aisle;
mod announcement;
mod audit;
mod auth;
mod calendar;
mod chat;
mod chore;
mod chore_stats;
mod db;
mod error;
mod feature;
mod grocery;
mod note;
mod push;
mod pwa;
mod ratelimit;
mod recipe;
mod reminder;
mod routes;
mod security;
mod seed;
mod session;
mod sync;
mod throttle;

use auth::SetupGate;
use db::Db;
use ratelimit::ClientIpConfig;
use throttle::LoginThrottle;

#[derive(Clone)]
struct AppState {
    db: Db,
    cookie_secure: bool,
    /// Per-family SSE broadcaster shared by all entities (grocery, calendar, …).
    /// Handlers send `()` pokes; each entity's SSE endpoint subscribes. Created
    /// lazily when the first SSE client connects for a family.
    sync_tx: Arc<Mutex<HashMap<i64, tokio::sync::broadcast::Sender<()>>>>,
    /// The one Web Push transport, built at startup and shared by the test
    /// endpoint, announcements and the reminder scheduler. `Arc` makes the
    /// `#[derive(Clone)]` above a cheap refcount bump, not a new HTTP client.
    pusher: Arc<push::WebPushPusher>,
    /// Who may run first-run setup (the one-time token from the startup log).
    setup_gate: SetupGate,
    /// Per-username login backoff, shared by every request.
    login_throttle: Arc<LoginThrottle>,
}

#[cfg(test)]
impl AppState {
    /// Construct a test state with an empty grocery broadcaster. Callers in
    /// tests use this instead of writing out the struct literal so adding
    /// fields here doesn't break every test at once.
    pub fn for_test(db: Db) -> Self {
        Self {
            db,
            cookie_secure: false,
            sync_tx: Arc::new(Mutex::new(HashMap::new())),
            pusher: push::test_support::shared_pusher(),
            // Tests call /api/setup without a token (see `SetupGate::Open`).
            setup_gate: SetupGate::Open,
            login_throttle: Arc::new(LoginThrottle::default()),
        }
    }
}

async fn health() -> Json<ApiHealth> {
    Json(ApiHealth::current())
}

fn warn_if_bundle_missing(dist_dir: &str) {
    let index = std::path::Path::new(dist_dir).join("index.html");
    if !index.exists() {
        let abs = std::path::absolute(&index).unwrap_or(index);
        tracing::warn!(
            "web bundle not found at {} — page requests will return an empty 404 \
             (Safari downloads these as a 0-byte file). Build it with \
             `cd crates/app && dx build --release`, run the server from the repo \
             root, or set PERKELE_DIST to the bundle directory.",
            abs.display(),
        );
    }
}

/// `/api/*` paths that match no route. Without this they would fall through
/// to the SPA fallback and get `index.html` with a 200 — confusing for API
/// clients and for the app's own error handling.
async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(ErrorResponse {
            error: "Ei löydy.".to_owned(),
        }),
    )
}

/// The whole app. Also returns the rate limiter's cleanup function, which
/// `run` calls periodically (see `ratelimit::apply`).
fn router(
    state: AppState,
    dist_dir: &str,
    ip_config: Arc<ClientIpConfig>,
) -> (Router, impl Fn() + Send + Sync + 'static) {
    let spa = ServeDir::new(dist_dir).fallback(ServeFile::new(format!("{dist_dir}/index.html")));

    let (limited, rl_cleanup) = ratelimit::apply(routes::limited_router(), ip_config);
    let api = limited
        .merge(routes::general_router())
        .merge(grocery::router())
        .merge(aisle::router())
        .merge(recipe::router())
        .merge(calendar::router())
        .merge(push::router())
        .merge(announcement::router())
        .merge(chat::router())
        .merge(chore::router())
        .merge(chore_stats::router())
        .merge(note::router())
        .merge(audit::router())
        .merge(feature::router());

    let cookie_secure = state.cookie_secure;
    let app = Router::new()
        .route("/api/health", get(health))
        // `{*rest}` is a catch-all; the router always prefers a more specific
        // route, so this only answers /api paths nothing else claimed.
        .route("/api", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .merge(api)
        .merge(pwa::router())
        .fallback_service(spa)
        .layer(axum::middleware::from_fn_with_state(
            cookie_secure,
            security::security_headers,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state);
    (app, rl_cleanup)
}

fn main() {
    // Error reporting to GlitchTip (Sentry protocol). Disabled unless
    // PERKELE_SENTRY_DSN is set, so local dev stays quiet. The guard must
    // live for the whole program: dropping it flushes queued events, and
    // Sentry wants to be initialized *before* the async runtime starts so
    // its transport thread isn't parented to a tokio worker — that's why
    // this is `fn main` + explicit runtime instead of `#[tokio::main]`.
    // Empty/whitespace counts as unset: compose.yaml always passes the variable
    // (`${PERKELE_SENTRY_DSN:-}`), so "" is the normal "disabled" value there.
    let dsn = std::env::var("PERKELE_SENTRY_DSN")
        .ok()
        .filter(|d| !d.trim().is_empty());
    let _sentry = sentry::init(sentry::ClientOptions {
        dsn: dsn.and_then(|d| match d.parse() {
            Ok(dsn) => Some(dsn),
            Err(e) => {
                eprintln!("ignoring invalid PERKELE_SENTRY_DSN: {e}");
                None
            }
        }),
        release: sentry::release_name!(),
        ..Default::default()
    });

    // Layered subscriber instead of the plain fmt() builder: the Sentry layer
    // rides alongside the console output. With it, every `tracing::error!`
    // becomes a GlitchTip event and warn/info lines become breadcrumbs.
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(sentry::integrations::tracing::layer())
        .init();

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime")
        .block_on(run());
}

async fn run() {
    let dist_dir = std::env::var("PERKELE_DIST")
        .unwrap_or_else(|_| "target/dx/perkele-app/release/web/public".into());
    let addr = std::env::var("PERKELE_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let db_url = std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://perkele.db".into());
    let cookie_secure = std::env::var("PERKELE_COOKIE_SECURE")
        .map(|v| v != "false")
        .unwrap_or(true);
    // Parsed once, up front: a typo here should stop the server, not quietly
    // key the rate limiter on the wrong address.
    let ip_config = Arc::new(ClientIpConfig::from_env().unwrap_or_else(|e| panic!("{e}")));
    tracing::info!(
        "rate limiting by client IP from {:?} (trusted proxies: {} ranges)",
        ip_config.source,
        ip_config.trusted_proxies.len(),
    );

    warn_if_bundle_missing(&dist_dir);

    let db = db::connect(&db_url)
        .await
        .unwrap_or_else(|e| panic!("failed to open database {db_url}: {e}"));
    tracing::info!("database ready at {db_url}");

    // Seed default recipes into any family that has never had one.
    // Failure is fatal, like a failed migration: the per-family transaction
    // prevents half-seeded families, and failing loudly beats silently
    // starting without the defaults.
    seed::seed_default_recipes(&db)
        .await
        .unwrap_or_else(|e| panic!("failed to seed default recipes: {e:?}"));

    // One push transport for the whole process (VAPID key generated and
    // persisted on first boot). Fatal on failure, like seeding: this only
    // fails on a DB error or OS resource exhaustion.
    let vapid_private = push::ensure_vapid_private_key(&db)
        .await
        .unwrap_or_else(|e| panic!("failed to load VAPID key: {e:?}"));
    let pusher = Arc::new(
        push::WebPushPusher::new(vapid_private)
            .unwrap_or_else(|e| panic!("failed to create push client: {e:?}")),
    );

    let setup_gate = setup_gate_for(&db).await;
    routes::prime_dummy_hash().await;

    let state = AppState {
        db,
        cookie_secure,
        sync_tx: Arc::new(Mutex::new(HashMap::new())),
        pusher,
        setup_gate,
        login_throttle: Arc::new(LoginThrottle::default()),
    };

    // Background reminder scheduler (Phase 5C): 60 s scan-and-push loop.
    // Reminders compare floating wall-clock event times against the process's
    // LOCAL time, so the server MUST run in the family's timezone. Log the
    // resolved offset at startup: a container stuck on UTC (offset +00:00 in
    // Helsinki) is the tell that reminders will fire hours late.
    let now = chrono::Local::now();
    tracing::info!(
        "local time {} (UTC offset {}) — reminders assume this is the family's timezone",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.offset(),
    );
    // Audit-log retention window (Muutosloki): how many days of history the
    // daily prune keeps. Non-positive or unparseable values fall back to the
    // 90-day default rather than disabling retention entirely.
    let audit_retention_days = std::env::var("PERKELE_AUDIT_RETENTION_DAYS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|d| *d > 0)
        .unwrap_or(90);
    reminder::spawn(state.db.clone(), state.pusher.clone(), audit_retention_days);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));
    tracing::info!("Perhe-elämää listening on http://{addr}, serving app from {dist_dir}");

    let (app, rl_cleanup) = router(state, &dist_dir, ip_config);
    // Forget per-client rate-limit buckets that have fully refilled, so the
    // map of seen IPs doesn't grow for the life of the process.
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tick.tick().await;
            rl_cleanup();
        }
    });

    // `into_make_service_with_connect_info` records each connection's peer
    // address in the request extensions (`ConnectInfo<SocketAddr>`) — the
    // rate limiter keys on it.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .expect("server crashed");
}

/// Decide the first-run setup gate. While no family exists, print the token
/// prominently: the operator copies it from the log into the setup screen.
/// Once set up, setup is refused anyway (409); the gate then holds a random
/// token that is never shown.
async fn setup_gate_for(db: &Db) -> SetupGate {
    let families: i64 = sqlx::query_scalar!(r#"SELECT count(*) AS "n!: i64" FROM families"#)
        .fetch_one(db)
        .await
        .unwrap_or_else(|e| panic!("failed to read families: {e}"));
    if families > 0 {
        return SetupGate::from_env_or_random(None)
            .expect("random token is always valid")
            .0;
    }
    let env = std::env::var("PERKELE_SETUP_TOKEN").ok();
    let (gate, shown) =
        SetupGate::from_env_or_random(env.as_deref()).unwrap_or_else(|e| panic!("{e}"));
    let from_env = env.as_deref().is_some_and(|t| !t.trim().is_empty());
    // The token goes to stderr with `eprintln!`, NOT through `tracing`: the
    // Sentry tracing layer turns INFO events into breadcrumbs, which would
    // ship the token to the error tracker with the next reported error.
    // stderr still shows up in `docker compose logs`.
    eprintln!("{}", setup_banner(&shown, from_env));
    // (Worded so it does NOT match the docs' `grep "setup token"`.)
    tracing::info!("first-run setup pending; the token is printed to stderr, not logged");
    gate
}

/// The first-run banner. Must keep containing the phrase "setup token": the
/// docs tell operators to `grep -a "setup token"` the container logs.
fn setup_banner(shown: &str, from_env: bool) -> String {
    let rule = "=".repeat(64);
    if from_env {
        format!(
            "\n{rule}\n  Perhe-elämää needs first-run setup. Setup token: from PERKELE_SETUP_TOKEN\n{rule}"
        )
    } else {
        format!(
            "\n{rule}\n  Perhe-elämää setup token: {shown} — enter it on the setup screen\n  \
             (new token on every restart until setup is done)\n{rule}"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use tower::ServiceExt;

    async fn app() -> Router {
        let state = AppState::for_test(db::test_pool().await);
        let ip = Arc::new(ClientIpConfig::parse(None, None).unwrap());
        // A dist dir that doesn't exist: page requests 404 with an empty body,
        // which is enough to tell them apart from the JSON API 404.
        router(state, "/nonexistent-perkele-dist", ip).0
    }

    #[test]
    fn setup_banner_carries_the_token_and_the_documented_grep_phrase() {
        let b = setup_banner("ABCD-EFGH-JKLM-NPQR", false);
        // docs/deploy.md: `docker compose logs perkele | grep -a "setup token"`
        let line = b.lines().find(|l| l.contains("setup token")).unwrap();
        assert!(line.contains("ABCD-EFGH-JKLM-NPQR"), "{line}");
        // From the env var: the token itself is never echoed.
        assert!(!setup_banner("SECRET-TOKEN", true).contains("SECRET-TOKEN"));
    }

    #[tokio::test]
    async fn unknown_api_path_is_a_json_404() {
        for path in ["/api/nope", "/api/grocery/nope/deeper", "/api"] {
            let resp = app()
                .await
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(
                resp.headers().get(header::CONTENT_TYPE).unwrap(),
                "application/json",
                "{path}"
            );
        }
    }

    #[tokio::test]
    async fn known_api_routes_still_win_over_the_catch_all() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // Wrong method on a real route stays a 405, not a 404.
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/auth/login")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn non_api_paths_go_to_the_spa_fallback() {
        let resp = app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/ostoslista")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // No bundle in the test, so ServeDir's empty 404 — but NOT JSON.
        assert!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .is_none_or(|v| v != "application/json")
        );
    }

    #[tokio::test]
    async fn health_reports_current_version() {
        let Json(body) = health().await;
        assert_eq!(body, ApiHealth::current());
    }
}
