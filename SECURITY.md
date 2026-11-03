# Security Policy

## Reporting a vulnerability

Please **do not open a public issue** for security problems. Report them privately through
GitHub's [Report a vulnerability](https://github.com/Puskae/perhe-elamaa/security/advisories/new)
form (Security Advisories) on `github.com/Puskae/perhe-elamaa`.

Include what you found, how to reproduce it, and the version or commit you tested against.

## Supported versions

Only the latest `main` branch / most recent release is supported. Fixes are not backported.

## What to expect

Perhe-elämää is a hobby project maintained by one person. Reports are handled on a best-effort
basis, with no guaranteed response time, and there is no bug bounty.

## Security model

Perhe-elämää is meant to be self-hosted for a single family, reachable only over HTTPS — behind
Tailscale (`tailscale serve`) or a reverse proxy that terminates TLS. It is not designed to be
exposed directly to the internet over plain HTTP.

The session cookie is marked `Secure` by default. Setting `PERKELE_COOKIE_SECURE=false` is
intended only for local development over `http://127.0.0.1`; do not use it in a deployment.

The credential endpoints (setup, login, invite redemption) are rate-limited per client IP with
a global backstop, and login additionally backs off per username after repeated failures.
Behind a proxy, the client IP comes from a forwarded header that is only trusted from
`PERKELE_TRUSTED_PROXIES` — see [docs/deploy.md](docs/deploy.md#client-ip-and-rate-limiting).
First-run setup requires a one-time token printed to the server's stderr (or
`PERKELE_SETUP_TOKEN`); it is deliberately kept out of the `tracing` log so it can't reach
the optional error tracker (`PERKELE_SENTRY_DSN`) as a breadcrumb.

### Login backoff can be triggered by anyone who knows a username

The per-username backoff (5 wrong passwords, then 1 s, 2 s, 4 s, … up to 15 minutes between
attempts) is keyed **only on the username**, not on who is guessing. That is a deliberate
trade-off:

- It is what stops *distributed* password guessing: an attacker spread over many IP addresses
  still gets at most one guess per lockout period against a given account.
- The cost is a lockout denial of service: **anyone who knows a username can keep that account
  in backoff** by sending wrong passwords for it. While it lasts, even the correct password is
  refused with 429.
- **Existing sessions keep working.** Backoff only blocks new logins; devices that are already
  signed in are unaffected.

For an install reachable from the internet, give the admin account a **non-obvious username**
(not `admin`, not a first name), and keep at least one device logged in so you are never
dependent on being able to log in fresh.

## Known limitations (planned for 1.1)

Account management is minimal in 1.0. There is **no** in-app:

- password change,
- removal of a family member,
- role change,
- revocation of an unused invite code (they expire after 7 days),
- "sign out everywhere".

Until then an operator can cut off a member directly in the SQLite database. With the
container stopped (`docker compose stop`) and the `sqlite3` CLI pointed at the database in the
data volume (see the backup commands in [docs/deploy.md](docs/deploy.md#4-backups)), for
username `matti`:

```sql
-- Sign them out on every device:
DELETE FROM sessions WHERE user_id = (SELECT id FROM users WHERE username = 'matti');
-- Stop further logins: a value that is not a valid password hash never verifies.
UPDATE users SET pw_hash = '!disabled' WHERE username = 'matti';
-- Stop push notifications to their devices:
DELETE FROM push_subscriptions WHERE user_id = (SELECT id FROM users WHERE username = 'matti');
```

Deleting only the `sessions` rows signs a member out but does not stop them logging in again.
The `users` row itself is referenced by their content (authorship, audit log), so disable it
rather than deleting it.

Roles are coarse:

- **Admin only:** creating, editing and deleting chores; deleting grocery aisles
  (*Hyllyt*); pinning announcements; minting invite codes.
- **Author or admin:** editing or deleting recipes, calendar events, notes, announcements;
  deleting chat messages and individual grocery items. Deleting a grocery **section** is
  all-or-nothing: allowed only if the caller may delete *every* item in it.
- **Completer or admin:** undoing a chore completion.
- **Open to every family member (collaborative, on purpose):** adding grocery items, checking
  them off, renaming them / changing the quantity, clearing checked items, creating, renaming
  and reordering aisles; setting and clearing the meal plan; completing chores; ticking note
  checklists; adding events, recipes, notes, announcements and chat messages.
- The `kid` role currently has **the same rights as `member`**. Kid-specific restrictions are
  planned.
