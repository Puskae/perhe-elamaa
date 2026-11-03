//! Grocery list screen: add, check off, delete, and clear items.
//! Refetches on the app-wide sync tick (NavLayout's single SSE stream) so
//! all open tabs stay in sync.
//!
//! Offline support: the list is seeded from localStorage on mount so it
//! appears instantly. Mutations while offline are queued; NavLayout replays
//! the queue when its SSE stream reconnects and bumps the tick, which lands
//! here as a fresh fetch.

use crate::store::SortMode;
use crate::{api, store};
use dioxus::prelude::*;
use perkele_shared::aisle::{Aisle, ItemAisle, SetItemAisleRequest};
use perkele_shared::grocery::{
    AddItemRequest, EditItemRequest, GroceryItem, SetCheckedRequest, SyncResponse,
    finnish_sort_key, validate_grocery_item_name, validate_section_name,
};
use std::collections::HashMap;

/// One grocery row (checkbox, name, qty, delete) — shared by the main list
/// and the section groups. Not a `#[component]`: it's called from inside
/// GroceryScreen's render loops and just returns markup wired to the screen's
/// signals. `apply_sync` is the screen's stale-guarded sync applier; it and
/// the signals are all `Copy`, so each handler carries its own copy.
#[allow(clippy::too_many_arguments)] // plain fn threading screen signals through, not a struct-of-args API
fn item_row(
    item: GroceryItem,
    starred: bool,
    // Author-or-admin: false hides the ✕ (the server would 403 it).
    may_delete: bool,
    mut items: Signal<Vec<GroceryItem>>,
    mut pending: Signal<bool>,
    mut inflight: Signal<u32>,
    mut last_applied_seq: Signal<i64>,
    mut apply_sync: impl FnMut(SyncResponse) + Copy + 'static,
    mut open_edit: impl FnMut(GroceryItem) + Copy + 'static,
) -> Element {
    let id = item.id;
    let checked = item.checked;
    rsx! {
        li {
            key: "{id}",
            class: if checked { "grocery-item checked" } else { "grocery-item" },
            input {
                r#type: "checkbox",
                checked: checked,
                onchange: move |_| {
                    // Optimistic: flip locally and re-sort right away, so
                    // the item moves down instantly no matter how slow the
                    // network is. The request follows in the background.
                    let target = {
                        let mut list = items.write();
                        let Some(i) = list.iter_mut().find(|i| i.id == id) else {
                            return;
                        };
                        i.checked = !i.checked;
                        let target = i.checked;
                        // Same order the server uses: unchecked first,
                        // oldest first (created_at is RFC3339, so string
                        // order == time order). Sections group at render
                        // time, so a global sort keeps per-section order
                        // right too.
                        list.sort_by(|a, b| {
                            a.checked
                                .cmp(&b.checked)
                                .then_with(|| a.created_at.cmp(&b.created_at))
                        });
                        target
                    };
                    store::set_items(&items());
                    spawn(async move {
                        if is_online() {
                            // Read into a plain u32 before calling set():
                            // peek() returns a guard borrowing the signal,
                            // and it must drop before set() can borrow
                            // mutably (E0502 otherwise).
                            let n = *inflight.peek();
                            inflight.set(n + 1);
                            let sent = match api::set_checked(id, target).await {
                                Ok(resp) => {
                                    // Raise the stale-snapshot watermark to
                                    // our own write. Without this, a sync
                                    // fetched BEFORE the PUT landed could be
                                    // applied AFTER it and briefly un-check
                                    // this item (the check-off flicker bug) —
                                    // apply_sync now rejects it as stale.
                                    if resp.seq > *last_applied_seq.peek() {
                                        last_applied_seq.set(resp.seq);
                                    }
                                    true
                                }
                                Err(_) => false,
                            };
                            let n = *inflight.peek();
                            inflight.set(n.saturating_sub(1));
                            // No refetch here: the UI is already right, and
                            // the server's SSE poke triggers one anyway.
                            if sent {
                                return;
                            }
                        }
                        // Offline, or the request died on a bad connection:
                        // queue the *intent* (target state, not a flip) for
                        // replay when the network returns.
                        store::enqueue(store::QueuedMutation {
                            method: "PUT".into(),
                            url: format!("/api/grocery/{id}/check"),
                            body: serde_json::to_string(
                                &SetCheckedRequest { checked: target },
                            )
                            .ok(),
                        });
                        pending.set(true);
                    });
                },
            }
            span {
                class: "item-name assignable",
                onclick: {
                    let item = item.clone();
                    move |_| open_edit(item.clone())
                },
                "{item.name}"
                if starred {
                    span { class: "item-star", "⭐" }
                }
            }
            if item.qty.is_some() || item.unit.is_some() {
                span { class: "item-qty",
                    {format!(
                        "{}{}",
                        item.qty.as_deref().unwrap_or(""),
                        item.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default()
                    )}
                }
            }
            if may_delete {
                button {
                    class: "del",
                    title: "Poista",
                    onclick: move |_| {
                        spawn(async move {
                            if is_online() {
                                let _ = api::delete_item(id).await;
                                if let Ok(resp) = api::sync().await {
                                    apply_sync(resp);
                                }
                            } else {
                                // Optimistic remove
                                items.write().retain(|i| i.id != id);
                                store::set_items(&items());
                                store::enqueue(store::QueuedMutation {
                                    method: "DELETE".into(),
                                    url: format!("/api/grocery/{id}"),
                                    body: None,
                                });
                                pending.set(true);
                            }
                        });
                    },
                    "✕"
                }
            }
        }
    }
}

