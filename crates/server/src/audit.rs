//! Audit log (Muutosloki): a dedicated "who changed what, when" trail over the
//! seven shared-data domains. Separate table, separate concern from sync_log.
//!
//! `record` is the ONE write helper — called at each mutation site right after
//! the mutation succeeds, mirroring where `append_sync_log` is called today.
//! It stores the structured triple (op, entity, label); the frontend composes
//! the Finnish sentence, so no phrasing lives in the Rust handlers.

use crate::AppState;
use crate::error::ApiError;
use crate::session::AdminUser;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use perkele_shared::audit::{AuditEntry, DEFAULT_AUDIT_LIMIT, MAX_AUDIT_LIMIT};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Insert one audit row. `entity_id` is `None` for bulk ops (e.g. grocery
/// clear-checked logs ONE row, not N). `label` is the entity's display name
/// captured at write time, so a deleted item stays readable.
///
/// Consistency tradeoff (spec): this runs in the handler flow, so an insert
/// failure surfaces as a 500 — same as sync_log today. On local single-writer
/// SQLite it effectively never fails, and it keeps the pattern testable.
///
/// Generic over the sqlx `Executor`, so callers can pass either the pool
/// (`&state.db`, its own implicit transaction) or an open transaction
/// (`&mut *tx`) to make the audit row commit/roll back with the mutation.
pub(crate) async fn record<'e, E>(
    db: E,
    family_id: i64,
    actor_user_id: i64,
    entity: &str,
    entity_id: Option<i64>,
    op: &str,
    label: &str,
) -> Result<(), ApiError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let created_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;
    sqlx::query!(
        "INSERT INTO audit_log
            (family_id, actor_user_id, entity, entity_id, op, label, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        family_id,
        actor_user_id,
        entity,
        entity_id,
        op,
        label,
        created_at,
    )
    .execute(db)
    .await?;
    Ok(())
}

/// Query params for `GET /api/audit`. All optional; absent means "no filter"
/// (or, for `limit`/`before`, the documented defaults).
#[derive(Debug, Deserialize)]
struct AuditQuery {
    limit: Option<i64>,
    before: Option<i64>, // cursor: return rows with id < before
    entity: Option<String>,
    actor: Option<i64>, // actor_user_id filter
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/audit", get(list))
}

