//! All UI screens and the router that wires them together.
//!
//! `Route` is the single source of truth for navigation. `NavLayout` wraps
//! every route with the bottom tab bar. Stub screens (Calendar, Recipes) live
//! here until they get their own modules in later phases.

pub mod announcements;
pub mod audit;
pub mod auth;
pub mod calendar;
pub mod chat;
pub mod chore_stats;
pub mod chores;
pub mod dates;
pub mod grocery;
pub mod grocery_aisles;
pub mod home;
pub mod me;
pub mod mealplan;
pub mod notes;
pub mod recipes;

pub use announcements::AnnouncementsScreen;
pub use audit::AuditScreen;
pub use auth::{LoginOrRedeem, SetupScreen};
pub use calendar::{CalendarScreen, EventEditScreen, EventNewScreen};
pub use chat::ChatScreen;
pub use chore_stats::ChoreStatsScreen;
pub use chores::{ChoreEditScreen, ChoreNewScreen, ChoresScreen};
pub use grocery::GroceryScreen;
pub use grocery_aisles::AisleEditor;
pub use home::HomeScreen;
pub use me::MeScreen;
pub use mealplan::MealPlanScreen;
pub use notes::{NoteTopicScreen, NotesScreen};
pub use recipes::{RecipeEditScreen, RecipeNewScreen, RecipeViewScreen, RecipesScreen};

use crate::api;
use dioxus::prelude::*;
use perkele_shared::auth::UserView;

/// Client mirror of the server's author-or-admin rule (`grocery::can_modify`
/// on the server): may `me` delete / overwrite something `created_by` made?
/// Used only to HIDE buttons that would 403 — the server is what enforces it.
/// `me` is `None` while `/api/me` is loading (or offline); we then say "no",
/// callers that must stay usable offline handle that case themselves.
pub(crate) fn can_modify(me: Option<&UserView>, created_by: i64) -> bool {
    me.is_some_and(|u| u.id == created_by || u.role.is_admin())
}

// The `Screen` suffix on every variant is intentional and idiomatic for
// dioxus-router route enums; the shared-postfix lint is noise here.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Routable, PartialEq)]
#[rustfmt::skip]
pub enum Route {
    #[layout(NavLayout)]
    #[route("/")]
    HomeScreen {},
    #[route("/grocery")]
    GroceryScreen {},
    #[route("/calendar")]
    CalendarScreen {},
    // `?:date` is a query parameter: "/calendar/new?date=2026-07-09" seeds the
    // form with that day (month view taps); a missing param parses as "".
    #[route("/calendar/new?:date")]
    EventNewScreen { date: String },
    #[route("/calendar/:uid")]
    EventEditScreen { uid: String },
    #[route("/recipes")]
    RecipesScreen {},
    #[route("/recipes/new")]
    RecipeNewScreen {},
    #[route("/recipes/:id")]
    RecipeViewScreen { id: i64 },
    #[route("/recipes/:id/edit")]
    RecipeEditScreen { id: i64 },
    #[route("/mealplan")]
    MealPlanScreen {},
    #[route("/chores")]
    ChoresScreen {},
    #[route("/chores/new")]
    ChoreNewScreen {},
    #[route("/chores/stats")]
    ChoreStatsScreen {},
    #[route("/chores/:id")]
    ChoreEditScreen { id: i64 },
    #[route("/board")]
    AnnouncementsScreen {},
    #[route("/notes")]
    NotesScreen {},
    #[route("/notes/:id")]
    NoteTopicScreen { id: i64 },
    #[route("/chat")]
    ChatScreen {},
    #[route("/me")]
    MeScreen {},
    #[route("/me/audit")]
    AuditScreen {},
}

