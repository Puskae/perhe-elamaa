//! Admin-hideable features (Asetukset → "Näkyvät osiot").
//!
//! The family's set of HIDDEN features lives in `family_hidden_features`
//! (one row per hidden feature; no row = shown). Everyone reads it via
//! `GET /api/family/features`; only an admin may replace it via `PUT`.
//! The push senders (reminders, chores, announcements) call [`is_hidden`]
//! to stay quiet for a hidden feature. Data is never touched.

use crate::AppState;
use crate::error::ApiError;
use crate::session::{AdminUser, CurrentUser};
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use perkele_shared::feature::{FamilyFeatures, Feature};

pub fn router() -> Router<AppState> {
    // One path, two methods: `get(..).put(..)` chains a MethodRouter.
    Router::new().route("/api/family/features", get(get_features).put(put_features))
}

/// The family's hidden set, normalized (enum order, no duplicates).
/// Unknown stored keys — e.g. a feature added by a newer version and then
/// rolled back — are skipped rather than failing the whole read.
///
/// Generic over the sqlx `Executor` (like `audit::record`), so it can read
/// through the pool or inside an open transaction (`&mut *tx`).
pub async fn hidden_features<'e, E>(db: E, family_id: i64) -> Result<FamilyFeatures, ApiError>
where
    E: sqlx::Executor<'e, Database = sqlx::Sqlite>,
{
    let keys = sqlx::query_scalar!(
        "SELECT feature FROM family_hidden_features WHERE family_id = ?",
        family_id,
    )
    .fetch_all(db)
    .await?;
    // `filter_map` keeps only the `Some(..)` results of `from_key`.
    let hidden = keys.iter().filter_map(|k| Feature::from_key(k)).collect();
    Ok(FamilyFeatures { hidden }.normalized())
}

/// Is `feature` hidden for this family? Used by the push senders.
pub async fn is_hidden(
    db: &crate::db::Db,
    family_id: i64,
    feature: Feature,
) -> Result<bool, ApiError> {
    let key = feature.key();
    let n = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM family_hidden_features
           WHERE family_id = ? AND feature = ?"#,
        family_id,
        key,
    )
    .fetch_one(db)
    .await?;
    Ok(n > 0)
}

/// Test helper (the PUT handler uses `write_hidden` in its own tx): replace
/// the family's hidden set with `hidden` in ONE transaction, so a
/// concurrent reader never sees a half-written (e.g. empty) set.
#[cfg(test)]
pub(crate) async fn replace_hidden(
    db: &crate::db::Db,
    family_id: i64,
    hidden: &[Feature],
) -> Result<(), ApiError> {
    let mut tx = db.begin().await?;
    write_hidden(&mut tx, family_id, hidden).await?;
    tx.commit().await?;
    Ok(())
}

