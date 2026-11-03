# Perhe-elämää — Family App: Architecture & Build Plan

> The original design and phase plan, kept for history. Some parts were not built as described here (e.g. `Authorization: Bearer` token auth, Litestream backups, a `GET /sync?since=` delta endpoint) — see the [README](../README.md) for the current state.

## Context

Perhe-elämää is a hobby (no-deadline) family coordination app built to learn Rust. Core features: shared calendar, grocery list integrated with recipes, meal planner (the glue: recipes → week plan → grocery list → calendar), reminders/notifications, chores, and a family announcements board. Essential features only — no fluff.

Users: one family at first, each member with their own account (parent = admin, kids = limited). Devices: Android, iPhone, iPad, Mac, PC → **web-first PWA**, dedicated native apps later. Runs **self-hosted at home behind Tailscale** initially; the architecture must allow a later pivot to a public SaaS (domain + Cloudflare Tunnel or VPS, token-based API) **without a rewrite** — so multi-tenancy and proper token auth are designed in from day one.

Priorities stated by the user: **good, secure, and FAST**.

## Stack decision (with rationale)

**Full Rust, Dioxus frontend** (user's explicit choice — they want to learn Rust):

- **Cargo workspace** with three crates:
  - `crates/shared` — domain types, DTOs, validation logic. Compiled into both server and client. This is the payoff of full-Rust: one set of types, no API drift.
  - `crates/server` — Axum API + static file serving (one binary serves everything).
  - `crates/app` — Dioxus app, **web (WASM) target first**; Dioxus's desktop/mobile renderers cover the "dedicated apps later" goal from the same codebase.
- **Database: SQLite via `sqlx`** (compile-time checked queries). Perfect for a single home-server box: zero admin, extremely fast, trivially backed up. Write portable SQL; if SaaS happens, migrate to Postgres (sqlx supports both). **Litestream** for continuous backup to NAS/object storage.
- **Auth: opaque session tokens** (random 256-bit, hashed in DB) — revocable, simple, no JWT footguns. Delivered as httpOnly `SameSite=Strict` cookie for the web app; the same middleware also accepts `Authorization: Bearer` for future native apps / public API. Passwords hashed with **argon2id**.
- **Multi-tenancy from day one**: `family` table; every domain row carries `family_id`; every query is scoped by the family from the authenticated session. Roles: `admin`, `member`, `kid`.
- **Offline sync (deliberately NOT CRDTs)**: server-authoritative with a per-family **sync log**.
  - Every mutation appends to `sync_log` (family_id, monotonically increasing `seq`, entity type, entity id, op).
  - Client stores data locally (IndexedDB via `web-sys`/`idb` bindings), tracks `last_seen_seq`, pulls deltas with `GET /sync?since=<seq>`.
  - Offline writes go into a local **mutation queue**, replayed on reconnect. Conflict policy: last-write-wins per row; deletes via tombstones. Grocery items / chores / events are independent rows, so real conflicts are rare for a family. This is ~10% of CRDT complexity and covers ~99% of the need.
- **Live updates: SSE** (Server-Sent Events), not WebSockets. Server pushes "new seq available" pokes; clients then pull the delta through the same sync path. One code path for live + offline catch-up. Simpler to secure and proxy.
- **Notifications: Web Push (VAPID)** via the `web-push` crate — works on Android, desktop, and iOS ≥16.4 for installed PWAs. A tokio background task scans upcoming reminders/chores and sends pushes.
- **Recurrence**: store RFC 5545 RRULE strings (use the `rrule` crate), expand occurrences server-side for both calendar events and chores — one recurrence engine for both features.
- **PWA shell**: manifest + a small handwritten service worker (cache app shell, offline fallback). Dioxus doesn't generate this; it's ~100 lines of JS, written once in Phase 3.
- **Deployment**: single Docker image (server binary + WASM assets), `docker compose` on the home server, exposed via `tailscale serve` (automatic valid HTTPS, app never touches the public internet). GitHub Actions CI: check, clippy, test, build.

### Alternatives considered (rejected)

- *Leptos instead of Dioxus*: similar maturity; Dioxus chosen for its native desktop/mobile renderers matching the roadmap.
- *CRDT sync (automerge etc.)*: overkill for row-independent family data; large complexity and binary-size cost.
- *Postgres now*: adds a second container and admin burden for zero benefit at family scale.
- *JWT auth*: not revocable without a denylist (which is just sessions with extra steps).

## Security checklist (build it in, don't bolt it on)

- argon2id password hashing; rate limiting on auth endpoints (`tower_governor`); generic login errors.
- All queries through sqlx parameterized macros (no string SQL).
- Validation lives in `crates/shared`, enforced server-side (client-side is UX only).
- httpOnly + Secure + SameSite=Strict cookies; CSRF not an issue with SameSite=Strict + custom-header check on mutations.
- Security headers middleware: strict CSP (WASM needs `wasm-unsafe-eval`), X-Content-Type-Options, frame-ancestors none.
- Role checks server-side on every route (kid accounts: no member management, no deleting others' data).
  *As implemented in 1.0:* one rule for everyone — deleting or overwriting shared content is limited to its author or an admin; collaborative actions (adding, checking, completing) are open to all members. The `kid` role currently has exactly the same rights as `member`; kid-specific limits are future work.
- `cargo audit` + `cargo deny` in CI.
- Tailscale = no public exposure during the entire build phase; the public-facing hardening (Cloudflare Access, stricter rate limits) is a Phase-8 concern.

## Data model (core tables)

```
family(id, name, created_at)
user(id, family_id, name, email, pw_hash, role, created_at)
session(id, user_id, token_hash, expires_at, created_at)
push_subscription(id, user_id, endpoint, keys_json)

event(id, family_id, title, starts_at, ends_at, all_day, rrule, location, notes, created_by)
event_attendee(event_id, user_id)
reminder(id, family_id, event_id|chore_id, remind_at_offset_min, target_user_id)

recipe(id, family_id, title, instructions, servings, tags, prep_minutes)
recipe_ingredient(id, recipe_id, name, qty, unit, category)

grocery_item(id, family_id, name, qty, unit, category, checked, added_by, recipe_id?, created_at)
meal_plan_entry(id, family_id, date, slot, recipe_id?, free_text?)

chore(id, family_id, title, rrule, assigned_user_id?, rotation_json?)
chore_completion(id, chore_id, completed_by, completed_on)

announcement(id, family_id, body, pinned, created_by, created_at)

sync_log(family_id, seq, entity, entity_id, op, updated_at)  -- PK (family_id, seq)
```

Notes: one implicit active grocery list per family (checked items = history; "clear checked" action). Soft-delete (tombstone) columns on synced entities. All timestamps UTC; client renders local.

## Build phases

Each phase ends with something usable. TDD throughout (backend logic especially — sync, recurrence, auth). Commit the design doc as `docs/design.md` in step 0.

### Phase 0 — Skeleton & Rust ramp-up
- `git init`, Cargo workspace, the three crates.
- Axum hello-world serving a Dioxus hello-world WASM bundle; one shared type proving the round trip.
- Dockerfile (multi-stage: build WASM with `dioxus-cli`, build server, slim runtime image), compose file.
- `tailscale serve` setup on the home server; app reachable on all family devices over HTTPS.
- GitHub Actions: fmt, clippy, test, cargo audit.
- **Learning focus**: ownership/borrowing, modules, error handling (`thiserror`/`anyhow`), async basics.

### Phase 1 — Foundation: DB + auth + family
- sqlx + migrations (`sqlx migrate`); family/user/session tables.
- Family bootstrap (first user creates family, becomes admin), invite-code flow for adding members, login/logout, session middleware, role extractor.
- Rate limiting + security headers middleware.
- Minimal UI: login page, family member list, admin can add members.

### Phase 2 — Grocery list (online-only)
- Simplest domain, highest daily value — and the test bed for all patterns: CRUD handlers, shared DTOs, Dioxus state management, sync_log written on every mutation.
- Items: add (with smart category), check/uncheck, qty, "clear checked".
- SSE channel: live updates between two phones while shopping.

### Phase 3 — Offline PWA layer
- Manifest + service worker (app shell caching, installability on iOS/Android).
- IndexedDB local store; `GET /sync?since=seq` delta pull; offline mutation queue + replay; tombstones.
- Validate entirely against the grocery list: airplane-mode shopping test is the acceptance criterion.

### Phase 4 — Recipes + meal planner
- Recipe CRUD with structured ingredients.
- Week view meal planner; assign recipe (or free text) to day/slot.
- **The glue**: "add week to grocery list" → ingredients flow in, grouped by category, deduplicated by name+unit.

### Phase 5 — Calendar + reminders + push
- Month/week/agenda views; events with attendees; RRULE recurrence via `rrule` crate.
- Web Push subscriptions; background reminder scheduler task.
- Meal plan entries surface on the calendar (read-only projection).

### Phase 6 — Chores + announcements
- Chores reuse the recurrence engine; optional rotation between members; done-tracking; kid-friendly view.
- Announcements: single pinned "fridge door" board.
- Chore reminders ride the existing push pipeline.

### Phase 7 — Hardening & polish
- Litestream backups + tested restore procedure.
- `cargo deny` policy, dependency review, load sanity check (it's Rust — it'll be fine).
- UX pass: keyboard-fast grocery entry, dark mode, Finnish/English i18n if wanted.

### Phase 8 (optional, later) — Going public
- Postgres migration, public signup (multi-family already works), Cloudflare Tunnel or VPS, Cloudflare Access in front during beta, stricter rate limits, ToS/privacy. The architecture above means this phase touches hosting and onboarding — not the core.

## Verification

- **Per phase**: `cargo test` (unit + handler tests with an in-memory SQLite), `cargo clippy -- -D warnings`, manual check in browser via `dx serve`.
- **Sync correctness** (Phase 3): scripted test — two simulated clients, interleaved offline mutations, assert convergence; plus the real-world airplane-mode grocery test.
- **Security**: auth tests (role escalation attempts, cross-family data access attempts must 404/403), `cargo audit` in CI.
- **End-to-end**: install PWA on one iPhone + one Android over Tailscale; live-update test with two devices on the grocery list.

## First concrete steps (when execution starts)

1. `git init`, write `docs/design.md` (this design), initial commit.
2. Scaffold the Cargo workspace + three crates; install `dioxus-cli`; hello-world round trip.
3. Set up CI and Dockerfile before any features — keeps every later phase deployable.
