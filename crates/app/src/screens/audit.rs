//! "Muutosloki" — admin-only audit feed. Read-only. All Finnish phrasing lives
//! in `sentence()`: the server stores the structured triple (op, entity, label)
//! and this ONE formatter turns it into a sentence, so no phrasing is scattered
//! across the Rust handlers.

use crate::AuthState;
use crate::api;
use dioxus::prelude::*;
use perkele_shared::audit::{AuditEntry, ENTITY_KINDS};

/// op → Finnish verb.
fn verb(op: &str) -> &'static str {
    match op {
        "create" => "Lisäsi",
        "update" => "Muutti",
        "delete" => "Poisti",
        _ => "Muutti",
    }
}

/// entity → the accusative noun the verb takes (e.g. "Poisti ostoksen 'Maito'").
fn noun(entity: &str) -> &'static str {
    match entity {
        "calendar_event" => "tapahtuman",
        "grocery_item" => "ostoksen",
        "recipe" => "reseptin",
        "dinner" => "päivällisen",
        "chore" => "kotityön",
        "announcement" => "tiedotteen",
        "aisle" => "hyllyn",
        "note" => "muistion",
        "feature" => "näkyviä toimintoja",
        _ => "kohteen",
    }
}

/// e.g. "Poisti ostoksen 'Maito'".
pub(crate) fn sentence(entry: &AuditEntry) -> String {
    // A feature change has no single named item: its label is already a
    // summary ("Piilotettu: Ruoka; Näytetty: Chatti"), so it follows a colon
    // instead of being quoted like a name.
    if entry.entity == "feature" {
        return format!(
            "{} {}: {}",
            verb(&entry.op),
            noun(&entry.entity),
            entry.label
        );
    }
    format!(
        "{} {} '{}'",
        verb(&entry.op),
        noun(&entry.entity),
        entry.label
    )
}

/// Finnish chip label per entity kind. Takes the input's lifetime (not
/// `'static`) because the fallback arm echoes `kind` back unchanged.
fn entity_label(kind: &str) -> &str {
    match kind {
        "calendar_event" => "Kalenteri",
        "grocery_item" => "Ostokset",
        "recipe" => "Reseptit",
        "dinner" => "Päivälliset",
        "chore" => "Kotityöt",
        "announcement" => "Tiedotteet",
        "aisle" => "Hyllyt",
        "note" => "Muistiot",
        "feature" => "Toiminnot",
        _ => kind,
    }
}

