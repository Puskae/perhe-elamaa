# Deploying Perhe-elämää

Perhe-elämää ships as a single Docker image: one server binary plus the prebuilt
WASM web app, with SQLite as the only storage. Run it on any always-on Linux
box you control and put HTTPS in front of it. The published image is
**linux/amd64 only**; on ARM64 (e.g. a Raspberry Pi 4/5 on a 64-bit OS) build it
locally with `docker compose up -d --build` (see below) — the Dockerfile builds
on both amd64 and arm64.

```
 browser / installed PWA  ──HTTPS──▶  tailscale serve or reverse proxy  ──HTTP──▶  perkele container
                                         (on <your-server>)                         (127.0.0.1:8080)
```

The container listens on plain HTTP. **You need HTTPS in front of it** — the
session cookie is `Secure` by default, and Web Push, the service worker and PWA
installation all require a secure origin. The recommended front door is
`tailscale serve` (private to your tailnet, real certificates, no open ports);
any reverse proxy with TLS works too.

## 1. Run the container

Copy [`compose.yaml`](../compose.yaml) to a directory on your server (or clone
the repo there), then either pull the published image:

```sh
docker compose pull          # ghcr.io/puskae/perhe-elamaa:latest
docker compose up -d
```

or build it from source (compiles the server and the WASM app — takes several
minutes the first time):

```sh
docker compose up -d --build
```

