use serde::{Deserialize, Serialize};

/// A calendar event. `uid` is the client-generated, client-facing identity
/// (routes and the local cache key on it). Timed and all-day events share the
/// same RFC3339 UTC `starts_at`/`ends_at`; `all_day` tells the client how to
/// render them (all-day: render the date portion, no timezone conversion).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub uid: String,
    pub title: String,
    pub all_day: bool,
    pub starts_at: String,
    pub ends_at: String,
    pub location: Option<String>,
    pub notes: Option<String>,
    /// RFC 5545 recurrence rule; `None` = one-off event. When set, this row
    /// is a series *master*: clients expand it into occurrences locally via
    /// `crate::recur::expand`. `#[serde(default)]` keeps pre-5B JSON (old
    /// localStorage caches) deserializing as None instead of erroring.
    #[serde(default)]
    pub rrule: Option<String>,
    /// Set on *override* rows only: the uid of the series master whose
    /// occurrence this event replaces ("edit just this one").
    #[serde(default)]
    pub series_uid: Option<String>,
    /// Set on override rows only: the original start datetime of the
    /// occurrence being replaced (iCalendar RECURRENCE-ID).
    #[serde(default)]
    pub recurrence_id: Option<String>,
    /// Minutes before the (occurrence) start to send a push reminder;
    /// `None` = no reminder. Shared by everyone the event concerns.
    #[serde(default)]
    pub reminder_minutes: Option<i64>,
    pub attendee_ids: Vec<i64>,
    pub created_by: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// Body for POST (create) and PUT (update). Attendees are replace-all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SaveEventRequest {
    pub uid: String,
    pub title: String,
    pub all_day: bool,
    pub starts_at: String,
    pub ends_at: String,
    pub location: Option<String>,
    pub notes: Option<String>,
    /// Recurrence rule to store; absent/None keeps the event a one-off.
    #[serde(default)]
    pub rrule: Option<String>,
    /// How far an edit/delete of a recurring event reaches. `None` means
    /// `All` (and is what one-off events and pre-5B clients send).
    #[serde(default)]
    pub scope: Option<EditScope>,
    /// `ThisOnly` only: the occurrence start being edited. The request `uid`
    /// is then the *new override row's* uid (client-generated so offline
    /// replay is idempotent); the master is named by the URL path.
    #[serde(default)]
    pub recurrence_id: Option<String>,
    /// Reminder offset to store; absent/None = no reminder.
    #[serde(default)]
    pub reminder_minutes: Option<i64>,
    pub attendee_ids: Vec<i64>,
}

/// Google-Calendar-style edit reach. `ThisAndFollowing` splits the series:
/// the old master is trimmed to end the day before the tapped occurrence and
/// a new master (the request body) starts from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditScope {
    All,
    ThisOnly,
    ThisAndFollowing,
}

/// One skipped occurrence of a series (iCalendar EXDATE): "the event on
/// `occ_start` does not happen". Both delete-this-one and the removal half
/// of edit-this-one are stored as these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exdate {
    pub series_uid: String,
    pub occ_start: String,
}

/// Response of `GET /api/calendar/sync`: the family's full event set
/// (one-offs + masters + overrides), its exdates, + seq.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalendarSync {
    pub events: Vec<Event>,
    #[serde(default)]
    pub exdates: Vec<Exdate>,
    pub seq: i64,
}

pub fn validate_event_title(title: &str) -> Result<(), &'static str> {
    let len = title.trim().chars().count();
    if len == 0 {
        return Err("Tapahtuman nimi ei voi olla tyhjä.");
    }
    if len > 100 {
        return Err("Tapahtuman nimi saa olla enintään 100 merkkiä.");
    }
    Ok(())
}

/// Both timestamps must be exact floating `YYYY-MM-DDTHH:MM:SSZ` values
/// (real dates, years within [`crate::dates::MIN_YEAR`]..=`MAX_YEAR`). That
/// uniform, zero-padded shape is what makes a lexical string compare a valid
/// chronological compare — an unshaped string like ":00Z" would otherwise
/// sort AFTER every real timestamp (the old "forever event" bug). End must
/// not precede start.
pub fn validate_event_times(starts_at: &str, ends_at: &str) -> Result<(), &'static str> {
    if starts_at.trim().is_empty() || ends_at.trim().is_empty() {
        return Err("Alku- ja loppuaika vaaditaan.");
    }
    if crate::dates::parse_timestamp(starts_at).is_none()
        || crate::dates::parse_timestamp(ends_at).is_none()
    {
        return Err("Tarkista alku- ja loppuaika.");
    }
    if ends_at < starts_at {
        return Err("Loppuaika ei voi olla ennen alkua.");
    }
    Ok(())
}

