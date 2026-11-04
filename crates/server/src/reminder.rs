//! The reminder scheduler: derive what is due from the calendar as it is NOW
//! (no materialized schedule), claim it in reminder_sends, push it.

use crate::db::Db;
use crate::error::ApiError;
use crate::push::{Pusher, WebPushPusher};
use perkele_shared::feature::Feature;
use std::sync::Arc;

/// Reminders more than this late are skipped (spec decision).
pub const LATE_CUTOFF_MIN: i64 = 30;

/// Current LOCAL wall-clock as a floating string. Event times are floating
/// local-as-Z, so the scheduler must compare against local time, not UTC —
/// the home server lives in the family's timezone (documented constraint).
fn local_now_floating() -> String {
    chrono::Local::now()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

/// Spawn the scheduler: tick every 60 s forever. Reminder pushes run every
/// tick; the audit-log and expired-session prunes run once a day (every
/// 1440th tick) — cheap DELETEs, and a family app needs no finer granularity.
pub fn spawn(db: Db, pusher: Arc<WebPushPusher>, audit_retention_days: i64) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        let mut ticks: u64 = 0;
        loop {
            tick.tick().await;
            // Each tick's scans run in their OWN task. If anything in there
            // panics (one malformed row tripping an `unwrap` in a library —
            // web-push's VAPID signer did exactly that), tokio catches the
            // panic at the task boundary and hands it back as a JoinError
            // instead of unwinding through this loop. So one bad row costs one
            // tick, never the scheduler. (Chosen over `catch_unwind`, which
            // needs `UnwindSafe` bounds and wrapping of async code.)
            let (db2, pusher2) = (db.clone(), pusher.clone());
            run_isolated("reminder scan", async move {
                let now = local_now_floating();
                match scan_once(&db2, &*pusher2, &now).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("reminders: sent {n} push(es)"),
                    Err(e) => tracing::warn!("reminder scan failed: {e:?}"),
                }
                // Same tick, second pass: chores (6D) ride the same transport.
                match scan_chores_once(&db2, &*pusher2, &now).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("chore reminders: sent {n} push(es)"),
                    Err(e) => tracing::warn!("chore reminder scan failed: {e:?}"),
                }
            })
            .await;

            // Retention: prune on the first tick and once a day after.
            if ticks.is_multiple_of(1440) {
                let now = time::OffsetDateTime::now_utc();
                match crate::audit::prune_once(&db, audit_retention_days, now).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("audit: pruned {n} row(s) past retention"),
                    Err(e) => tracing::warn!("audit prune failed: {e:?}"),
                }
                match prune_expired_sessions(&db, now).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!("sessions: pruned {n} expired row(s)"),
                    Err(e) => tracing::warn!("session prune failed: {e:?}"),
                }
            }
            ticks = ticks.wrapping_add(1);
        }
    });
}

/// Run `work` as a separate tokio task and wait for it; a panic inside is
/// logged and swallowed (see the comment in [`spawn`]). Returns false if it
/// panicked. `'static` because a spawned task may outlive this stack frame,
/// so it must own everything it uses (hence the clones at the call site).
async fn run_isolated<F>(label: &str, work: F) -> bool
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    match tokio::spawn(work).await {
        Ok(()) => true,
        Err(e) => {
            tracing::error!("{label} panicked; scheduler continues: {e}");
            false
        }
    }
}

/// Delete sessions past their expiry. `lookup` already refuses them; this
/// just keeps dead token hashes from piling up. `expires_at` is RFC 3339
/// (sqlx's encoding of `OffsetDateTime`); julianday() compares the instants,
/// which plain string comparison would get wrong for fractional seconds.
pub async fn prune_expired_sessions(db: &Db, now: time::OffsetDateTime) -> Result<u64, ApiError> {
    Ok(sqlx::query!(
        "DELETE FROM sessions WHERE julianday(expires_at) < julianday(?)",
        now
    )
    .execute(db)
    .await?
    .rows_affected())
}

