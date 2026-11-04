//! Audit-log DTOs shared between server and app.
//!
//! WASM-safe: no sqlx/tokio/time here. `created_at` is an RFC3339 UTC String,
//! same as every other shared DTO that carries a timestamp.

use serde::{Deserialize, Serialize};

/// The valid `entity` values, in the order they appear in the filter chips.
/// Kept here so the frontend renders the filter list from the same source the
/// server writes.
pub const ENTITY_KINDS: [&str; 9] = [
    "calendar_event",
    "grocery_item",
    "recipe",
    "dinner",
    "chore",
    "announcement",
    "aisle",
    "note",
    "feature", // admin's "Näkyvät toiminnot" changes (no entity id)
];

/// Default page size for `GET /api/audit` when `limit` is absent.
pub const DEFAULT_AUDIT_LIMIT: i64 = 50;
/// Upper bound so a caller can't ask for the whole table in one request.
pub const MAX_AUDIT_LIMIT: i64 = 200;

/// One row of the audit log as the client sees it: the actor's display name is
/// joined in server-side, and the structured triple (op, entity, label) is
/// turned into a Finnish sentence by the frontend formatter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub actor_name: String,
    pub entity: String,
    pub entity_id: Option<i64>,
    pub op: String,
    pub label: String,
    pub created_at: String, // RFC3339 UTC, matching existing shared DTOs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_entry_serde_round_trips() {
        let entry = AuditEntry {
            id: 7,
            actor_name: "Aino".to_owned(),
            entity: "grocery_item".to_owned(),
            entity_id: Some(42),
            op: "delete".to_owned(),
            label: "Maito".to_owned(),
            created_at: "2026-07-23T09:00:00Z".to_owned(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    #[test]
    fn entity_kinds_covers_all_domains() {
        assert_eq!(ENTITY_KINDS.len(), 9);
    }
}
