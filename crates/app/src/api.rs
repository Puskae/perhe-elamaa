//! Client for the Perhe-elämää API.
//!
//! Every call returns `Result<T, String>` where the error string is the
//! server's human-readable message (from the shared `ErrorResponse`), ready to
//! show in the UI. Cookies ride along automatically because the app is served
//! from the same origin as the API.

use perkele_shared::ErrorResponse;
use perkele_shared::aisle::{
    Aisle, AislesResponse, ReorderAislesRequest, SaveAisleRequest, SetFavoriteRequest,
    SetItemAisleRequest,
};
use perkele_shared::announcement::{Announcement, SaveAnnouncementRequest, SetPinnedRequest};
use perkele_shared::audit::{AuditEntry, DEFAULT_AUDIT_LIMIT};
use perkele_shared::auth::{
    CreateInviteRequest, InviteResponse, LoginRequest, RedeemRequest, Role, SetupRequest,
    SetupStatus, UserView,
};
use perkele_shared::calendar::{CalendarSync, Event, SaveEventRequest};
use perkele_shared::chat::{
    ChatMessage, ChatPage, MarkReadRequest, SendMessageRequest, ToggleReactionRequest,
};
use perkele_shared::chore::{Chore, ChoreStats, DueChore, SaveChoreRequest};
use perkele_shared::feature::FamilyFeatures;
use perkele_shared::grocery::{
    AddItemRequest, EditItemRequest, GroceryItem, SetCheckedRequest, SetCheckedResponse,
    SyncResponse,
};
use perkele_shared::note::{
    CreateTopicRequest, NoteTopic, NoteTopicSummary, SaveTopicRequest, SetBlockCheckedRequest,
};
use perkele_shared::push::{SubscribeRequest, UnsubscribeRequest, VapidResponse};
use perkele_shared::recipe::{
    ImportSummary, MealPlanEntry, MealPlanWeek, Recipe, RecipeSummary, SaveRecipeRequest,
    SetMealRequest,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Absolute URL for an API path. reqwest can't resolve relative URLs, so in the
/// browser we prefix the current origin.
pub fn url(path: &str) -> String {
    format!("{}{path}", base())
}

/// Percent-encode a value for use as one URL path segment. Section names are
/// user text ("Äidin apteekki", even "a/b"), so anything outside the RFC 3986
/// unreserved set is escaped byte-by-byte — otherwise a '/' in the name would
/// split the path and the route wouldn't match.
pub fn encode_path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
fn base() -> String {
    web_sys::window()
        .expect("no window")
        .location()
        .origin()
        .expect("no origin")
}

#[cfg(not(target_arch = "wasm32"))]
fn base() -> String {
    option_env!("PERKELE_API_BASE")
        .unwrap_or("http://127.0.0.1:8080")
        .to_owned()
}

/// Turn a finished response into either the decoded body or the server's error
/// message. A 401 is special-cased to a stable marker so callers can detect
/// "not logged in" without string matching.
async fn handle<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, String> {
    if resp.status().is_success() {
        resp.json::<T>().await.map_err(|e| e.to_string())
    } else if resp.status().as_u16() == 401 {
        Err(UNAUTHENTICATED.to_owned())
    } else {
        // Try to read the structured error; fall back to a generic message.
        match resp.json::<ErrorResponse>().await {
            Ok(body) => Err(body.error),
            Err(_) => Err("Pyyntö epäonnistui.".to_owned()),
        }
    }
}

/// Sentinel error returned for HTTP 401, so `me()` callers can branch on it.
pub const UNAUTHENTICATED: &str = "unauthenticated";

async fn get<T: DeserializeOwned>(path: &str) -> Result<T, String> {
    let resp = reqwest::Client::new()
        .get(url(path))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle(resp).await
}

async fn post_json<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T, String> {
    let resp = reqwest::Client::new()
        .post(url(path))
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle(resp).await
}

async fn put_json<B: Serialize, T: DeserializeOwned>(path: &str, body: &B) -> Result<T, String> {
    let resp = reqwest::Client::new()
        .put(url(path))
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    handle(resp).await
}

// --- endpoints -----------------------------------------------------------

pub async fn setup_status() -> Result<SetupStatus, String> {
    get("/api/setup/status").await
}

pub async fn me() -> Result<UserView, String> {
    get("/api/me").await
}

pub async fn setup(req: SetupRequest) -> Result<UserView, String> {
    post_json("/api/setup", &req).await
}

pub async fn login(req: LoginRequest) -> Result<UserView, String> {
    post_json("/api/auth/login", &req).await
}

pub async fn redeem(req: RedeemRequest) -> Result<UserView, String> {
    post_json("/api/auth/redeem", &req).await
}

pub async fn members() -> Result<Vec<UserView>, String> {
    get("/api/family/members").await
}

/// Which bottom-bar features the family has hidden (any member may read it).
pub async fn get_family_features() -> Result<FamilyFeatures, String> {
    get("/api/family/features").await
}

/// Replace the hidden set (admin only; others get the server's 403 message).
/// The server answers with the stored, normalized set.
pub async fn put_family_features(ff: &FamilyFeatures) -> Result<FamilyFeatures, String> {
    put_json("/api/family/features", ff).await
}

pub async fn create_invite(role: Role) -> Result<InviteResponse, String> {
    post_json("/api/family/invites", &CreateInviteRequest { role }).await
}

/// Like `no_body_req`, but the request carries a JSON body (the response is
/// still empty, e.g. 201/204).
async fn json_req_no_body<B: Serialize>(
    method: reqwest::Method,
    path: &str,
    body: &B,
) -> Result<(), String> {
    // Most callers only want the message; `map_err` drops the status code.
    json_req_no_body_status(method, path, body)
        .await
        .map_err(|e| e.message)
}

/// A failed request with its HTTP status kept, for the few callers that must
/// react to a specific code (not just show the message). `status` is `None`
/// when no response arrived at all (network error).
#[derive(Debug, Clone)]
pub struct HttpError {
    pub status: Option<u16>,
    pub message: String,
}

/// [`json_req_no_body`], but the error keeps the status code.
async fn json_req_no_body_status<B: Serialize>(
    method: reqwest::Method,
    path: &str,
    body: &B,
) -> Result<(), HttpError> {
    let resp = reqwest::Client::new()
        .request(method, url(path))
        .json(body)
        .send()
        .await
        .map_err(|e| HttpError {
            status: None,
            message: e.to_string(),
        })?;
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    let message = match resp.json::<ErrorResponse>().await {
        Ok(body) => body.error,
        Err(_) => "Pyyntö epäonnistui.".to_owned(),
    };
    Err(HttpError {
        status: Some(status.as_u16()),
        message,
    })
}

async fn no_body_req(method: reqwest::Method, path: &str) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .request(method, url(path))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        match resp.json::<ErrorResponse>().await {
            Ok(body) => Err(body.error),
            Err(_) => Err("Pyyntö epäonnistui.".to_owned()),
        }
    }
}