/// Client uids are `crypto.randomUUID()` (36 chars) or the `uid-<millis>`
/// fallback; 64 leaves headroom.
pub const UID_MAX: usize = 64;
pub const LOCATION_MAX: usize = 200;
pub const NOTES_MAX: usize = 4000;
/// A family is a handful of people; 50 is far past any real event and keeps
/// the server's per-attendee membership check bounded.
pub const ATTENDEES_MAX: usize = 50;

/// `true` if the optional text is longer than `max` characters. `chars()`
/// counts Unicode scalar values, so "ä" is 1, not its 2 UTF-8 bytes.
fn too_long(s: Option<&str>, max: usize) -> bool {
    s.is_some_and(|s| s.chars().count() > max)
}

/// Everything a save (create or update, any scope) must satisfy. The client
/// pre-checks with this and the server enforces it on every write.
pub fn validate_event(req: &SaveEventRequest) -> Result<(), &'static str> {
    validate_event_title(&req.title)?;
    validate_event_times(&req.starts_at, &req.ends_at)?;
    if req.uid.trim().is_empty() {
        return Err("uid vaaditaan.");
    }
    if req.uid.chars().count() > UID_MAX {
        return Err("uid on liian pitkä.");
    }
    if too_long(req.location.as_deref(), LOCATION_MAX) {
        return Err("Paikka saa olla enintään 200 merkkiä.");
    }
    if too_long(req.notes.as_deref(), NOTES_MAX) {
        return Err("Lisätiedot saavat olla enintään 4000 merkkiä.");
    }
    // Same rule as the client editor: only store RRULEs our UI can render.
    if let Some(rule) = &req.rrule {
        crate::recur::validate(rule)?;
    }
    if let Some(occ) = &req.recurrence_id {
        validate_occurrence_start(occ)?;
    }
    if let Some(m) = req.reminder_minutes {
        validate_reminder_minutes(m)?;
    }
    if req.attendee_ids.len() > ATTENDEES_MAX {
        return Err("Osallistujia saa olla enintään 50.");
    }
    // Sort a copy so duplicates end up adjacent; `windows(2)` then yields
    // every neighbouring pair. A duplicate would break event_attendees' PK.
    let mut ids = req.attendee_ids.clone();
    ids.sort_unstable();
    if ids.windows(2).any(|w| w[0] == w[1]) {
        return Err("Sama osallistuja on listalla kahdesti.");
    }
    Ok(())
}

/// A reminder fires 0..=[`MAX_REMINDER_OFFSET_MIN`] minutes before the
/// start. Anything else is not offered by the editor, and huge values used
/// to crash the reminder scheduler inside chrono.
pub fn validate_reminder_minutes(m: i64) -> Result<(), &'static str> {
    if (0..=MAX_REMINDER_OFFSET_MIN).contains(&m) {
        Ok(())
    } else {
        Err("Muistutus voi olla enintään vuorokautta ennen.")
    }
}

/// An occurrence start (`recurrence_id`, `?occ=`, `?from=`) is a stored
/// timestamp like any other: exact format, bounded year.
pub fn validate_occurrence_start(s: &str) -> Result<(), &'static str> {
    crate::dates::parse_timestamp(s)
        .map(|_| ())
        .ok_or("Virheellinen toistokerta.")
}

/// True if the event intersects the window [win_start, win_end]. Uniform
/// RFC3339 formatting makes this a pure string comparison.
pub fn event_overlaps(ev: &Event, win_start: &str, win_end: &str) -> bool {
    ev.starts_at.as_str() <= win_end && ev.ends_at.as_str() >= win_start
}

/// True if the event's START DAY falls in [`from_day`, `to_day`] (inclusive,
/// both 'YYYY-MM-DD'). [`expand_events`] bounds only the recurring expansion —
/// one-off events pass through untouched, past ones included — so list views
/// that group by day must clamp the result themselves or old events linger.
/// Day granularity (not `event_overlaps`) keeps a running multi-day event off
/// the list under a past day heading, and keeps today's earlier events visible.
pub fn starts_within(ev: &Event, from_day: &str, to_day: &str) -> bool {
    let day = &ev.starts_at[..10.min(ev.starts_at.len())];
    day >= from_day && day <= to_day
}

