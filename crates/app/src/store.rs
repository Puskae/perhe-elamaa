//! localStorage persistence for the grocery list.
//!
//! The browser's localStorage survives page reloads and keeps the grocery list
//! visible immediately on mount — before the first network fetch completes —
//! and allows offline mutations to be queued and replayed later.
//!
//! All reads return `Option` or a sensible default; localStorage failures
//! (private-browsing quotas, serialisation bugs) are silently ignored because
//! the feature degrades gracefully: the app just falls back to network-only.

use perkele_shared::aisle::AislesResponse;
use perkele_shared::calendar::{Event, Exdate};
use perkele_shared::grocery::GroceryItem;
use serde::{Deserialize, Serialize};

const KEY_ITEMS: &str = "perkele_items";
const KEY_SEQ: &str = "perkele_seq";
const KEY_QUEUE: &str = "perkele_queue";
const KEY_AISLES: &str = "perkele_aisles";
const KEY_SORT_MODE: &str = "perkele_sort_mode";
const KEY_EVENTS: &str = "perkele_events";
const KEY_EXDATES: &str = "perkele_exdates";
const KEY_CAL_QUEUE: &str = "perkele_cal_queue";
const KEY_CAL_VIEW: &str = "perkele_cal_view";

/// A mutation that couldn't reach the server while offline.
/// On reconnect these are replayed in order, then cleared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedMutation {
    /// HTTP method, e.g. "POST" or "DELETE".
    pub method: String,
    /// Path relative to the origin, e.g. "/api/grocery/42/check".
    pub url: String,
    /// JSON body for create/edit; None for body-less ops (toggle/delete/clear).
    #[serde(default)]
    pub body: Option<String>,
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok()?
}

fn read<T: for<'de> Deserialize<'de>>(key: &str) -> Option<T> {
    let raw = storage()?.get_item(key).ok()??;
    serde_json::from_str(&raw).ok()
}

fn write<T: Serialize + ?Sized>(key: &str, value: &T) {
    if let Some(s) = storage()
        && let Ok(json) = serde_json::to_string(value)
    {
        let _ = s.set_item(key, &json);
    }
}

pub fn get_items() -> Option<Vec<GroceryItem>> {
    read(KEY_ITEMS)
}

pub fn set_items(items: &[GroceryItem]) {
    write(KEY_ITEMS, items);
}

pub fn get_aisles() -> Option<AislesResponse> {
    read(KEY_AISLES)
}

pub fn set_aisles(resp: &AislesResponse) {
    write(KEY_AISLES, resp);
}

/// Returns 0 when no sync has happened yet.
pub fn get_seq() -> i64 {
    read::<i64>(KEY_SEQ).unwrap_or(0)
}

pub fn set_seq(seq: i64) {
    write(KEY_SEQ, &seq);
}

pub fn get_queue() -> Vec<QueuedMutation> {
    read(KEY_QUEUE).unwrap_or_default()
}

pub fn set_queue(queue: &[QueuedMutation]) {
    write(KEY_QUEUE, queue);
}

/// Append one mutation to the offline queue.
pub fn enqueue(mutation: QueuedMutation) {
    let mut q = get_queue();
    q.push(mutation);
    set_queue(&q);
}

/// Clear the offline queue after a successful replay.
pub fn clear_queue() {
    set_queue(&[]);
}

/// How the grocery list is ordered on screen. Persisted per-device.
/// `Aisles` is the default (today's walking-order view).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortMode {
    Aisles,
    Alpha,
    Star,
}

impl SortMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SortMode::Aisles => "aisles",
            SortMode::Alpha => "alpha",
            SortMode::Star => "star",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> SortMode {
        match s {
            "alpha" => SortMode::Alpha,
            "star" => SortMode::Star,
            _ => SortMode::Aisles,
        }
    }
}

/// The user's last-used sort mode (default Aisles on first visit).
pub fn get_sort_mode() -> SortMode {
    read::<String>(KEY_SORT_MODE)
        .map(|s| SortMode::from_str(&s))
        .unwrap_or(SortMode::Aisles)
}

pub fn set_sort_mode(mode: SortMode) {
    write(KEY_SORT_MODE, mode.as_str());
}

// --- calendar --------------------------------------------------------------

pub fn get_events() -> Option<Vec<Event>> {
    read(KEY_EVENTS)
}

pub fn set_events(events: &[Event]) {
    write(KEY_EVENTS, events);
}

/// Skipped occurrences of recurring series, cached alongside the events so
/// offline rendering drops the same occurrences the server would.
pub fn get_exdates() -> Vec<Exdate> {
    read(KEY_EXDATES).unwrap_or_default()
}

pub fn set_exdates(exdates: &[Exdate]) {
    write(KEY_EXDATES, exdates);
}

/// Optimistically record one skip locally (offline "poista/muokkaa vain tämä").
pub fn add_exdate(exdate: Exdate) {
    let mut all = get_exdates();
    if !all.contains(&exdate) {
        all.push(exdate);
        set_exdates(&all);
    }
}

pub fn get_cal_queue() -> Vec<QueuedMutation> {
    read(KEY_CAL_QUEUE).unwrap_or_default()
}

