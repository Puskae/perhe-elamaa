//! Create/edit an event. Shared `EventForm` drives both the "new" and "edit"
//! routes. Timed events use datetime-local inputs stored as floating wall-clock
//! (see `input_to_stored`); all-day events use date inputs normalized to
//! T00:00:00Z. Attendees are a checkbox list of family members.

use crate::api;
use crate::screens::Route;
use crate::store;
use dioxus::prelude::*;
use perkele_shared::auth::UserView;
use perkele_shared::calendar::{
    EditScope, Exdate, SaveEventRequest, validate_event, validate_event_times, validate_event_title,
};
use perkele_shared::recur::{End, Freq, RecurrenceSpec, Weekday, occurrence_end, trim_until};

/// The recurrence <select> options: (stored key, Finnish label, freq).
/// Key "" = no repeat; the keys are what the select's value carries.
const REPEATS: [(&str, &str, Freq); 4] = [
    ("d", "Päivittäin", Freq::Daily),
    ("w", "Viikoittain", Freq::Weekly),
    ("m", "Kuukausittain", Freq::Monthly),
    ("y", "Vuosittain", Freq::Yearly),
];

/// Weekday-picker checkboxes for weekly repeats.
const PICK_DAYS: [(&str, Weekday); 7] = [
    ("Ma", Weekday::Mon),
    ("Ti", Weekday::Tue),
    ("Ke", Weekday::Wed),
    ("To", Weekday::Thu),
    ("Pe", Weekday::Fri),
    ("La", Weekday::Sat),
    ("Su", Weekday::Sun),
];

/// "Muistutus" choices: (minutes, label). None-equivalent is the "" option.
const REMINDERS: [(i64, &str); 5] = [
    (5, "5 min ennen"),
    (15, "15 min ennen"),
    (30, "30 min ennen"),
    (60, "1 h ennen"),
    (1440, "1 vrk ennen"),
];

fn freq_to_key(f: Freq) -> &'static str {
    REPEATS
        .iter()
        .find(|(_, _, rf)| *rf == f)
        .map(|(k, _, _)| *k)
        .unwrap_or("")
}

#[component]
pub fn EventNewScreen(date: String) -> Element {
    // `date` comes from the route's query param: 'YYYY-MM-DD' when the form
    // was opened by tapping a day in the month view, 'YYYY-MM-DDTHH:MM' from
    // an hour slot in the week view, "" from the + button.
    let seed_start = seed_start_from_query(&date);
    rsx! {
        EventForm { uid: None, seed_start }
    }
}

#[component]
pub fn EventEditScreen(uid: String) -> Element {
    // A tap on a synthetic occurrence carries "masterUid@occStart" (see
    // expand_events): the form gets the master's uid plus which occurrence
    // was tapped, and offers the this/all scope choice. uids are UUIDs,
    // which never contain '@', so the split is unambiguous.
    let (master, occ) = match uid.split_once('@') {
        Some((m, o)) => (m.to_owned(), Some(o.to_owned())),
        None => (uid, None),
    };
    rsx! {
        EventForm { uid: Some(master), occ_start: occ }
    }
}

/// The new-event start ('YYYY-MM-DDTHH:MM') implied by the route's `date`
/// query param: a bare date starts at noon, a full date-time is used as is,
/// anything else (empty, garbage, a hand-edited URL) → `None`, i.e. the
/// "next full hour" default. Pure, so it is unit-tested natively.
fn seed_start_from_query(q: &str) -> Option<String> {
    // Work on bytes: indexing a `&str` by position could panic on a
    // multi-byte character, a `&[u8]` can't. Checking every position for an
    // ASCII digit/separator also guarantees the string is pure ASCII.
    let b = q.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    // Two ASCII digits → their number (only called after `digits` passed).
    let num = |i: usize| u32::from(b[i] - b'0') * 10 + u32::from(b[i + 1] - b'0');
    let is_date = b.len() >= 10
        && digits(0..4)
        && b[4] == b'-'
        && digits(5..7)
        && b[7] == b'-'
        && digits(8..10)
        // Month 1..=12, day 1..=31 (not per-month exact; the date input
        // and server validation catch e.g. Feb 30).
        && (1..=12).contains(&num(5))
        && (1..=31).contains(&num(8));
    match b.len() {
        10 if is_date => Some(format!("{q}T12:00")),
        16 if is_date
            && b[10] == b'T'
            && digits(11..13)
            && b[13] == b':'
            && digits(14..16)
            && num(11) < 24
            && num(14) < 60 =>
        {
            Some(q.to_owned())
        }
        _ => None,
    }
}