/// Logout returns 204 (no body), so it's handled separately from `handle`.
pub async fn logout() -> Result<(), String> {
    let resp = reqwest::Client::new()
        .post(url("/api/auth/logout"))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err("Kirjautuminen ulos epäonnistui.".to_owned())
    }
}

pub async fn add_item(req: AddItemRequest) -> Result<GroceryItem, String> {
    post_json("/api/grocery", &req).await
}

/// Set an item's checked state ("set semantics"). The body carries the target
/// state so a duplicate or delayed request can't flip the item back — see
/// `SetCheckedRequest` in `shared` for the full story. Returns the sync seq
/// the write produced, so the caller can reject older snapshots.
pub async fn set_checked(id: i64, checked: bool) -> Result<SetCheckedResponse, String> {
    put_json(
        &format!("/api/grocery/{id}/check"),
        &SetCheckedRequest { checked },
    )
    .await
}

pub async fn delete_item(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/grocery/{id}")).await
}

/// Edit an existing item's text fields. Server returns 200 with no body, so
/// this uses the empty-response helper. The aisle is a separate concern
/// handled by `set_item_aisle`.
pub async fn edit_item(id: i64, req: EditItemRequest) -> Result<(), String> {
    json_req_no_body(reqwest::Method::PUT, &format!("/api/grocery/{id}"), &req).await
}

pub async fn clear_checked() -> Result<(), String> {
    no_body_req(reqwest::Method::POST, "/api/grocery/clear-checked").await
}

/// Remove a whole section: the server soft-deletes every live item carrying
/// this section name.
pub async fn delete_section(name: &str) -> Result<(), String> {
    no_body_req(
        reqwest::Method::DELETE,
        &format!("/api/grocery/section/{}", encode_path_segment(name)),
    )
    .await
}