/// Admin-only screen listing every logged mutation, newest first, with an
/// entity filter and cursor-based "load more". Non-admins (and logged-out
/// visitors) never reach the endpoint — the guard here just avoids the
/// pointless fetch and shows a plain message instead.
#[component]
pub fn AuditScreen() -> Element {
    let auth = use_context::<Signal<AuthState>>();
    let user = match auth() {
        AuthState::LoggedIn(u) => u,
        _ => return rsx! { p { "Ei kirjautuneena." } },
    };
    if !user.role.is_admin() {
        return rsx! { p { "Vain ylläpitäjä näkee muutoslokin." } };
    }

    let mut entries = use_signal(Vec::<AuditEntry>::new);
    let mut filter = use_signal(|| Option::<String>::None);
    let mut error = use_signal(|| Option::<String>::None);
    let mut done = use_signal(|| false);
    let mut busy = use_signal(|| false);
    // Bumped every time the filter changes. A fetch captures the epoch it
    // started under (`started`); if a newer filter change has since bumped
    // `epoch`, the fetch is for a chip the user has already left — its
    // result must be discarded instead of overwriting/appending onto the
    // (different-filter) list that's now on screen. This is what stops the
    // race: "load more" in flight + a chip click before it resolves.
    let mut epoch = use_signal(|| 0u32);

    // Fetch one page tagged with generation `started`. Always runs — callers
    // decide whether `busy` should gate the call; this only performs the
    // request and, on completion, checks the epoch is still current before
    // touching `entries`/`done`/`error`/`busy`.
    let mut fetch_page = move |before: Option<i64>, started: u32| {
        busy.set(true);
        let ent = filter();
        spawn(async move {
            match api::audit(before, ent.as_deref()).await {
                Ok(page) => {
                    // Superseded by a later filter change — drop it.
                    if epoch() == started {
                        let n = page.len();
                        if before.is_none() {
                            entries.set(page);
                        } else {
                            entries.write().extend(page);
                        }
                        // Fewer than a full page means we've reached the end.
                        if n < perkele_shared::audit::DEFAULT_AUDIT_LIMIT as usize {
                            done.set(true);
                        }
                    }
                }
                Err(e) => {
                    if epoch() == started {
                        error.set(Some(e));
                    }
                }
            }
            // Only the still-current generation clears `busy` — a stale
            // request finishing shouldn't flip the "Lataa lisää" button back
            // to enabled while the fresh (superseding) fetch is in flight.
            if epoch() == started {
                busy.set(false);
            }
        });
    };

    // "Lataa lisää": one more page under the CURRENT filter. Busy-gated so a
    // double-tap can't fire two overlapping requests for the same page.
    let mut load_more = move |before: Option<i64>| {
        if busy() {
            return;
        }
        fetch_page(before, epoch());
    };

    // Initial load + reload whenever the filter changes. This reset path is
    // NOT gated by `busy` — a filter change must always win immediately:
    // bump the epoch (so any in-flight fetch for the old filter is discarded
    // on arrival), reset the list synchronously, then kick a fresh first page.
    use_effect(move || {
        let _ = filter(); // subscribe to filter changes
        // .peek() is an UNTRACKED read: a tracked `epoch()` read here would
        // make this effect depend on `epoch` too, and since the very next
        // line writes `epoch`, the effect would re-run itself forever (an
        // unbounded synchronous loop that pins a CPU core and hangs the tab
        // before anything ever renders). The effect's only dependency must
        // be `filter`.
        let started = *epoch.peek() + 1;
        epoch.set(started);
        entries.set(Vec::new());
        done.set(false);
        error.set(None);
        fetch_page(None, started);
    });

    rsx! {
        div { class: "card wide",
            h1 { "Muutosloki" }

            // Entity filter chips: "Kaikki" + one per domain.
            div { class: "row", style: "flex-wrap: wrap; gap: 6px;",
                button {
                    class: if filter().is_none() { "chip active" } else { "chip" },
                    onclick: move |_| filter.set(None),
                    "Kaikki"
                }
                for kind in ENTITY_KINDS {
                    button {
                        class: if filter().as_deref() == Some(kind) { "chip active" } else { "chip" },
                        onclick: move |_| filter.set(Some(kind.to_owned())),
                        "{entity_label(kind)}"
                    }
                }
            }

            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }

            ul { class: "audit",
                for entry in entries() {
                    li { key: "{entry.id}",
                        span { class: "actor", "{entry.actor_name}" }
                        span { class: "what", " {sentence(&entry)}" }
                        // No relative-time helper exists yet (dates.rs only has
                        // UTC 'YYYY-MM-DD' helpers for the calendar/planner) —
                        // shown as the raw RFC3339 timestamp for now.
                        span { class: "muted small", " · {entry.created_at}" }
                    }
                }
            }

            if !done() {
                button { class: "ghost", disabled: busy(),
                    onclick: move |_| {
                        let last = entries().last().map(|e| e.id);
                        load_more(last);
                    },
                    if busy() { "Ladataan…" } else { "Lataa lisää" }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(op: &str, entity: &str, label: &str) -> AuditEntry {
        AuditEntry {
            id: 1,
            actor_name: "Aino".into(),
            entity: entity.into(),
            entity_id: None,
            op: op.into(),
            label: label.into(),
            created_at: "2026-07-23T09:00:00Z".into(),
        }
    }

    #[test]
    fn composes_finnish_sentences() {
        assert_eq!(
            sentence(&e("delete", "grocery_item", "Maito")),
            "Poisti ostoksen 'Maito'"
        );
        assert_eq!(
            sentence(&e("create", "calendar_event", "Hammaslääkäri")),
            "Lisäsi tapahtuman 'Hammaslääkäri'"
        );
        assert_eq!(
            sentence(&e("update", "dinner", "2026-07-24")),
            "Muutti päivällisen '2026-07-24'"
        );
    }

    #[test]
    fn feature_change_reads_as_a_summary() {
        assert_eq!(
            sentence(&e(
                "update",
                "feature",
                "Piilotettu: Ruoka; Näytetty: Chatti"
            )),
            "Muutti näkyviä toimintoja: Piilotettu: Ruoka; Näytetty: Chatti"
        );
        assert_eq!(entity_label("feature"), "Toiminnot");
    }
}
