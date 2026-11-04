//! "Minä" screen: who you are, the family member list, logout, and (for
//! admins) generating a single-use invite code.

use crate::AuthState;
use crate::api;
use crate::screens::Route;
use dioxus::prelude::*;
use perkele_shared::auth::Role;
use perkele_shared::feature::{FamilyFeatures, Feature};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

/// Profile and family overview. Pulls the member list via `use_resource` so it
/// refetches automatically if the component remounts.
#[component]
pub fn MeScreen() -> Element {
    let mut auth = use_context::<Signal<AuthState>>();
    let user = match auth() {
        AuthState::LoggedIn(u) => u,
        _ => return rsx! { p { "Ei kirjautuneena." } },
    };
    let members = use_resource(|| async { api::members().await });

    let logout = move |_| {
        spawn(async move {
            // Push subscriptions belong to the member, not the session: left
            // alone, this device would keep getting their reminders and
            // announcements after logout. Drop it while the session still
            // authorizes the server-side delete. Best effort — if that call
            // fails, the browser-side unsubscribe still makes the push
            // service answer 410, and the server prunes the row on next send.
            let _ = drop_push_for_logout().await;
            let _ = api::logout().await;
            crate::store::clear_session_data();
            auth.set(AuthState::LoggedOut);
        });
    };

    rsx! {
        div { class: "card wide",
            div { class: "row spread",
                div {
                    h1 { "{user.display_name}" }
                    span { class: "badge", "{user.role:?}" }
                }
                button { class: "ghost", onclick: logout, "Kirjaudu ulos" }
            }

            h2 { "Perhe" }
            match &*members.read() {
                Some(Ok(list)) => rsx! {
                    ul { class: "members",
                        for m in list.clone() {
                            li { key: "{m.id}",
                                span { "{m.display_name}" }
                                span { class: "muted", "@{m.username}" }
                                span { class: "badge small", "{m.role:?}" }
                            }
                        }
                    }
                },
                Some(Err(e)) => rsx! { div { class: "error", "{e}" } },
                None => rsx! { p { class: "muted", "Ladataan jäseniä…" } },
            }

            NotificationsPanel {}

            if user.role.is_admin() {
                div { style: "margin-top: 12px;",
                    Link { to: Route::AuditScreen {}, class: "switch", "→ Muutosloki" }
                }
                FeaturesPanel {}
                InvitePanel {}
            }
        }
    }
}

const PUSH_ERR: &str = "Ilmoitusten käyttöönotto epäonnistui.";

/// The device's current PushSubscription, if any. `JsFuture::from(promise)`
/// bridges a JS Promise into a Rust Future — the whole Web Push API is
/// promise-based, so this pattern repeats through the flow.
async fn current_subscription() -> Result<Option<web_sys::PushSubscription>, String> {
    let sw = web_sys::window()
        .ok_or(PUSH_ERR)?
        .navigator()
        .service_worker();
    let reg: web_sys::ServiceWorkerRegistration = JsFuture::from(sw.ready().map_err(|_| PUSH_ERR)?)
        .await
        .map_err(|_| PUSH_ERR)?
        .unchecked_into();
    let sub = JsFuture::from(
        reg.push_manager()
            .map_err(|_| PUSH_ERR)?
            .get_subscription()
            .map_err(|_| PUSH_ERR)?,
    )
    .await
    .map_err(|_| PUSH_ERR)?;
    Ok(if sub.is_undefined() || sub.is_null() {
        None
    } else {
        Some(sub.unchecked_into())
    })
}

/// Unsubscribe this device on logout. Unlike `current_subscription`, this
/// uses `getRegistration()`, which resolves to `undefined` when no service
/// worker is registered — `ready` would never resolve then, and logout would
/// hang forever.
async fn drop_push_for_logout() -> Result<(), String> {
    let sw = web_sys::window()
        .ok_or(PUSH_ERR)?
        .navigator()
        .service_worker();
    let reg = JsFuture::from(sw.get_registration())
        .await
        .map_err(|_| PUSH_ERR)?;
    if reg.is_undefined() || reg.is_null() {
        return Ok(());
    }
    let reg: web_sys::ServiceWorkerRegistration = reg.unchecked_into();
    let sub = JsFuture::from(
        reg.push_manager()
            .map_err(|_| PUSH_ERR)?
            .get_subscription()
            .map_err(|_| PUSH_ERR)?,
    )
    .await
    .map_err(|_| PUSH_ERR)?;
    if sub.is_undefined() || sub.is_null() {
        return Ok(());
    }
    let sub: web_sys::PushSubscription = sub.unchecked_into();
    let endpoint = sub.endpoint();
    if let Ok(p) = sub.unsubscribe() {
        let _ = JsFuture::from(p).await;
    }
    api::push_unsubscribe(&endpoint).await
}