/// Full sync: returns current items + server's latest seq number.
/// Use this on app load and after replaying the offline queue.
pub async fn sync() -> Result<SyncResponse, String> {
    get("/api/grocery/sync").await
}

// --- aisles ("hyllyt") ------------------------------------------------------

/// Aisles in walking order + the item-name → aisle map, one fetch.
pub async fn get_aisles() -> Result<AislesResponse, String> {
    get("/api/aisles").await
}

pub async fn create_aisle(name: &str) -> Result<Aisle, String> {
    post_json(
        "/api/aisles",
        &SaveAisleRequest {
            name: name.to_owned(),
        },
    )
    .await
}

pub async fn rename_aisle(id: i64, name: &str) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        &format!("/api/aisles/{id}"),
        &SaveAisleRequest {
            name: name.to_owned(),
        },
    )
    .await
}

pub async fn delete_aisle(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/aisles/{id}")).await
}

/// Send the FULL id list — the server rejects partial lists, which protects
/// against reordering a stale snapshot.
pub async fn reorder_aisles(ids: Vec<i64>) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        "/api/aisles/order",
        &ReorderAislesRequest { ids },
    )
    .await
}

/// The "learning" write: maps every current and future item with this name.
pub async fn set_item_aisle(item_name: &str, aisle_id: Option<i64>) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        "/api/aisles/map",
        &SetItemAisleRequest {
            item_name: item_name.to_owned(),
            aisle_id,
        },
    )
    .await
}

/// Star/unstar an item by name (family-wide). Mirrors `set_item_aisle`.
pub async fn set_favorite(item_name: &str, starred: bool) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        "/api/aisles/favorite",
        &SetFavoriteRequest {
            item_name: item_name.to_owned(),
            starred,
        },
    )
    .await
}

// --- announcements ---------------------------------------------------------

pub async fn list_announcements() -> Result<Vec<Announcement>, String> {
    get("/api/announcements").await
}

pub async fn post_announcement(body: String) -> Result<Announcement, String> {
    post_json("/api/announcements", &SaveAnnouncementRequest { body }).await
}

/// 204 responses ride the body-carrying no-body helper, like set_meal does.
pub async fn edit_announcement(id: i64, body: String) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        &format!("/api/announcements/{id}"),
        &SaveAnnouncementRequest { body },
    )
    .await
}

pub async fn set_announcement_pinned(id: i64, pinned: bool) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        &format!("/api/announcements/{id}/pinned"),
        &SetPinnedRequest { pinned },
    )
    .await
}

pub async fn delete_announcement(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/announcements/{id}")).await
}

// --- chat (Perhechatti) ------------------------------------------------------

/// One page of chat. `before` = oldest already-loaded id (None = latest page);
/// `limit` mainly lets the nav badge fetch cheaply with 1.
pub async fn chat_page(before: Option<i64>, limit: Option<i64>) -> Result<ChatPage, String> {
    let mut q = Vec::new();
    if let Some(b) = before {
        q.push(format!("before={b}"));
    }
    if let Some(l) = limit {
        q.push(format!("limit={l}"));
    }
    let path = if q.is_empty() {
        "/api/chat/messages".to_owned()
    } else {
        format!("/api/chat/messages?{}", q.join("&"))
    };
    get(&path).await
}

pub async fn send_chat_message(body: String) -> Result<ChatMessage, String> {
    post_json("/api/chat/messages", &SendMessageRequest { body }).await
}

pub async fn delete_chat_message(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/chat/messages/{id}")).await
}

pub async fn toggle_chat_reaction(message_id: i64, emoji: String) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        "/api/chat/reactions",
        &ToggleReactionRequest { message_id, emoji },
    )
    .await
}

pub async fn mark_chat_read(last_read_id: i64) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        "/api/chat/read",
        &MarkReadRequest { last_read_id },
    )
    .await
}

// --- chores ----------------------------------------------------------------

pub async fn list_chores() -> Result<Vec<Chore>, String> {
    get("/api/chores").await
}

pub async fn chores_today() -> Result<Vec<DueChore>, String> {
    get("/api/chores/today").await
}

pub async fn create_chore(req: SaveChoreRequest) -> Result<Chore, String> {
    post_json("/api/chores", &req).await
}

pub async fn update_chore(id: i64, req: SaveChoreRequest) -> Result<(), String> {
    json_req_no_body(reqwest::Method::PUT, &format!("/api/chores/{id}"), &req).await
}

