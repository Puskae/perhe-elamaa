//! Muistiot: topics of mixed free-text + checklists ("MENS jobs before
//! wedding"). Reading and checkbox taps work offline (cache + replay queue);
//! creating topics and full-text edits need the network, like posting an
//! announcement.

use crate::api;
use crate::screens::Route;
use crate::screens::calendar::is_online;
use crate::store::{self, QueuedMutation};
use dioxus::prelude::*;
use perkele_shared::note::{
    BlockKind, NoteTopic, NoteTopicSummary, SaveTopicRequest, SetBlockCheckedRequest,
    parse_note_text, render_note_text, validate_topic_title,
};

#[component]
pub fn NotesScreen() -> Element {
    let mut topics: Signal<Vec<NoteTopicSummary>> = use_signal(store::get_notes);
    let mut draft = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let nav = use_navigator();

    // App-wide sync tick from NavLayout (its single SSE stream bumps it).
    let refresh = use_context::<Signal<u32>>();
    let fetched = use_resource(move || {
        let _ = refresh(); // subscribe
        async move { api::list_notes().await }
    });
    use_effect(move || {
        if let Some(Ok(list)) = fetched() {
            store::set_notes(&list);
            topics.set(list);
        }
    });

    let online = is_online();
    let submit = move |_| {
        let title = draft();
        if let Err(m) = validate_topic_title(&title) {
            error.set(Some(m.to_owned()));
            return;
        }
        spawn(async move {
            match api::create_note(title).await {
                // Straight into the new topic — that's where writing happens.
                Ok(t) => {
                    nav.push(Route::NoteTopicScreen { id: t.id });
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };

    rsx! {
        div { class: "card wide",
            h1 { "Muistiot" }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            div { class: "add-form",
                input {
                    class: "item-input",
                    placeholder: "Uusi aihe…",
                    value: "{draft}",
                    oninput: move |e| draft.set(e.value()),
                }
                button {
                    class: "primary",
                    disabled: !online,
                    onclick: submit,
                    "Luo"
                }
            }
            if !online {
                div { class: "offline-banner", "Uuden aiheen luonti vaatii verkkoyhteyden." }
            }
            ul { class: "members",
                for t in topics() {
                    li {
                        Link {
                            to: Route::NoteTopicScreen { id: t.id },
                            style: "flex:1;display:flex;gap:10px;text-decoration:none;color:inherit;",
                            span { "📝 {t.title}" }
                            if t.check_total > 0 {
                                span { class: "muted", style: "margin-left:auto;", "{t.check_done}/{t.check_total}" }
                            }
                        }
                    }
                }
            }
            if topics().is_empty() {
                p { class: "muted", "Ei muistioita vielä. Luo ensimmäinen yllä!" }
            }
        }
    }
}

#[component]
pub fn NoteTopicScreen(id: i64) -> Element {
    let mut topic: Signal<Option<NoteTopic>> = use_signal(move || store::get_note(id));
    // Some(text) = editor open with that draft; None = read view. The editor
    // holds its own draft string, so a background refetch swapping `topic`
    // never wipes typing.
    let mut editing = use_signal(|| Option::<String>::None);
    let mut title_draft = use_signal(String::new);
    let mut error = use_signal(|| Option::<String>::None);
    let mut missing = use_signal(|| false);
    // Guards the irreversible delete: the button opens this confirm dialog,
    // the dialog's "Kyllä" is what actually fires the DELETE.
    let mut confirming_delete = use_signal(|| false);
    let nav = use_navigator();
    let me = use_resource(api::me);

    let mut refresh = use_context::<Signal<u32>>();
    let fetched = use_resource(move || {
        let _ = refresh(); // subscribe: SSE pokes refetch other phones' edits
        async move { api::get_note(id).await }
    });
    use_effect(move || {
        match fetched() {
            Some(Ok(t)) => {
                store::set_note(&t);
                topic.set(Some(t));
            }
            // Treat only an auth'd "not found" as deletion — a network error
            // while offline must NOT bounce the user off their cached note.
            // (Server 404s carry a Finnish "ei löytynyt" message.)
            Some(Err(e)) if e.contains("löytynyt") || e.contains("not found") => {
                missing.set(true);
            }
            _ => {}
        }
    });

    // Optimistic idempotent tick: flip locally, PUT in the background, queue
    // the intent for replay if the request can't get out (grocery pattern).
    let mut tick = move |block_id: i64, target: bool| {
        if let Some(mut t) = topic() {
            for b in &mut t.blocks {
                if b.id == block_id {
                    b.checked = target;
                }
            }
            store::set_note(&t);
            topic.set(Some(t));
        }
        spawn(async move {
            if api::set_block_checked(id, block_id, target).await.is_err() {
                let body = serde_json::to_string(&SetBlockCheckedRequest { checked: target }).ok();
                store::enqueue(QueuedMutation {
                    method: "PUT".to_owned(),
                    url: format!("/api/notes/{id}/blocks/{block_id}/checked"),
                    body,
                });
            }
        });
    };

    let online = is_online();
    let my = me().and_then(|r| r.ok());
    // Author-or-admin for BOTH rewriting (Muokkaa → replace-all PUT) and
    // deleting; ticking checkboxes below stays open to everyone.
    let can_modify = topic()
        .as_ref()
        .is_some_and(|t| super::can_modify(my.as_ref(), t.created_by));

    if missing() {
        return rsx! {
            div { class: "card wide",
                p { class: "muted", "Muistiota ei löytynyt (ehkä poistettu)." }
                button {
                    class: "ghost",
                    onclick: move |_| {
                        nav.push(Route::NotesScreen {});
                    },
                    "← Muistiot"
                }
            }
        };
    }
    let Some(t) = topic() else {
        return rsx! {
            div { class: "card wide",
                p { class: "muted", "Ladataan…" }
            }
        };
    };

    rsx! {
        div { class: "card wide",
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            if let Some(text) = editing() {
                // --- editor mode: the whole note as plain text -------------
                label { "Otsikko" }
                input {
                    value: "{title_draft}",
                    oninput: move |e| title_draft.set(e.value()),
                }
                label { "Sisältö — rivit \"- [ ]\" ja \"- [x]\" ovat valintaruutuja" }
                textarea {
                    class: "note-editor",
                    value: "{text}",
                    oninput: move |e| editing.set(Some(e.value())),
                }
                div { class: "row",
                    button {
                        class: "primary",
                        disabled: !online,
                        onclick: move |_| {
                            let title = title_draft();
                            if let Err(m) = validate_topic_title(&title) {
                                error.set(Some(m.to_owned()));
                                return;
                            }
                            let blocks = parse_note_text(&editing().unwrap_or_default());
                            spawn(async move {
                                match api::save_note(id, &SaveTopicRequest { title, blocks }).await {
                                    Ok(fresh) => {
                                        store::set_note(&fresh);
                                        topic.set(Some(fresh));
                                        editing.set(None);
                                        error.set(None);
                                        let n = refresh.peek().wrapping_add(1);
                                        refresh.set(n);
                                    }
                                    Err(e) => error.set(Some(e)),
                                }
                            });
                        },
                        "Tallenna"
                    }
                    button {
                        class: "ghost",
                        onclick: move |_| editing.set(None),
                        "Peruuta"
                    }
                }
                if !online {
                    div { class: "offline-banner", "Tallennus vaatii verkkoyhteyden." }
                }
            } else {
                // --- read view: prose + tappable checkboxes ----------------
                div { class: "row spread",
                    h1 { "{t.title}" }
                    if can_modify {
                        button {
                            class: "ghost",
                            style: "margin:0;",
                            onclick: move |_| {
                                if let Some(t) = topic() {
                                    title_draft.set(t.title.clone());
                                    editing.set(Some(render_note_text(&t.blocks)));
                                }
                            },
                            "Muokkaa"
                        }
                    }
                }
                for b in t.blocks.clone() {
                    {
                        match b.kind {
                            BlockKind::Check => rsx! {
                                label { class: "note-check",
                                    input {
                                        r#type: "checkbox",
                                        checked: b.checked,
                                        onchange: move |_| tick(b.id, !b.checked),
                                    }
                                    span {
                                        class: if b.checked { "muted" } else { "" },
                                        style: if b.checked { "text-decoration:line-through;" } else { "" },
                                        "{b.content}"
                                    }
                                }
                            },
                            BlockKind::Text => rsx! {
                                p { class: "note-text", "{b.content}" }
                            },
                        }
                    }
                }
                if t.blocks.is_empty() {
                    p { class: "muted",
                        if can_modify {
                            "Tyhjä muistio — paina Muokkaa ja ala kirjoittaa."
                        } else {
                            "Tyhjä muistio."
                        }
                    }
                }
                if can_modify {
                    button {
                        // `note-delete` adds breathing room above; `.clear-btn`
                        // itself is shared with the grocery screen.
                        class: "clear-btn note-delete",
                        disabled: !online,
                        // Don't delete on the spot — pop the confirm dialog first.
                        onclick: move |_| confirming_delete.set(true),
                        "Poista muistio"
                    }
                }
            }
            if confirming_delete() {
                // Modal overlay: backdrop dims the page, the box holds the
                // warning + the "Kyllä / Ei" choice.
                div { class: "modal-backdrop",
                    div { class: "modal",
                        p { "Oletko varma? Poistettuasi muistion, sitä ei voi enää palauttaa." }
                        div { class: "row",
                            button {
                                class: "danger",
                                onclick: move |_| {
                                    confirming_delete.set(false);
                                    spawn(async move {
                                        match api::delete_note(id).await {
                                            Ok(()) => {
                                                nav.push(Route::NotesScreen {});
                                            }
                                            Err(e) => error.set(Some(e)),
                                        }
                                    });
                                },
                                "Kyllä"
                            }
                            button {
                                class: "ghost",
                                onclick: move |_| confirming_delete.set(false),
                                "Ei"
                            }
                        }
                    }
                }
            }
        }
    }
}