#[component]
fn NavLayout() -> Element {
    // App-wide sync tick, provided via context to every screen. The server
    // pokes ONE per-family SSE channel on any mutation (grocery, calendar,
    // announcements, chat all share it), so the app opens exactly ONE
    // EventSource — here, for the whole app lifetime — and every screen keys
    // its refetch on this signal. Screens bump it themselves after their own
    // mutations.
    //
    // One stream per window matters: Chrome allows only 6 concurrent HTTP/1.1
    // connections per origin (shared across tabs of a profile), and each SSE
    // stream holds one open. Per-screen streams leaked on navigation used to
    // eat all 6 slots and freeze every fetch in the app until a refresh.
    let sync_tick = use_context_provider(|| Signal::new(0u32));
    // Queued offline edits the server refused on replay (e.g. 403 "Ei
    // sallittu." for someone else's event). The op is dropped either way —
    // the queue is cleared after one pass so a permanent refusal can't wedge
    // it — but the user must SEE that their offline change didn't stick.
    let mut replay_errors = use_signal(Vec::<String>::new);

    #[cfg(target_arch = "wasm32")]
    use_future(move || async move {
        use wasm_bindgen::prelude::*;
        use web_sys::MessageEvent;
        let es = web_sys::EventSource::new("/api/chat/stream").expect("EventSource");

        // onopen fires on initial connect AND every reconnect after offline:
        // replay queued offline mutations (grocery + calendar), then bump the
        // tick so all screens refetch fresh state. spawn_local, not spawn —
        // raw JS callbacks run outside the Dioxus runtime, where spawn panics.
        let onopen_cb = Closure::<dyn FnMut()>::new(move || {
            let mut sync_tick = sync_tick;
            wasm_bindgen_futures::spawn_local(async move {
                // Each queue is replayed ONCE and then cleared, success or
                // not: a refused op (403/404/400) would fail identically on
                // every retry, so keeping it would wedge the queue forever.
                // Failures are collected and shown in the banner instead.
                let mut failed = Vec::new();
                let gq = crate::store::get_queue();
                for m in &gq {
                    if let Err(e) = api::replay_mutation(&m.method, &m.url, m.body.as_deref()).await
                    {
                        failed.push(format!("Ostoslista: {e}"));
                    }
                }
                if !gq.is_empty() {
                    crate::store::clear_queue();
                }
                let cq = crate::store::get_cal_queue();
                for m in &cq {
                    if let Err(e) = api::replay_mutation(&m.method, &m.url, m.body.as_deref()).await
                    {
                        failed.push(format!("Kalenteri: {e}"));
                    }
                }
                if !cq.is_empty() {
                    crate::store::clear_cal_queue();
                }
                if !failed.is_empty() {
                    replay_errors.write().extend(failed);
                }
                let n = sync_tick.peek().wrapping_add(1);
                sync_tick.set(n);
            });
        });
        es.set_onopen(Some(onopen_cb.as_ref().unchecked_ref()));
        onopen_cb.forget();

        let cb = Closure::<dyn FnMut(MessageEvent)>::new(move |_: MessageEvent| {
            let mut sync_tick = sync_tick;
            {
                let n = sync_tick.peek().wrapping_add(1);
                sync_tick.set(n);
            }
        });
        es.set_onmessage(Some(cb.as_ref().unchecked_ref()));
        cb.forget();
        std::mem::forget(es);
        std::future::pending::<()>().await
    });

    // Badge data: limit=1 keeps the poll cheap; unread_count does the math
    // server-side against the caller's read marker.
    let unread = use_resource(move || {
        let _ = sync_tick(); // subscribe
        async move { api::chat_page(None, Some(1)).await }
    });
    let unread_count = unread()
        .and_then(|r| r.ok())
        .map(|p| p.unread_count)
        .unwrap_or(0);

    rsx! {
        div { class: "app-layout",
            div { class: "page-content",
                if !replay_errors().is_empty() {
                    div { class: "card wide",
                        div { class: "error",
                            "Osaa offline-muutoksista ei voitu tallentaa:"
                            ul {
                                for e in replay_errors() {
                                    li { "{e}" }
                                }
                            }
                        }
                        button {
                            class: "ghost",
                            onclick: move |_| replay_errors.write().clear(),
                            "Selvä"
                        }
                    }
                }
                Outlet::<Route> {}
            }
            nav { class: "bottom-nav",
                Link {
                    to: Route::HomeScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "🏠" }
                    span { class: "nav-label", "Koti" }
                }
                Link {
                    to: Route::GroceryScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "🛒" }
                    span { class: "nav-label", "Ostokset" }
                }
                Link {
                    to: Route::CalendarScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "📅" }
                    span { class: "nav-label", "Kalenteri" }
                }
                Link {
                    to: Route::ChoresScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "🧹" }
                    span { class: "nav-label", "Kotityöt" }
                }
                Link {
                    to: Route::RecipesScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "🍲" }
                    span { class: "nav-label", "Ruoka" }
                }
                Link {
                    to: Route::NotesScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon", "📝" }
                    span { class: "nav-label", "Muistiot" }
                }
                // Ilmoitustaulu is reached from the Koti hub card and Minä
                // from the hub's ⋯ menu — neither spends a tab anymore.
                Link {
                    to: Route::ChatScreen {},
                    class: "nav-tab",
                    active_class: "active",
                    span { class: "nav-icon",
                        "💬"
                        if unread_count > 0 {
                            span { class: "nav-badge", "{unread_count}" }
                        }
                    }
                    span { class: "nav-label", "Chatti" }
                }
            }
        }
    }
}