/// Subscribe this browser with the server's VAPID key. If the browser is
/// already subscribed, the Push API hands back that same subscription.
async fn browser_subscribe(
    reg: &web_sys::ServiceWorkerRegistration,
    vapid_public_key: &str,
) -> Result<web_sys::PushSubscription, String> {
    // The spec allows passing the base64url string directly as
    // applicationServerKey.
    let opts = web_sys::PushSubscriptionOptionsInit::new();
    opts.set_user_visible_only(true);
    opts.set_application_server_key(&wasm_bindgen::JsValue::from_str(vapid_public_key));
    Ok(JsFuture::from(
        reg.push_manager()
            .map_err(|_| PUSH_ERR)?
            .subscribe_with_options(&opts)
            .map_err(|_| PUSH_ERR)?,
    )
    .await
    .map_err(|_| PUSH_ERR)?
    .unchecked_into())
}

/// Hand a subscription to the server. JSON.stringify carries endpoint + keys;
/// serde picks the pieces out. Returns the API error with its status intact,
/// so the caller can tell a 409 apart. (A local failure has `status: None`.)
async fn register_subscription(sub: &web_sys::PushSubscription) -> Result<(), api::HttpError> {
    let local_err = || api::HttpError {
        status: None,
        message: PUSH_ERR.to_owned(),
    };
    let json = js_sys::JSON::stringify(sub).map_err(|_| local_err())?;
    #[derive(serde::Deserialize)]
    struct Keys {
        p256dh: String,
        auth: String,
    }
    #[derive(serde::Deserialize)]
    struct SubJson {
        endpoint: String,
        keys: Keys,
    }
    let parsed: SubJson = serde_json::from_str(&String::from(json)).map_err(|_| local_err())?;
    api::push_subscribe(&perkele_shared::push::SubscribeRequest {
        endpoint: parsed.endpoint,
        p256dh: parsed.keys.p256dh,
        auth: parsed.keys.auth,
    })
    .await
}

/// Ask permission, subscribe with the server's VAPID key, register with the API.
async fn enable_push() -> Result<bool, String> {
    // 1) Notification permission (browser prompt).
    let perm = JsFuture::from(web_sys::Notification::request_permission().map_err(|_| PUSH_ERR)?)
        .await
        .map_err(|_| PUSH_ERR)?;
    if perm.as_string().as_deref() != Some("granted") {
        return Err("Ilmoituslupa evättiin selaimessa.".to_owned());
    }
    // 2) Subscribe with the VAPID public key.
    let vapid = api::vapid_key().await?;
    let sw = web_sys::window()
        .ok_or(PUSH_ERR)?
        .navigator()
        .service_worker();
    let reg: web_sys::ServiceWorkerRegistration = JsFuture::from(sw.ready().map_err(|_| PUSH_ERR)?)
        .await
        .map_err(|_| PUSH_ERR)?
        .unchecked_into();
    let sub = browser_subscribe(&reg, &vapid.public_key).await?;
    // 3) Register it with the server.
    match register_subscription(&sub).await {
        Ok(()) => Ok(true),
        // 409: this browser's endpoint is still registered to ANOTHER user
        // (a shared browser where someone else enabled notifications). The
        // server won't re-home it, so drop the browser subscription and make
        // a new one — a fresh subscription gets a fresh endpoint — then try
        // exactly once more. The old row dies on the server's next failed
        // send (the push service answers 410 for the dropped endpoint).
        Err(e) if e.status == Some(409) => {
            JsFuture::from(sub.unsubscribe().map_err(|_| PUSH_ERR)?)
                .await
                .map_err(|_| PUSH_ERR)?;
            let fresh = browser_subscribe(&reg, &vapid.public_key).await?;
            if let Err(e) = register_subscription(&fresh).await {
                // Best effort: don't leave the browser subscribed to
                // something the server doesn't know, or the toggle would
                // show "on" on the next visit.
                if let Ok(p) = fresh.unsubscribe() {
                    let _ = JsFuture::from(p).await;
                }
                return Err(e.message);
            }
            Ok(true)
        }
        Err(e) => Err(e.message),
    }
}

/// Unsubscribe in the browser and forget the row server-side.
async fn disable_push() -> Result<bool, String> {
    if let Some(sub) = current_subscription().await? {
        let endpoint = sub.endpoint();
        let _ = JsFuture::from(sub.unsubscribe().map_err(|_| PUSH_ERR)?).await;
        api::push_unsubscribe(&endpoint).await?;
    }
    Ok(false)
}