pub async fn scan_once<P: Pusher>(db: &Db, pusher: &P, now: &str) -> Result<u32, ApiError> {
    let families = sqlx::query_scalar!(
        r#"SELECT DISTINCT family_id AS "family_id!: i64" FROM events
           WHERE reminder_minutes IS NOT NULL AND deleted_at IS NULL"#
    )
    .fetch_all(db)
    .await?;

    let mut sent_total = 0;
    for family_id in families {
        // Calendar hidden → mute. Skipped BEFORE the claim below, so nothing
        // is recorded as sent: re-showing within the late cutoff still fires.
        if crate::feature::is_hidden(db, family_id, Feature::Calendar).await? {
            continue;
        }
        let events = crate::calendar::load_events(db, family_id).await?;
        let exdates = crate::calendar::load_exdates(db, family_id).await?;
        for due in perkele_shared::calendar::due_reminders(&events, &exdates, now, LATE_CUTOFF_MIN)
        {
            // Claim first: the UNIQUE key makes retries/races send at most once.
            let claimed = sqlx::query!(
                "INSERT INTO reminder_sends (family_id, event_uid, occ_start, sent_at)
                 VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
                family_id,
                due.event_uid,
                due.occ_start,
                now,
            )
            .execute(db)
            .await?
            .rows_affected();
            if claimed == 0 {
                continue;
            }
            let recipients = if due.attendee_ids.is_empty() {
                // No attendees = the whole family's business.
                sqlx::query_scalar!(
                    r#"SELECT id AS "id!: i64" FROM users WHERE family_id = ?"#,
                    family_id,
                )
                .fetch_all(db)
                .await?
            } else {
                due.attendee_ids.clone()
            };
            // All-day starts are a nominal 00:00 — "Alkaa klo 00:00" would mislead.
            let body = if due.all_day {
                "Koko päivän tapahtuma".to_owned()
            } else {
                format!("Alkaa klo {}", &due.occ_start[11..16])
            };
            let payload = serde_json::json!({
                "title": due.title,
                "body": body,
            })
            .to_string();
            for user_id in recipients {
                sent_total +=
                    crate::push::send_to_user(db, pusher, family_id, user_id, &payload).await?;
            }
        }
    }
    Ok(sent_total)
}

