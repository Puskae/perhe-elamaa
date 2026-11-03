//! Week view: an hourly time grid (Mon–Sun × 00–24), like a desktop calendar.
//!
//! Layout, top to bottom:
//! - a header row with the seven day labels ("Ma 3"), today highlighted;
//! - an all-day strip per day holding all-day events and the 🍽 dinner chip;
//! - a fixed-height, vertically scrolling body: a narrow hour-label column
//!   plus seven day columns, each hour `HOUR_PX` tall. Timed events are
//!   absolutely positioned blocks (top = start, height = duration); events
//!   that cross midnight are clipped to each day they touch. Overlapping
//!   events share the column width side by side via [`assign_lanes`].
//!
//! On mount and on every week change the body scrolls so 07:00 is at the top.
//! Prev/next shifts by a week.

use crate::api;
use crate::screens::Route;
use crate::screens::dates::{current_monday, shift_day, today};
use dioxus::prelude::*;
use perkele_shared::calendar::{Event, Exdate, event_overlaps, expand_events};
use perkele_shared::recipe::MealPlanEntry;

const WEEKDAYS: [&str; 7] = ["Ma", "Ti", "Ke", "To", "Pe", "La", "Su"];

/// Height of one hour row in pixels. Must match `.week-hour` / the hour-line
/// gradient period in the CSS (screens/mod.rs).
const HOUR_PX: u32 = 40;
/// Minutes in a day — the clip bound for events that run past midnight.
const DAY_MIN: u32 = 24 * 60;
/// Smallest drawn block, in minutes (27 min × 40/60 px = 18 px), so a
/// 5-minute event is still a tappable target.
const MIN_BLOCK_MIN: u32 = 27;
/// Hour shown at the top of the body after mount / week change.
const SCROLL_TO_HOUR: u32 = 7;
/// DOM id of the scrolling body, targeted by the scroll-to-07:00 JS.
const BODY_ID: &str = "week-body";

fn hhmm(utc: &str) -> String {
    if utc.len() >= 16 {
        utc[11..16].to_owned()
    } else {
        utc.to_owned()
    }
}

/// "HH:MM" part of a "YYYY-MM-DDTHH:MM:SSZ" string as minutes since 00:00.
/// Malformed input reads as 0 rather than panicking.
fn minutes_of(ts: &str) -> u32 {
    // `.get(a..b)` is the non-panicking slice: None if out of range.
    let h: u32 = ts.get(11..13).and_then(|s| s.parse().ok()).unwrap_or(0);
    let m: u32 = ts.get(14..16).and_then(|s| s.parse().ok()).unwrap_or(0);
    (h * 60 + m).min(DAY_MIN)
}

/// The event's [start, end) in minutes within `day` ('YYYY-MM-DD'), clipped to
/// 00:00–24:00. `None` when the event doesn't actually occupy any of the day
/// (e.g. one ending exactly at 00:00 of this day — `event_overlaps` is
/// inclusive, so it would otherwise show up as a zero-height block).
fn clip_to_day(starts_at: &str, ends_at: &str, day: &str) -> Option<(u32, u32)> {
    let start_day = starts_at.get(..10).unwrap_or("");
    let end_day = ends_at.get(..10).unwrap_or("");
    // String comparison works because the dates are zero-padded ISO.
    let start = if start_day < day {
        0
    } else if start_day == day {
        minutes_of(starts_at)
    } else {
        return None; // starts after this day
    };
    let end = if end_day > day {
        DAY_MIN
    } else if end_day == day {
        minutes_of(ends_at)
    } else {
        return None; // ended before this day
    };
    // A zero-length event that *starts* today is still shown (min height);
    // one that started earlier and ends at 00:00 today is not.
    if end < start || (end == start && start_day != day) {
        return None;
    }
    Some((start, end))
}

