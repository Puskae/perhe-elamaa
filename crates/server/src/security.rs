//! Response hardening: a middleware that stamps security headers onto every
//! response. The rate limiter lives in `ratelimit`.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// Content-Security-Policy for the WASM single-page app.
///
/// - `script-src 'wasm-unsafe-eval'` — required to instantiate WebAssembly.
/// - `connect-src 'self'` — XHR/fetch and the future SSE stream, same origin.
/// - `frame-ancestors 'none'` — can't be embedded in an iframe (clickjacking).
///
/// `style-src 'unsafe-inline'` is permitted for now because the framework emits
/// inline styles; revisit once the UI is in place (Phase 1F).
///
/// NEVER use `document::eval`, `js_sys::Function` or any other JS `eval` in
/// the app (crates/app). They are built on `new Function(...)`, which this
/// CSP blocks (no `'unsafe-eval'`) — and the resulting EvalError is thrown
/// inside a wasm-bindgen import, which kills the *whole* WASM app, not just
/// the one call. The same goes for `document::Title`: Dioxus web implements
/// `set_title` via eval. Use direct web-sys DOM calls instead. Do not loosen the CSP
/// to make eval work. Guarded by `app_never_uses_js_eval` below.
const CSP: &str = "default-src 'self'; \
     script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; \
     connect-src 'self'; \
     base-uri 'self'; \
     form-action 'self'; \
     frame-ancestors 'none'";

/// Cache policy chosen by request path:
/// - hashed assets (`/assets/…`, `/wasm/…`) have content-hashed names and never
///   change → cache them forever;
/// - API responses must never be cached;
/// - everything else is the HTML shell / SPA fallback → `no-cache` (revalidate
///   every load), so a redeployed app is picked up immediately instead of a
///   stale `index.html` pinning the browser to the old bundle.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") || path.starts_with("/wasm/") {
        "public, max-age=31536000, immutable"
    } else if path.starts_with("/api/") {
        "no-store"
    } else {
        "no-cache"
    }
}

/// HSTS: once a browser has seen this over HTTPS it refuses plain http for the
/// host for a year. Only sent when cookies are `Secure` (i.e. the deployment is
/// HTTPS) — sending it from a plain-http dev server would be pointless at best.
const HSTS: &str = "max-age=31536000";