/// Per-master ceiling for [`expand_events`]: a daily rule across a 6-week
/// month grid needs 42; anything past ~3/day is a rule gone wrong.
pub const EXPAND_CAP: u16 = 120;

/// What the calendar views render: one-off events (overrides included) pass
/// through untouched; each series master is REPLACED by synthetic occurrence
/// events expanded over the window, minus any occurrence listed in `exdates`.
/// A synthetic occurrence copies the master's fields, gets
/// `uid = "<masterUid>@<occStart>"` (so a tap can identify the occurrence;
/// the editor splits on '@' to find the series), and an end time shifted to
/// preserve the master's duration.
pub fn expand_events(
    events: &[Event],
    exdates: &[Exdate],
    win_start: &str,
    win_end: &str,
) -> Vec<Event> {
    let mut out = Vec::with_capacity(events.len());
    for ev in events {
        let Some(rule) = &ev.rrule else {
            out.push(ev.clone());
            continue;
        };
        // Which occurrences of THIS series are skipped. A linear scan per
        // master is fine: exdates stay few (one per hand-edited occurrence).
        let skipped: Vec<&str> = exdates
            .iter()
            .filter(|x| x.series_uid == ev.uid)
            .map(|x| x.occ_start.as_str())
            .collect();
        for occ_start in crate::recur::expand(rule, &ev.starts_at, win_start, win_end, EXPAND_CAP) {
            if skipped.contains(&occ_start.as_str()) {
                continue;
            }
            let ends_at = crate::recur::occurrence_end(&ev.starts_at, &ev.ends_at, &occ_start);
            out.push(Event {
                uid: format!("{}@{}", ev.uid, occ_start),
                starts_at: occ_start,
                ends_at,
                ..ev.clone()
            });
        }
    }
    out
}

/// The largest offset the editor offers (1 vrk). Bounds the scan window.
pub const MAX_REMINDER_OFFSET_MIN: i64 = 1440;

/// All-day events start at 00:00, which is a bad moment to be reminded. Their
/// reminders count back from 09:00 (540 min past midnight) on the start's
/// DATE instead, so "1 vrk ennen" fires at 09:00 the day before. The anchor is
/// taken from the date alone, so it holds even if a stored all-day start is
/// not exactly midnight.
pub const ALL_DAY_ANCHOR_MIN: i64 = 540;

/// How far an all-day occurrence's start can lie AFTER its 09:00 anchor: a
/// start at 23:59 is up to 24 h − 9 h = 15 h past it. Widens the scan
/// window's end in [`due_reminders`].
const ALL_DAY_START_AFTER_ANCHOR_MIN: i64 = 24 * 60 - ALL_DAY_ANCHOR_MIN;

/// The moment an occurrence's reminder offset counts back from: its start
/// for timed events, 09:00 on the start's date for all-day ones. `None` on a
/// malformed timestamp.
fn reminder_anchor(occ: &Event) -> Option<String> {
    if !occ.all_day {
        return Some(occ.starts_at.clone());
    }
    // `.get(..10)` is the non-panicking slice: None if the string is shorter
    // (or 10 isn't a char boundary), instead of a panic like `[..10]`.
    let date = occ.starts_at.get(..10)?;
    crate::recur::shift_minutes(&format!("{date}T00:00:00Z"), ALL_DAY_ANCHOR_MIN)
}

/// One reminder that should fire now.
#[derive(Debug, Clone, PartialEq)]
pub struct DueReminder {
    /// Master/one-off uid (any synthetic-occurrence '@' suffix stripped).
    pub event_uid: String,
    pub occ_start: String,
    pub title: String,
    pub attendee_ids: Vec<i64>,
    /// Whether the occurrence is an all-day event (the push text differs).
    pub all_day: bool,
}

