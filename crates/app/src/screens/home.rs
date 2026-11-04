//! Koti: the front page. Composes every subsystem's "what matters right now"
//! into one glance — no new server endpoints, just the existing ones fetched
//! in parallel (each its own use_resource on the app-wide sync tick) and
//! seeded from the same localStorage caches the feature screens maintain, so
//! the page paints instantly and degrades per-card when offline.

use crate::api;
use crate::screens::Route;
use crate::screens::announcements::stamp;
use crate::screens::calendar::agenda::{day_label, time_label};
use crate::screens::calendar::meal_label;
use crate::screens::dates::{shift_day, today, weekday_fi};
use crate::store;
use dioxus::prelude::*;
use perkele_shared::announcement::Announcement;
use perkele_shared::calendar::{Event, Exdate, expand_events, starts_within};
use perkele_shared::chore::DueChore;
use perkele_shared::feature::Feature;
use perkele_shared::grocery::GroceryItem;
use std::collections::BTreeMap;

#[component]
pub fn HomeScreen() -> Element {
    // ⋯ menu (top-right): rarely-used, settings-ish destinations live here
    // instead of spending a nav tab (Minä is visited a few times a year).
    let mut menu_open = use_signal(|| false);
    let mut refresh = use_context::<Signal<u32>>(); // app-wide sync tick
    // Admin-hidden features: their cards are skipped below. The fetches stay
    // unconditional (hooks must run in the same order every render) — only
    // the rendering is gated.
    let features = super::use_family_features();
    let shown = move |f: Feature| !features.read().is_hidden(f);

    // --- 📌 Ilmoitustaulu ---------------------------------------------------
    let mut anns: Signal<Vec<Announcement>> = use_signal(store::get_announcements);
    let fetched_anns = use_resource(move || {
        let _ = refresh();
        async move { api::list_announcements().await }
    });
    use_effect(move || {
        if let Some(Ok(list)) = fetched_anns() {
            store::set_announcements(&list);
            anns.set(list);
        }
    });
    // Pinned posts (list is already pinned-first); none pinned → latest one.
    let pinned: Vec<Announcement> = {
        let all = anns();
        let p: Vec<Announcement> = all.iter().filter(|a| a.pinned).take(3).cloned().collect();
        if p.is_empty() {
            all.into_iter().take(1).collect()
        } else {
            p
        }
    };

    // --- 📅 Tulevat menot (7 days) -------------------------------------------
    let mut events: Signal<Vec<Event>> = use_signal(|| store::get_events().unwrap_or_default());
    let mut exdates: Signal<Vec<Exdate>> = use_signal(store::get_exdates);
    let fetched_cal = use_resource(move || {
        let _ = refresh();
        async move { api::calendar_sync().await }
    });
    use_effect(move || {
        if let Some(Ok(s)) = fetched_cal() {
            store::set_events(&s.events);
            store::set_exdates(&s.exdates);
            events.set(s.events);
            exdates.set(s.exdates);
        }
    });
    let day0 = today();
    let day7 = shift_day(&day0, 7);
    let win_start = format!("{day0}T00:00:00Z");
    let win_end = format!("{day7}T23:59:59Z");
    let mut upcoming = expand_events(&events(), &exdates(), &win_start, &win_end);
    // expand_events passes one-off events through UNTOUCHED (past ones too),
    // so clamp to the window ourselves before sorting.
    upcoming.retain(|e| starts_within(e, &day0, &day7));
    upcoming.sort_by(|a, b| a.starts_at.cmp(&b.starts_at));
    upcoming.truncate(6);
    let mut by_day: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    for ev in upcoming {
        let day = ev.starts_at[..10.min(ev.starts_at.len())].to_owned();
        by_day.entry(day).or_default().push(ev);
    }

    // --- 🍽 Päivällinen (today + tomorrow) -----------------------------------
    let meals = use_resource(move || {
        let _ = refresh();
        async move {
            let from = today();
            let to = shift_day(&from, 1);
            api::mealplan_range(&from, &to).await.unwrap_or_default()
        }
    });
    let meals = meals().unwrap_or_default();
    let dinner_today: Option<String> = meals
        .iter()
        .filter(|m| m.date == day0)
        .find_map(meal_label)
        .map(str::to_owned);
    let day1 = shift_day(&day0, 1);
    let dinner_tomorrow: Option<String> = meals
        .iter()
        .filter(|m| m.date == day1)
        .find_map(meal_label)
        .map(str::to_owned);

    // --- 🧹 Minun hommat tänään ----------------------------------------------
    let mut chores: Signal<Vec<DueChore>> = use_signal(store::get_chores_today);
    let fetched_chores = use_resource(move || {
        let _ = refresh();
        async move { api::chores_today().await }
    });
    use_effect(move || {
        if let Some(Ok(list)) = fetched_chores() {
            store::set_chores_today(&list);
            chores.set(list);
        }
    });
    let me = use_resource(api::me);
    let my = me().and_then(|r| r.ok());
    let my_id = my.as_ref().map(|u| u.id);
    // Same completer-or-admin rule for undo as the Kotityöt screen: someone
    // else's tick can't be undone (the server would 403 and keep the points).
    let may_toggle = |c: &DueChore| {
        c.done_by
            .is_none_or(|d| my.is_none() || super::can_modify(my.as_ref(), d))
    };
    // Mine = assigned to me or to anyone (same rule as the Kotityöt screen).
    let mine: Vec<DueChore> = chores()
        .into_iter()
        .filter(|c| c.assignee_id.is_none() || c.assignee_id == my_id)
        .collect();
    let toggle = move |c: DueChore| {
        spawn(async move {
            let _ = if c.done_by.is_some() {
                api::uncomplete_chore(c.chore_id).await
            } else {
                api::complete_chore(c.chore_id).await
            };
            let n = refresh.peek().wrapping_add(1);
            refresh.set(n);
        });
    };

    // --- 💬 Chatti teaser -----------------------------------------------------
    let chat = use_resource(move || {
        let _ = refresh();
        async move { api::chat_page(None, Some(1)).await }
    });
    let chat = chat().and_then(|r| r.ok());
    let unread = chat.as_ref().map(|p| p.unread_count).unwrap_or(0);
    // The page's messages are ascending, so the newest is the LAST one.
    let latest = chat.and_then(|p| p.messages.into_iter().next_back());
    let unread_label = if unread > 0 {
        format!(" — {unread} uutta")
    } else {
        String::new()
    };

    // --- 🛒 Ostoslista count --------------------------------------------------
    let mut items: Signal<Vec<GroceryItem>> = use_signal(|| store::get_items().unwrap_or_default());
    let fetched_items = use_resource(move || {
        let _ = refresh();
        async move { api::sync().await }
    });
    use_effect(move || {
        if let Some(Ok(s)) = fetched_items() {
            store::set_items(&s.items);
            store::set_seq(s.seq);
            items.set(s.items);
        }
    });
    let unchecked = items().iter().filter(|i| !i.checked).count();
    let grocery_label = if unchecked == 0 {
        "Ostoslista on tyhjä.".to_owned()
    } else {
        format!("Ostoslistalla {unchecked} tuotetta.")
    };

    rsx! {
        div { class: "card wide",
            div { class: "row spread",
                h1 { "Koti" }
                div { class: "home-menu",
                    button {
                        class: "ghost",
                        style: "margin:0;padding:4px 12px;font-size:1.2rem;",
                        onclick: move |_| {
                            let open = *menu_open.peek();
                            menu_open.set(!open);
                        },
                        "⋯"
                    }
                    if menu_open() {
                        div { class: "menu-pop",
                            Link { to: Route::MeScreen {}, "👤 Minä" }
                        }
                    }
                }
            }

            if shown(Feature::Announcements) {
                // 📌 Ilmoitustaulu
                Link { to: Route::AnnouncementsScreen {}, class: "hub-card",
                    div { class: "hub-title",
                        span { "📌 Ilmoitustaulu" }
                        span { "Näytä kaikki →" }
                    }
                    if pinned.is_empty() {
                        p { class: "muted", style: "margin:0;", "Ei ilmoituksia." }
                    }
                    for a in pinned {
                        div { class: "hub-row",
                            span { class: "muted", "{stamp(&a.created_at)}" }
                            span { style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{a.body}" }
                        }
                    }
                }
            }

            if shown(Feature::Calendar) {
                // 📅 Tulevat menot
                Link { to: Route::CalendarScreen {}, class: "hub-card",
                    div { class: "hub-title",
                        span { "📅 Tulevat menot" }
                        span { "→ Kalenteri" }
                    }
                    if by_day.is_empty() {
                        p { class: "muted", style: "margin:0;", "Ei tapahtumia seuraavaan 7 päivään." }
                    }
                    for (day , evs) in by_day {
                        div { class: "hub-row",
                            span { class: "muted", "{weekday_fi(&day)} {day_label(&day)}" }
                            span {
                                for (i , ev) in evs.iter().enumerate() {
                                    if i > 0 {
                                        " · "
                                    }
                                    "{time_label(ev)} {ev.title}"
                                }
                            }
                        }
                    }
                }
            }

            if shown(Feature::Food) {
                // 🍽 Päivällinen
                Link { to: Route::MealPlanScreen {}, class: "hub-card",
                    div { class: "hub-title",
                        span { "🍽 Päivällinen" }
                        span { "→ Ruokalista" }
                    }
                    div { class: "hub-row",
                        span { class: "muted", "Tänään" }
                        span { {dinner_today.unwrap_or_else(|| "Ei suunniteltu.".to_owned())} }
                    }
                    if let Some(m) = dinner_tomorrow {
                        div { class: "hub-row",
                            span { class: "muted", "Huomenna" }
                            span { "{m}" }
                        }
                    }
                }
            }

            if shown(Feature::Chores) {
                // 🧹 Minun hommat tänään — tap-to-complete right here on the hub.
                // (A div, not a Link: the chore cards inside are buttons, and
                // interactive elements can't nest inside an anchor.)
                div { class: "hub-card",
                    div { class: "hub-title",
                        span { "🧹 Minun hommat tänään" }
                        Link {
                            to: Route::ChoresScreen {},
                            style: "color:inherit;text-decoration:none;",
                            "→ Kotityöt"
                        }
                    }
                    if mine.is_empty() {
                        p { class: "muted", style: "margin:0;", "Ei kotitöitä tänään. 🎉" }
                    }
                    div { class: "chore-cards",
                        for c in mine {
                            {
                                let done = c.done_by.is_some();
                                let can_toggle = may_toggle(&c);
                                let c2 = c.clone();
                                rsx! {
                                    button {
                                        class: if done { "chore-card done" } else { "chore-card" },
                                        disabled: !can_toggle,
                                        onclick: move |_| toggle(c2.clone()),
                                        span { class: "chore-check", if done { "✅" } else { "⬜" } }
                                        span { class: "chore-title", "{c.title}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if shown(Feature::Chat) {
                // 💬 Chatti
                Link { to: Route::ChatScreen {}, class: "hub-card",
                    div { class: "hub-title",
                        span { "💬 Chatti{unread_label}" }
                        span { "→ Chatti" }
                    }
                    if let Some(m) = latest {
                        div { class: "hub-row",
                            span { class: "muted", "{m.author_name}:" }
                            span { style: "overflow:hidden;text-overflow:ellipsis;white-space:nowrap;", "{m.body}" }
                        }
                    } else {
                        p { class: "muted", style: "margin:0;", "Ei viestejä vielä." }
                    }
                }
            }

            if shown(Feature::Grocery) {
                // 🛒 Ostoslista
                Link { to: Route::GroceryScreen {}, class: "hub-card",
                    div { class: "hub-title",
                        span { "🛒 Ostoslista" }
                        span { "→ Ostokset" }
                    }
                    p { class: "muted", style: "margin:0;", "{grocery_label}" }
                }
            }
        }
    }
}