pub async fn delete_chore(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/chores/{id}")).await
}

pub async fn complete_chore(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::POST, &format!("/api/chores/{id}/complete")).await
}

pub async fn uncomplete_chore(id: i64) -> Result<(), String> {
    no_body_req(
        reqwest::Method::DELETE,
        &format!("/api/chores/{id}/complete"),
    )
    .await
}

pub async fn chore_stats(window: &str) -> Result<ChoreStats, String> {
    get(&format!("/api/chores/stats?window={window}")).await
}

// --- notes (Muistiot) --------------------------------------------------------

pub async fn list_notes() -> Result<Vec<NoteTopicSummary>, String> {
    get("/api/notes").await
}

pub async fn create_note(title: String) -> Result<NoteTopic, String> {
    post_json("/api/notes", &CreateTopicRequest { title }).await
}

pub async fn get_note(id: i64) -> Result<NoteTopic, String> {
    get(&format!("/api/notes/{id}")).await
}

/// Replace-all save; the response carries the fresh blocks (NEW ids).
pub async fn save_note(id: i64, req: &SaveTopicRequest) -> Result<NoteTopic, String> {
    put_json(&format!("/api/notes/{id}"), req).await
}

/// Set semantics like grocery's set_checked — a duplicate can't undo a tap.
pub async fn set_block_checked(note_id: i64, block_id: i64, checked: bool) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::PUT,
        &format!("/api/notes/{note_id}/blocks/{block_id}/checked"),
        &SetBlockCheckedRequest { checked },
    )
    .await
}

pub async fn delete_note(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/notes/{id}")).await
}

// --- recipes --------------------------------------------------------------

pub async fn list_recipes() -> Result<Vec<RecipeSummary>, String> {
    get("/api/recipes").await
}

pub async fn get_recipe(id: i64) -> Result<Recipe, String> {
    get(&format!("/api/recipes/{id}")).await
}

pub async fn create_recipe(req: SaveRecipeRequest) -> Result<Recipe, String> {
    post_json("/api/recipes", &req).await
}

pub async fn update_recipe(id: i64, req: SaveRecipeRequest) -> Result<Recipe, String> {
    put_json(&format!("/api/recipes/{id}"), &req).await
}

pub async fn delete_recipe(id: i64) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/recipes/{id}")).await
}

// --- calendar --------------------------------------------------------------

pub async fn calendar_sync() -> Result<CalendarSync, String> {
    get("/api/calendar/sync").await
}

pub async fn create_event(req: &SaveEventRequest) -> Result<Event, String> {
    post_json("/api/calendar/events", req).await
}

pub async fn update_event(uid: &str, req: &SaveEventRequest) -> Result<Event, String> {
    put_json(&format!("/api/calendar/events/{uid}"), req).await
}

pub async fn delete_event(uid: &str) -> Result<(), String> {
    no_body_req(
        reqwest::Method::DELETE,
        &format!("/api/calendar/events/{uid}"),
    )
    .await
}

/// "Poista vain tämä": skips one occurrence of a series (server records an
/// EXDATE). occ_start is `[0-9TZ:-]` only, so it needs no URL encoding.
pub async fn delete_occurrence(master_uid: &str, occ_start: &str) -> Result<(), String> {
    no_body_req(
        reqwest::Method::DELETE,
        &format!("/api/calendar/events/{master_uid}?occ={occ_start}"),
    )
    .await
}

/// "Poista tästä eteenpäin": trims the series so it ends before occ_start.
pub async fn delete_following(master_uid: &str, occ_start: &str) -> Result<(), String> {
    no_body_req(
        reqwest::Method::DELETE,
        &format!("/api/calendar/events/{master_uid}?from={occ_start}"),
    )
    .await
}

// --- push (Phase 5C) ---------------------------------------------------------

pub async fn vapid_key() -> Result<VapidResponse, String> {
    get("/api/push/vapid").await
}

/// Keeps the status: a 409 means this browser's endpoint is registered to
/// another user, which the caller fixes by re-subscribing (see `me.rs`).
pub async fn push_subscribe(req: &SubscribeRequest) -> Result<(), HttpError> {
    json_req_no_body_status(reqwest::Method::POST, "/api/push/subscribe", req).await
}

