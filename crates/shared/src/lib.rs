//! Domain types and DTOs shared between the server and the app.
//!
//! Everything that crosses the API boundary is defined here, so the client
//! and server can never drift apart.

use serde::{Deserialize, Serialize};

pub mod aisle;
pub mod announcement;
pub mod audit;
pub mod auth;
pub mod calendar;
pub mod chat;
pub mod chore;
pub mod client_report;
pub mod dates;
pub mod feature;
pub mod grocery;
pub mod note;
pub mod push;
pub mod recipe;
pub mod recur;

/// Response of `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiHealth {
    pub service: String,
    pub version: String,
}

impl ApiHealth {
    pub fn current() -> Self {
        Self {
            service: "perkele".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// The JSON body the server returns for any failed request: `{ "error": "…" }`.
/// Defined here so the client deserializes exactly what the server serializes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_health_serde_round_trip() {
        let health = ApiHealth::current();
        let json = serde_json::to_string(&health).unwrap();
        let back: ApiHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(health, back);
    }
}