/// Middleware that adds security headers to every response.
///
/// `State(https)` is the `cookie_secure` flag, handed in by
/// `from_fn_with_state` in `main` — middleware gets state the same way
/// handlers do.
pub async fn security_headers(State(https): State<bool>, req: Request, next: Next) -> Response {
    // Capture the path before the request is consumed; it decides caching below.
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    // Don't let browsers MIME-sniff responses into a different content type.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Belt-and-braces with the CSP frame-ancestors directive.
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Don't leak the app's URLs to other origins.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(&path)),
    );
    if https {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static(HSTS),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(https: bool) -> Router {
        Router::new().route("/", get(|| async { "hi" })).layer(
            axum::middleware::from_fn_with_state(https, security_headers),
        )
    }

    async fn get_root(app: Router) -> axum::response::Response {
        app.oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn middleware_sets_security_headers() {
        let resp = get_root(app(false)).await;
        let h = resp.headers();
        assert!(h.get(header::CONTENT_SECURITY_POLICY).is_some());
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        // The HTML shell must always revalidate so redeploys are picked up.
        assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-cache");
        // Plain-http dev: no HSTS.
        assert!(h.get(header::STRICT_TRANSPORT_SECURITY).is_none());
    }

    #[tokio::test]
    async fn hsts_is_sent_when_cookies_are_secure() {
        let resp = get_root(app(true)).await;
        assert_eq!(
            resp.headers()
                .get(header::STRICT_TRANSPORT_SECURITY)
                .unwrap(),
            "max-age=31536000"
        );
    }

    #[test]
    fn cache_control_policy_by_path() {
        assert_eq!(cache_control("/"), "no-cache");
        assert_eq!(cache_control("/login"), "no-cache"); // SPA route
        assert_eq!(
            cache_control("/assets/perkele-app-dxh123.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control("/wasm/perkele-app_bg.wasm"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("/api/me"), "no-store");
    }

    /// Is this source line a forbidden JS-eval use? Hardened against the easy
    /// bypasses of a plain substring scan: trailing comments, `document :: eval`
    /// spacing, and grouped imports (`use dioxus::document::{eval, Title};`).
    /// Heuristic (no parser), but it covers what code review would miss.
    fn forbidden_js(line: &str) -> bool {
        // Drop a trailing `// ...` comment. Naive about `//` inside string
        // literals (e.g. URLs) — that can only hide code *after* it on the line.
        let code = line.split_once("//").map_or(line, |(code, _)| code);
        // Normalise `document :: eval` → `document::eval` by deleting all
        // whitespace; Rust paths can't contain spaces anyway.
        let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();

        // `document::Title` is here too: Dioxus web's `set_title` is built on eval.
        // `Function::new` covers `new_no_args` / `new_with_args`.
        const FORBIDDEN: [&str; 4] = [
            "document::eval",
            "js_sys::Function",
            "document::Title",
            "Function::new",
        ];
        if FORBIDDEN.iter().any(|f| code.contains(f)) {
            return true;
        }

        // A bare `eval(` call (e.g. after `use ...::{eval}`), but not the tail
        // of a longer identifier like `retrieval(`. `match_indices` yields the
        // byte offset of each hit, so we can look at the char before it.
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        if code
            .match_indices("eval(")
            .any(|(i, _)| !code[..i].chars().next_back().is_some_and(is_ident))
        {
            return true;
        }

        // Grouped imports: `use dioxus::document::{eval, Title};` or
        // `use js_sys::{Date, Function};`. Whitespace is gone, so the group
        // is the text between `{` and `}` split on commas. `as` renames
        // (`Titleas T`, after whitespace removal) are caught by `starts_with`.
        let names = |prefix: &str| -> Vec<String> {
            code.split_once(prefix)
                .and_then(|(_, rest)| rest.split_once('}'))
                .map(|(group, _)| group.split(',').map(str::to_owned).collect())
                .unwrap_or_default()
        };
        if code.starts_with("use") || code.starts_with("pubuse") {
            let doc = names("document::{");
            if doc
                .iter()
                .any(|n| n.starts_with("eval") || n.starts_with("Title"))
            {
                return true;
            }
            if names("js_sys::{").iter().any(|n| n.starts_with("Function")) {
                return true;
            }
        }
        false
    }

    #[test]
    fn forbidden_js_classifier() {
        for bad in [
            "use dioxus::document::{eval, Title};",
            "use dioxus::document::{self, Title};",
            "    let r = eval(\"1+1\");",
            "document :: eval(\"x\")",
            "use js_sys::{Date, Function};",
            "let f = Function::new_no_args(\"x\");",
            "let f = Function::new_with_args(\"a\", \"a\");",
            "dioxus::document::Title { \"x\" }",
            "let f = js_sys::Function::from(v);",
        ] {
            assert!(forbidden_js(bad), "should flag: {bad}");
        }
        for ok in [
            "retrieval(x)",
            "// document::eval is banned",
            "foo(); // no document::eval",
            "use dioxus::document::{Stylesheet};",
            "use js_sys::{Date};",
            "let x = my_eval(1);",
        ] {
            assert!(!forbidden_js(ok), "should not flag: {ok}");
        }
    }

    /// Regression guard for the CSP note above: the app must never call
    /// JS eval (it crashes the whole WASM app under our CSP). A source scan
    /// (no new deps) over every `.rs` file in crates/app/src, line by line
    /// through [`forbidden_js`].
    #[test]
    fn app_never_uses_js_eval() {
        use std::path::{Path, PathBuf};

        // `concat!` + `env!` are expanded at compile time: CARGO_MANIFEST_DIR
        // is this crate's directory (crates/server), so this is crates/app/src.
        let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../app/src"));

        // Recursive walk with an explicit stack instead of recursion.
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        let mut scanned = 0;
        let mut offenders = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read app src dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    scanned += 1;
                    let src = std::fs::read_to_string(&path).expect("read .rs file");
                    for (n, line) in src.lines().enumerate() {
                        // Comments (incl. `///` docs) may mention the ban.
                        if line.trim_start().starts_with("//") {
                            continue;
                        }
                        if forbidden_js(line) {
                            offenders.push(format!(
                                "{}:{}: {}",
                                path.display(),
                                n + 1,
                                line.trim()
                            ));
                        }
                    }
                }
            }
        }
        // Guard against the scan silently passing because the path is wrong.
        assert!(scanned > 0, "no .rs files found under {}", root.display());
        assert!(
            offenders.is_empty(),
            "JS eval is blocked by the CSP and crashes the WASM app; use web-sys instead:\n{}",
            offenders.join("\n")
        );
    }
}