/// Admin-only, family-scoped, reverse-chronological, cursor-paginated audit
/// feed. `AdminUser` makes this a type-enforced 403 for non-admins.
async fn list(
    admin: AdminUser,
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> Result<Json<Vec<AuditEntry>>, ApiError> {
    let user = admin.0;
    // Clamp the page size to a sane window regardless of what the client asks.
    let limit = q
        .limit
        .unwrap_or(DEFAULT_AUDIT_LIMIT)
        .clamp(1, MAX_AUDIT_LIMIT);
    // Cursor: rows strictly older than `before` (by id). i64::MAX means "from
    // the top" so the first page needs no special-casing.
    let before = q.before.unwrap_or(i64::MAX);
    // NULL sentinels turn each optional filter into a no-op when absent:
    // `(? IS NULL OR col = ?)`. Bound TWICE with plain sequential `?`
    // placeholders below (see the doc comment on the query for why).
    let entity = q.entity.as_deref();
    let actor = q.actor;

    // sqlx note: tried the brief's numbered-param form (`?2`/`?4` reused from
    // an earlier bind) first. It COMPILES, but is silently wrong: SQLite
    // assigns anonymous `?` positions by left-to-right scan order in the SQL
    // text, so the plain `?` in `a.id < ?` already claims position 2 before
    // `?2` appears in the entity clause — `?2` ends up aliasing `before`
    // instead of `entity`. Confirmed via the endpoint tests: it built cleanly
    // but 2 of them failed (500s / wrong rows) instead of 404ing. Falling
    // back to the brief's documented alternative: bind each optional filter
    // value TWICE, once per `?` in `(? IS NULL OR col = ?)`, in plain
    // left-to-right positional order. Functionally identical, compiles AND
    // behaves correctly (verified below).
    let rows = sqlx::query!(
        r#"SELECT a.id            AS "id!: i64",
                  u.display_name  AS "actor_name!",
                  a.entity        AS "entity!",
                  a.entity_id     AS "entity_id?: i64",
                  a.op            AS "op!",
                  a.label         AS "label!",
                  a.created_at    AS "created_at!"
           FROM audit_log a
           JOIN users u ON u.id = a.actor_user_id
           WHERE a.family_id = ?
             AND a.id < ?
             AND (? IS NULL OR a.entity = ?)
             AND (? IS NULL OR a.actor_user_id = ?)
           ORDER BY a.id DESC
           LIMIT ?"#,
        user.family_id,
        before,
        entity,
        entity,
        actor,
        actor,
        limit,
    )
    .fetch_all(&state.db)
    .await?;

    let entries = rows
        .into_iter()
        .map(|r| AuditEntry {
            id: r.id,
            actor_name: r.actor_name,
            entity: r.entity,
            entity_id: r.entity_id,
            op: r.op,
            label: r.label,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(entries))
}

/// Delete audit rows older than the retention window. Called once a day by the
/// scheduler. `created_at` is RFC3339 UTC, so a lexical `<` against the RFC3339
/// cutoff is a correct chronological comparison (fixed-width, same zone).
pub(crate) async fn prune_once(
    db: &crate::db::Db,
    retention_days: i64,
    now: OffsetDateTime,
) -> Result<u64, ApiError> {
    let cutoff = (now - time::Duration::days(retention_days))
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;
    let deleted = sqlx::query!("DELETE FROM audit_log WHERE created_at < ?", cutoff)
        .execute(db)
        .await?
        .rows_affected();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_inserts_a_row_with_the_given_fields() {
        let db = crate::db::test_pool().await;
        // Minimal FK parents: a family and a user.
        // NOTE: adjusted from the brief's literal INSERTs to match the real
        // schema (crates/server/migrations/0001_init.sql):
        //   - families.created_at is NOT NULL, so it must be supplied here.
        //   - users has no `password_hash` column; the real column is
        //     `pw_hash`. This is just a valid FK parent row, not a real
        //     password.
        sqlx::query!(
            "INSERT INTO families (id, name, created_at) VALUES (1, 'Testiperhe', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO users (id, family_id, username, display_name, pw_hash, role, created_at)
             VALUES (1, 1, 'aino', 'Aino', 'x', 'admin', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();

        record(&db, 1, 1, "grocery_item", Some(42), "delete", "Maito")
            .await
            .unwrap();

        let row = sqlx::query!(
            r#"SELECT entity AS "entity!", entity_id AS "entity_id?: i64",
                      op AS "op!", label AS "label!", actor_user_id AS "actor_user_id!: i64"
               FROM audit_log WHERE family_id = 1"#
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(row.entity, "grocery_item");
        assert_eq!(row.entity_id, Some(42));
        assert_eq!(row.op, "delete");
        assert_eq!(row.label, "Maito");
        assert_eq!(row.actor_user_id, 1);
    }

    #[tokio::test]
    async fn record_allows_null_entity_id_for_bulk_ops() {
        let db = crate::db::test_pool().await;
        sqlx::query!(
            "INSERT INTO families (id, name, created_at) VALUES (1, 'Testiperhe', '2026-07-23T00:00:00Z')"
        )
        .execute(&db).await.unwrap();
        sqlx::query!(
            "INSERT INTO users (id, family_id, username, display_name, pw_hash, role, created_at)
             VALUES (1, 1, 'aino', 'Aino', 'x', 'admin', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();

        record(&db, 1, 1, "grocery_item", None, "delete", "3 ostosta")
            .await
            .unwrap();

        let entity_id = sqlx::query_scalar!(
            r#"SELECT entity_id AS "entity_id?: i64" FROM audit_log WHERE family_id = 1"#
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(entity_id, None);
    }

    #[tokio::test]
    async fn prune_deletes_old_rows_and_keeps_recent() {
        use time::Duration;
        let db = crate::db::test_pool().await;
        sqlx::query!(
            "INSERT INTO families (id, name, created_at) VALUES (1, 'Testiperhe', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO users (id, family_id, username, display_name, pw_hash, role, created_at)
             VALUES (1, 1, 'aino', 'Aino', 'x', 'admin', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();

        let now = OffsetDateTime::now_utc();
        let old = (now - Duration::days(100)).format(&Rfc3339).unwrap();
        let fresh = (now - Duration::days(10)).format(&Rfc3339).unwrap();
        for ts in [&old, &fresh] {
            sqlx::query!(
                "INSERT INTO audit_log (family_id, actor_user_id, entity, entity_id, op, label, created_at)
                 VALUES (1, 1, 'grocery_item', NULL, 'create', 'x', ?)", ts
            ).execute(&db).await.unwrap();
        }

        let deleted = prune_once(&db, 90, now).await.unwrap();
        assert_eq!(deleted, 1); // the 100-day-old row

        let remaining = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "c!: i64" FROM audit_log"#)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(remaining, 1); // the 10-day-old row survives
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Router + pool. In-memory SQLite lives in ONE connection: seeding must
    /// use this pool, never a second one. Mirrors `announcement::tests::board_app_with_db`.
    async fn audit_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    fn req(method: &str, path: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
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

    /// First-run setup: creates family "Virtanen" + admin "mikko". Only ONE family
    /// can be created this way per test DB — the setup handler is gated to
    /// run once globally (see `routes::setup`), so a second family for
    /// scoping tests must be seeded directly with SQL (see `seed_family`).
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

    /// Admin mints a member invite, "matti" redeems it; returns matti's cookie.
    async fn join_member(app: &axum::Router, admin: &str) -> String {
        let resp = app
            .clone()
            .oneshot(req_with_body(
                "POST",
                "/api/family/invites",
                admin,
                json!({ "role": "member" }),
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

    fn req_with_body(method: &str, path: &str, cookie: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, cookie)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// Seeds a second family + admin user directly via SQL (bypassing the
    /// once-only `/api/setup` gate) so scoping tests have two tenants.
    async fn seed_family(db: &crate::db::Db, family_id: i64, family_name: &str, user_id: i64) {
        sqlx::query!(
            "INSERT INTO families (id, name, created_at) VALUES (?, ?, '2026-07-23T00:00:00Z')",
            family_id,
            family_name,
        )
        .execute(db)
        .await
        .unwrap();
        sqlx::query!(
            "INSERT INTO users (id, family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, ?, 'seeduser', 'Seed', 'x', 'admin', '2026-07-23T00:00:00Z')",
            user_id,
            family_id,
        )
        .execute(db)
        .await
        .unwrap();
    }

    /// Inserts one audit row directly (bypassing `record`, which is fine —
    /// these tests exercise the read side). Returns the new row's id.
    async fn seed_row(
        db: &crate::db::Db,
        family_id: i64,
        actor_user_id: i64,
        entity: &str,
        label: &str,
    ) -> i64 {
        sqlx::query!(
            "INSERT INTO audit_log (family_id, actor_user_id, entity, entity_id, op, label, created_at)
             VALUES (?, ?, ?, NULL, 'delete', ?, '2026-07-23T00:00:00Z')",
            family_id,
            actor_user_id,
            entity,
            label,
        )
        .execute(db)
        .await
        .unwrap()
        .last_insert_rowid()
    }

    #[tokio::test]
    async fn non_admin_gets_403() {
        let (app, _db) = audit_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;

        let resp = app
            .oneshot(req("GET", "/api/audit", &member))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn results_are_family_scoped() {
        let (app, db) = audit_app_with_db().await;
        let admin = setup_admin(&app).await;
        // Family A is whatever /api/setup created (id 1, admin user id 1).
        seed_row(&db, 1, 1, "grocery_item", "Family A row").await;
        // Family B is seeded directly.
        seed_family(&db, 2, "Toinen perhe", 2).await;
        seed_row(&db, 2, 2, "grocery_item", "Family B row").await;

        let resp = app.oneshot(req("GET", "/api/audit", &admin)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let entries = body.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["label"], "Family A row");
    }

    #[tokio::test]
    async fn pagination_before_cursor_returns_next_page() {
        let (app, db) = audit_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id1 = seed_row(&db, 1, 1, "grocery_item", "first").await;
        let id2 = seed_row(&db, 1, 1, "grocery_item", "second").await;
        let id3 = seed_row(&db, 1, 1, "grocery_item", "third").await;

        let resp = app
            .clone()
            .oneshot(req("GET", "/api/audit?limit=2", &admin))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let entries = body.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        // Newest first: id3 then id2.
        assert_eq!(entries[0]["id"], id3);
        assert_eq!(entries[1]["id"], id2);

        let resp = app
            .oneshot(req("GET", &format!("/api/audit?before={id2}"), &admin))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let entries = body.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], id1);
    }

    #[tokio::test]
    async fn entity_and_actor_filters_narrow_results() {
        let (app, db) = audit_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member_cookie = join_member(&app, &admin).await;
        // Find matti's user_id via /api/me so we can filter by actor.
        let me_resp = app
            .clone()
            .oneshot(req("GET", "/api/me", &member_cookie))
            .await
            .unwrap();
        let matti_id = json_body(me_resp).await["id"].as_i64().unwrap();

        seed_row(&db, 1, 1, "grocery_item", "admin grocery").await;
        seed_row(&db, 1, 1, "recipe", "admin recipe").await;
        seed_row(&db, 1, matti_id, "grocery_item", "matti grocery").await;

        // entity filter
        let resp = app
            .clone()
            .oneshot(req("GET", "/api/audit?entity=recipe", &admin))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let entries = body.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["label"], "admin recipe");

        // actor filter
        let resp = app
            .oneshot(req("GET", &format!("/api/audit?actor={matti_id}"), &admin))
            .await
            .unwrap();
        let body = json_body(resp).await;
        let entries = body.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["label"], "matti grocery");
    }
}
