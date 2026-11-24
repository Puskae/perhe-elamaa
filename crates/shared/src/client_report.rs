//! Client-side error reports.
//!
//! The WASM app can't talk to GlitchTip directly (the Sentry Rust SDK doesn't
//! support wasm32, and shipping the DSN to browsers is undesirable anyway).
//! Instead the app posts a small report to `POST /api/client-error`, and the
//! server forwards it into its own error pipeline via `tracing::error!`.

use serde::{Deserialize, Serialize};

/// Body of `POST /api/client-error`.
///
/// Sent with `navigator.sendBeacon` from the app's panic hook, so it must stay
/// small and simple — a beacon is fire-and-forget and can't carry headers.
///
/// `deny_unknown_fields`: the server parses this from untrusted input, and the
/// app sends exactly these two fields (it uses this same struct), so anything
/// extra is not our client — reject it rather than silently ignore it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientErrorReport {
    /// Human-readable description (for a panic: the panic message).
    pub message: String,
    /// The app URL/route where the error happened, if known.
    pub url: Option<String>,
}