// Time model: floating wall-clock. The family is single-timezone, so we store
// exactly what the user typed with a `Z` suffix (no real-UTC conversion) and
// display it back verbatim. This keeps "14:00 in → 14:00 shown" and makes
// day-boundary grouping align with the local calendar.

/// A datetime-local value ('YYYY-MM-DDTHH:MM') → stored 'YYYY-MM-DDTHH:MM:00Z'.
fn input_to_stored(local: &str) -> String {
    format!("{local}:00Z")
}

/// Stored 'YYYY-MM-DDTHH:MM:00Z' → datetime-local value 'YYYY-MM-DDTHH:MM'.
fn stored_to_input(stored: &str) -> String {
    stored[..16.min(stored.len())].to_owned()
}

/// A js_sys::Date → datetime-local value in LOCAL wall-clock time, matching
/// the floating-time model above.
fn fmt_local(d: &js_sys::Date) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}",
        d.get_full_year(),
        d.get_month() + 1, // JS months are 0-based
        d.get_date(),
        d.get_hours(),
        d.get_minutes(),
    )
}

/// Local now rounded UP to the next full hour — the default start for a new
/// event, so the picker usually needs no interaction at all.
fn next_full_hour() -> String {
    let d = js_sys::Date::new_0();
    d.set_minutes(0);
    d.set_seconds(0);
    d.set_milliseconds(0);
    // +1 h in epoch ms; Date handles day/month rollover.
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(d.get_time() + 3_600_000.0));
    fmt_local(&d)
}

/// 'YYYY-MM-DDTHH:MM' + one hour. A datetime string without a zone offset is
/// parsed as local time, so this stays in wall-clock land throughout.
fn plus_one_hour(input: &str) -> String {
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(input));
    if d.get_time().is_nan() {
        return input.to_owned(); // half-typed value — leave it alone
    }
    let d = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(d.get_time() + 3_600_000.0));
    fmt_local(&d)
}

/// Optimistically upsert a just-saved event into the local cache by uid, so the
/// agenda shows it immediately (used by the offline path; harmless online too
/// since a sync follows). For a this-only override, `series_uid` names the
/// master and the skipped occurrence is recorded locally, mirroring what the
/// server will do when the queued mutation replays.
fn apply_to_cache(req: &SaveEventRequest, series_uid: Option<&str>) {
    let mut events = store::get_events().unwrap_or_default();
    let ev = perkele_shared::calendar::Event {
        uid: req.uid.clone(),
        title: req.title.clone(),
        all_day: req.all_day,
        starts_at: req.starts_at.clone(),
        ends_at: req.ends_at.clone(),
        location: req.location.clone(),
        notes: req.notes.clone(),
        rrule: req.rrule.clone(),
        series_uid: series_uid.map(str::to_owned),
        recurrence_id: req.recurrence_id.clone(),
        reminder_minutes: req.reminder_minutes,
        attendee_ids: req.attendee_ids.clone(),
        created_by: 0,
        created_at: req.starts_at.clone(),
        updated_at: req.starts_at.clone(),
    };
    match events.iter_mut().find(|e| e.uid == req.uid) {
        Some(slot) => *slot = ev,
        None => events.push(ev),
    }
    store::set_events(&events);
    if let (Some(series), Some(occ)) = (series_uid, &req.recurrence_id) {
        store::add_exdate(perkele_shared::calendar::Exdate {
            series_uid: series.to_owned(),
            occ_start: occ.clone(),
        });
    }
}