/// Split items into the unsectioned main list and named sections, for display.
/// Sections are sorted alphabetically; item order within each group is
/// preserved from the input (server order: unchecked first, oldest first).
fn group_items(items: &[GroceryItem]) -> (Vec<GroceryItem>, Vec<(String, Vec<GroceryItem>)>) {
    let mut main = Vec::new();
    let mut sections: Vec<(String, Vec<GroceryItem>)> = Vec::new();
    for item in items {
        match &item.category {
            None => main.push(item.clone()),
            Some(name) => match sections.iter_mut().find(|(n, _)| n == name) {
                Some((_, list)) => list.push(item.clone()),
                None => sections.push((name.clone(), vec![item.clone()])),
            },
        }
    }
    sections.sort_by(|a, b| a.0.cmp(&b.0));
    (main, sections)
}

/// Unchecked main-list items grouped by aisle for display: `None` label is
/// the unmapped "Lajittelematon" bucket, `Some(name)` an aisle name, groups
/// in the display order `group_main_by_aisle` decides.
type AisleGroups = Vec<(Option<String>, Vec<GroceryItem>)>;

/// Group the main list's UNCHECKED items by aisle for display: the unmapped
/// group ("Lajittelematon", label None) first so new items are seen and easy
/// to assign, then aisles in walking order; empty groups are omitted.
/// Checked items come back separately — they always render at the bottom, so
/// the staples pile keeps working exactly as before.
fn group_main_by_aisle(
    main: &[GroceryItem],
    aisles: &[Aisle],
    map: &HashMap<String, i64>,
) -> (AisleGroups, Vec<GroceryItem>) {
    let mut checked = Vec::new();
    let mut unsorted = Vec::new();
    // One bucket per aisle, in the given (position) order.
    let mut buckets: Vec<Vec<GroceryItem>> = vec![Vec::new(); aisles.len()];

    for item in main {
        if item.checked {
            checked.push(item.clone());
            continue;
        }
        let key = item.name.trim().to_lowercase();
        match map
            .get(&key)
            .and_then(|aid| aisles.iter().position(|a| a.id == *aid))
        {
            Some(idx) => buckets[idx].push(item.clone()),
            None => unsorted.push(item.clone()),
        }
    }

    let mut groups = Vec::new();
    if !unsorted.is_empty() {
        groups.push((None, unsorted));
    }
    for (aisle, bucket) in aisles.iter().zip(buckets) {
        if !bucket.is_empty() {
            groups.push((Some(aisle.name.clone()), bucket));
        }
    }
    (groups, checked)
}