/// Pure scheduler core, shared so Phase 6 chores can reuse it. Due iff
/// `fire_at ≤ now < fire_at + late_cutoff` with
/// `fire_at = anchor − reminder_minutes`, where `anchor` is the occurrence
/// start for timed events and 09:00 ([`ALL_DAY_ANCHOR_MIN`]) on the start's
/// date for all-day ones (lexical string compares — uniform floating
/// formatting makes that valid).
pub fn due_reminders(
    events: &[Event],
    exdates: &[Exdate],
    now: &str,
    late_cutoff_min: i64,
) -> Vec<DueReminder> {
    // Due means anchor ∈ (now + offset − late_cutoff, now + offset], with
    // 0 ≤ offset ≤ MAX_OFFSET. For timed events start = anchor, so starts lie
    // in [now − late_cutoff, now + MAX_OFFSET]. For all-day ones the anchor is
    // 09:00 of the start's date, so start − anchor ∈ [−9 h, +15 h): the window
    // opens ALL_DAY_ANCHOR_MIN earlier (a midnight start, anchor 9 h later)
    // and closes ALL_DAY_START_AFTER_ANCHOR_MIN later (a late-evening start,
    // anchor up to 15 h earlier). Widening only lets more occurrences be
    // *considered*; the `fire_at ≤ now < fire_end` check below still decides
    // what is due.
    // `checked_*` integer ops return None on overflow instead of panicking
    // (debug) or silently wrapping (release).
    let (Some(win_start), Some(win_end)) = (
        late_cutoff_min
            .checked_add(ALL_DAY_ANCHOR_MIN)
            .and_then(i64::checked_neg)
            .and_then(|m| crate::recur::shift_minutes(now, m)),
        MAX_REMINDER_OFFSET_MIN
            .checked_add(late_cutoff_min)
            .and_then(|m| m.checked_add(ALL_DAY_START_AFTER_ANCHOR_MIN))
            .and_then(|m| crate::recur::shift_minutes(now, m)),
    ) else {
        return Vec::new();
    };
    let mut due = Vec::new();
    for occ in expand_events(events, exdates, &win_start, &win_end) {
        // One-offs pass through expand_events; masters come back as
        // occurrences — either way starts_at IS the occurrence start.
        let Some(offset) = occ.reminder_minutes else {
            continue;
        };
        // Timed events count back from their start; all-day ones from 09:00
        // on their date. A malformed start skips the row.
        let Some(anchor) = reminder_anchor(&occ) else {
            continue;
        };
        // The offset comes from the DB and may predate validation: skip the
        // row (never panic) if the arithmetic overflows. `-i64::MIN` itself
        // overflows, hence `checked_neg`.
        let (Some(fire_at), Some(fire_end)) = (
            offset
                .checked_neg()
                .and_then(|m| crate::recur::shift_minutes(&anchor, m)),
            late_cutoff_min
                .checked_sub(offset)
                .and_then(|m| crate::recur::shift_minutes(&anchor, m)),
        ) else {
            continue;
        };
        if fire_at.as_str() <= now && now < fire_end.as_str() {
            // Synthetic occurrence uids are "master@occStart" — keep the master part.
            let uid = occ.uid.split('@').next().unwrap_or(&occ.uid).to_owned();
            due.push(DueReminder {
                event_uid: uid,
                occ_start: occ.starts_at.clone(),
                title: occ.title.clone(),
                attendee_ids: occ.attendee_ids.clone(),
                all_day: occ.all_day,
            });
        }
    }
    due
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(start: &str, end: &str) -> Event {
        Event {
            uid: "u1".into(),
            title: "T".into(),
            all_day: false,
            starts_at: start.into(),
            ends_at: end.into(),
            location: None,
            notes: None,
            rrule: None,
            series_uid: None,
            recurrence_id: None,
            reminder_minutes: None,
            attendee_ids: vec![],
            created_by: 1,
            created_at: start.into(),
            updated_at: start.into(),
        }
    }

    #[test]
    fn title_rules() {
        assert!(validate_event_title("Hammaslääkäri").is_ok());
        assert!(validate_event_title("  ").is_err());
        assert!(validate_event_title(&"x".repeat(101)).is_err());
    }

    #[test]
    fn end_before_start_rejected() {
        assert!(validate_event_times("2026-07-01T10:00:00Z", "2026-07-01T09:00:00Z").is_err());
        assert!(validate_event_times("2026-07-01T10:00:00Z", "2026-07-01T11:00:00Z").is_ok());
        assert!(validate_event_times("2026-07-01T10:00:00Z", "2026-07-01T10:00:00Z").is_ok());
    }

    #[test]
    fn malformed_timestamps_rejected() {
        // The forever-event bug: an empty end input formatted into ":00Z",
        // which is non-empty AND — because ':' sorts after every digit —
        // string-compares later than any real timestamp, so the event
        // overlapped every future window. The shape check closes the hole.
        assert!(validate_event_times("2026-07-01T10:00:00Z", ":00Z").is_err());
        assert!(validate_event_times(":00Z", "2026-07-01T10:00:00Z").is_err());
        // Empty all-day date formats into a bare "T00:00:00Z".
        assert!(validate_event_times("T00:00:00Z", "2026-07-01T00:00:00Z").is_err());
        // Unpadded components would also break lexical comparison.
        assert!(validate_event_times("2026-7-1T10:00:00Z", "2026-07-01T11:00:00Z").is_err());
        assert!(validate_event_times("garbage", "2026-07-01T11:00:00Z").is_err());
        // The two real stored shapes still pass: timed and all-day midnight.
        assert!(validate_event_times("2026-07-01T10:30:00Z", "2026-07-01T11:00:00Z").is_ok());
        assert!(validate_event_times("2026-07-01T00:00:00Z", "2026-07-02T00:00:00Z").is_ok());
    }

    fn save_req() -> SaveEventRequest {
        SaveEventRequest {
            // A real `crypto.randomUUID()` value: must stay accepted.
            uid: "3b241101-e2bb-4255-8caf-4136c566a962".into(),
            title: "Hammaslääkäri".into(),
            all_day: false,
            starts_at: "2026-07-01T10:00:00Z".into(),
            ends_at: "2026-07-01T11:00:00Z".into(),
            location: Some("Keskusta".into()),
            notes: None,
            rrule: None,
            scope: None,
            recurrence_id: None,
            reminder_minutes: Some(30),
            attendee_ids: vec![],
        }
    }

    #[test]
    fn validate_event_accepts_real_request() {
        assert!(validate_event(&save_req()).is_ok());
        let mut r = save_req();
        r.uid = "uid-1782642789123".into(); // the no-crypto fallback shape
        r.reminder_minutes = Some(MAX_REMINDER_OFFSET_MIN);
        r.recurrence_id = Some("2026-07-13T18:00:00Z".into());
        r.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        assert!(validate_event(&r).is_ok());
        r.reminder_minutes = Some(0);
        assert!(validate_event(&r).is_ok());
    }

    #[test]
    fn validate_event_rejects_out_of_range_reminders() {
        for m in [
            -1,
            MAX_REMINDER_OFFSET_MIN + 1,
            200_000_000_000_000,
            -100_000_000_000_000,
            i64::MIN,
            i64::MAX,
        ] {
            let mut r = save_req();
            r.reminder_minutes = Some(m);
            assert!(validate_event(&r).is_err(), "{m} should be rejected");
        }
    }

    #[test]
    fn validate_event_rejects_ancient_or_far_future_start() {
        // Year 0001 + FREQ=DAILY made every expansion walk 2000 years.
        let mut r = save_req();
        r.starts_at = "0001-01-01T00:00:00Z".into();
        r.rrule = Some("FREQ=DAILY".into());
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.ends_at = "9999-01-01T00:00:00Z".into();
        assert!(validate_event(&r).is_err());
        // Shape-valid but not a real date.
        let mut r = save_req();
        r.starts_at = "2026-02-30T10:00:00Z".into();
        assert!(validate_event(&r).is_err());
    }

    #[test]
    fn validate_event_field_limits() {
        let mut r = save_req();
        r.uid = "x".repeat(UID_MAX + 1);
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.uid = " ".into();
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.location = Some("ä".repeat(LOCATION_MAX)); // chars, not bytes
        assert!(validate_event(&r).is_ok());
        r.location = Some("ä".repeat(LOCATION_MAX + 1));
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.notes = Some("x".repeat(NOTES_MAX));
        assert!(validate_event(&r).is_ok());
        r.notes = Some("x".repeat(NOTES_MAX + 1));
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.recurrence_id = Some("garbage".into());
        assert!(validate_event(&r).is_err());
        let mut r = save_req();
        r.rrule = Some("FREQ=BOGUS".into());
        assert!(validate_event(&r).is_err());
    }

    #[test]
    fn validate_event_attendee_limits() {
        let mut r = save_req();
        r.attendee_ids = (1..=ATTENDEES_MAX as i64).collect();
        assert!(validate_event(&r).is_ok());
        r.attendee_ids = (1..=ATTENDEES_MAX as i64 + 1).collect();
        assert!(validate_event(&r).is_err());
        // A duplicate id would violate event_attendees' primary key (a 500).
        let mut r = save_req();
        r.attendee_ids = vec![3, 1, 3];
        assert!(validate_event(&r).is_err());
    }

    #[test]
    fn occurrence_start_must_be_a_timestamp() {
        assert!(validate_occurrence_start("2026-07-13T18:00:00Z").is_ok());
        assert!(validate_occurrence_start("x").is_err());
        assert!(validate_occurrence_start("0001-01-01T00:00:00Z").is_err());
    }

    #[test]
    fn overlaps_timed_within_window() {
        let e = ev("2026-07-01T10:00:00Z", "2026-07-01T11:00:00Z");
        assert!(event_overlaps(
            &e,
            "2026-07-01T00:00:00Z",
            "2026-07-01T23:59:59Z"
        ));
    }

    #[test]
    fn no_overlap_outside_window() {
        let e = ev("2026-07-05T10:00:00Z", "2026-07-05T11:00:00Z");
        assert!(!event_overlaps(
            &e,
            "2026-07-01T00:00:00Z",
            "2026-07-01T23:59:59Z"
        ));
    }

    #[test]
    fn multiday_allday_spans_window_edge() {
        // Fri–Sun all-day trip; window is just Saturday.
        let mut e = ev("2026-07-03T00:00:00Z", "2026-07-05T00:00:00Z");
        e.all_day = true;
        assert!(event_overlaps(
            &e,
            "2026-07-04T00:00:00Z",
            "2026-07-04T23:59:59Z"
        ));
    }

    #[test]
    fn starts_within_clamps_both_ends() {
        let e = ev("2026-07-10T10:00:00Z", "2026-07-10T11:00:00Z");
        assert!(starts_within(&e, "2026-07-10", "2026-07-10")); // inclusive both ends
        assert!(starts_within(&e, "2026-07-01", "2026-07-31"));
        assert!(!starts_within(&e, "2026-07-11", "2026-07-31")); // starts before window
        assert!(!starts_within(&e, "2026-07-01", "2026-07-09")); // starts after window
    }

    #[test]
    fn agenda_window_drops_past_one_offs() {
        // The agenda bug: expand_events bounds only the RECURRING expansion, so
        // a one-off from last week came through and got its own past day heading.
        let past = ev("2026-07-03T10:00:00Z", "2026-07-03T11:00:00Z");
        let today_ev = ev("2026-07-10T09:00:00Z", "2026-07-10T10:00:00Z");
        let beyond = ev("2026-09-20T09:00:00Z", "2026-09-20T10:00:00Z"); // past the 8-week horizon
        let mut out = expand_events(
            &[past, today_ev.clone(), beyond],
            &[],
            "2026-07-10T00:00:00Z",
            "2026-09-04T23:59:59Z",
        );
        assert_eq!(out.len(), 3, "expand_events itself does not clamp one-offs");
        out.retain(|e| starts_within(e, "2026-07-10", "2026-09-04"));
        assert_eq!(out, vec![today_ev]);
    }

    #[test]
    fn expand_events_passes_one_offs_through() {
        let plain = ev("2026-07-10T10:00:00Z", "2026-07-10T11:00:00Z");
        let out = expand_events(
            std::slice::from_ref(&plain),
            &[],
            "2026-07-01T00:00:00Z",
            "2026-07-31T23:59:59Z",
        );
        assert_eq!(out, vec![plain]);
    }

    #[test]
    fn expand_events_replaces_master_with_occurrences() {
        // Weekly Monday 18:00–19:30 series; window covers two Mondays.
        let mut master = ev("2026-07-06T18:00:00Z", "2026-07-06T19:30:00Z");
        master.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        let out = expand_events(
            &[master],
            &[],
            "2026-07-06T00:00:00Z",
            "2026-07-19T23:59:59Z",
        );
        assert_eq!(out.len(), 2);
        // The raw master row is gone; both entries are synthetic occurrences.
        assert_eq!(out[0].uid, "u1@2026-07-06T18:00:00Z");
        assert_eq!(out[0].starts_at, "2026-07-06T18:00:00Z");
        assert_eq!(out[0].ends_at, "2026-07-06T19:30:00Z");
        assert_eq!(out[1].uid, "u1@2026-07-13T18:00:00Z");
        assert_eq!(out[1].starts_at, "2026-07-13T18:00:00Z");
        assert_eq!(out[1].ends_at, "2026-07-13T19:30:00Z"); // duration preserved
        assert_eq!(out[1].title, "T"); // fields copied from the master
    }

    #[test]
    fn expand_events_drops_master_with_no_occurrence_in_window() {
        let mut master = ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z");
        master.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        // Window is a Friday–Sunday with no Mondays.
        let out = expand_events(
            &[master],
            &[],
            "2026-07-10T00:00:00Z",
            "2026-07-12T23:59:59Z",
        );
        assert!(out.is_empty());
    }

    #[test]
    fn expand_events_skips_exdated_occurrences() {
        let mut master = ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z");
        master.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        // The 13th is deleted ("vain tämä"): only the 6th should render.
        let ex = Exdate {
            series_uid: "u1".into(),
            occ_start: "2026-07-13T18:00:00Z".into(),
        };
        let out = expand_events(
            &[master],
            &[ex],
            "2026-07-06T00:00:00Z",
            "2026-07-19T23:59:59Z",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].starts_at, "2026-07-06T18:00:00Z");
    }

    #[test]
    fn exdates_only_hit_their_own_series() {
        let mut master = ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z");
        master.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        // Same datetime but a different series' exdate must not skip ours.
        let ex = Exdate {
            series_uid: "someone-else".into(),
            occ_start: "2026-07-13T18:00:00Z".into(),
        };
        let out = expand_events(
            &[master],
            &[ex],
            "2026-07-06T00:00:00Z",
            "2026-07-19T23:59:59Z",
        );
        assert_eq!(out.len(), 2);
    }

    fn rem_ev(start: &str, end: &str, minutes: i64) -> Event {
        let mut e = ev(start, end);
        e.reminder_minutes = Some(minutes);
        e
    }

    #[test]
    fn one_off_reminder_due_window() {
        let e = rem_ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z", 30);
        // fire_at = 17:30. Not due before it…
        assert!(
            due_reminders(std::slice::from_ref(&e), &[], "2026-07-06T17:29:59Z", 30).is_empty()
        );
        // …due at/after it…
        let due = due_reminders(std::slice::from_ref(&e), &[], "2026-07-06T17:30:00Z", 30);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_uid, "u1");
        assert_eq!(due[0].occ_start, "2026-07-06T18:00:00Z");
        // …still due 29 min late, but not 30+ (the cutoff).
        assert_eq!(
            due_reminders(std::slice::from_ref(&e), &[], "2026-07-06T17:59:00Z", 30).len(),
            1
        );
        assert!(due_reminders(&[e], &[], "2026-07-06T18:00:00Z", 30).is_empty());
    }

    #[test]
    fn events_without_reminder_are_ignored() {
        let e = ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z");
        assert!(due_reminders(&[e], &[], "2026-07-06T17:30:00Z", 30).is_empty());
    }

    #[test]
    fn recurring_occurrence_reminds_but_exdated_does_not() {
        let mut m = rem_ev("2026-07-06T18:00:00Z", "2026-07-06T19:00:00Z", 15);
        m.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        // Second occurrence (13.7.) fires at 17:45; uid comes back WITHOUT '@…'.
        let due = due_reminders(&[m.clone()], &[], "2026-07-13T17:45:00Z", 30);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_uid, "u1");
        assert_eq!(due[0].occ_start, "2026-07-13T18:00:00Z");
        // The same moment with that occurrence exdated: nothing.
        let ex = Exdate {
            series_uid: "u1".into(),
            occ_start: "2026-07-13T18:00:00Z".into(),
        };
        assert!(due_reminders(&[m], &[ex], "2026-07-13T17:45:00Z", 30).is_empty());
    }

    #[test]
    fn override_reminds_at_its_own_time() {
        // An override is a one-off row linked to a series; it reminds at ITS
        // (moved) time, independent of the master.
        let mut o = rem_ev("2026-07-13T20:00:00Z", "2026-07-13T21:00:00Z", 15);
        o.series_uid = Some("master".into());
        o.recurrence_id = Some("2026-07-13T18:00:00Z".into());
        let due = due_reminders(&[o], &[], "2026-07-13T19:45:00Z", 30);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occ_start, "2026-07-13T20:00:00Z");
    }

    #[test]
    fn max_offset_day_before_is_found() {
        // 1 vrk (1440 min) reminder: event tomorrow 09:00 fires today 09:00 —
        // proves the expansion window reaches MAX_REMINDER_OFFSET_MIN ahead.
        let e = rem_ev("2026-07-07T09:00:00Z", "2026-07-07T10:00:00Z", 1440);
        assert_eq!(
            due_reminders(&[e], &[], "2026-07-06T09:00:00Z", 30).len(),
            1
        );
    }

    fn all_day_rem_ev(day: &str, next_day: &str, minutes: i64) -> Event {
        let mut e = rem_ev(
            &format!("{day}T00:00:00Z"),
            &format!("{next_day}T00:00:00Z"),
            minutes,
        );
        e.all_day = true;
        e
    }

    #[test]
    fn all_day_day_before_reminder_anchors_at_nine() {
        // All-day event tomorrow (7.7.) with "1 vrk ennen": anchor is 7.7.
        // 09:00, so it fires 6.7. 09:00 — NOT at midnight.
        let e = all_day_rem_ev("2026-07-07", "2026-07-08", 1440);
        let at = |now: &str| due_reminders(std::slice::from_ref(&e), &[], now, 30);
        assert!(at("2026-07-06T00:00:00Z").is_empty());
        assert!(at("2026-07-06T08:59:59Z").is_empty());
        let due = at("2026-07-06T09:00:00Z");
        assert_eq!(due.len(), 1);
        assert!(due[0].all_day);
        // occ_start stays the real (midnight) start: it is the dedup key.
        assert_eq!(due[0].occ_start, "2026-07-07T00:00:00Z");
        assert_eq!(at("2026-07-06T09:29:00Z").len(), 1);
        // Past the 30 min late cutoff.
        assert!(at("2026-07-06T09:31:00Z").is_empty());
    }

    #[test]
    fn all_day_same_day_reminder_fires_before_nine() {
        // 60 min before a today-all-day event: 08:00 the same day. Its
        // occurrence started at 00:00, i.e. 8 h before `now` — the widened
        // expansion window must still reach it (matters for recurring ones).
        let mut e = all_day_rem_ev("2026-07-06", "2026-07-07", 60);
        e.rrule = Some("FREQ=WEEKLY;BYDAY=MO".into());
        let at = |now: &str| due_reminders(std::slice::from_ref(&e), &[], now, 30);
        assert!(at("2026-07-06T07:59:00Z").is_empty());
        let due = at("2026-07-13T08:00:00Z");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occ_start, "2026-07-13T00:00:00Z");
        assert!(at("2026-07-06T08:30:00Z").is_empty());
    }

    #[test]
    fn all_day_anchor_uses_the_start_date_not_start_plus_nine_hours() {
        // A (legacy/odd) all-day start of 20:00 on 7.7.: the anchor is still
        // 7.7. 09:00, so "1 vrk ennen" fires 6.7. 09:00 — not 7.7. 05:00
        // (which `start + 540 min − 1440` would give). The occurrence starts
        // 35 h after `now`, beyond the timed-event window; making it a series
        // (one-offs bypass the window) checks the window's end is widened.
        let mut e = rem_ev("2026-07-07T20:00:00Z", "2026-07-08T00:00:00Z", 1440);
        e.all_day = true;
        e.rrule = Some("FREQ=WEEKLY;BYDAY=TU".into());
        let at = |now: &str| due_reminders(std::slice::from_ref(&e), &[], now, 30);
        assert!(at("2026-07-06T08:59:00Z").is_empty());
        let due = at("2026-07-06T09:00:00Z");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].occ_start, "2026-07-07T20:00:00Z");
        assert!(at("2026-07-07T05:00:00Z").is_empty());
    }

    #[test]
    fn timed_reminders_are_not_anchored_and_flagged_timed() {
        let e = rem_ev("2026-07-06T00:00:00Z", "2026-07-06T01:00:00Z", 60);
        let due = due_reminders(std::slice::from_ref(&e), &[], "2026-07-05T23:00:00Z", 30);
        assert_eq!(due.len(), 1);
        assert!(!due[0].all_day);
        // The widened window must not make a timed event fire at "09:00 − offset".
        assert!(due_reminders(&[e], &[], "2026-07-06T08:00:00Z", 30).is_empty());
    }

    /// Hostile offsets stored before validation existed must not panic the
    /// scheduler (it runs `due_reminders` over every family's events).
    #[test]
    fn extreme_reminder_offsets_do_not_panic() {
        for m in [
            200_000_000_000_000,
            -100_000_000_000_000,
            200_000_000_000,
            i64::MIN,
            i64::MAX,
        ] {
            let e = rem_ev("2026-07-07T09:00:00Z", "2026-07-07T10:00:00Z", m);
            assert!(due_reminders(&[e], &[], "2026-07-06T09:00:00Z", 30).is_empty());
        }
    }

    #[test]
    fn serde_round_trip() {
        let e = ev("2026-07-01T10:00:00Z", "2026-07-01T11:00:00Z");
        let j = serde_json::to_string(&e).unwrap();
        assert_eq!(e, serde_json::from_str::<Event>(&j).unwrap());
    }
}