/// Per-device Web Push opt-in. State comes from pushManager.getSubscription()
/// (the browser's truth), not localStorage.
#[component]
fn NotificationsPanel() -> Element {
    let mut subscribed = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    let mut info = use_signal(|| Option::<String>::None);

    // On mount: reflect the browser's current subscription state.
    use_future(move || async move {
        if let Ok(Some(_)) = current_subscription().await {
            subscribed.set(true);
        }
    });

    let toggle = move |_| {
        if busy() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn(async move {
            let res = if subscribed() {
                disable_push().await
            } else {
                enable_push().await
            };
            match res {
                Ok(on) => subscribed.set(on),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    let send_test = move |_| {
        spawn(async move {
            match api::push_test().await {
                Ok(()) => info.set(Some("Testi lähetetty.".to_owned())),
                Err(e) => error.set(Some(e)),
            }
        });
    };

    rsx! {
        h2 { "Ilmoitukset" }
        label { class: "row", style: "gap:8px;align-items:center;",
            input {
                r#type: "checkbox",
                style: "width:auto;",
                checked: subscribed(),
                disabled: busy(),
                onchange: toggle,
            }
            "Ilmoitukset tällä laitteella"
        }
        if subscribed() {
            button { class: "ghost", onclick: send_test, "Lähetä testi-ilmoitus" }
        }
        if let Some(i) = info() {
            p { class: "muted", "{i}" }
        }
        if let Some(e) = error() {
            div { class: "error", "{e}" }
        }
    }
}

/// Admin-only panel: which bottom-bar features the whole family sees.
/// Each toggle PUTs the full new set right away. The checkbox flips
/// optimistically; a failed save puts the previous set back and shows why.
#[component]
fn FeaturesPanel() -> Element {
    let mut features = crate::screens::use_family_features();
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| Option::<String>::None);
    // The browser's onLine flag isn't a signal, so reading it alone would
    // never re-render this panel. Reading NavLayout's `sync_tick` context
    // signal subscribes the panel to it: every SSE message or reconnect
    // (which is what happens when the network comes back) bumps the tick,
    // the panel re-renders, and `is_online()` is read fresh.
    let sync_tick = use_context::<Signal<u32>>();
    let _ = sync_tick(); // subscribe
    let online = crate::screens::calendar::is_online();

    // `move |f, visible|` builds a handler per checkbox below. Signals are
    // `Copy`, so each closure gets its own handle to the same state.
    let mut toggle = move |f: Feature, visible: bool| {
        if busy() {
            return;
        }
        let previous = features();
        let mut hidden = previous.hidden.clone();
        hidden.retain(|h| *h != f);
        if !visible {
            hidden.push(f);
        }
        let next = FamilyFeatures { hidden }.normalized();
        error.set(None);
        busy.set(true);
        features.set(next.clone()); // optimistic
        spawn(async move {
            match api::put_family_features(&next).await {
                Ok(saved) => {
                    crate::store::set_hidden_features(&saved);
                    features.set(saved);
                }
                Err(e) => {
                    features.set(previous);
                    error.set(Some(e));
                }
            }
            busy.set(false);
        });
    };

    rsx! {
        div { class: "panel",
            h2 { "Näkyvät toiminnot" }
            p { class: "muted small",
                "Piilotetut toiminnot katoavat koko perheen alapalkista ja Koti-näkymästä, ja niiden ilmoitukset hiljennetään. Tiedot säilyvät."
            }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            for f in Feature::ALL {
                label {
                    key: "{f.key()}",
                    class: "row",
                    style: "gap:8px;align-items:center;",
                    input {
                        r#type: "checkbox",
                        style: "width:auto;",
                        checked: !features.read().is_hidden(f),
                        disabled: busy() || !online,
                        onchange: move |e| toggle(f, e.checked()),
                    }
                    "{f.label_fi()}"
                }
            }
            if !online {
                div { class: "offline-banner", "Asetusten muuttaminen vaatii verkkoyhteyden." }
            }
        }
    }
}

/// Admin-only panel: pick a role, generate a single-use invite code to share.
#[component]
fn InvitePanel() -> Element {
    let mut role = use_signal(|| Role::Member);
    let mut code = use_signal(|| Option::<String>::None);
    let mut error = use_signal(|| Option::<String>::None);
    let mut busy = use_signal(|| false);

    let generate = move |_| {
        if busy() {
            return;
        }
        error.set(None);
        busy.set(true);
        let chosen = role();
        spawn(async move {
            match api::create_invite(chosen).await {
                Ok(invite) => code.set(Some(invite.code)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };

    rsx! {
        div { class: "panel",
            h2 { "Kutsu jäsen" }
            if let Some(e) = error() {
                div { class: "error", "{e}" }
            }
            div { class: "row",
                select {
                    onchange: move |e| {
                        if let Some(r) = Role::from_db(&e.value()) {
                            role.set(r);
                        }
                    },
                    option { value: "member", "Jäsen" }
                    option { value: "kid", "Lapsi" }
                    option { value: "admin", "Ylläpitäjä" }
                }
                button { class: "primary", disabled: busy(), onclick: generate,
                    if busy() { "Luodaan…" } else { "Luo kutsukoodi" }
                }
            }
            if let Some(c) = code() {
                div { class: "code",
                    "Jaa tämä koodi: "
                    strong { "{c}" }
                    p { class: "muted small", "Koodi toimii kerran ja vanhenee 7 päivässä." }
                }
            }
        }
    }
}
