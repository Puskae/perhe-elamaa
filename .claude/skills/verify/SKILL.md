---
name: verify
description: Build, launch, and drive Perhe-elämää locally to verify a change end-to-end (server API via curl; app bundle serving).
---

# Verifying Perhe-elämää locally

## Build + launch

```sh
cd crates/app && dx build --release        # WASM bundle (dx needs ~/.cargo/bin on PATH)
# from the REPO ROOT (bundle path is relative), throwaway DB in a scratch dir:
DATABASE_URL="sqlite:///path/to/scratch/verify.db?mode=rwc" \
  PERKELE_COOKIE_SECURE=false PERKELE_ADDR=127.0.0.1:8099 \
  PERKELE_SETUP_TOKEN=verify-setup-token-local \
  cargo run -p perkele-server > server.log 2>&1 &
# wait for /api/health, then drive
curl -s http://127.0.0.1:8099/api/health
```

`?mode=rwc` in DATABASE_URL makes sqlite create the file; migrations run on
startup automatically.

## Drive the API

First-run setup needs the setup token (fixed above via `PERKELE_SETUP_TOKEN`;
without it the server prints a random one in its log — `grep -a 'setup token'
server.log`). Wrong/missing token → 403. Setup gives a session cookie; keep it
in a curl cookie jar:

```sh
curl -s -c /tmp/pk.jar -X POST $B/api/setup -H 'content-type: application/json' \
  -d '{"setup_token":"verify-setup-token-local","family_name":"Testi","username":"mikko","display_name":"Mikko","password":"hunter2!"}'
curl -s -b /tmp/pk.jar $B/api/grocery/sync
```

All domain endpoints need `-b /tmp/pk.jar`. Auth endpoints (`/api/setup`,
`/api/auth/login`, `/api/auth/redeem`) are rate-limited per client IP (burst
10, then one per 2 s) — fine for manual driving. Login also backs off per
username after 5 wrong passwords (429 with `Retry-After`).

## Gotchas

- `GET /` should return 200 with ~1 KB of HTML; an empty 404 means the bundle
  wasn't built or the server wasn't started from the repo root.
- Server log is plain tracing output; grep it (`grep -a`, it has ANSI codes)
  to confirm `tracing::error!` paths (e.g. `client error:` from
  `/api/client-error`).
- Kill the background server when done; port stays bound otherwise.
