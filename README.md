# Perhe-elämää

▎ Perhe-elämää. Because in every family there comes a moment when perkele is the only word left.

A self-hosted family coordination app: shared calendar, grocery list, recipes,
meal planner, chores, reminders, and a family announcements board.

Built in Rust end-to-end: [Axum](https://github.com/tokio-rs/axum) backend,
[Dioxus](https://dioxuslabs.com) frontend (web PWA first, native apps later),
SQLite storage, offline-capable sync.

The project and app are called **Perhe-elämää**; the repo is `perhe-elamaa`.
The UI is in Finnish. Internal names — the crates and server binary
(`perkele-server` etc.), the `PERKELE_*` environment variables and the
`perkele-data` compose volume — keep the project's historical `perkele` name,
so existing configs and databases keep working.

**Status:** The core feature set is complete:

- accounts, families, roles (admin / member / kid) and invite codes
- a grocery list with live sync between devices, offline support (PWA) and aisle sorting
- recipes with a servings scaler and "add to grocery list"
- a dinner meal planner
- a shared calendar with recurrence and Web Push reminders
- chores with rotation, reminders and gamified stats
- an announcements board, a family group chat and shared notes

Everything is family-scoped behind server-side auth. The architecture and
original build plan are in [docs/design.md](docs/design.md).

> **The UI is Finnish-only for now.** A translation layer is planned; until
> then every screen, message and notification is in Finnish.

## Workspace layout

```
crates/
  shared/   # domain types + DTOs + validation, used by server AND app
  server/   # Axum API, serves the WASM bundle, owns the database
  app/      # Dioxus frontend (web target first)
```

## Setting up on a new computer

```sh
# 1. Install Rust (rustup picks up rust-toolchain.toml automatically,
#    including the wasm32 target, rustfmt and clippy)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# restart your terminal, or: source "$HOME/.cargo/env"

# 2. Install the Dioxus CLI (binstall downloads a prebuilt binary = fast)
cargo install cargo-binstall
cargo binstall dioxus-cli -y

# 3. Clone and build
git clone https://github.com/Puskae/perhe-elamaa.git && cd perhe-elamaa
cargo build --workspace
```

## Essential commands

```sh
# --- daily development (two terminals) ---
PERKELE_COOKIE_SECURE=false cargo run -p perkele-server
                                   # terminal 1: API on http://127.0.0.1:8080
                                   # (plain http, so the cookie can't be Secure)
cd crates/app && dx serve --port 8081
                                   # terminal 2: frontend with hot reload on
                                   # http://127.0.0.1:8081, proxies /api to
                                   # :8080 (see Dioxus.toml)

# --- checks (run before committing; CI runs the same) ---
cargo fmt --all                    # format code
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace             # run all tests

# --- production-style build ---
cd crates/app && dx build --release   # WASM bundle -> target/dx/perkele-app/release/web/public
cargo run --release -p perkele-server # serves API + the built bundle on :8080

# --- useful cargo basics ---
cargo check --workspace            # fast compile check, no binary
cargo add <crate> -p perkele-server   # add a dependency to one crate
cargo update                       # update Cargo.lock within semver ranges
```

### Configuration

The server is configured with environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `DATABASE_URL` | `sqlite://perkele.db` | SQLite database; created and migrated on startup. |
| `PERKELE_ADDR` | `127.0.0.1:8080` | Listen address. |
| `PERKELE_DIST` | `target/dx/perkele-app/release/web/public` | Directory of the built web app to serve. |
| `PERKELE_COOKIE_SECURE` | `true` | Mark the session cookie `Secure`. Set `false` for local plain-http dev, or login silently fails. |
| `PERKELE_SENTRY_DSN` | unset (disabled) | Optional error reporting to any Sentry-compatible server (e.g. GlitchTip). |
| `PERKELE_AUDIT_RETENTION_DAYS` | `90` | Days of change history kept; invalid or non-positive values fall back to 90. |
| `PERKELE_CLIENT_IP_SOURCE` | `peer` | Where the login rate limiter gets the client's IP: `peer` (TCP peer address), `x-forwarded-for` (rightmost entry) or `cf-connecting-ip`. Headers are only believed from `PERKELE_TRUSTED_PROXIES`. An invalid value stops the server at startup. |
| `PERKELE_TRUSTED_PROXIES` | `127.0.0.1/32,::1/128,172.16.0.0/12` | Comma-separated IPs/CIDRs of proxies whose forwarded-IP header is trusted (loopback plus Docker's default bridge range). |
| `PERKELE_SETUP_TOKEN` | unset (random, printed to stderr) | Token required by first-run setup. Unset: a random one is generated and printed to stderr (not the tracing log) at every start until setup is done. If set: at least 12 letters/digits. |

Reminders compare stored wall-clock times against the server's local time, so
run the server in your household's timezone (`TZ`). The Docker image sets its
own defaults for some of these — see [docs/deploy.md](docs/deploy.md).

> The server serves the prebuilt WASM bundle from `PERKELE_DIST`. If that
> bundle is missing, page requests return an empty 404 (which Safari downloads
> as a 0-byte file) — so build the frontend (`dx build`) and run the server from
> the repo root. The server logs a clear warning at startup if the bundle isn't
> found.

## Deployment

Single Docker image (server binary + built WASM app), published as
`ghcr.io/puskae/perhe-elamaa` for **linux/amd64 only**. On ARM64 (e.g. a Raspberry
Pi 4/5 on a 64-bit OS) build it locally instead with `docker compose up -d --build`
— the Dockerfile builds on amd64 and arm64 and needs BuildKit via the buildx plugin
(Arch `docker-buildx`, Debian/Ubuntu `docker-buildx-plugin`; check with `docker buildx version`).
The recommended setup runs it on a home server and
reaches it over [Tailscale](https://tailscale.com) via `tailscale serve`
(automatic HTTPS, private to your tailnet):

```sh
docker compose up -d          # uses ghcr.io/puskae/perhe-elamaa:latest
tailscale serve --bg 8080     # → https://<machine>.<tailnet>.ts.net
```

HTTPS is required in production (secure cookies, Web Push, PWA install); any
reverse proxy works as well. Full walkthrough — HTTPS options, first-run setup,
environment, backups, upgrades and error reporting — in
[docs/deploy.md](docs/deploy.md).

### Releases

Versions follow [semantic versioning](https://semver.org). A release is a git
tag `vX.Y.Z`; each one publishes the container image as
`ghcr.io/puskae/perhe-elamaa:X.Y.Z`, `:X.Y` and `:latest`. Pin `X.Y.Z` (or `X.Y`)
in `compose.yaml` if you'd rather upgrade deliberately. 1.0.0 is the
first public release.

Images up to 1.0.4 were published as `ghcr.io/puskae/perkele`; from 1.0.5 on
they are `ghcr.io/puskae/perhe-elamaa`. Existing installs switch the `image:`
line in their `compose.yaml` when upgrading.

## Security

Please report vulnerabilities privately as described in
[SECURITY.md](SECURITY.md) rather than in a public issue.

## License

Copyright (C) 2026 Jani Puska. Licensed under the GNU AGPL v3.0 or later — see
[LICENSE](LICENSE).

In plain English: you can use, modify and self-host Perhe-elämää freely, but if you
run a modified version as a network service for others, you must offer those
users the source code of your modified version.