/// App stylesheet. Kept inline (a `&str` constant) so there's a single asset
/// to serve and no separate CSS file to keep in sync with the Rust code.
pub const CSS: &str = r#"
:root {
    --bg: #11131a;
    --card: #1b1f2a;
    --ink: #eef1f7;
    --muted: #9aa3b2;
    --accent: #6c8cff;
    --accent-ink: #0b0e16;
    --error: #ff8d8d;
    --line: #2a3040;
    --nav-h: 60px;
}
* { box-sizing: border-box; }
/* The iOS notch/status-bar area and overscroll show the html background, not body's gradient. */
html { background: var(--bg); }
body {
    margin: 0;
    font-family: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
    background: radial-gradient(1200px 600px at 50% -10%, #1d2233, var(--bg));
    background-color: var(--bg);
    color: var(--ink);
}
/* Auth screens (centred card layout) */
.app {
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    /* Add the device safe-area insets so the centred card clears the notch /
       home indicator when installed as a full-screen PWA. */
    padding: calc(24px + env(safe-area-inset-top)) calc(24px + env(safe-area-inset-right)) calc(24px + env(safe-area-inset-bottom)) calc(24px + env(safe-area-inset-left));
}
.center { text-align: center; }
.card {
    background: var(--card);
    border: 1px solid var(--line);
    border-radius: 16px;
    padding: 28px;
    width: 100%;
    max-width: 380px;
    box-shadow: 0 20px 60px rgba(0,0,0,0.35);
}
.card.wide { max-width: 520px; }
h1 { margin: 0 0 4px; font-size: 1.6rem; letter-spacing: 0.5px; }
h2 { margin: 22px 0 10px; font-size: 1.05rem; }
.muted { color: var(--muted); }
.small { font-size: 0.85rem; }
label { display: block; margin: 14px 0 6px; font-size: 0.85rem; color: var(--muted); }
input, select, textarea {
    width: 100%;
    padding: 11px 12px;
    border-radius: 10px;
    border: 1px solid var(--line);
    background: #141823;
    color: var(--ink);
    font-size: 1rem;
}
input:focus, select:focus, textarea:focus { outline: 2px solid var(--accent); border-color: transparent; }
button {
    margin-top: 18px;
    padding: 11px 16px;
    border-radius: 10px;
    border: 0;
    font-size: 1rem;
    font-weight: 600;
    cursor: pointer;
}
button:disabled { opacity: 0.6; cursor: default; }
button.primary { background: var(--accent); color: var(--accent-ink); width: 100%; }
button.ghost { background: transparent; color: var(--muted); border: 1px solid var(--line); margin: 0; }
.error {
    background: rgba(255,80,80,0.12);
    border: 1px solid rgba(255,120,120,0.4);
    color: var(--error);
    padding: 10px 12px;
    border-radius: 10px;
    margin-top: 12px;
    font-size: 0.9rem;
}
.switch { margin-top: 16px; font-size: 0.9rem; color: var(--muted); }
.switch a { color: var(--accent); cursor: pointer; }
.row { display: flex; gap: 10px; align-items: center; }
.row.spread { justify-content: space-between; align-items: flex-start; }
.badge {
    display: inline-block; margin-top: 6px;
    background: rgba(108,140,255,0.15); color: var(--accent);
    padding: 2px 10px; border-radius: 999px; font-size: 0.75rem; text-transform: capitalize;
}
.badge.small { margin: 0; }
.members { list-style: none; padding: 0; margin: 0; }
.members li {
    display: flex; align-items: center; gap: 10px;
    padding: 10px 0; border-bottom: 1px solid var(--line);
}
.members li span:first-child { font-weight: 600; }
.members li .muted { margin-left: auto; }
.panel { margin-top: 8px; border-top: 1px solid var(--line); padding-top: 8px; }
.code {
    margin-top: 14px; background: #141823; border: 1px dashed var(--accent);
    border-radius: 10px; padding: 12px;
}
.code strong { font-family: ui-monospace, monospace; font-size: 1.2rem; letter-spacing: 1px; color: var(--accent); }
/* Routed app layout */
.app-layout {
    display: flex;
    flex-direction: column;
    min-height: 100vh;
}
.page-content {
    flex: 1;
    overflow-y: auto;
    /* Top/side insets clear the status bar & landscape notch; the bottom pad
       clears both the fixed nav bar and the home-indicator inset under it. */
    padding:
        calc(16px + env(safe-area-inset-top))
        calc(16px + env(safe-area-inset-right))
        calc(var(--nav-h) + env(safe-area-inset-bottom) + 16px)
        calc(16px + env(safe-area-inset-left));
    display: flex;
    align-items: flex-start;
    justify-content: center;
}
.bottom-nav {
    position: fixed;
    bottom: 0; left: 0; right: 0;
    /* Grow by the home-indicator inset and pad the bar's own content up off it,
       so the tab icons/labels sit above the indicator rather than under it. */
    height: calc(var(--nav-h) + env(safe-area-inset-bottom));
    padding-bottom: env(safe-area-inset-bottom);
    background: var(--card);
    border-top: 1px solid var(--line);
    display: flex;
    align-items: stretch;
}
.nav-tab {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 3px;
    text-decoration: none;
    color: var(--muted);
    font-size: 0.7rem;
    transition: color 0.15s;
    cursor: pointer;
    background: none;
    border: none;
    padding: 0;
}
.nav-tab .nav-icon { font-size: 1.3rem; }
.nav-tab.active { color: var(--accent); }
/* Grocery list */
.grocery-list { list-style: none; padding: 0; margin: 0 0 16px; }
.grocery-item {
    display: flex; align-items: center; gap: 12px;
    padding: 12px 0;
    border-bottom: 1px solid var(--line);
}
.grocery-item input[type="checkbox"] {
    width: 20px; height: 20px; flex-shrink: 0;
    accent-color: var(--accent);
}
.grocery-item .item-name { flex: 1; font-size: 1rem; }
.grocery-item .item-qty { color: var(--muted); font-size: 0.85rem; white-space: nowrap; }
.grocery-item.checked .item-name { text-decoration: line-through; color: var(--muted); }
.grocery-item button.del {
    background: none; border: none; color: var(--muted);
    font-size: 1.1rem; cursor: pointer; padding: 4px 8px; margin: 0;
    opacity: 0.5;
}
.grocery-item button.del:hover { opacity: 1; color: var(--error); }
.add-form { display: flex; gap: 8px; align-items: flex-end; flex-wrap: wrap; margin-top: 8px; }
.add-form input.item-input { flex: 1; min-width: 140px; }
.add-form input.qty-input { width: 60px; }
.add-form input.unit-input { width: 60px; }
.add-form button { margin: 0; padding: 11px 18px; }
/* Floating "jump to add" ↓ on long grocery lists. Sits 16px above the fixed
   bottom nav, whose height already includes the home-indicator inset; the
   right inset clears a landscape notch. Below modal overlays (z-index 50+). */
.jump-add {
    position: fixed;
    right: calc(16px + env(safe-area-inset-right));
    bottom: calc(var(--nav-h) + env(safe-area-inset-bottom) + 16px);
    z-index: 10;
    width: 48px; height: 48px; margin: 0; padding: 0;
    border-radius: 50%;
    background: var(--accent); color: var(--accent-ink);
    font-size: 1.4rem; line-height: 1;
    display: flex; align-items: center; justify-content: center;
    box-shadow: 0 4px 14px rgba(0,0,0,0.45);
}
/* Room for the floating ↓ at the bottom of the grocery card: 48px button +
   16px gap + the card's own 28px. Toggled from Rust rather than `:has()`,
   which older Safari lacks. */
.card.has-jump { padding-bottom: 92px; }
/* Grocery sections (Apteekki, Rautakauppa, …) */
.grocery-section { margin-top: 22px; }
.section-header {
    display: flex; align-items: center; justify-content: space-between;
    border-bottom: 2px solid var(--line);
    padding-bottom: 4px; margin-bottom: 4px;
}
.section-header .section-title { font-size: 1rem; margin: 0; }
.section-header button.del {
    background: none; border: none; color: var(--muted);
    font-size: 1.1rem; cursor: pointer; padding: 4px 8px; margin: 0;
    opacity: 0.5;
}
.section-header button.del:hover { opacity: 1; color: var(--error); }
.section-empty { font-size: 0.85rem; margin: 6px 0; }
/* Aisle dividers in the main list (unsectioned items, grouped by store aisle) */
.aisle-divider {
    display: flex; align-items: center; gap: 8px;
    margin: 10px 0 2px; color: var(--muted);
    font-size: 0.72rem; text-transform: uppercase; letter-spacing: 0.06em;
}
.aisle-divider::after { content: ""; flex: 1; border-top: 1px solid var(--line); }
.item-name.assignable { cursor: pointer; }
.sheet-backdrop {
    position: fixed; inset: 0; background: rgba(0,0,0,0.4);
    display: flex; align-items: flex-end; justify-content: center; z-index: 50;
}
.assign-sheet {
    background: var(--card); width: 100%; max-width: 480px;
    border-radius: 12px 12px 0 0; padding: 16px; max-height: 70vh; overflow-y: auto;
}
.assign-title { font-weight: 600; margin: 0 0 10px; }
.assign-option {
    display: block; width: 100%; text-align: left; padding: 10px 12px;
    border: none; background: none; font-size: 1rem; border-radius: 8px;
}
.assign-option:hover { background: rgba(128,128,128,0.12); }
.assign-option.muted { color: var(--muted); }
.assign-cancel { display: block; width: 100%; margin-top: 8px; padding: 10px; }
.edit-sheet {
    background: var(--card); width: 100%; max-width: 480px;
    border-radius: 12px 12px 0 0; padding: 16px; max-height: 70vh; overflow-y: auto;
}
.edit-field {
    width: 100%; box-sizing: border-box; padding: 10px 12px;
    margin: 4px 0; font-size: 1rem;
    border: 1px solid var(--line); border-radius: 8px; background: var(--card); color: inherit;
}
.edit-row { display: flex; gap: 8px; }
.edit-row .edit-field { flex: 1; }
.assign-option.selected { background: rgba(128,128,128,0.18); font-weight: 600; }
.assign-save { display: block; width: 100%; margin-top: 8px; padding: 10px; font-weight: 600; }
/* Sort control (Hyllyt / Aakkoset / Suosikit) + star marker */
.sort-control { display: flex; gap: 6px; margin: 0 0 14px; }
.sort-btn {
    flex: 1; padding: 8px 10px; border: 1px solid var(--line);
    background: none; color: inherit; border-radius: 8px; cursor: pointer; font-size: 0.85rem;
}
.sort-btn.active { background: var(--accent); color: var(--accent-ink); border-color: var(--accent); }
.item-star { margin-left: 4px; }
/* Hyllyt (aisle editor) */
.aisle-editor { margin-top: 18px; padding: 12px; border: 1px solid var(--line); border-radius: 10px; background: var(--card); }
.aisle-editor-list { list-style: none; padding: 0; margin: 8px 0; }
.aisle-editor-row { display: flex; align-items: center; gap: 6px; padding: 6px 0; }
.aisle-editor-row .aisle-name { flex: 1; cursor: pointer; }
.aisle-editor-row button { margin-top: 0; }
.aisle-editor-row button.del {
    background: none; border: none; padding: 4px 8px;
    opacity: 0.6; font-weight: 400;
}
.aisle-editor-row button.del:hover { opacity: 1; color: var(--error); }
.aisle-move { min-width: 34px; }
.new-section-btn {
    margin: 18px 0; width: 100%;
    background: transparent;
    border: 1px dashed var(--line);
    color: var(--muted);
    font-size: 0.9rem;
}
.clear-btn {
    margin-top: 12px; width: 100%;
    background: transparent;
    border: 1px solid var(--line);
    color: var(--muted);
    font-size: 0.9rem;
}
/* Offline queue banner (grocery + calendar) */
.offline-banner {
    background: rgba(255,190,90,0.14);
    border: 1px solid rgba(255,190,90,0.4);
    color: #ffd591;
    padding: 8px 12px; border-radius: 10px; margin: 8px 0;
    font-size: 0.85rem;
}
/* Confirm dialog: fixed backdrop covers the viewport, the box centres in it. */
.modal-backdrop {
    position: fixed; inset: 0; z-index: 100;
    background: rgba(0,0,0,0.6);
    display: flex; align-items: center; justify-content: center;
    padding: 24px;
}
.modal {
    background: var(--card);
    border: 1px solid var(--line);
    border-radius: 16px;
    padding: 24px;
    width: 100%; max-width: 360px;
    box-shadow: 0 20px 60px rgba(0,0,0,0.5);
}
.modal p { margin: 0 0 4px; }
/* Buttons share the row evenly; danger flags the irreversible choice. */
.modal .row button { flex: 1; }
button.danger { background: var(--error); color: var(--accent-ink); }
/* Calendar month grid */
.month-grid { display: grid; grid-template-columns: repeat(7, 1fr); gap: 2px; margin-top: 10px; }
.month-head { text-align: center; font-size: 0.7rem; color: var(--muted); padding: 4px 0; }
.month-cell { min-height: 64px; border: 1px solid var(--line); border-radius: 6px; padding: 3px; overflow: hidden; }
.month-cell.dim { opacity: 0.4; }
.month-cell.today { border-color: var(--accent); background: rgba(108,140,255,0.08); }
.month-daynum { font-size: 0.75rem; color: var(--muted); }
/* Today's day number: an accent pill so the current day is easy to spot. */
.month-daynum.today, .week-head .today {
    display: inline-block;
    background: var(--accent); color: var(--accent-ink);
    border-radius: 999px; padding: 0 6px; font-weight: 700;
}
.month-chip { display: block; font-size: 0.7rem; background: rgba(108,140,255,0.18); color: var(--accent); border-radius: 4px; padding: 1px 4px; margin-top: 2px; text-decoration: none; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
/* Calendar week view: hourly time grid. Every row (.week-row) shares one
   column template — a 22px hour-label column + 7 day columns — so labels,
   all-day strip and grid line up. minmax(0, 1fr) lets columns shrink below
   their content, so a long title can't push the page wider on a phone.
   All three rows reserve a scrollbar gutter, so the body's scrollbar (on
   desktop) doesn't shift its columns out of line with the header. */
.week { margin-top: 10px; }
.week-row { display: grid; grid-template-columns: 22px repeat(7, minmax(0, 1fr)); }
.week-heads, .week-allday-row { overflow-y: hidden; scrollbar-gutter: stable; }
.week-head { font-size: 0.7rem; color: var(--muted); text-align: center; padding: 4px 0; white-space: nowrap; overflow: hidden; }
.week-allday { min-width: 0; display: flex; flex-direction: column; gap: 2px; padding: 0 1px 4px; }
.week-allday.today-col { background: rgba(108,140,255,0.08); border-radius: 4px 4px 0 0; }
.week-chip { display: block; font-size: 0.62rem; background: rgba(108,140,255,0.18); color: var(--ink); border-radius: 4px; padding: 1px 3px; text-decoration: none; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.week-chip.allday { background: rgba(120,200,140,0.2); }
/* Body height = the month grid's cell area: 6 rows x 64px (.month-cell
   min-height) + 6 x 2px gaps = 396px, so switching Kuukausi <-> Viikko keeps
   the card roughly the same height. The 24h grid inside scrolls. */
.week-body {
    height: 396px; overflow-y: auto; overflow-x: hidden; scrollbar-gutter: stable;
    border: 1px solid var(--line); border-radius: 6px;
}
/* 24 hours x 40px (HOUR_PX in week.rs). */
.week-grid { height: 960px; }
.week-hours { display: flex; flex-direction: column; }
.week-hour { height: 40px; flex: none; font-size: 0.6rem; color: var(--muted); text-align: right; padding: 1px 3px 0 0; border-top: 1px solid var(--line); }
.week-hour:first-child { border-top: none; }
/* Faint hour lines: a 1px line at the top of every 40px band. */
.week-day {
    position: relative; min-width: 0; border-left: 1px solid var(--line);
    background-image: repeating-linear-gradient(to bottom, var(--line) 0 1px, transparent 1px 40px);
}
.week-day.today-col { background-color: rgba(108,140,255,0.08); box-shadow: inset 0 0 0 1px var(--accent); }
/* Positioned timed event; top/height/left/width come inline from week.rs. */
.week-event {
    position: absolute; z-index: 1; overflow: hidden;
    font-size: 0.6rem; line-height: 1.2; padding: 1px 2px;
    background: rgba(108,140,255,0.30); border-left: 2px solid var(--accent); border-radius: 3px;
    color: var(--ink); text-decoration: none; overflow-wrap: anywhere;
}
.week-event-time { display: block; font-weight: 600; color: var(--accent); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
/* Planned dinner (meal-plan projection): amber, and not a link — the chip is
   read-only; dinners are edited on the Ruoka screen, not in the calendar. */
.month-chip.meal, .week-chip.meal { background: rgba(240,170,60,0.20); color: var(--ink); }
/* Announcements board */
.board-input { width: 100%; min-height: 70px; resize: vertical; }
.board-body { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
/* Kid-friendly chore cards: one big touch target per chore. */
.chore-cards { display: flex; flex-direction: column; gap: 8px; margin: 8px 0; }
.chore-card {
    display: flex; align-items: center; gap: 12px;
    padding: 14px; font-size: 1.05rem; text-align: left;
    border: 1px solid var(--line); border-radius: 10px;
    background: transparent; color: var(--ink); width: 100%;
}
.chore-card.done { opacity: 0.55; }
.chore-card.done .chore-title { text-decoration: line-through; }
.chore-check { font-size: 1.4rem; }
/* Weekly points strip: compact leaderboard above the chore lists. */
.points-strip {
    display: flex; flex-wrap: wrap; gap: 4px 12px; align-items: center;
    font-size: 0.9rem; margin: 8px 0;
}
.points-link { margin-left: auto; color: var(--accent); text-decoration: none; white-space: nowrap; }
/* Keep "🥇 Name 12" together — don't let it wrap mid-entry on narrow phones. */
.points-entry { white-space: nowrap; }
/* Point value badge on chore cards/rows. */
.chore-points {
    margin-left: auto; flex-shrink: 0;
    background: rgba(108,140,255,0.18); color: var(--accent);
    border-radius: 999px; padding: 1px 8px; font-size: 0.8rem; font-weight: 700;
}
/* 7 tabs need slightly smaller labels to fit narrow phones. */
.nav-tab .nav-label { font-size: 0.62rem; }
/* Perhechatti */
.nav-icon { position: relative; }
.nav-badge {
    position: absolute; top: -4px; right: -12px;
    background: var(--error); color: var(--accent-ink);
    border-radius: 999px; padding: 0 5px;
    font-size: 0.62rem; font-weight: 700; line-height: 1.4;
}
/* column-reverse + newest-first DOM order = scroll pinned to the bottom for
   free (no scroll JS): visually oldest on top, newest at the bottom. */
.chat-log {
    display: flex; flex-direction: column-reverse;
    gap: 8px; overflow-y: auto; max-height: 62vh; margin: 10px 0;
}
.chat-msg {
    max-width: 82%; padding: 8px 12px; border-radius: 12px;
    background: #232a3a; align-self: flex-start;
}
.chat-msg.mine { align-self: flex-end; background: rgba(108,140,255,0.22); }
.chat-meta { font-size: 0.7rem; color: var(--muted); margin-bottom: 2px; }
.chat-body { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.chat-reactions { display: flex; gap: 4px; margin-top: 4px; flex-wrap: wrap; }
.chat-chip {
    border: 1px solid var(--line); border-radius: 999px;
    padding: 0 8px; font-size: 0.8rem;
    background: transparent; color: var(--ink); margin: 0; cursor: pointer;
}
.chat-chip.own { border-color: var(--accent); background: rgba(108,140,255,0.15); }
.chat-send { display: flex; gap: 8px; align-items: flex-end; }
.chat-send input { flex: 1; }
.chat-send button { margin: 0; padding: 11px 18px; width: auto; }
.chat-older { width: 100%; }
/* Muistiot */
.note-check {
    display: flex; align-items: baseline; gap: 10px;
    padding: 6px 0; font-size: 1rem; cursor: pointer;
}
.note-check input[type="checkbox"] {
    width: 20px; height: 20px; flex-shrink: 0; accent-color: var(--accent);
}
.note-text { margin: 6px 0; white-space: pre-wrap; overflow-wrap: anywhere; }
.note-editor { min-height: 45vh; font-family: ui-monospace, monospace; resize: vertical; }
.clear-btn.note-delete { margin-top: 28px; }
/* Koti hub */
.home-menu { position: relative; }
.menu-pop {
    position: absolute; right: 0; top: 110%; z-index: 20;
    background: var(--card); border: 1px solid var(--line);
    border-radius: 10px; padding: 10px 14px; white-space: nowrap;
    box-shadow: 0 10px 30px rgba(0,0,0,0.35);
}
.menu-pop a { color: var(--ink); text-decoration: none; display: block; padding: 4px 0; }
.hub-card {
    display: block; margin-top: 12px; padding: 12px 14px;
    border: 1px solid var(--line); border-radius: 12px;
    text-decoration: none; color: var(--ink); width: 100%;
}
.hub-title {
    display: flex; justify-content: space-between; gap: 8px;
    font-size: 0.78rem; color: var(--muted);
    text-transform: uppercase; letter-spacing: 0.5px; margin-bottom: 6px;
}
.hub-row { display: flex; gap: 10px; padding: 3px 0; font-size: 0.95rem; min-width: 0; }
.hub-row .muted { flex-shrink: 0; }
/* Tilastot: 12-week stacked columns. 2px gaps separate members' segments
   (the colorblind-relief spacer); only the top segment gets rounded ends. */
.trend-chart {
    display: flex; gap: 4px; align-items: stretch;
    height: 150px; margin: 10px 0 4px;
    border-bottom: 1px solid var(--line);
}
.trend-col { flex: 1; display: flex; flex-direction: column; min-width: 0; }
/* column-reverse: first segment in DOM sits at the baseline. */
.trend-bars { flex: 1; display: flex; flex-direction: column-reverse; gap: 2px; }
.trend-seg { flex-shrink: 0; }
.trend-bars .trend-seg:last-child { border-radius: 4px 4px 0 0; }
.trend-label {
    text-align: center; font-size: 0.6rem; color: var(--muted);
    padding: 3px 0 2px; white-space: nowrap; overflow: hidden;
}
.legend { display: flex; gap: 12px; flex-wrap: wrap; font-size: 0.8rem; margin: 6px 0; }
.legend-dot {
    display: inline-block; width: 10px; height: 10px;
    border-radius: 50%; margin-right: 5px; vertical-align: -1px;
}
/* Per-chore breakdown table */
.stats-table { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
.stats-table th, .stats-table td {
    padding: 6px 4px; border-bottom: 1px solid var(--line);
    text-align: left; vertical-align: top;
}
.stats-table th { color: var(--muted); font-weight: 500; font-size: 0.8rem; }
/* Muutosloki (audit log): entity filter chips + the feed list. */
.chip {
    border: 1px solid var(--line); border-radius: 999px;
    padding: 4px 12px; font-size: 0.85rem;
    background: transparent; color: var(--muted); margin: 0; cursor: pointer;
}
.chip.active { border-color: var(--accent); background: rgba(108,140,255,0.15); color: var(--accent); }
ul.audit { list-style: none; padding: 0; margin: 10px 0 0; }
ul.audit li {
    padding: 10px 0; border-bottom: 1px solid var(--line);
    font-size: 0.92rem; line-height: 1.4;
}
.audit .actor { font-weight: 600; }
.audit .what { color: var(--ink); }
"#;