/// Flat-mode grouping (Aakkoset / Suosikit). Unchecked items are sorted and
/// grouped; checked items always come back separately for the bottom pile,
/// so the staples workflow holds in every mode. `favorites` holds lowercased
/// starred names. `Alpha` → one unlabeled group; `Star` → starred group
/// ("Suosikit") then the rest. Both sort by finnish_sort_key.
fn flat_groups(
    items: &[GroceryItem],
    mode: SortMode,
    favorites: &std::collections::HashSet<String>,
) -> (AisleGroups, Vec<GroceryItem>) {
    let mut unchecked: Vec<GroceryItem> = Vec::new();
    let mut checked: Vec<GroceryItem> = Vec::new();
    for it in items {
        if it.checked {
            checked.push(it.clone());
        } else {
            unchecked.push(it.clone());
        }
    }
    checked.sort_by_key(|it| finnish_sort_key(&it.name));

    let is_fav = |it: &GroceryItem| favorites.contains(&it.name.trim().to_lowercase());

    let groups = match mode {
        SortMode::Star => {
            let (mut starred, mut rest): (Vec<_>, Vec<_>) =
                unchecked.into_iter().partition(|it| is_fav(it));
            starred.sort_by_key(|it| finnish_sort_key(&it.name));
            rest.sort_by_key(|it| finnish_sort_key(&it.name));
            let mut g = Vec::new();
            if !starred.is_empty() {
                g.push((Some("Suosikit".to_owned()), starred));
            }
            if !rest.is_empty() {
                g.push((None, rest));
            }
            g
        }
        // Alpha (and any non-flat value defensively): one sorted group.
        _ => {
            unchecked.sort_by_key(|it| finnish_sort_key(&it.name));
            if unchecked.is_empty() {
                Vec::new()
            } else {
                vec![(None, unchecked)]
            }
        }
    };
    (groups, checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: i64, name: &str, category: Option<&str>) -> GroceryItem {
        GroceryItem {
            id,
            name: name.into(),
            qty: None,
            unit: None,
            category: category.map(Into::into),
            checked: false,
            added_by: 1,
            recipe_id: None,
            created_at: format!("2026-07-15T00:00:0{id}Z"),
        }
    }

    #[test]
    fn group_items_splits_main_and_sorted_sections() {
        let items = [
            item(1, "naulat", Some("Rautakauppa")),
            item(2, "milk", None),
            item(3, "Burana", Some("Apteekki")),
            item(4, "Panadol", Some("Apteekki")),
            item(5, "bread", None),
        ];
        let (main, sections) = group_items(&items);

        // Main list: unsectioned items, input order preserved.
        let names: Vec<&str> = main.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["milk", "bread"]);

        // Sections alphabetical; items within a section keep input order.
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, "Apteekki");
        let names: Vec<&str> = sections[0].1.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["Burana", "Panadol"]);
        assert_eq!(sections[1].0, "Rautakauppa");
    }

    #[test]
    fn group_items_with_no_sections_is_all_main() {
        let items = [item(1, "milk", None)];
        let (main, sections) = group_items(&items);
        assert_eq!(main.len(), 1);
        assert!(sections.is_empty());
    }

    use perkele_shared::aisle::Aisle;

    fn aisle(id: i64, name: &str, position: i64) -> Aisle {
        Aisle {
            id,
            name: name.into(),
            position,
        }
    }

    fn checked_item(id: i64, name: &str) -> GroceryItem {
        GroceryItem {
            checked: true,
            ..item(id, name, None)
        }
    }

    #[test]
    fn aisle_grouping_orders_by_position_with_unsorted_first() {
        let main = [
            item(1, "jauheliha", None),
            item(2, "Maito", None), // case-insensitive lookup
            item(3, "sipuli", None),
            item(4, "purkka", None), // unmapped
            checked_item(5, "kahvi"),
        ];
        let aisles = [
            aisle(10, "kasvikset", 1),
            aisle(11, "maito", 2),
            aisle(12, "liha", 3),
        ];
        let map = HashMap::from([
            ("jauheliha".to_owned(), 12i64),
            ("maito".to_owned(), 11i64),
            ("sipuli".to_owned(), 10i64),
        ]);

        let (groups, checked) = group_main_by_aisle(&main, &aisles, &map);

        let labels: Vec<Option<&str>> = groups.iter().map(|(l, _)| l.as_deref()).collect();
        assert_eq!(
            labels,
            [None, Some("kasvikset"), Some("maito"), Some("liha")]
        );
        assert_eq!(groups[0].1[0].name, "purkka");
        assert_eq!(groups[2].1[0].name, "Maito");
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].name, "kahvi");
    }

    #[test]
    fn aisle_grouping_without_aisles_is_one_unlabeled_group() {
        let main = [item(1, "milk", None), item(2, "bread", None)];
        let (groups, checked) = group_main_by_aisle(&main, &[], &HashMap::new());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, None);
        assert_eq!(groups[0].1.len(), 2);
        assert!(checked.is_empty());
    }

    #[test]
    fn aisle_grouping_omits_empty_aisles() {
        let main = [item(1, "maito", None)];
        let aisles = [aisle(10, "kasvikset", 1), aisle(11, "maito", 2)];
        let map = HashMap::from([("maito".to_owned(), 11i64)]);
        let (groups, _) = group_main_by_aisle(&main, &aisles, &map);
        let labels: Vec<Option<&str>> = groups.iter().map(|(l, _)| l.as_deref()).collect();
        assert_eq!(labels, [Some("maito")]); // no empty kasvikset, no empty unsorted
    }

    use std::collections::HashSet;

    fn favs(names: &[&str]) -> HashSet<String> {
        names.iter().map(|n| n.to_lowercase()).collect()
    }

    #[test]
    fn flat_alpha_sorts_unchecked_finnish_checked_last() {
        let items = [
            item(1, "öljy", None),
            item(2, "banaani", None),
            checked_item(3, "apple"),
            item(4, "apple", None),
        ];
        let (groups, checked) = flat_groups(&items, SortMode::Alpha, &favs(&[]));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].0, None);
        let names: Vec<&str> = groups[0].1.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["apple", "banaani", "öljy"]);
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].name, "apple");
    }

    #[test]
    fn flat_star_groups_starred_first_case_insensitive() {
        let items = [
            item(1, "banaani", None),
            item(2, "Maito", None), // starred via lowercase key
            item(3, "apple", None),
            checked_item(4, "kahvi"),
        ];
        let (groups, checked) = flat_groups(&items, SortMode::Star, &favs(&["maito"]));
        assert_eq!(groups[0].0.as_deref(), Some("Suosikit"));
        let starred: Vec<&str> = groups[0].1.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(starred, ["Maito"]);
        assert_eq!(groups[1].0, None);
        let rest: Vec<&str> = groups[1].1.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(rest, ["apple", "banaani"]);
        assert_eq!(checked[0].name, "kahvi");
    }
}

/// True when the browser reports network connectivity (always true on native).
fn is_online() -> bool {
    web_sys::window()
        .map(|w| w.navigator().on_line())
        .unwrap_or(true)
}

/// DOM id of the main "Lisää tuote…" input, the target of the ↓ jump button.
const ADD_INPUT_ID: &str = "grocery-add-input";
/// The ↓ jump button shows only when more item rows than this are rendered.
const JUMP_ADD_MIN_ROWS: usize = 8;