fn set_cal_queue(queue: &[QueuedMutation]) {
    write(KEY_CAL_QUEUE, queue);
}

pub fn enqueue_cal(mutation: QueuedMutation) {
    let mut q = get_cal_queue();
    q.push(mutation);
    set_cal_queue(&q);
}

pub fn clear_cal_queue() {
    set_cal_queue(&[]);
}

/// The user's last-used calendar view ("agenda" | "month" | "week").
/// `None` on first visit, so the caller can pick its own default.
pub fn get_cal_view() -> Option<String> {
    read(KEY_CAL_VIEW)
}

pub fn set_cal_view(view: &str) {
    write(KEY_CAL_VIEW, view);
}

// --- announcements ----------------------------------------------------------

const KEY_ANNOUNCEMENTS: &str = "perkele_announcements";

pub fn get_announcements() -> Vec<perkele_shared::announcement::Announcement> {
    read(KEY_ANNOUNCEMENTS).unwrap_or_default()
}

pub fn set_announcements(list: &[perkele_shared::announcement::Announcement]) {
    write(KEY_ANNOUNCEMENTS, list);
}

// --- chores ------------------------------------------------------------------

const KEY_CHORES_TODAY: &str = "perkele_chores_today";

pub fn get_chores_today() -> Vec<perkele_shared::chore::DueChore> {
    read(KEY_CHORES_TODAY).unwrap_or_default()
}

pub fn set_chores_today(list: &[perkele_shared::chore::DueChore]) {
    write(KEY_CHORES_TODAY, list);
}

// --- hidden features --------------------------------------------------------

const KEY_HIDDEN_FEATURES: &str = "perkele_hidden_features";

/// The family's hidden-feature set as last seen from the server, so the
/// bottom bar paints with the right tabs on startup and offline. Nothing
/// cached → nothing hidden.
pub fn get_hidden_features() -> perkele_shared::feature::FamilyFeatures {
    read(KEY_HIDDEN_FEATURES).unwrap_or_default()
}

pub fn set_hidden_features(ff: &perkele_shared::feature::FamilyFeatures) {
    write(KEY_HIDDEN_FEATURES, ff);
}

// --- chat --------------------------------------------------------------------

const KEY_CHAT: &str = "perkele_chat";

/// The cached latest page — instant offline paint, like the other screens.
pub fn get_chat_messages() -> Vec<perkele_shared::chat::ChatMessage> {
    read(KEY_CHAT).unwrap_or_default()
}

pub fn set_chat_messages(list: &[perkele_shared::chat::ChatMessage]) {
    write(KEY_CHAT, list);
}

// --- notes (Muistiot) ---------------------------------------------------------

const KEY_NOTES: &str = "perkele_notes";

/// Cached topic summaries — instant offline paint for the Muistiot list.
pub fn get_notes() -> Vec<perkele_shared::note::NoteTopicSummary> {
    read(KEY_NOTES).unwrap_or_default()
}

pub fn set_notes(list: &[perkele_shared::note::NoteTopicSummary]) {
    write(KEY_NOTES, list);
}

/// Per-topic cache under its own key: only opened topics take storage, and
/// one topic updating doesn't rewrite the others.
pub fn get_note(id: i64) -> Option<perkele_shared::note::NoteTopic> {
    read(&format!("perkele_note_{id}"))
}

pub fn set_note(topic: &perkele_shared::note::NoteTopic) {
    write(&format!("perkele_note_{}", topic.id), topic);
}

/// A client-generated unique id for offline-first event creation.
pub fn new_uid() -> String {
    web_sys::window()
        .and_then(|w| w.crypto().ok())
        .map(|c| c.random_uuid())
        .unwrap_or_else(|| format!("uid-{}", js_sys::Date::now() as u64))
}

/// Which user the cached data and offline queues on this device belong to.
const KEY_OWNER: &str = "perkele_owner";

/// Per-device preferences that aren't family data; they survive a logout.
const KEEP_ON_LOGOUT: &[&str] = &[KEY_SORT_MODE, KEY_CAL_VIEW];

/// Forget everything cached for the signed-in member: list caches, the
/// offline mutation queues and the owner marker. Called on logout so a shared
/// device doesn't keep showing the family's data — and so a queued mutation
/// can't replay later under whoever logs in next.
pub fn clear_session_data() {
    let Some(s) = storage() else { return };
    let len = s.length().unwrap_or(0);
    // Collect first: removing while indexing would shift the keys.
    let doomed: Vec<String> = (0..len)
        .filter_map(|i| s.key(i).ok().flatten())
        .filter(|k| k.starts_with("perkele_") && !KEEP_ON_LOGOUT.contains(&k.as_str()))
        .collect();
    for k in doomed {
        let _ = s.remove_item(&k);
    }
}

/// Mark this device's cache as `user_id`'s, wiping it first if it belonged to
/// someone else (a session that expired instead of logging out). Call it
/// *before* switching to the logged-in state, so no screen paints from — or
/// replays — the previous member's cache. A cache with no owner predates this
/// marker and is kept.
pub fn claim_for(user_id: i64) {
    if read::<i64>(KEY_OWNER).is_some_and(|owner| owner != user_id) {
        clear_session_data();
    }
    write(KEY_OWNER, &user_id);
}