Building needs Docker with **BuildKit via the buildx plugin** (the Dockerfile
uses BuildKit features such as cache mounts and `TARGETARCH`). Docker only
uses BuildKit by default when that plugin is installed; without it,
the build falls back to the legacy builder and fails (`TARGETARCH is not set`).
Install `docker-buildx` on Arch or `docker-buildx-plugin` on Debian/Ubuntu
(from Docker's apt repo), and check with `docker buildx version`. Pulling the
published image doesn't need it.

Both use the same `image:` ref, so they are interchangeable: a local build
replaces the pulled tag, and the next `docker compose pull` puts the released
image back. Release images are tagged `latest`, `X.Y.Z` and `X.Y`; pin a
version in `compose.yaml` if you'd rather upgrade deliberately.

Check it:

```sh
docker compose ps                              # "healthy" after ~30 s (first check)
curl -fsS http://127.0.0.1:8080/api/health     # {"service":"perkele","version":"..."}
docker compose logs -f
```

The port is published on `127.0.0.1` only, so nothing outside the host can reach
it until you add the HTTPS layer below.

### Persistent data

The SQLite database lives in the `perkele-data` named volume, mounted at
`/app/data` (`DATABASE_URL=sqlite:///app/data/perkele.db`, set in the
Dockerfile). The database runs in WAL mode, so the volume also holds
`perkele.db-wal` and `perkele.db-shm` — keep them together. The volume survives
container recreation, image upgrades and rebuilds; `docker compose down -v`
deletes it.

Web Push (VAPID) keys are generated on first use and stored in the database, so
there are no key files to manage — but restoring an old backup restores the
old keys too, which is what you want.

### Environment

Set these in a `.env` file next to `compose.yaml`. Compose only passes through
variables that `compose.yaml` lists under `environment:`, so to set anything
else, add it there as well.

| Variable | Default in the image | Notes |
|---|---|---|
| `PERKELE_COOKIE_SECURE` | `true` | Leave `true` behind HTTPS. Set `false` **only** for plain-http testing (see below). |
| `TZ` | `Europe/Helsinki` | **Set this to your household's timezone.** Event and chore times are stored as local wall-clock time and the reminder scheduler compares them to the container's local time; a wrong zone makes reminders fire off by the UTC offset. The startup log prints the resolved offset. |
| `PERKELE_SENTRY_DSN` | unset (disabled) | Optional error reporting, see [below](#optional-error-reporting). |
| `PERKELE_AUDIT_RETENTION_DAYS` | `90` | Days of change history kept by the daily prune. Not listed in `compose.yaml` by default. |
| `PERKELE_CLIENT_IP_SOURCE` | `peer` (`compose.yaml`: `x-forwarded-for`) | Where the rate limiter reads the client's IP; see [Client IP and rate limiting](#client-ip-and-rate-limiting). |
| `PERKELE_TRUSTED_PROXIES` | `127.0.0.1/32,::1/128,172.16.0.0/12` | Proxies whose forwarded-IP header is believed. |
| `PERKELE_SETUP_TOKEN` | unset (random token in the log) | Fixed first-run setup token (≥ 12 letters/digits); see [First run](#3-first-run). |
| `DATABASE_URL` | `sqlite:///app/data/perkele.db` | Keep it inside `/app/data` or the data won't be in the volume. |
| `PERKELE_ADDR` | `0.0.0.0:8080` | Listen address inside the container. |
| `PERKELE_DIST` | `/app/public` | Where the built web app lives inside the image. |
| `RUST_LOG` | `info,tower_http=debug` | Standard `tracing` filter. |

### Testing over plain http first

Over plain http the browser may drop the `Secure` session cookie, so login
appears to succeed and every following request returns `unauthenticated`.
Chrome and Firefox treat `http://localhost` as a secure context and keep the
cookie there, but Safari does not — and every browser drops it when you reach
the server by IP address or hostname over http. For a quick local test only:

```sh
PERKELE_COOKIE_SECURE=false docker compose up -d
```

Once HTTPS is in place, recreate without the override (`docker compose up -d`).
Push notifications and PWA install won't work over plain http regardless.

## 2. HTTPS

### Recommended: `tailscale serve` (private)

With [Tailscale](https://tailscale.com) installed on the server and on every
family device, nothing is exposed to the public internet.

One-time, in the Tailscale admin console under **DNS**: enable **MagicDNS** and
**HTTPS Certificates**. Then on the server:

```sh
tailscale serve --bg 8080
```

That proxies `https://<machine>.<tailnet>.ts.net/` to `http://127.0.0.1:8080`
with a valid certificate. Useful commands:

```sh
tailscale serve status          # what is being served
tailscale serve --bg 8080 off   # stop serving
```

### Alternative: a reverse proxy

Any TLS-terminating proxy works. A minimal [Caddy](https://caddyserver.com)
`Caddyfile`, which obtains a certificate automatically:

```
perhe.your-domain.example {
    reverse_proxy 127.0.0.1:8080
}
```

The app uses Server-Sent Events for live updates; Caddy streams them without
extra configuration. With other proxies, turn response buffering off for
`/api/` (e.g. `proxy_buffering off;` in nginx) and keep idle timeouts generous.
The streaming endpoints are `/api/grocery/events`, `/api/calendar/stream`,
`/api/chat/stream` and `/api/announcements/stream`.

If you expose Perhe-elämää to the public internet rather than a private network, do
the first-run setup (next step) **before** opening it up, keep the host
patched, and consider putting an access layer in front of it.

### HSTS

When `PERKELE_COOKIE_SECURE=true` (the default) the server also sends
`Strict-Transport-Security: max-age=31536000`, so a browser that has reached
it over HTTPS refuses plain http for that host for a year. That is what you
want behind HTTPS; it is one more reason to set `PERKELE_COOKIE_SECURE=false`
for plain-http testing, where no HSTS header is sent.

### Client IP and rate limiting

The credential endpoints (`/api/setup`, `/api/auth/login`, `/api/auth/redeem`)
are rate-limited per client IP — a burst of 10 requests, then one more every
2 seconds — with a global backstop across all clients (burst of 100, then one
every 200 ms). IPv6 clients are bucketed by their **/64** prefix (one host
typically owns a whole /64); IPv4 addresses are used as-is. Separately, login
backs off per *username*: after 5 wrong passwords in a row each further attempt
must wait 1 s, 2 s, 4 s, … up to 15 minutes (HTTP 429 with `Retry-After`); a
correct password resets it. Concurrent attempts can't dodge this: only as many
password checks as there are attempts left may be in flight for one username at
once, the rest get 429 straight away.

**Lockout trade-off.** The username backoff is keyed on the username alone, so
**anyone who knows a username can keep that account in backoff** (5 attempts,
then waits of up to 15 minutes) and stop it logging in fresh. Existing sessions
keep working — backoff only blocks new logins. This is deliberate: the
alternative, keying on username *and* client IP, would let an attacker spread
over many addresses guess one account's password without slowing down. On an
internet-exposed deploy, give the admin account a **non-obvious username** and
keep at least one device logged in. See [SECURITY.md](../SECURITY.md).

Behind a proxy every request arrives from the proxy's address, so the limiter
must read the real client IP from a header the proxy adds — otherwise the whole
household shares one bucket. Headers are only believed when the connection comes
from an address in `PERKELE_TRUSTED_PROXIES`; from anyone else they are ignored.
Pick `PERKELE_CLIENT_IP_SOURCE` for your setup:

| Front door | `PERKELE_CLIENT_IP_SOURCE` | `PERKELE_TRUSTED_PROXIES` |
|---|---|---|
| `tailscale serve` | `x-forwarded-for` (the `compose.yaml` default) | default |
| Caddy / nginx on the same host | `x-forwarded-for` | default, or add the proxy's IP |
| Caddy / nginx in another container or on another machine | `x-forwarded-for` | add the proxy's IP |
| Cloudflare tunnel (`cloudflared`) | `cf-connecting-ip` | default, or add `cloudflared`'s IP |
| Clients connect directly, no proxy | `peer` | not used |

- `tailscale serve` sets `X-Forwarded-For` to the tailnet client's IP
  (replacing any value the client sent).
- `x-forwarded-for` uses the **rightmost** entry: each proxy appends the address
  it saw, so the last one was written by your proxy and everything left of it
  could be forged by the client. Caddy's `reverse_proxy` does this by default;
  in nginx use `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`
  (or `$remote_addr`).
- **Inside Docker**, a proxy running on the host (tailscale serve, host Caddy,
  host `cloudflared`) reaches the container through the published port, so the
  container sees the Docker network's **gateway** address, not `127.0.0.1`. The
  default `172.16.0.0/12` covers Docker's default address pool; if your Docker
  networks use another range (`docker network inspect <project>_default`), add
  it to `PERKELE_TRUSTED_PROXIES`.
- Setting it wrong fails safe but blunt: with `peer` behind a proxy everyone
  shares one bucket; with a header source but the proxy not trusted, the header
  is ignored and the proxy's address is used.

An unknown `PERKELE_CLIENT_IP_SOURCE` or a malformed `PERKELE_TRUSTED_PROXIES`
entry stops the server at startup with a message saying which.

## 3. First run

Open the URL. On an empty database the app shows a setup screen that creates the
first family and its admin account. Setup is refused once any family exists, so
it can't be re-run to take over an install.

Setup also asks for a **setup token** (*Asennustunnus*), so that a stranger who
reaches a fresh install first can't claim it. While no family exists the server
prints a random token at startup (to stderr, which `docker compose logs` shows;
it is kept out of the structured log so it never reaches error tracking):

```sh
docker compose logs perkele | grep -a "setup token"
```

A new token is generated on every restart until setup is done. To choose it
yourself (e.g. for automation), set `PERKELE_SETUP_TOKEN` in `.env` — at least
12 letters/digits; case, dashes and spaces are ignored when comparing. A wrong
or missing token is refused with 403.

The admin then invites the rest of the household with invite codes from the
app. On phones, use "Add to Home Screen" to install the PWA; on iOS, push
notifications only work for the installed app (iOS 16.4+).

## 4. Backups

Everything is in the one SQLite database. Because it runs in WAL mode, copying
just `perkele.db` from a running container can miss recent writes. Either:

**Stop, copy, start** (simplest):

```sh
docker compose stop
docker run --rm -v perhe-elamaa_perkele-data:/data -v "$PWD":/backup busybox \
    tar czf /backup/perkele-$(date +%F).tgz -C /data .
docker compose start
```

**Or take a hot backup** with SQLite's online backup API, without stopping:

```sh
docker run --rm -v perhe-elamaa_perkele-data:/data -v "$PWD":/backup alpine \
    sh -c 'apk add --no-cache sqlite >/dev/null && \
           sqlite3 /data/perkele.db ".backup /backup/perkele-$(date +%F).db"'
```

The volume name is `<compose project>_perkele-data`, and the compose project
name defaults to the directory `compose.yaml` sits in — so a checkout cloned as
`perhe-elamaa/` uses `perhe-elamaa_perkele-data`, as above. Installs cloned
before the rename (as `perkele/`) have `perkele_perkele-data`: substitute that,
and keep the directory — re-cloning or moving `compose.yaml` under a new
directory name starts the app on a new, empty volume. `docker volume ls | grep
perkele-data` shows the real name.

To restore, stop the container, replace the contents of the volume with the
backup (as `perkele.db`, with no stale `-wal`/`-shm` files next to it), and
start it again. Test a restore at least once.

## 5. Upgrading

```sh
docker compose pull
docker compose up -d
```

Database migrations are embedded in the server binary and run automatically at
startup, before the server starts listening — there is no separate migration
step. Take a backup before upgrading; migrations only move forward, so rolling
back to an older image after a schema change requires restoring that backup.

To run your own modified build instead, `docker compose up -d --build` from a
checkout.

The server sends `Cache-Control: no-cache` for the HTML shell and long-lived
`immutable` caching for the content-hashed JS/WASM, so browsers pick up a new
version on the next load. An installed PWA may need to be closed and reopened
once.

## Optional: error reporting

Set `PERKELE_SENTRY_DSN` to a DSN from any Sentry-compatible server (Sentry,
[GlitchTip](https://glitchtip.com), …) to report server errors and browser-side
panics. Use the full DSN from the project's settings, not the project page URL:

```sh
# .env next to compose.yaml
PERKELE_SENTRY_DSN=https://<key>@sentry.your-domain.example/<project-id>
```

Recreate to apply (`docker compose up -d`). Unset or empty disables reporting;
an invalid DSN is logged and ignored. Server `error!` log lines become events,
and the web app forwards its own panics through `POST /api/client-error`.

To smoke-test, log in, open the browser devtools console and run:

```js
fetch('/api/client-error', {
  method: 'POST',
  body: JSON.stringify({ message: 'error reporting smoke test', url: location.href }),
}).then(r => console.log(r.status))   // expect 204
```

The event should show up within seconds, and the server log has the same line
(`docker compose logs | grep "client error"`).