/// Minutes since midnight for a local 'HH:MM'; None on malformed input.
fn hm_minutes(hm: &str) -> Option<i64> {
    let (h, m) = hm.split_once(':')?;
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The chore pass: due today + remind_at reached (≤30 min late) + not yet
/// completed → claim (chore_id, date) and push to the day's assignee, or
/// the whole family for unassigned chores. Completing early = no nag.
pub async fn scan_chores_once<P: Pusher>(db: &Db, pusher: &P, now: &str) -> Result<u32, ApiError> {
    // `now` is the same floating local string scan_once gets.
    if now.len() < 16 {
        return Ok(0);
    }
    let date = &now[..10];
    let Some(now_min) = hm_minutes(&now[11..16]) else {
        return Ok(0);
    };

    let families = sqlx::query_scalar!(
        r#"SELECT DISTINCT family_id AS "family_id!: i64" FROM chores
           WHERE remind_at IS NOT NULL AND deleted_at IS NULL"#
    )
    .fetch_all(db)
    .await?;

    let mut sent_total = 0;
    for family_id in families {
        // Chores hidden → mute, skipped before claiming (same as scan_once).
        if crate::feature::is_hidden(db, family_id, Feature::Chores).await? {
            continue;
        }
        for c in crate::chore::load_chores(db, family_id).await? {
            let Some(remind) = c.remind_at.as_deref().and_then(hm_minutes) else {
                continue;
            };
            if now_min < remind || now_min - remind > LATE_CUTOFF_MIN {
                continue;
            }
            if !perkele_shared::chore::is_due_on(&c, date) {
                continue;
            }
            // Done already → the reminder's job is done for it.
            let done = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "n!: i64" FROM chore_completions
                   WHERE chore_id = ? AND date = ?"#,
                c.id,
                date,
            )
            .fetch_one(db)
            .await?;
            if done > 0 {
                continue;
            }
            // Claim first: the PK (chore_id, date) → at most one send per day.
            let claimed = sqlx::query!(
                "INSERT INTO chore_reminder_sends (chore_id, date, sent_at)
                 VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
                c.id,
                date,
                now,
            )
            .execute(db)
            .await?
            .rows_affected();
            if claimed == 0 {
                continue;
            }
            let recipients = match perkele_shared::chore::assignee_on(&c, date) {
                Some(uid) => vec![uid],
                None => {
                    sqlx::query_scalar!(
                        r#"SELECT id AS "id!: i64" FROM users WHERE family_id = ?"#,
                        family_id,
                    )
                    .fetch_all(db)
                    .await?
                }
            };
            let payload = serde_json::json!({
                "title": c.title,
                "body": "Kotityö odottaa 🧹",
            })
            .to_string();
            for user_id in recipients {
                sent_total +=
                    crate::push::send_to_user(db, pusher, family_id, user_id, &payload).await?;
            }
        }
    }

    // Prune old claims — ISO dates compare as strings; chrono only for math.
    if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        let cutoff = (d - chrono::Duration::days(7))
            .format("%Y-%m-%d")
            .to_string();
        sqlx::query!("DELETE FROM chore_reminder_sends WHERE date < ?", cutoff)
            .execute(db)
            .await?;
    }
    Ok(sent_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Like the other modules' test app, but hands back the pool too:
    /// in-memory SQLite lives in ONE connection, so scan_once MUST run on the
    /// router's pool, never a second one.
    async fn push_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = crate::AppState::for_test(db.clone());
        let app = crate::routes::router()
            .merge(crate::calendar::router())
            .merge(crate::push::router())
            .merge(crate::chore::router())
            .with_state(state);
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

    fn event(uid: &str, starts: &str, ends: &str, reminder: i64) -> Value {
        json!({
            "uid": uid, "title": "Jumppa", "all_day": false,
            "starts_at": starts, "ends_at": ends,
            "location": null, "notes": null, "attendee_ids": [],
            "reminder_minutes": reminder
        })
    }

    use crate::push::test_support::{FakePusher, fcm, sub_body};

    #[tokio::test]
    async fn a_panicking_tick_is_contained() {
        // The scheduler loop awaits run_isolated; a panic must come back as
        // `false`, not unwind into the caller.
        assert!(!run_isolated("test", async { panic!("bad row") }).await);
        assert!(run_isolated("test", async {}).await);
    }

    #[tokio::test]
    async fn session_prune_removes_only_expired_rows() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await; // creates one live session
        let now = time::OffsetDateTime::now_utc();
        let expired = now - time::Duration::hours(1);
        sqlx::query!(
            "INSERT INTO sessions (user_id, token_hash, expires_at, created_at)
             VALUES (1, 'old', ?, ?)",
            expired,
            expired,
        )
        .execute(&db)
        .await
        .unwrap();
        assert_eq!(prune_expired_sessions(&db, now).await.unwrap(), 1);
        let left: Vec<String> =
            sqlx::query_scalar!(r#"SELECT token_hash AS "t!: String" FROM sessions"#)
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(left.len(), 1);
        assert_ne!(left[0], "old");
        // The live session still works.
        let resp = app
            .oneshot(req("GET", "/api/me", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn scan_sends_once_within_cutoff_and_prunes_gone() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        // Event 18:00 with a 30 min reminder; subscribe one device.
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(event(
                    "r1",
                    "2026-07-06T18:00:00Z",
                    "2026-07-06T19:00:00Z",
                    30,
                )),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();

        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };

        // Too early: nothing.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:29:00Z")
                .await
                .unwrap(),
            0
        );
        // In the window: exactly one send, payload carries title + HH:MM.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:31:00Z")
                .await
                .unwrap(),
            1
        );
        let log = sent.lock().await.clone();
        assert_eq!(log.len(), 1);
        assert!(log[0].1.contains("18:00"));
        assert!(log[0].1.contains("Jumppa"));
        // Second pass in the same window: dedup, nothing more.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:32:00Z")
                .await
                .unwrap(),
            0
        );

        // A Gone endpoint gets pruned (fresh event so the send-log doesn't
        // dedup it away).
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(event(
                    "r2",
                    "2026-07-06T20:00:00Z",
                    "2026-07-06T21:00:00Z",
                    5,
                )),
            ))
            .await
            .unwrap();
        let gone = FakePusher {
            sent: sent.clone(),
            fail_gone: true,
        };
        assert_eq!(
            scan_once(&db, &gone, "2026-07-06T19:55:00Z").await.unwrap(),
            0
        );
        let left = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM push_subscriptions"#)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(left, 0);
    }

    async fn json_body(resp: axum::response::Response) -> Value {
        use http_body_util::BodyExt;
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
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

    /// A daily chore due every day since 2020 with the given reminder time.
    fn chore_body(remind: &str) -> Value {
        json!({ "title": "Tiskit", "rrule": "FREQ=DAILY", "start_date": "2020-01-01",
                "assigned_user_id": null, "rotation": null, "remind_at": remind })
    }

    #[tokio::test]
    async fn chore_scan_fires_once_within_cutoff() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/chores",
                &cookie,
                Some(chore_body("17:00")),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();

        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };

        // Before the reminder time: nothing.
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T16:59:00Z")
                .await
                .unwrap(),
            0
        );
        // Within [17:00, 17:30]: one send, payload names the chore.
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T17:05:00Z")
                .await
                .unwrap(),
            1
        );
        assert!(sent.lock().await[0].1.contains("Tiskit"));
        // Dedup: same day, nothing more.
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T17:10:00Z")
                .await
                .unwrap(),
            0
        );
        // Too late (>30 min): the next day's instance at 17:31 is skipped…
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-07T17:31:00Z")
                .await
                .unwrap(),
            0
        );
        // …but the day after, on time, fires again (new date, new claim).
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-08T17:00:00Z")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn chore_scan_is_muted_while_chores_hidden() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/chores",
                &cookie,
                Some(chore_body("17:00")),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();
        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };

        crate::feature::replace_hidden(&db, 1, &[Feature::Chores])
            .await
            .unwrap();
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T17:05:00Z")
                .await
                .unwrap(),
            0
        );
        assert!(sent.lock().await.is_empty());

        // Re-shown inside the window: nothing was claimed, so it fires now.
        crate::feature::replace_hidden(&db, 1, &[]).await.unwrap();
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T17:10:00Z")
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn calendar_scan_is_muted_while_calendar_hidden() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(event(
                    "h1",
                    "2026-07-06T18:00:00Z",
                    "2026-07-06T19:00:00Z",
                    30,
                )),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();
        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };

        // Calendar hidden: inside the reminder window, yet nothing is sent.
        crate::feature::replace_hidden(&db, 1, &[Feature::Calendar])
            .await
            .unwrap();
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:31:00Z")
                .await
                .unwrap(),
            0
        );
        assert!(sent.lock().await.is_empty());

        // Re-shown (only Chores hidden now): the unclaimed reminder fires,
        // still within the cutoff…
        crate::feature::replace_hidden(&db, 1, &[Feature::Chores])
            .await
            .unwrap();
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:35:00Z")
                .await
                .unwrap(),
            1
        );
        // …exactly once.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T17:36:00Z")
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn chore_scan_skips_completed_and_targets_assignee() {
        let (app, db) = push_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await; // user id 2
        // Admin and member each subscribe a device.
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &admin,
                Some(sub_body(&fcm("admin"))),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &member,
                Some(sub_body(&fcm("member"))),
            ))
            .await
            .unwrap();

        // Chore assigned to the member (rotation of one = deterministic).
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chores",
                    &admin,
                    Some(json!({ "title": "Roskat", "rrule": "FREQ=DAILY",
                                 "start_date": "2020-01-01", "assigned_user_id": null,
                                 "rotation": [2], "remind_at": "17:00" })),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };
        // Fires to the MEMBER's endpoint only.
        assert_eq!(
            scan_chores_once(&db, &pusher, "2026-07-06T17:00:00Z")
                .await
                .unwrap(),
            1
        );
        assert_eq!(sent.lock().await[0].0, fcm("member"));

        // Completing BEFORE the reminder time suppresses the push entirely.
        // (complete writes for the server's local today, so scan "today".)
        let today = crate::chore::local_today();
        app.clone()
            .oneshot(req(
                "POST",
                &format!("/api/chores/{id}/complete"),
                &member,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(
            scan_chores_once(&db, &pusher, &format!("{today}T17:00:00Z"))
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn scan_skips_stale_reminders() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(event(
                    "r1",
                    "2026-07-06T18:00:00Z",
                    "2026-07-06T19:00:00Z",
                    30,
                )),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();
        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent,
            fail_gone: false,
        };
        // Fire time was 17:30; at 18:05 it is 35 min late → skipped forever.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T18:05:00Z")
                .await
                .unwrap(),
            0
        );
    }
    #[tokio::test]
    async fn all_day_reminder_fires_at_nine_with_all_day_body() {
        let (app, db) = push_app_with_db().await;
        let cookie = setup_admin(&app).await;
        // All-day event on 7.7. with "1 vrk ennen".
        let mut ev = event("ad1", "2026-07-07T00:00:00Z", "2026-07-08T00:00:00Z", 1440);
        ev["all_day"] = json!(true);
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(ev)))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        app.clone()
            .oneshot(req(
                "POST",
                "/api/push/subscribe",
                &cookie,
                Some(sub_body(&fcm("dev1"))),
            ))
            .await
            .unwrap();
        let sent = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let pusher = FakePusher {
            sent: sent.clone(),
            fail_gone: false,
        };
        // Midnight the night before: not yet.
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T00:00:00Z")
                .await
                .unwrap(),
            0
        );
        // 09:00 the day before: one send, no bogus "Alkaa klo 00:00".
        assert_eq!(
            scan_once(&db, &pusher, "2026-07-06T09:00:00Z")
                .await
                .unwrap(),
            1
        );
        let log = sent.lock().await.clone();
        assert!(log[0].1.contains("Koko päivän tapahtuma"));
        assert!(!log[0].1.contains("Alkaa klo"));
    }
}