/// Delete + re-insert the hidden set on an ALREADY OPEN connection. The
/// caller owns the transaction (and decides when to commit), so other
/// writes — the audit row — can join the same one.
async fn write_hidden(
    conn: &mut sqlx::SqliteConnection,
    family_id: i64,
    hidden: &[Feature],
) -> Result<(), ApiError> {
    sqlx::query!(
        "DELETE FROM family_hidden_features WHERE family_id = ?",
        family_id
    )
    // `&mut *conn`: reborrow so `conn` stays usable for the next query.
    .execute(&mut *conn)
    .await?;
    for f in hidden {
        let key = f.key();
        sqlx::query!(
            "INSERT INTO family_hidden_features (family_id, feature) VALUES (?, ?)",
            family_id,
            key,
        )
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

async fn get_features(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<FamilyFeatures>, ApiError> {
    Ok(Json(hidden_features(&state.db, user.family_id).await?))
}

/// Admin-only (`AdminUser` → 403 for members). A body with an unknown
/// feature key fails `Json`'s deserialization before this runs, which axum
/// answers with 422 Unprocessable Entity (the crate's default rejection —
/// this codebase doesn't customize it).
async fn put_features(
    admin: AdminUser,
    State(state): State<AppState>,
    Json(body): Json<FamilyFeatures>,
) -> Result<Json<FamilyFeatures>, ApiError> {
    let user = admin.0;
    let new = body.normalized();

    // Read old → write new → audit, all in ONE transaction: the audit row
    // can't describe a change that didn't happen (or miss one that did), and
    // the `old` it diffs against can't be stale. `&mut *tx` reborrows the
    // transaction as a plain connection for each step.
    let mut tx = state.db.begin().await?;
    let old = hidden_features(&mut *tx, user.family_id).await?;
    write_hidden(&mut tx, user.family_id, &new.hidden).await?;
    if let Some(label) = change_label(&old, &new) {
        crate::audit::record(
            &mut *tx,
            user.family_id,
            user.user_id,
            "feature",
            None,
            "update",
            &label,
        )
        .await?;
    }
    // Dropping a `Transaction` without `commit` rolls it back — so any `?`
    // above leaves the old set (and no audit row) intact.
    tx.commit().await?;

    // Other open clients refetch on the SSE poke and re-render their tabs.
    crate::sync::poke(&state, user.family_id).await;
    Ok(Json(new))
}

/// Audit label for old → new, e.g. "Piilotettu: Kotityöt; Näytetty: Chatti".
/// `None` when nothing changed (a no-op PUT writes no audit row).
fn change_label(old: &FamilyFeatures, new: &FamilyFeatures) -> Option<String> {
    // Collect the Finnish labels of features in `a` but not in `b`.
    let diff = |a: &FamilyFeatures, b: &FamilyFeatures| -> Vec<&'static str> {
        a.hidden
            .iter()
            .filter(|f| !b.is_hidden(**f))
            .map(|f| f.label_fi())
            .collect()
    };
    let hidden = diff(new, old);
    let shown = diff(old, new);
    let mut parts = Vec::new();
    if !hidden.is_empty() {
        parts.push(format!("Piilotettu: {}", hidden.join(", ")));
    }
    if !shown.is_empty() {
        parts.push(format!("Näytetty: {}", shown.join(", ")));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
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

    async fn put(app: &axum::Router, cookie: &str, body: Value) -> axum::response::Response {
        app.clone()
            .oneshot(req("PUT", "/api/family/features", cookie, Some(body)))
            .await
            .unwrap()
    }

    async fn get_hidden(app: &axum::Router, cookie: &str) -> Value {
        let resp = app
            .clone()
            .oneshot(req("GET", "/api/family/features", cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        json_body(resp).await
    }

    #[tokio::test]
    async fn member_get_is_empty_by_default() {
        let (app, _db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        assert_eq!(get_hidden(&app, &member).await, json!({ "hidden": [] }));
    }

    #[tokio::test]
    async fn get_requires_session() {
        let (app, _db) = app_with_db().await;
        let resp = app
            .oneshot(req("GET", "/api/family/features", "", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_put_replaces_set_and_get_reflects_it() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;

        let resp = put(&app, &admin, json!({ "hidden": ["chores"] })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await, json!({ "hidden": ["chores"] }));
        // Visible to the whole family, not just the admin.
        assert_eq!(
            get_hidden(&app, &member).await,
            json!({ "hidden": ["chores"] })
        );
        assert!(is_hidden(&db, 1, Feature::Chores).await.unwrap());
        assert!(!is_hidden(&db, 1, Feature::Chat).await.unwrap());

        // Replace (not merge), normalized: sorted + deduped.
        let resp = put(
            &app,
            &admin,
            json!({ "hidden": ["chat", "grocery", "chat"] }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            get_hidden(&app, &admin).await,
            json!({ "hidden": ["grocery", "chat"] })
        );

        // Empty list = show everything again.
        let resp = put(&app, &admin, json!({ "hidden": [] })).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(get_hidden(&app, &admin).await, json!({ "hidden": [] }));
    }

    #[tokio::test]
    async fn member_put_is_forbidden() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let resp = put(&app, &member, json!({ "hidden": ["chores"] })).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert!(!is_hidden(&db, 1, Feature::Chores).await.unwrap());
    }

    #[tokio::test]
    async fn unknown_feature_key_is_422_and_changes_nothing() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        put(&app, &admin, json!({ "hidden": ["chat"] })).await;
        // "koti" isn't a Feature → serde fails → axum's Json rejection (422).
        let resp = put(&app, &admin, json!({ "hidden": ["chores", "koti"] })).await;
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            hidden_features(&db, 1).await.unwrap().hidden,
            vec![Feature::Chat]
        );
    }

    #[tokio::test]
    async fn unknown_stored_key_is_skipped_on_read() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        // As if a newer version had stored a feature this one doesn't know.
        sqlx::query!(
            "INSERT INTO family_hidden_features (family_id, feature) VALUES (1, 'future'), (1, 'notes')"
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(
            get_hidden(&app, &admin).await,
            json!({ "hidden": ["notes"] })
        );
    }

    #[tokio::test]
    async fn put_is_scoped_to_own_family() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        // Family B (id 2) seeded directly — /api/setup only runs once.
        sqlx::query!(
            "INSERT INTO families (id, name, created_at) VALUES (2, 'Muut', '2026-07-23T00:00:00Z')"
        )
        .execute(&db)
        .await
        .unwrap();
        replace_hidden(&db, 2, &[Feature::Food]).await.unwrap();

        // Family A's GET doesn't see B's row, and A's PUT doesn't wipe it.
        assert_eq!(get_hidden(&app, &admin).await, json!({ "hidden": [] }));
        put(&app, &admin, json!({ "hidden": ["chores"] })).await;
        assert_eq!(
            hidden_features(&db, 2).await.unwrap().hidden,
            vec![Feature::Food]
        );
        assert!(!is_hidden(&db, 2, Feature::Chores).await.unwrap());
    }

    #[tokio::test]
    async fn put_writes_one_audit_row_per_change() {
        let (app, db) = app_with_db().await;
        let admin = setup_admin(&app).await;
        put(&app, &admin, json!({ "hidden": ["chores", "chat"] })).await;
        put(&app, &admin, json!({ "hidden": ["chores", "food"] })).await;
        // A no-op PUT writes nothing.
        put(&app, &admin, json!({ "hidden": ["food", "chores"] })).await;

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!", entity_id
               FROM audit_log WHERE family_id = 1 ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].entity, "feature");
        assert_eq!(rows[0].op, "update");
        assert_eq!(rows[0].entity_id, None);
        assert_eq!(rows[0].label, "Piilotettu: Kotityöt, Chatti");
        assert_eq!(rows[1].label, "Piilotettu: Ruoka; Näytetty: Chatti");
    }
}