/// Side-by-side layout for overlapping events in one day column.
///
/// Input: `(start, end)` minute ranges (any order). Output, index-aligned with
/// the input: `(lane, lanes)` — the event's lane (0-based) and how many lanes
/// its overlap cluster uses, so it is drawn at `left = lane/lanes`,
/// `width = 1/lanes`.
///
/// Greedy: sort by start; each event takes the first lane whose last event
/// ended at or before it starts, else opens a new lane. A *cluster* is a run
/// of transitively overlapping events; when an event starts after everything
/// so far has ended, the cluster closes and lanes reset, so an unrelated
/// event later in the day gets the full width again.
fn assign_lanes(ranges: &[(u32, u32)]) -> Vec<(usize, usize)> {
    let mut order: Vec<usize> = (0..ranges.len()).collect();
    // Sort indices, not the ranges, so results map back to input positions.
    order.sort_by_key(|&i| ranges[i]);

    let mut out = vec![(0, 1); ranges.len()];
    let mut lane_ends: Vec<u32> = Vec::new(); // last end per lane, current cluster
    let mut cluster: Vec<usize> = Vec::new(); // input indices in current cluster
    let mut cluster_end = 0;

    // Stamp the final lane count onto every member of a finished cluster.
    // A plain fn (not a closure) so it doesn't hold borrows across the loop.
    fn close(out: &mut [(usize, usize)], cluster: &mut Vec<usize>, lanes: usize) {
        for i in cluster.drain(..) {
            out[i].1 = lanes;
        }
    }

    for i in order {
        let (start, end) = ranges[i];
        if !cluster.is_empty() && start >= cluster_end {
            close(&mut out, &mut cluster, lane_ends.len());
            lane_ends.clear();
        }
        let lane = match lane_ends.iter().position(|&e| e <= start) {
            Some(l) => {
                lane_ends[l] = end;
                l
            }
            None => {
                lane_ends.push(end);
                lane_ends.len() - 1
            }
        };
        out[i].0 = lane;
        cluster.push(i);
        cluster_end = cluster_end.max(end);
    }
    close(&mut out, &mut cluster, lane_ends.len());
    out
}

/// One positioned block in a day column.
struct Block {
    ev: Event,
    top_px: u32,
    height_px: u32,
    lane: usize,
    lanes: usize,
}

/// The minute range actually drawn for a clipped `(start, end)`: at least
/// `MIN_BLOCK_MIN` tall, and nudged up near midnight so it stays inside the
/// 24:00 bottom edge instead of spilling out of the column.
fn drawn_range((start, end): (u32, u32)) -> (u32, u32) {
    let start = start.min(DAY_MIN - MIN_BLOCK_MIN);
    (start, end.max(start + MIN_BLOCK_MIN))
}

/// Lay out a day's timed events (already filtered to non-all-day).
fn day_blocks(events: &[&Event], day: &str) -> Vec<Block> {
    // (event, start, end) for the ones that actually occupy this day.
    // (event, drawn start, drawn end) for the ones that occupy this day.
    let placed: Vec<(&Event, (u32, u32))> = events
        .iter()
        .filter_map(|e| clip_to_day(&e.starts_at, &e.ends_at, day).map(|r| (*e, drawn_range(r))))
        .collect();
    // Lanes use the *drawn* range (min height applied), so two short events
    // 10 min apart don't render on top of each other.
    let ranges: Vec<(u32, u32)> = placed.iter().map(|&(_, r)| r).collect();
    let lanes = assign_lanes(&ranges);
    placed
        .into_iter()
        .zip(lanes)
        .map(|((ev, (start, end)), (lane, lanes))| Block {
            ev: ev.clone(),
            top_px: start * HOUR_PX / 60,
            height_px: (end - start) * HOUR_PX / 60,
            lane,
            lanes,
        })
        .collect()
}