pub async fn push_unsubscribe(endpoint: &str) -> Result<(), String> {
    json_req_no_body(
        reqwest::Method::DELETE,
        "/api/push/subscribe",
        &UnsubscribeRequest {
            endpoint: endpoint.to_owned(),
        },
    )
    .await
}

pub async fn push_test() -> Result<(), String> {
    no_body_req(reqwest::Method::POST, "/api/push/test").await
}

// --- audit log (Muutosloki) -------------------------------------------------

/// Admin-only audit feed. `before` is the id-cursor for "load more" (None = first page).
pub async fn audit(before: Option<i64>, entity: Option<&str>) -> Result<Vec<AuditEntry>, String> {
    let mut qs = format!("?limit={DEFAULT_AUDIT_LIMIT}");
    if let Some(b) = before {
        qs.push_str(&format!("&before={b}"));
    }
    if let Some(ent) = entity {
        qs.push_str(&format!("&entity={ent}"));
    }
    get(&format!("/api/audit{qs}")).await
}

// --- meal planner ----------------------------------------------------------

pub async fn get_week(monday: &str) -> Result<MealPlanWeek, String> {
    get(&format!("/api/mealplan?week={monday}")).await
}

/// Planned dinners in [from, to] (inclusive) — only days that have one.
/// Feeds the calendar's read-only meal projection.
pub async fn mealplan_range(from: &str, to: &str) -> Result<Vec<MealPlanEntry>, String> {
    get(&format!("/api/mealplan/range?from={from}&to={to}")).await
}

/// PUT a day's dinner. The server replies 204 (no body), so we don't use
/// `handle` here — we send the JSON and just check the status.
pub async fn set_meal(date: &str, req: SetMealRequest) -> Result<(), String> {
    let resp = reqwest::Client::new()
        .put(url(&format!("/api/mealplan/{date}")))
        .json(&req)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        match resp.json::<ErrorResponse>().await {
            Ok(b) => Err(b.error),
            Err(_) => Err("Pyyntö epäonnistui.".to_owned()),
        }
    }
}

pub async fn clear_meal(date: &str) -> Result<(), String> {
    no_body_req(reqwest::Method::DELETE, &format!("/api/mealplan/{date}")).await
}

/// Both grocery imports reply with the same ImportSummary shape from
/// `shared` — added / skipped (already listed) / unchecked (staple revived).
pub async fn add_week_to_grocery(monday: &str) -> Result<ImportSummary, String> {
    post_json(&format!("/api/mealplan/grocery?week={monday}"), &()).await
}

/// The recipe view's "Lisää ainekset kauppalistaan" button. `servings` is
/// the stepper's current value; the server rescales from the recipe's own
/// base servings.
pub async fn add_recipe_to_grocery(id: i64, servings: i64) -> Result<ImportSummary, String> {
    post_json(
        &format!("/api/recipes/{id}/grocery?servings={servings}"),
        &(),
    )
    .await
}

/// Replay a queued offline mutation (method + path, plus an optional JSON body
/// for create/edit). Only the wasm build calls this — its callers are the SSE
/// `onopen` handlers, which are `#[cfg(target_arch = "wasm32")]`.
#[cfg(target_arch = "wasm32")]
pub async fn replay_mutation(method: &str, path: &str, body: Option<&str>) -> Result<(), String> {
    let m = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| "Tuntematon HTTP-metodi.".to_owned())?;
    let mut rb = reqwest::Client::new().request(m, url(path));
    if let Some(b) = body {
        rb = rb
            .header("content-type", "application/json")
            .body(b.to_owned());
    }
    let resp = rb.send().await.map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        match resp.json::<ErrorResponse>().await {
            Ok(b) => Err(b.error),
            Err(_) => Err("Pyyntö epäonnistui.".to_owned()),
        }
    }
}

// Moved to the bottom of the file (matching the convention everywhere else
// in the workspace) — a `mod tests` sitting near the top was tripping
// clippy's `items_after_test_module` lint once the file no longer had a
// struct definition after it to (apparently) mask the lint's item scan.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_segment_escapes_reserved_and_non_ascii() {
        assert_eq!(encode_path_segment("Apteekki"), "Apteekki");
        assert_eq!(
            encode_path_segment("Äidin apteekki"),
            "%C3%84idin%20apteekki"
        );
        // '/' and '?' would otherwise change the URL's structure.
        assert_eq!(encode_path_segment("a/b?c"), "a%2Fb%3Fc");
    }
}