#[component]
pub fn GroceryScreen() -> Element {
    let me = use_resource(api::me);
    // Seed immediately from cache so the list is visible before the network
    // call completes.
    let mut items: Signal<Vec<GroceryItem>> = use_signal(|| store::get_items().unwrap_or_default());
    let mut add_name = use_signal(String::new);
    let mut add_qty = use_signal(String::new);
    let mut add_unit = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);
    // Becomes true when offline mutations are sitting in the queue.
    let mut pending = use_signal(|| !store::get_queue().is_empty());
    // Sections that exist only client-side so far (freshly created, no items
    // yet). One becomes real server-side the moment its first item is added,
    // and vanishes on reload if still empty — sections are created right
    // before use, so that's acceptable.
    let mut new_sections: Signal<Vec<String>> = use_signal(Vec::new);
    // Per-section add-row drafts, keyed by section name. One HashMap signal
    // instead of a signal per row, because hooks can't be created in loops.
    let mut section_inputs: Signal<HashMap<String, String>> = use_signal(HashMap::new);
    let mut new_section_name = use_signal(String::new);
    let mut show_new_section = use_signal(|| false);
    let mut show_aisles = use_signal(|| false);
    // Highest sync seq applied this session. On a slow connection responses
    // arrive out of order; anything older than this is a stale snapshot and
    // must not overwrite newer state. (In-memory on purpose: the guard only
    // matters within one session, and a dev-DB reset would otherwise wedge a
    // persisted value.)
    let mut last_applied_seq = use_signal(|| 0i64);
    // Number of our own check-off requests currently on the wire.
    let inflight = use_signal(|| 0u32);
    // Item id being edited (edit sheet open when Some), plus the field drafts
    // and the aisle currently chosen in the sheet. Drafts are seeded once when
    // the sheet opens (see `open_edit`) so typing isn't clobbered on re-render.
    let mut editing: Signal<Option<i64>> = use_signal(|| None);
    let mut edit_name = use_signal(String::new);
    let mut edit_qty = use_signal(String::new);
    let mut edit_unit = use_signal(String::new);
    let mut edit_aisle: Signal<Option<i64>> = use_signal(|| None);
    // Whether the item being edited is starred; seeded in `open_edit`, applied
    // in `save_edit` via `pick_favorite`.
    let mut edit_starred = use_signal(|| false);

    // Chosen sort mode, seeded from localStorage (default Aisles). Persisted
    // on every change so it survives reloads (per-device, not synced).
    let mut sort_mode = use_signal(store::get_sort_mode);

    // Applies a sync response unless it's stale. Signals are `Copy`, so this
    // closure is too, and every handler below can carry its own copy.
    // `.peek()` reads without subscribing (a plain call would re-run reactive
    // scopes that use this closure whenever the seq changes — a feedback loop).
    let mut apply_sync = move |resp: SyncResponse| {
        if resp.seq < *last_applied_seq.peek() {
            return;
        }
        last_applied_seq.set(resp.seq);
        items.set(resp.items.clone());
        store::set_items(&resp.items);
        store::set_seq(resp.seq);
    };

    // Full sync, re-run on every app-wide tick: runs once on mount, then
    // whenever NavLayout's SSE stream reports a family mutation or finishes
    // replaying the offline queue.
    let sync_tick = use_context::<Signal<u32>>();
    let fetched = use_resource(move || {
        let _ = sync_tick(); // subscribe
        async move { api::sync().await }
    });
    use_effect(move || {
        if let Some(res) = fetched() {
            match res {
                Ok(resp) => {
                    // While our own check-offs are on the wire, a fetched
                    // snapshot may predate them — applying it would visually
                    // "uncheck" items just tapped. Skip it; reading inflight()
                    // subscribes this effect, so it re-runs (and applies the
                    // latest snapshot) once the count drops back to zero.
                    if inflight() > 0 {
                        return;
                    }
                    apply_sync(resp);
                    // NavLayout clears the queue before bumping the tick, so
                    // this flips pending off once the replay has landed.
                    pending.set(!store::get_queue().is_empty());
                }
                Err(e) => error.set(Some(e)),
            }
        }
    });

    // Aisles + item→aisle map, cached for offline, refetched on the same
    // app-wide tick as the items so both stay in step.
    let mut aisles_data: Signal<Option<perkele_shared::aisle::AislesResponse>> =
        use_signal(store::get_aisles);
    let mut aisles_fetched = use_resource(move || {
        let _ = sync_tick(); // subscribe
        async move { api::get_aisles().await }
    });
    use_effect(move || {
        if let Some(Ok(resp)) = aisles_fetched() {
            store::set_aisles(&resp);
            aisles_data.set(Some(resp));
        }
    });

    let mut submit_add = move || {
        let name = add_name().trim().to_owned();
        if let Err(e) = validate_grocery_item_name(&name) {
            error.set(Some(e.to_owned()));
            return;
        }
        if busy() {
            return;
        }
        error.set(None);
        busy.set(true);

        let qty = add_qty().trim().to_owned();
        let unit = add_unit().trim().to_owned();
        spawn(async move {
            // Adding offline is not supported: it would require a temporary
            // ID that conflicts with the server's ID space. Show an error.
            if !is_online() {
                error.set(Some(
                    "Ei yhteyttä — uuden tuotteen lisääminen vaatii verkon.".into(),
                ));
                busy.set(false);
                return;
            }
            let req = AddItemRequest {
                name,
                qty: if qty.is_empty() { None } else { Some(qty) },
                unit: if unit.is_empty() { None } else { Some(unit) },
                section: None,
            };
            match api::add_item(req).await {
                Ok(_) => {
                    add_name.set(String::new());
                    add_qty.set(String::new());
                    add_unit.set(String::new());
                    if let Ok(resp) = api::sync().await {
                        apply_sync(resp);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    // Add an item into a named section (the section groups' compact add row).
    // Capturing only Copy signals keeps this closure Copy, so every section's
    // handlers can carry their own copy.
    let mut submit_section_add = move |section: String| {
        let name = section_inputs
            .peek()
            .get(&section)
            .cloned()
            .unwrap_or_default()
            .trim()
            .to_owned();
        if let Err(e) = validate_grocery_item_name(&name) {
            error.set(Some(e.to_owned()));
            return;
        }
        if busy() {
            return;
        }
        error.set(None);
        busy.set(true);
        spawn(async move {
            if !is_online() {
                error.set(Some(
                    "Ei yhteyttä — uuden tuotteen lisääminen vaatii verkon.".into(),
                ));
                busy.set(false);
                return;
            }
            let req = AddItemRequest {
                name,
                qty: None,
                unit: None,
                section: Some(section.clone()),
            };
            match api::add_item(req).await {
                Ok(_) => {
                    section_inputs.write().remove(&section);
                    // The section is real (server-side) now.
                    new_sections.write().retain(|s| s != &section);
                    if let Ok(resp) = api::sync().await {
                        apply_sync(resp);
                    }
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    // Remove a section and its items ("done with the Apteekki trip").
    let mut remove_section = move |name: String| {
        // A section that only exists client-side (created but still empty)
        // can just be dropped locally — there's nothing on the server yet.
        let has_items = items
            .peek()
            .iter()
            .any(|i| i.category.as_deref() == Some(name.as_str()));
        new_sections.write().retain(|s| s != &name);
        section_inputs.write().remove(&name);
        if !has_items {
            return;
        }
        let confirmed = web_sys::window()
            .map(|w| {
                w.confirm_with_message(&format!("Poistetaanko osio ”{name}” ja sen tuotteet?"))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !confirmed {
            return;
        }
        spawn(async move {
            if is_online() {
                match api::delete_section(&name).await {
                    Ok(_) => {
                        if let Ok(resp) = api::sync().await {
                            apply_sync(resp);
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
            } else {
                // Optimistic remove + queue the intent for replay, like the
                // per-item delete does.
                items
                    .write()
                    .retain(|i| i.category.as_deref() != Some(name.as_str()));
                store::set_items(&items());
                store::enqueue(store::QueuedMutation {
                    method: "DELETE".into(),
                    url: format!("/api/grocery/section/{}", api::encode_path_segment(&name)),
                    body: None,
                });
                pending.set(true);
            }
        });
    };

    // Create a new (still empty) section from the "+ Uusi osio" input.
    let mut create_section = move || {
        let name = new_section_name().trim().to_owned();
        if let Err(e) = validate_section_name(&name) {
            error.set(Some(e.to_owned()));
            return;
        }
        error.set(None);
        // If the section already exists (live items or already pending),
        // don't add a duplicate group — just close the input.
        let exists = items
            .peek()
            .iter()
            .any(|i| i.category.as_deref() == Some(name.as_str()))
            || new_sections.peek().contains(&name);
        if !exists {
            new_sections.write().push(name);
        }
        new_section_name.set(String::new());
        show_new_section.set(false);
    };

    // Assign `name` to an aisle (or clear with None): optimistic local map
    // update + server write; offline falls back to the replay queue like
    // every other mutation.
    let mut pick_aisle = move |name: String, aisle_id: Option<i64>| {
        let key = name.trim().to_lowercase();
        // Optimistic: patch the cached map so the list regroups immediately.
        if let Some(mut data) = aisles_data() {
            data.map.retain(|m| m.item_name != key);
            if let Some(aid) = aisle_id {
                data.map.push(ItemAisle {
                    item_name: key.clone(),
                    aisle_id: aid,
                });
            }
            store::set_aisles(&data);
            aisles_data.set(Some(data));
        }
        spawn(async move {
            if is_online() && api::set_item_aisle(&key, aisle_id).await.is_ok() {
                return; // SSE tick will confirm
            }
            store::enqueue(store::QueuedMutation {
                method: "PUT".into(),
                url: "/api/aisles/map".into(),
                body: serde_json::to_string(&SetItemAisleRequest {
                    item_name: key,
                    aisle_id,
                })
                .ok(),
            });
            pending.set(true);
        });
    };

    // Star/unstar `name` (per-name, family-wide): optimistic local patch of
    // the cached favorites + server write, offline falls back to the replay
    // queue. Mirrors `pick_aisle` exactly.
    let mut pick_favorite = move |name: String, starred: bool| {
        let key = name.trim().to_lowercase();
        if let Some(mut data) = aisles_data() {
            data.favorites.retain(|f| f != &key);
            if starred {
                data.favorites.push(key.clone());
            }
            store::set_aisles(&data);
            aisles_data.set(Some(data));
        }
        spawn(async move {
            if is_online() && api::set_favorite(&key, starred).await.is_ok() {
                return;
            }
            store::enqueue(store::QueuedMutation {
                method: "PUT".into(),
                url: "/api/aisles/favorite".into(),
                body: serde_json::to_string(&perkele_shared::aisle::SetFavoriteRequest {
                    item_name: key,
                    starred,
                })
                .ok(),
            });
            pending.set(true);
        });
    };

    // Seed the sheet from the tapped item and open it. Signals are Copy, so
    // this closure is Copy and can be handed to every row.
    let open_edit = move |item: GroceryItem| {
        // Clear any stale validation error from a previous edit attempt, so a
        // rejected-then-cancelled edit doesn't leave its banner showing.
        error.set(None);
        edit_name.set(item.name.clone());
        edit_qty.set(item.qty.clone().unwrap_or_default());
        edit_unit.set(item.unit.clone().unwrap_or_default());
        // The item's current aisle, looked up in the name→aisle map by the
        // same normalized key pick_aisle writes (trimmed + lowercased).
        let key = item.name.trim().to_lowercase();
        let current = aisles_data().and_then(|d| {
            d.map
                .iter()
                .find(|m| m.item_name == key)
                .map(|m| m.aisle_id)
        });
        edit_aisle.set(current);
        // Seed the star toggle from the family-wide favorites list, keyed the
        // same way pick_favorite writes it.
        edit_starred.set(
            aisles_data()
                .map(|d| d.favorites.iter().any(|f| f == &key))
                .unwrap_or(false),
        );
        editing.set(Some(item.id));
    };

    // Commit the edit: validate, optimistically patch local state, PUT the item
    // (or queue it offline), then re-apply the aisle only if it changed.
    let mut save_edit = move |id: i64| {
        let name = edit_name().trim().to_owned();
        if let Err(msg) = validate_grocery_item_name(&name) {
            error.set(Some(msg.to_owned()));
            return;
        }
        // Blank qty/unit mean "no value" → None, matching add_item.
        let qty = {
            let q = edit_qty().trim().to_owned();
            (!q.is_empty()).then_some(q)
        };
        let unit = {
            let u = edit_unit().trim().to_owned();
            (!u.is_empty()).then_some(u)
        };

        // Optimistic: patch the item in the local list so the row updates now.
        {
            let mut list = items.write();
            if let Some(it) = list.iter_mut().find(|i| i.id == id) {
                it.name = name.clone();
                it.qty = qty.clone();
                it.unit = unit.clone();
            }
        }
        store::set_items(&items());

        // Send the item edit (online) or queue it (offline) for later replay.
        let req = EditItemRequest {
            name: name.clone(),
            qty: qty.clone(),
            unit: unit.clone(),
        };
        spawn(async move {
            if is_online() && api::edit_item(id, req.clone()).await.is_ok() {
                return; // SSE tick confirms
            }
            store::enqueue(store::QueuedMutation {
                method: "PUT".into(),
                url: format!("/api/grocery/{id}"),
                body: serde_json::to_string(&req).ok(),
            });
            pending.set(true);
        });

        // Aisle is keyed by name. Re-apply under the (possibly renamed) name
        // only if the chosen aisle differs from what the map has now — this
        // reuses pick_aisle's own online/offline handling. Renaming detaches
        // from the old name's mapping by design (family-wide map, left intact).
        let key = name.to_lowercase();
        let current = aisles_data().and_then(|d| {
            d.map
                .iter()
                .find(|m| m.item_name == key)
                .map(|m| m.aisle_id)
        });
        if edit_aisle() != current {
            pick_aisle(name.clone(), edit_aisle());
        }

        // Apply the star only if it changed, reusing pick_favorite's online/
        // offline handling. Keyed by the (possibly renamed) name.
        let fav_now = aisles_data()
            .map(|d| d.favorites.iter().any(|f| f == &key))
            .unwrap_or(false);
        if edit_starred() != fav_now {
            pick_favorite(name.clone(), edit_starred());
        }

        error.set(None);
        editing.set(None);
    };

    let has_checked = items().iter().any(|i| i.checked);

    // Regroup for display on every render: unsectioned items first, then
    // sections (alphabetical), then freshly created still-empty sections.
    let (main_items, mut section_groups) = group_items(&items());
    for name in new_sections() {
        if !section_groups.iter().any(|(n, _)| n == &name) {
            section_groups.push((name, Vec::new()));
        }
    }

    // Main list, grouped by aisle: "Lajittelematon" first, then the walking
    // order; checked staples stay at the very bottom. Precomputed here (not
    // inline in the rsx! below) because block expressions inside rsx! don't
    // mix well with the macro.
    let (aisles, aisle_map): (Vec<Aisle>, HashMap<String, i64>) = match aisles_data() {
        Some(d) => (
            d.aisles,
            d.map
                .into_iter()
                .map(|m| (m.item_name, m.aisle_id))
                .collect(),
        ),
        None => (Vec::new(), HashMap::new()),
    };
    let (aisle_groups, checked_items) = group_main_by_aisle(&main_items, &aisles, &aisle_map);
    let aisle_groups_labeled =
        aisle_groups.len() > 1 || aisle_groups.first().is_some_and(|(l, _)| l.is_some());

    // Lowercased starred names, for O(1) row lookups + Star-mode grouping.
    let favorites: std::collections::HashSet<String> = aisles_data()
        .map(|d| d.favorites.into_iter().collect())
        .unwrap_or_default();

    // Flat (Alpha/Star) grouping, precomputed like the aisle grouping above —
    // only built outside Aisles mode since it folds ALL items (sections
    // included) into one flat structure.
    let flat = if sort_mode() == SortMode::Aisles {
        None
    } else {
        Some(flat_groups(&items(), sort_mode(), &favorites))
    };

    // Delete permissions (author-or-admin, mirrored from the server). The
    // grocery list is offline-first, so an UNKNOWN viewer (`/api/me` failed
    // while offline) keeps every button: the server still enforces the rule,
    // and a refused replay shows up in the app-wide banner. A plain `|..|`
    // closure (no `move`) borrows `my` for this render only.
    let my = me().and_then(|r| r.ok());
    let may_delete = |added_by: i64| {
        my.as_ref()
            .is_none_or(|u| u.id == added_by || u.role.is_admin())
    };
    // A section's ✕ removes ALL its items, so the server wants every one of
    // them to be deletable by the caller (all-or-nothing, see delete_section).
    let may_delete_section = |sec: &[GroceryItem]| sec.iter().all(|i| may_delete(i.added_by));

    // The "jump to add" button only earns its place once the list is long
    // enough to push the main add form below the fold. Every item renders as
    // exactly one row in every sort mode (sections + aisle groups + checked
    // pile in Hyllyt, one flat fold in Aakkoset/Suosikit), so the item count
    // is the rendered row count.
    let show_jump_add = items().len() > JUMP_ADD_MIN_ROWS;

    rsx! {
        // `has-jump` reserves room under the last control so the floating ↓
        // can't cover it at max scroll — only while the button is shown.
        div { class: if show_jump_add { "card wide has-jump" } else { "card wide" },
            h1 { "Ostoslista" }

            // Floating ↓ (fixed-position, so where it sits in the tree doesn't
            // matter). `document::eval` runs the JS synchronously inside this
            // click handler, i.e. still inside the user gesture — which iOS
            // Safari requires before `focus()` may open the keyboard. Focus
            // goes first (with preventScroll so the browser doesn't jump), then
            // the smooth scroll brings the form to mid-screen.
            if show_jump_add {
                button {
                    class: "jump-add",
                    title: "Lisää tuote",
                    aria_label: "Lisää tuote",
                    onclick: move |_| {
                        document::eval(&format!(
                            "const el = document.getElementById('{ADD_INPUT_ID}'); \
                             if (el) {{ el.focus({{preventScroll: true}}); \
                             el.scrollIntoView({{behavior: 'smooth', block: 'center'}}); }}"
                        ));
                    },
                    "↓"
                }
            }

            // Sort control: which view of the list is shown below. Persisted
            // per-device via `store::set_sort_mode` so it survives reloads.
            div { class: "sort-control",
                button {
                    class: if sort_mode() == SortMode::Aisles { "sort-btn active" } else { "sort-btn" },
                    onclick: move |_| { sort_mode.set(SortMode::Aisles); store::set_sort_mode(SortMode::Aisles); },
                    "Hyllyt"
                }
                button {
                    class: if sort_mode() == SortMode::Alpha { "sort-btn active" } else { "sort-btn" },
                    onclick: move |_| { sort_mode.set(SortMode::Alpha); store::set_sort_mode(SortMode::Alpha); },
                    "Aakkoset"
                }
                button {
                    class: if sort_mode() == SortMode::Star { "sort-btn active" } else { "sort-btn" },
                    onclick: move |_| { sort_mode.set(SortMode::Star); store::set_sort_mode(SortMode::Star); },
                    "Suosikit"
                }
            }

            if pending() {
                p { class: "offline-banner",
                    "Muutoksia jonossa — synkronoidaan kun yhteys palautuu."
                }
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            if items().is_empty() && new_sections().is_empty() {
                p { class: "muted", "Lista on tyhjä. Lisää jotain!" }
            }

            // Hyllyt mode: sections on top, then the main list grouped by
            // aisle, checked staples at the very bottom — exactly the
            // pre-Task-7 layout. Aakkoset/Suosikit fold sections into one
            // flat list instead (see the `else if let` branch below), so
            // none of this renders in those modes.
            if sort_mode() == SortMode::Aisles {
                // Sections render ABOVE the main list on purpose: the main
                // list holds long-lived staples (many of them checked off),
                // so a section created for today's errand would be buried
                // below the crossed-off pile if it came last.
                for (name, sec_items) in section_groups {
                    div { key: "{name}", class: "grocery-section",
                        div { class: "section-header",
                            h2 { class: "section-title", "{name}" }
                            if may_delete_section(&sec_items) {
                                button {
                                    class: "del",
                                    title: "Poista osio",
                                    onclick: {
                                        let name = name.clone();
                                        move |_| remove_section(name.clone())
                                    },
                                    "✕"
                                }
                            }
                        }
                        if sec_items.is_empty() {
                            p { class: "muted section-empty", "Ei tuotteita vielä." }
                        } else {
                            ul { class: "grocery-list",
                                for item in sec_items {
                                    {
                                        let starred = favorites.contains(&item.name.trim().to_lowercase());
                                        let may_delete = may_delete(item.added_by);
                                        item_row(item, starred, may_delete, items, pending, inflight, last_applied_seq, apply_sync, open_edit)
                                    }
                                }
                            }
                        }
                        div { class: "add-form section-add",
                            input {
                                class: "item-input",
                                placeholder: "Lisää tuote…",
                                value: "{section_inputs().get(&name).cloned().unwrap_or_default()}",
                                oninput: {
                                    let name = name.clone();
                                    move |e: Event<FormData>| {
                                        section_inputs.write().insert(name.clone(), e.value());
                                    }
                                },
                                onkeydown: {
                                    let name = name.clone();
                                    move |e: Event<KeyboardData>| {
                                        if e.key() == Key::Enter {
                                            submit_section_add(name.clone());
                                        }
                                    }
                                },
                            }
                            button {
                                class: "primary",
                                disabled: busy(),
                                onclick: {
                                    let name = name.clone();
                                    move |_| submit_section_add(name.clone())
                                },
                                "Lisää"
                            }
                        }
                    }
                }

                for (label, group) in aisle_groups {
                    if aisle_groups_labeled {
                        div { class: "aisle-divider",
                            span { {label.clone().unwrap_or_else(|| "Lajittelematon".to_owned())} }
                        }
                    }
                    ul { class: "grocery-list",
                        for item in group {
                            {
                                let starred = favorites.contains(&item.name.trim().to_lowercase());
                                let may_delete = may_delete(item.added_by);
                                item_row(item, starred, may_delete, items, pending, inflight, last_applied_seq, apply_sync, open_edit)
                            }
                        }
                    }
                }
                if !checked_items.is_empty() {
                    ul { class: "grocery-list",
                        for item in checked_items {
                            {
                                let starred = favorites.contains(&item.name.trim().to_lowercase());
                                let may_delete = may_delete(item.added_by);
                                item_row(item, starred, may_delete, items, pending, inflight, last_applied_seq, apply_sync, open_edit)
                            }
                        }
                    }
                }
            } else if let Some((flat_groups_vec, flat_checked)) = flat {
                // Aakkoset (one unlabeled group) / Suosikit ("Suosikit" then
                // the rest): sections fold in because `flat_groups` sorted
                // ALL items, ignoring `category`. Checked items still land in
                // their own pile at the bottom.
                for (label, group) in flat_groups_vec {
                    if let Some(label) = label {
                        div { class: "aisle-divider",
                            span { "{label}" }
                        }
                    }
                    ul { class: "grocery-list",
                        for item in group {
                            {
                                let starred = favorites.contains(&item.name.trim().to_lowercase());
                                let may_delete = may_delete(item.added_by);
                                item_row(item, starred, may_delete, items, pending, inflight, last_applied_seq, apply_sync, open_edit)
                            }
                        }
                    }
                }
                if !flat_checked.is_empty() {
                    ul { class: "grocery-list",
                        for item in flat_checked {
                            {
                                let starred = favorites.contains(&item.name.trim().to_lowercase());
                                let may_delete = may_delete(item.added_by);
                                item_row(item, starred, may_delete, items, pending, inflight, last_applied_seq, apply_sync, open_edit)
                            }
                        }
                    }
                }
            }

            div { class: "add-form",
                input {
                    id: ADD_INPUT_ID,
                    class: "item-input",
                    placeholder: "Lisää tuote…",
                    value: "{add_name}",
                    oninput: move |e| add_name.set(e.value()),
                    onkeydown: move |e| {
                        if e.key() == Key::Enter {
                            submit_add();
                        }
                    },
                }
                input {
                    class: "qty-input",
                    placeholder: "Määrä",
                    value: "{add_qty}",
                    oninput: move |e| add_qty.set(e.value()),
                }
                input {
                    class: "unit-input",
                    placeholder: "Yksikkö",
                    value: "{add_unit}",
                    oninput: move |e| add_unit.set(e.value()),
                }
                button {
                    class: "primary",
                    disabled: busy(),
                    onclick: move |_| submit_add(),
                    if busy() { "Lisätään…" } else { "Lisää" }
                }
            }

            if has_checked {
                button {
                    class: "clear-btn",
                    onclick: move |_| {
                        spawn(async move {
                            if is_online() {
                                let _ = api::clear_checked().await;
                                if let Ok(resp) = api::sync().await {
                                    apply_sync(resp);
                                }
                            } else {
                                // Optimistic: remove checked items from local view
                                items.write().retain(|i| !i.checked);
                                store::set_items(&items());
                                store::enqueue(store::QueuedMutation {
                                    method: "POST".into(),
                                    url: "/api/grocery/clear-checked".into(),
                                    body: None,
                                });
                                pending.set(true);
                            }
                        });
                    },
                    "Poista merkityt tuotteet"
                }
            }

            // List management (new section / aisle editor) lives at the very
            // bottom: it is secondary to adding and checking off items, so it
            // stays out of the way below the lists and the add form.
            if show_new_section() {
                div { class: "add-form new-section-form",
                    input {
                        class: "item-input",
                        placeholder: "Osion nimi…",
                        value: "{new_section_name}",
                        autofocus: true,
                        oninput: move |e| new_section_name.set(e.value()),
                        onkeydown: move |e| {
                            if e.key() == Key::Enter {
                                create_section();
                            }
                        },
                    }
                    button { class: "primary", onclick: move |_| create_section(), "Luo" }
                    button {
                        onclick: move |_| {
                            show_new_section.set(false);
                            new_section_name.set(String::new());
                        },
                        "Peru"
                    }
                }
            } else {
                button {
                    class: "new-section-btn",
                    onclick: move |_| show_new_section.set(true),
                    "+ Uusi osio"
                }
            }
            button {
                class: "new-section-btn",
                onclick: move |_| show_aisles.set(!show_aisles()),
                if show_aisles() { "Sulje hyllyt" } else { "Hyllyt" }
            }
            if show_aisles() {
                super::AisleEditor {
                    aisles_data,
                    on_changed: move |_| {
                        aisles_fetched.restart();
                    },
                }
            }

            if let Some(id) = editing() {
                div { class: "sheet-backdrop", onclick: move |_| { error.set(None); editing.set(None); },
                    div { class: "edit-sheet", onclick: move |e| e.stop_propagation(),
                        p { class: "assign-title", "Muokkaa tuotetta" }
                        input {
                            class: "edit-field",
                            r#type: "text",
                            placeholder: "Nimi",
                            value: "{edit_name}",
                            oninput: move |e| edit_name.set(e.value()),
                        }
                        div { class: "edit-row",
                            input {
                                class: "edit-field",
                                r#type: "text",
                                placeholder: "Määrä",
                                value: "{edit_qty}",
                                oninput: move |e| edit_qty.set(e.value()),
                            }
                            input {
                                class: "edit-field",
                                r#type: "text",
                                placeholder: "Yksikkö",
                                value: "{edit_unit}",
                                oninput: move |e| edit_unit.set(e.value()),
                            }
                        }
                        p { class: "assign-title", "Hylly" }
                        for a in aisles_data().map(|d| d.aisles).unwrap_or_default() {
                            button {
                                key: "{a.id}",
                                class: if edit_aisle() == Some(a.id) { "assign-option selected" } else { "assign-option" },
                                onclick: move |_| edit_aisle.set(Some(a.id)),
                                "{a.name}"
                            }
                        }
                        button {
                            class: if edit_aisle().is_none() { "assign-option muted selected" } else { "assign-option muted" },
                            onclick: move |_| edit_aisle.set(None),
                            "Ei hyllyä"
                        }
                        p { class: "assign-title", "Suosikki" }
                        button {
                            class: if edit_starred() { "assign-option selected" } else { "assign-option" },
                            onclick: move |_| edit_starred.set(!edit_starred()),
                            if edit_starred() { "⭐ Suosikki" } else { "Merkitse suosikiksi" }
                        }
                        button { class: "assign-save", onclick: move |_| save_edit(id), "Tallenna" }
                        button { class: "assign-cancel", onclick: move |_| { error.set(None); editing.set(None); }, "Peru" }
                    }
                }
            }
        }
    }
}