#[component]
pub fn WeekView(events: Vec<Event>, exdates: Vec<Exdate>) -> Element {
    let mut monday = use_signal(current_monday);

    let days: Vec<String> = (0..7).map(|i| shift_day(&monday(), i)).collect();
    let today_str = today();

    // Planned dinners for the visible week (read-only projection, 5D).
    // Reading `monday()` subscribes the resource, so week navigation refetches;
    // a failed fetch (offline) just means no meal chips.
    let meals = use_resource(move || {
        let from = monday();
        let to = shift_day(&from, 6);
        async move { api::mealplan_range(&from, &to).await.unwrap_or_default() }
    });
    let meals: Vec<MealPlanEntry> = meals().unwrap_or_default();

    // Scroll the body so 07:00 is at the top. Effects run after the DOM is
    // updated, so the element exists. Reading `monday()` subscribes the effect
    // to the signal: it re-runs on every week change (and once on mount).
    // `document::eval` fires the JS immediately; we don't need its result,
    // so the returned `Eval` handle is simply dropped.
    use_effect(move || {
        let _ = monday();
        let top = SCROLL_TO_HOUR * HOUR_PX;
        document::eval(&format!(
            "const el = document.getElementById('{BODY_ID}'); if (el) el.scrollTop = {top};"
        ));
    });

    // Expand series masters into occurrences over the visible week.
    // `expand_events` only yields occurrences that *start* inside the window,
    // so the window opens 7 days before Monday: an occurrence that starts on
    // the previous Sunday 22:00 (or a multi-day recurring all-day event) still
    // reaches into this week and must be drawn. Anything that doesn't is
    // dropped later by the per-day `event_overlaps` / `clip_to_day` filters.
    let events = expand_events(
        &events,
        &exdates,
        &format!("{}T00:00:00Z", shift_day(&days[0], -7)),
        &format!("{}T23:59:59Z", days[6]),
    );

    // Per-day data, computed once and reused by the three rows below.
    struct Day {
        date: String,
        label: &'static str,
        dnum: String,
        is_today: bool,
        all_day: Vec<Event>,
        meal: Option<String>,
        blocks: Vec<Block>,
    }
    let week: Vec<Day> = days
        .iter()
        .enumerate()
        .map(|(i, day)| {
            let win_start = format!("{day}T00:00:00Z");
            let win_end = format!("{day}T23:59:59Z");
            let (all_day, timed): (Vec<&Event>, Vec<&Event>) = events
                .iter()
                .filter(|e| event_overlaps(e, &win_start, &win_end))
                .partition(|e| e.all_day);
            Day {
                date: day.clone(),
                label: WEEKDAYS[i],
                dnum: day[8..10].trim_start_matches('0').to_owned(),
                is_today: *day == today_str,
                all_day: all_day.into_iter().cloned().collect(),
                // At most one dinner per day (UNIQUE(family_id, date)).
                meal: meals
                    .iter()
                    .filter(|m| m.date == *day)
                    .find_map(super::meal_label)
                    .map(str::to_owned),
                blocks: day_blocks(&timed, day),
            }
        })
        .collect();
    let has_strip = week
        .iter()
        .any(|d| !d.all_day.is_empty() || d.meal.is_some());

    rsx! {
        div { class: "row spread",
            button { class: "ghost", onclick: move |_| monday.set(shift_day(&monday(), -7)), "‹" }
            div { class: "row", style: "gap:8px;",
                span { class: "muted", "{monday}" }
                button {
                    class: "ghost",
                    style: "margin:0;padding:4px 10px;font-size:0.8rem;",
                    onclick: move |_| monday.set(current_monday()),
                    "Tänään"
                }
            }
            button { class: "ghost", onclick: move |_| monday.set(shift_day(&monday(), 7)), "›" }
        }
        div { class: "week",
            // Day labels. The empty first cell sits above the hour column.
            div { class: "week-row week-heads",
                div {}
                for d in week.iter() {
                    div { class: if d.is_today { "week-head today-col" } else { "week-head" },
                        "{d.label} "
                        if d.is_today {
                            span { class: "today", "{d.dnum}" }
                        } else {
                            "{d.dnum}"
                        }
                    }
                }
            }
            // All-day strip: only rendered when the week has something for it.
            if has_strip {
                div { class: "week-row week-allday-row",
                    div {}
                    for d in week.iter() {
                        div { class: if d.is_today { "week-allday today-col" } else { "week-allday" },
                            for ev in d.all_day.iter() {
                                Link {
                                    to: Route::EventEditScreen { uid: ev.uid.clone() },
                                    class: "week-chip allday",
                                    "{ev.title}"
                                }
                            }
                            if let Some(m) = &d.meal {
                                div { class: "week-chip meal", title: "{m}", "🍽 {m}" }
                            }
                        }
                    }
                }
            }
            // Scrolling time grid.
            div { id: BODY_ID, class: "week-body",
                div { class: "week-row week-grid",
                    div { class: "week-hours",
                        for h in 0..24u32 {
                            div { class: "week-hour", "{h:02}" }
                        }
                    }
                    for d in week.iter() {
                        div { class: if d.is_today { "week-day today-col" } else { "week-day" },
                            for b in d.blocks.iter() {
                                Link {
                                    to: Route::EventEditScreen { uid: b.ev.uid.clone() },
                                    class: "week-event",
                                    // calc() keeps a 1px gap between side-by-side lanes.
                                    style: "top:{b.top_px}px;height:{b.height_px}px;left:calc({b.lane} * 100% / {b.lanes});width:calc(100% / {b.lanes} - 1px);",
                                    // A continuation (started on an earlier day) shows ↳
                                    // instead of a start time that isn't on this day.
                                    span { class: "week-event-time",
                                        if b.ev.starts_at.get(..10) == Some(d.date.as_str()) {
                                            "{hhmm(&b.ev.starts_at)}"
                                        } else {
                                            "↳"
                                        }
                                    }
                                    " {b.ev.title}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_event_gets_full_width() {
        assert_eq!(assign_lanes(&[(60, 120)]), vec![(0, 1)]);
    }

    #[test]
    fn overlapping_pair_then_separate_event() {
        // 9–10 and 9:30–10:30 overlap → lanes 0 and 1 of 2.
        // 11–12 starts after both ended → new cluster, lane 0 of 1.
        let got = assign_lanes(&[(540, 600), (570, 630), (660, 720)]);
        assert_eq!(got, vec![(0, 2), (1, 2), (0, 1)]);
    }

    #[test]
    fn freed_lane_is_reused_within_cluster() {
        // A 9–10, B 9:30–11, C 10–10:30: C overlaps B but not A, so it
        // reuses lane 0; all three belong to one 2-lane cluster.
        let got = assign_lanes(&[(540, 600), (570, 660), (600, 630)]);
        assert_eq!(got, vec![(0, 2), (1, 2), (0, 2)]);
    }

    #[test]
    fn results_follow_input_order_not_sorted_order() {
        let got = assign_lanes(&[(570, 630), (540, 600)]);
        assert_eq!(got, vec![(1, 2), (0, 2)]);
    }

    #[test]
    fn three_way_overlap() {
        let got = assign_lanes(&[(0, 100), (10, 100), (20, 100)]);
        assert_eq!(got, vec![(0, 3), (1, 3), (2, 3)]);
    }

    #[test]
    fn empty_input() {
        assert!(assign_lanes(&[]).is_empty());
    }

    #[test]
    fn clip_same_day_event() {
        let got = clip_to_day("2026-11-03T09:15:00Z", "2026-11-03T10:00:00Z", "2026-11-03");
        assert_eq!(got, Some((555, 600)));
    }

    #[test]
    fn clip_event_crossing_midnight() {
        let (s, e) = ("2026-11-03T22:00:00Z", "2026-11-04T02:00:00Z");
        assert_eq!(clip_to_day(s, e, "2026-11-03"), Some((1320, 1440)));
        assert_eq!(clip_to_day(s, e, "2026-11-04"), Some((0, 120)));
        assert_eq!(clip_to_day(s, e, "2026-11-05"), None);
    }

    #[test]
    fn clip_drops_event_ending_exactly_at_midnight() {
        let (s, e) = ("2026-11-03T23:00:00Z", "2026-11-04T00:00:00Z");
        assert_eq!(clip_to_day(s, e, "2026-11-04"), None);
    }

    #[test]
    fn drawn_range_applies_min_height_and_stays_in_day() {
        assert_eq!(drawn_range((600, 605)), (600, 627));
        assert_eq!(drawn_range((1435, 1440)), (1413, 1440));
        assert_eq!(drawn_range((60, 180)), (60, 180));
    }

    #[test]
    fn minutes_of_parses_and_tolerates_garbage() {
        assert_eq!(minutes_of("2026-11-03T07:45:00Z"), 465);
        assert_eq!(minutes_of("bad"), 0);
    }
}