/// `occ_start` is set when the form was opened from a synthetic occurrence of
/// a recurring series: `uid` is then the master's uid and the scope picker
/// decides whether a save/delete touches just that occurrence or the series.
/// `seed_start` ('YYYY-MM-DDTHH:MM', see [`seed_start_from_query`]) pre-picks
/// the start for a NEW event (month-day or week-slot tap); it is ignored when
/// editing.
#[component]
fn EventForm(
    uid: Option<String>,
    occ_start: Option<String>,
    seed_start: Option<String>,
) -> Element {
    let nav = use_navigator();
    let members = use_resource(api::members);
    let me = use_resource(api::me);
    // Who created the event being edited (from the cache; None on "new").
    let mut author = use_signal(|| Option::<i64>::None);

    let mut title = use_signal(String::new);
    let mut all_day = use_signal(|| false);
    // For timed: datetime-local strings. For all-day: date strings.
    // A NEW event opens prefilled (start = next full hour, end = +1 h) so the
    // common case needs no picker at all; editing seeds from the event below.
    let editing = uid.is_some();
    let (seed_start, seed_end) = if editing {
        (String::new(), String::new())
    } else {
        // A tapped month-view day / week-view hour seeds the start; the +
        // button keeps the "next full hour today" default.
        let s = seed_start.clone().unwrap_or_else(next_full_hour);
        let e = plus_one_hour(&s);
        (s, e)
    };
    let mut start_input = use_signal(move || seed_start);
    let mut end_input = use_signal(move || seed_end);
    // Päättyy follows Alkaa (+1 h) until the user edits it themselves; on the
    // edit form the stored end always wins, so it starts "touched".
    let mut end_touched = use_signal(|| editing);
    let mut location = use_signal(String::new);
    let mut notes = use_signal(String::new);
    let mut attendees = use_signal(Vec::<i64>::new);
    // "" = ei muistutusta, else the offset in minutes as a string.
    let mut reminder = use_signal(String::new);
    // Recurrence editor state ("" = no repeat; see REPEATS for the keys).
    let mut repeat = use_signal(String::new);
    let mut rep_interval = use_signal(|| "1".to_owned());
    let mut rep_days = use_signal(Vec::<Weekday>::new);
    let mut rep_end_mode = use_signal(|| "never".to_owned()); // never | date | count
    let mut rep_end_date = use_signal(String::new);
    let mut rep_end_count = use_signal(|| "10".to_owned());
    // Occurrence editing: "this" (vain tämä kerta, default), "following"
    // (tästä eteenpäin = series split) or "all" (koko sarja). Switching
    // re-seeds the time inputs from these stashes so each scope always shows
    // its own honest times ("all" = the master's, the others = the tapped
    // occurrence's).
    let mut scope_sel = use_signal(|| "this".to_owned());
    let mut occ_times = use_signal(|| (String::new(), String::new()));
    let mut master_times = use_signal(|| (String::new(), String::new()));
    let mut error = use_signal(|| Option::<String>::None);
    // True while a save is in flight, so the button shows progress and can't be
    // double-submitted.
    let mut saving = use_signal(|| false);

    // On edit, seed from the cached event (sync already populated the cache).
    if let Some(u) = uid.clone() {
        let seed_occ = occ_start.clone();
        use_future(move || {
            let u = u.clone();
            let seed_occ = seed_occ.clone();
            async move {
                if let Some(ev) = store::get_events()
                    .unwrap_or_default()
                    .into_iter()
                    .find(|e| e.uid == u)
                {
                    author.set(Some(ev.created_by));
                    title.set(ev.title);
                    all_day.set(ev.all_day);
                    // Stored 'Z' datetimes → input-shaped strings (date-only
                    // for all-day events, datetime-local otherwise).
                    let to_input = |stored: &str, aday: bool| {
                        if aday {
                            stored[..10.min(stored.len())].to_owned()
                        } else {
                            stored_to_input(stored)
                        }
                    };
                    let m_start = to_input(&ev.starts_at, ev.all_day);
                    let m_end = to_input(&ev.ends_at, ev.all_day);
                    master_times.set((m_start.clone(), m_end.clone()));
                    // Opened from an occurrence: show ITS times (occurrence
                    // start + the master's duration), not the master's.
                    if let Some(occ) = &seed_occ {
                        let occ_end = occurrence_end(&ev.starts_at, &ev.ends_at, occ);
                        let o_start = to_input(occ, ev.all_day);
                        let o_end = to_input(&occ_end, ev.all_day);
                        occ_times.set((o_start.clone(), o_end.clone()));
                        start_input.set(o_start);
                        end_input.set(o_end);
                    } else {
                        start_input.set(m_start);
                        end_input.set(m_end);
                    }
                    location.set(ev.location.unwrap_or_default());
                    notes.set(ev.notes.unwrap_or_default());
                    attendees.set(ev.attendee_ids);
                    reminder.set(
                        ev.reminder_minutes
                            .map(|m| m.to_string())
                            .unwrap_or_default(),
                    );
                    // Seed the recurrence controls from the stored RRULE. The
                    // server only accepts editor-subset rules, so from_rrule
                    // succeeding is the normal case, not a lucky one.
                    if let Some(spec) = ev.rrule.as_deref().and_then(RecurrenceSpec::from_rrule) {
                        repeat.set(freq_to_key(spec.freq).to_owned());
                        rep_interval.set(spec.interval.to_string());
                        rep_days.set(spec.byday);
                        match spec.end {
                            End::Never => rep_end_mode.set("never".to_owned()),
                            End::OnDate(d) => {
                                rep_end_mode.set("date".to_owned());
                                rep_end_date.set(d);
                            }
                            End::AfterCount(n) => {
                                rep_end_mode.set("count".to_owned());
                                rep_end_count.set(n.to_string());
                            }
                        }
                    }
                }
            }
        });
    }

    let save_uid = uid.clone();
    let save_occ = occ_start.clone();
    let save = move |_| {
        // Ignore repeat taps while a save is already running.
        if saving() {
            return;
        }
        let uid = save_uid.clone();
        // Occurrence scopes: "vain tämä" → override request; "tästä
        // eteenpäin" → series split. Both create a NEW row on the server.
        let this_only = save_occ.is_some() && scope_sel() == "this";
        let following = save_occ.is_some() && scope_sel() == "following";
        let t = title();
        if let Err(m) = validate_event_title(&t) {
            error.set(Some(m.to_owned()));
            return;
        }
        // Build UTC timestamps from the inputs based on the all_day toggle.
        let (starts_at, ends_at) = if all_day() {
            let s = start_input();
            let e = if end_input().trim().is_empty() {
                s.clone()
            } else {
                end_input()
            };
            (format!("{s}T00:00:00Z"), format!("{e}T00:00:00Z"))
        } else {
            let s = start_input();
            // No end picked → the event stops at midnight of its start day.
            // (An empty end used to format into ":00Z", which string-compares
            // after every real timestamp = an event on all future dates.)
            let e = if end_input().trim().is_empty() {
                format!("{}T23:59", &s[..10.min(s.len())])
            } else {
                end_input()
            };
            (input_to_stored(&s), input_to_stored(&e))
        };
        if let Err(m) = validate_event_times(&starts_at, &ends_at) {
            error.set(Some(m.to_owned()));
            return;
        }
        let blank = |s: String| {
            let t = s.trim().to_owned();
            if t.is_empty() { None } else { Some(t) }
        };
        // Assemble the RRULE from the recurrence controls; the spec's
        // to_rrule() means the UI never hand-writes RRULE grammar.
        let rrule = REPEATS
            .iter()
            .find(|(k, _, _)| *k == repeat())
            .map(|(_, _, freq)| {
                let end = match rep_end_mode().as_str() {
                    "date" if !rep_end_date().trim().is_empty() => {
                        End::OnDate(rep_end_date().trim().to_owned())
                    }
                    "count" => End::AfterCount(rep_end_count().trim().parse().unwrap_or(1).max(1)),
                    _ => End::Never,
                };
                RecurrenceSpec {
                    freq: *freq,
                    interval: rep_interval().trim().parse().unwrap_or(1).max(1),
                    // The weekday picker only applies to weekly repeats.
                    byday: if *freq == Freq::Weekly {
                        rep_days()
                    } else {
                        vec![]
                    },
                    end,
                }
                .to_rrule()
            });
        let req = SaveEventRequest {
            // Override and split both create a NEW row (override / new
            // master), so they need a fresh uid — generated here so an
            // offline replay is idempotent; the old master stays addressed
            // by the URL path.
            uid: if this_only || following {
                store::new_uid()
            } else {
                uid.clone().unwrap_or_else(store::new_uid)
            },
            title: t.trim().to_owned(),
            all_day: all_day(),
            starts_at,
            ends_at,
            location: blank(location()),
            notes: blank(notes()),
            // An override is a one-off; series-wide saves and the split's
            // new master carry the rule from the Toisto controls.
            rrule: if this_only { None } else { rrule },
            scope: if this_only {
                Some(EditScope::ThisOnly)
            } else if following {
                Some(EditScope::ThisAndFollowing)
            } else {
                None
            },
            recurrence_id: if this_only || following {
                save_occ.clone()
            } else {
                None
            },
            reminder_minutes: reminder().parse::<i64>().ok(),
            attendee_ids: attendees(),
        };
        // Full shared rule set (lengths, reminder range, …) — an offline
        // save that the server would later reject must fail here instead.
        if let Err(m) = validate_event(&req) {
            error.set(Some(m.to_owned()));
            return;
        }
        let editing = uid.clone();
        error.set(None);
        saving.set(true);
        let req_json = serde_json::to_string(&req).unwrap_or_default();
        spawn(async move {
            if super::is_online() {
                let res = match &editing {
                    Some(u) => api::update_event(u, &req).await.map(|_| ()),
                    None => api::create_event(&req).await.map(|_| ()),
                };
                match res {
                    Ok(()) => {
                        // Refresh cache so the agenda shows the change immediately.
                        if let Ok(sync) = api::calendar_sync().await {
                            store::set_events(&sync.events);
                            store::set_exdates(&sync.exdates);
                        }
                        nav.push(Route::CalendarScreen {});
                    }
                    Err(e) => {
                        // Re-enable the button so the user can retry after a failure.
                        saving.set(false);
                        error.set(Some(e));
                    }
                }
            } else {
                // Offline: apply to cache + queue the full request (body carries uid).
                if following {
                    // Mirror the server's split locally: trim the cached
                    // master's rule and sweep exceptions from the cut on.
                    if let (Some(master), Some(occ)) = (&editing, &req.recurrence_id) {
                        let mut events = store::get_events().unwrap_or_default();
                        if let Some(m) = events.iter_mut().find(|e| &e.uid == master)
                            && let Some(rule) = m.rrule.clone()
                            && let Some(trimmed) = trim_until(&rule, occ)
                        {
                            m.rrule = Some(trimmed);
                        }
                        events.retain(|e| {
                            e.series_uid.as_ref() != Some(master)
                                || e.recurrence_id.as_ref().is_none_or(|r| r < occ)
                        });
                        store::set_events(&events);
                        let kept: Vec<Exdate> = store::get_exdates()
                            .into_iter()
                            .filter(|x| &x.series_uid != master || x.occ_start < *occ)
                            .collect();
                        store::set_exdates(&kept);
                    }
                    // The new master enters the cache as a plain series row.
                    let mut cache_req = req.clone();
                    cache_req.recurrence_id = None;
                    apply_to_cache(&cache_req, None);
                } else {
                    apply_to_cache(&req, if this_only { editing.as_deref() } else { None });
                }
                let (method, url) = match &editing {
                    Some(u) => ("PUT".to_owned(), format!("/api/calendar/events/{u}")),
                    None => ("POST".to_owned(), "/api/calendar/events".to_owned()),
                };
                store::enqueue_cal(store::QueuedMutation {
                    method,
                    url,
                    body: Some(req_json),
                });
                nav.push(Route::CalendarScreen {});
            }
        });
    };

    let del_uid = uid.clone();
    let del_occ = occ_start.clone();
    let delete = move |_| {
        if let Some(u) = del_uid.clone() {
            // Occurrence scopes narrow the delete: "vain tämä" skips one
            // occurrence (EXDATE), "tästä eteenpäin" trims the series at it;
            // "koko sarja" (or a plain event) deletes the row + exceptions.
            let occ = del_occ.clone().filter(|_| scope_sel() != "all");
            let following = scope_sel() == "following";
            spawn(async move {
                if super::is_online() {
                    let res = match (&occ, following) {
                        (Some(o), false) => api::delete_occurrence(&u, o).await,
                        (Some(o), true) => api::delete_following(&u, o).await,
                        (None, _) => api::delete_event(&u).await,
                    };
                    if res.is_ok()
                        && let Ok(sync) = api::calendar_sync().await
                    {
                        store::set_events(&sync.events);
                        store::set_exdates(&sync.exdates);
                    }
                } else {
                    match (&occ, following) {
                        (Some(o), false) => {
                            // Optimistic skip, mirroring the server's EXDATE.
                            store::add_exdate(Exdate {
                                series_uid: u.clone(),
                                occ_start: o.clone(),
                            });
                            store::enqueue_cal(store::QueuedMutation {
                                method: "DELETE".to_owned(),
                                url: format!("/api/calendar/events/{u}?occ={o}"),
                                body: None,
                            });
                        }
                        (Some(o), true) => {
                            // Optimistic trim + sweep, mirroring the server.
                            let mut events = store::get_events().unwrap_or_default();
                            if let Some(m) = events.iter_mut().find(|e| e.uid == u)
                                && let Some(rule) = m.rrule.clone()
                                && let Some(trimmed) = trim_until(&rule, o)
                            {
                                m.rrule = Some(trimmed);
                            }
                            events.retain(|e| {
                                e.series_uid.as_deref() != Some(u.as_str())
                                    || e.recurrence_id.as_ref().is_none_or(|r| r < o)
                            });
                            store::set_events(&events);
                            let kept: Vec<Exdate> = store::get_exdates()
                                .into_iter()
                                .filter(|x| x.series_uid != u || x.occ_start < *o)
                                .collect();
                            store::set_exdates(&kept);
                            store::enqueue_cal(store::QueuedMutation {
                                method: "DELETE".to_owned(),
                                url: format!("/api/calendar/events/{u}?from={o}"),
                                body: None,
                            });
                        }
                        (None, _) => {
                            // Whole event/series: sweep its overrides and
                            // exdates locally like the server cascade does.
                            let mut events = store::get_events().unwrap_or_default();
                            events.retain(|e| {
                                e.uid != u && e.series_uid.as_deref() != Some(u.as_str())
                            });
                            store::set_events(&events);
                            let kept: Vec<Exdate> = store::get_exdates()
                                .into_iter()
                                .filter(|x| x.series_uid != u)
                                .collect();
                            store::set_exdates(&kept);
                            store::enqueue_cal(store::QueuedMutation {
                                method: "DELETE".to_owned(),
                                url: format!("/api/calendar/events/{u}"),
                                body: None,
                            });
                        }
                    }
                }
                nav.push(Route::CalendarScreen {});
            });
        }
    };

    let member_list: Vec<UserView> = match &*members.read_unchecked() {
        Some(Ok(m)) => m.clone(),
        _ => vec![],
    };

    // Author-or-admin, mirroring the server: only hide Tallenna/Poista when
    // we KNOW the viewer may not touch this event. If either side is unknown
    // (offline and `me` failed, or a not-yet-synced offline event whose
    // cached `created_by` is the 0 placeholder) we leave the buttons on; the
    // server still decides, and a refused offline replay shows a banner.
    let my = me().and_then(|r| r.ok());
    let read_only = match (my.as_ref(), author()) {
        (Some(u), Some(a)) if a != 0 => a != u.id && !u.role.is_admin(),
        _ => false,
    };

    rsx! {
        div { class: "card wide",
            h1 {
                if read_only {
                    "Tapahtuma"
                } else if uid.is_some() {
                    "Muokkaa tapahtumaa"
                } else {
                    "Uusi tapahtuma"
                }
            }

            // Opened from one occurrence of a series: choose the edit reach.
            if occ_start.is_some() {
                label { "Muutosten laajuus" }
                select {
                    style: "width:100%;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                    onchange: move |e| {
                        let sel = e.value();
                        // Each scope shows its own times: the master's for
                        // "koko sarja", the tapped occurrence's otherwise.
                        let (s, en) = if sel == "all" { master_times() } else { occ_times() };
                        scope_sel.set(sel);
                        start_input.set(s);
                        end_input.set(en);
                    },
                    option { value: "this", selected: scope_sel() == "this", "Vain tämä kerta" }
                    option {
                        value: "following",
                        selected: scope_sel() == "following",
                        "Tästä eteenpäin"
                    }
                    option { value: "all", selected: scope_sel() == "all", "Koko sarja" }
                }
            }

            label { "Nimi" }
            input {
                value: "{title}",
                oninput: move |e| title.set(e.value()),
                placeholder: "esim. Hammaslääkäri",
            }

            label { class: "row", style: "gap:8px;align-items:center;",
                input {
                    r#type: "checkbox",
                    checked: all_day(),
                    style: "width:auto;",
                    onchange: move |e| {
                        // The inputs switch between date and datetime-local,
                        // so convert the values or the browser blanks them.
                        let on = e.checked();
                        let day = |v: &str| v[..10.min(v.len())].to_owned();
                        if on {
                            // Into locals first: set() can't run while the
                            // peek() borrow guard is still alive.
                            let s = day(start_input.peek().as_str());
                            let en = day(end_input.peek().as_str());
                            start_input.set(s);
                            end_input.set(en);
                        } else {
                            // Date-only values get their times back: keep the
                            // picked day, re-apply the default hours.
                            let s = start_input.peek().as_str().to_owned();
                            if s.len() == 10 {
                                let time = &next_full_hour()[10..];
                                let s = format!("{s}{time}");
                                end_input.set(plus_one_hour(&s));
                                start_input.set(s);
                            }
                        }
                        all_day.set(on);
                    },
                }
                "Koko päivä"
            }

            label { "Alkaa" }
            input {
                r#type: if all_day() { "date" } else { "datetime-local" },
                value: "{start_input}",
                oninput: move |e| {
                    let v = e.value();
                    // Drag the end along (+1 h) until the user owns it.
                    if !end_touched() && !all_day() {
                        end_input.set(plus_one_hour(&v));
                    }
                    start_input.set(v);
                },
            }
            label { "Päättyy" }
            input {
                r#type: if all_day() { "date" } else { "datetime-local" },
                value: "{end_input}",
                oninput: move |e| {
                    end_touched.set(true);
                    end_input.set(e.value());
                },
            }

            // A single occurrence can't get its own repeat rule; the Toisto
            // controls apply to the series, so show them only for that scope.
            // "Vain tämä kerta" produces a one-off override, so it has no
            // rule to edit; the split and series scopes do.
            if occ_start.is_none() || scope_sel() != "this" {
                label { "Toisto" }
                select {
                    style: "width:100%;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                    onchange: move |e| repeat.set(e.value()),
                    option { value: "", selected: repeat().is_empty(), "Ei toistu" }
                    for (key , label , _) in REPEATS {
                        option { value: "{key}", selected: repeat() == key, "{label}" }
                    }
                }
            }

            if !repeat().is_empty() && (occ_start.is_none() || scope_sel() != "this") {
                label { "Toistoväli" }
                div { class: "row", style: "gap:8px;align-items:center;",
                    span { class: "muted", "joka" }
                    input {
                        r#type: "number",
                        min: "1",
                        style: "width:5em;",
                        value: "{rep_interval}",
                        oninput: move |e| rep_interval.set(e.value()),
                    }
                    span { class: "muted",
                        match repeat().as_str() {
                            "d" => "päivä",
                            "w" => "viikko",
                            "m" => "kuukausi",
                            _ => "vuosi",
                        }
                    }
                }

                if repeat() == "w" {
                    label { "Viikonpäivät" }
                    div { class: "row", style: "gap:10px;flex-wrap:wrap;",
                        for (label , day) in PICK_DAYS {
                            label { class: "row", style: "gap:4px;align-items:center;",
                                input {
                                    r#type: "checkbox",
                                    style: "width:auto;",
                                    checked: rep_days().contains(&day),
                                    onchange: move |e| {
                                        let mut d = rep_days();
                                        if e.checked() {
                                            if !d.contains(&day) {
                                                d.push(day);
                                            }
                                        } else {
                                            d.retain(|x| *x != day);
                                        }
                                        rep_days.set(d);
                                    },
                                }
                                "{label}"
                            }
                        }
                    }
                }

                label { "Toisto päättyy" }
                select {
                    style: "width:100%;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                    onchange: move |e| rep_end_mode.set(e.value()),
                    option { value: "never", selected: rep_end_mode() == "never", "Ei koskaan" }
                    option { value: "date", selected: rep_end_mode() == "date", "Päivämääränä" }
                    option { value: "count", selected: rep_end_mode() == "count", "Toistojen jälkeen" }
                }
                if rep_end_mode() == "date" {
                    input {
                        r#type: "date",
                        value: "{rep_end_date}",
                        oninput: move |e| rep_end_date.set(e.value()),
                    }
                }
                if rep_end_mode() == "count" {
                    div { class: "row", style: "gap:8px;align-items:center;",
                        input {
                            r#type: "number",
                            min: "1",
                            style: "width:5em;",
                            value: "{rep_end_count}",
                            oninput: move |e| rep_end_count.set(e.value()),
                        }
                        span { class: "muted", "toiston jälkeen" }
                    }
                }
            }

            label { "Muistutus" }
            select {
                style: "width:100%;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                onchange: move |e| reminder.set(e.value()),
                option { value: "", selected: reminder().is_empty(), "Ei muistutusta" }
                for (mins , label) in REMINDERS {
                    option {
                        value: "{mins}",
                        selected: reminder() == mins.to_string(),
                        "{label}"
                    }
                }
            }
            if all_day() && !reminder().is_empty() {
                p { class: "muted", "Koko päivän tapahtumissa muistutus lasketaan klo 9:stä." }
            }

            label { "Paikka" }
            input { value: "{location}", oninput: move |e| location.set(e.value()) }

            label { "Osallistujat" }
            for m in member_list {
                label { key: "{m.id}", class: "row", style: "gap:8px;align-items:center;",
                    input {
                        r#type: "checkbox",
                        style: "width:auto;",
                        checked: attendees().contains(&m.id),
                        onchange: move |e| {
                            let mut a = attendees();
                            if e.checked() {
                                if !a.contains(&m.id) {
                                    a.push(m.id);
                                }
                            } else {
                                a.retain(|x| *x != m.id);
                            }
                            attendees.set(a);
                        },
                    }
                    "{m.display_name}"
                }
            }

            label { "Muistiinpano" }
            textarea {
                style: "width:100%;min-height:80px;padding:11px 12px;border-radius:10px;border:1px solid var(--line);background:#141823;color:var(--ink);font-size:1rem;",
                value: "{notes}",
                oninput: move |e| notes.set(e.value()),
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            if read_only {
                p { class: "muted",
                    "Vain tapahtuman luoja tai ylläpitäjä voi muokata tai poistaa tämän."
                }
            } else {
                button {
                    class: "primary",
                    disabled: saving(),
                    onclick: save,
                    if saving() { "Tallennetaan…" } else { "Tallenna" }
                }
            }
            if uid.is_some() && !read_only {
                button {
                    class: "clear-btn",
                    disabled: saving(),
                    onclick: delete,
                    if occ_start.is_some() && scope_sel() == "this" {
                        "Poista tämä kerta"
                    } else if occ_start.is_some() && scope_sel() == "following" {
                        "Poista tästä eteenpäin"
                    } else {
                        "Poista tapahtuma"
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
    fn seed_date_only_starts_at_noon() {
        assert_eq!(
            seed_start_from_query("2026-11-03").as_deref(),
            Some("2026-11-03T12:00")
        );
    }

    #[test]
    fn seed_datetime_is_used_as_is() {
        assert_eq!(
            seed_start_from_query("2026-11-03T07:00").as_deref(),
            Some("2026-11-03T07:00")
        );
    }

    #[test]
    fn seed_garbage_falls_back_to_default() {
        for q in [
            "",
            "garbage",
            "2026-11-0x",
            "2026/11/03",
            "2026-11-03T7:00",
            "2026-11-03X07:00",
            "2026-11-03T25:00",
            "2026-11-03T07:60",
            "2026-11-03T07:00:00",
            "2026-13-01",
            "2026-00-10T07:00",
            "2026-05-32T07:00",
            "ää-ää-ääää",
        ] {
            assert_eq!(seed_start_from_query(q), None, "{q:?}");
        }
    }
}
