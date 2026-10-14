# Local Development

Running the whole system on a laptop, and how to test money flows without touching
real money.

> Deployment: [`docs/deployment.md`](deployment.md). This document is for the
> development loop.

> **Superseded: identity is Rust-owned.** The Phase 6 identity port has landed —
> `accounts.pb_user_id` is dropped, `POST /auth/exchange` is deleted, the
> PocketBase HTTP client is gone from `server/`, and identity is served natively
> by this crate (`accounts` + `identities`, Argon2id). Where the text below still
> says PocketBase is the current identity provider, this notice governs;
> [`architecture/identity.md`](architecture/identity.md) is the operative
> description.

## What must run locally

| Component | Required? | Note |
| --- | --- | --- |
| Rust API + proxy | **Yes** | The thing under development |
| SQLite | **No server** | A file at `data/server.db`, created by the migrate binary |
| Mail relay (`[email]` in `config/apikita.toml`) | **No** | Identity's one external dependency. `smtp_host = ""` is a supported state: signup still works, the account is just unverified. See [`architecture/identity.md`](architecture/identity.md) and `server/src/identity/email.rs` |
| Frontend dev server | Only for UI work | Pages Functions are not used |
| Midtrans | **No** | Faked locally — see below |
| Upstream providers | **No** | Faked locally |

There is no database container and no `5432`. The port table below has no row for
one because there is nothing to listen — SQLite is a library, and the API opens the
file directly.

## Ports

| Service | Port |
| --- | --- |
| Rust API | 8080 |
| Nginx edge relay | 8000 |
| Frontend dev server | 4321 (Astro default) — **UI work only**, see below |

**Cookie domains are the trap.** Use `localhost` for everything and make the API
reachable at the same origin the frontend uses, or session cookies will be set for
a domain the browser will not send them to. This is the local version of the
production subdomain decision in [`docs/architecture.md`](architecture.md).

### One origin: the relay serves the site *and* the API

`http://localhost:8000` **is** that one origin locally. The relay serves the built
site from `website/dist` (bind-mounted read-only into the container — see
`docker-compose.yml`) and proxies the API slice to the Rust server on the host:

| Path | Where it goes |
| --- | --- |
| `/events` | backend — SSE, **unbuffered** |
| `/v1/` | backend — LLM streaming, **unbuffered** |
| `/auth/`, `/api/`, `/webhooks/`, `= /health` | backend — buffered |
| `= /healthz` | the relay itself (compose healthcheck) |
| everything else | the static file under `website/dist`, else **404** |

That last row is deliberate. This build is **multi-page** (`output: 'static'`), so an
unknown path is a 404 and never a silent `index.html`. A catch-all fallback turns
every typo and every deleted page into a 200 of the landing page — it hides exactly
the 404s you need to see while building the dashboard.

Nothing else about the relay changed: same rate-limit zones, same unbuffered
`/events` and `/v1/`, same `access_log off`. What differs from production is **which
hostname serves the site**:

| | Site | API | Cookie question |
| --- | --- | --- | --- |
| Production | Cloudflare Pages | Northflank | **subdomain** decision — [`architecture.md`](architecture.md) |
| Local | the relay, `:8000` | the relay, `:8000` | same origin — already settled |

So local development resolves the trap **by construction** and deliberately does
*not* answer the production subdomain question. One origin locally because it has
to be, not because production will be.

## First run

```
# 1. the relay. There is no database container any more — the database is a file
#    (step 2). NAME THE SERVICE: `docker compose up -d` with no service starts
#    EVERY service in the file, which since the maintenance scheduler was added
#    means `scheduler` as well. That is a nightly container a front-end session
#    does not want, and it prints a wall of WIRED/NOT WIRED banners before
#    settling into an 80000-second sleep.
docker compose up -d nginx

# 2. database — a file, created and migrated by the migrate binary
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate

# 3. api, on the HOST. The relay reaches it as host.docker.internal:8080, which
#    is why it is not a compose service.
cp .env.example .env    # fill in what you need; fakes need nothing
cargo run --bin apikita-server

# 4. the site, built for ONE ORIGIN
#    PUBLIC_API_BASE_URL= is not cosmetic: it is a PUBLIC_* variable INLINED
#    into the JS at build time. Empty makes every call relative, so the bundle
#    calls whatever origin served it (:8000). Leave it unset and the bundle
#    hardcodes http://localhost:8080 — a different origin from the relay, and
#    the session cookie will never be sent. That is the whole trap.
cd website && PUBLIC_API_BASE_URL= npm run build

# 5. frontend (only for UI work) — do NOT use it to judge the dashboard
cd website && npm run dev
```

There is no identity service to start: the Rust server owns `accounts` and
`identities` in the same SQLite file, and verification and reset mail goes out
through the `[email]` relay in `config/apikita.toml` — see
[`architecture/identity.md`](architecture/identity.md).

Then open **<http://localhost:8000>** — site and API, one origin.

> **Rebuild after every frontend change.** The container serves `website/dist`
> from the host, so an edit to `website/src` is not visible until you re-run the
> build above. No `docker compose` command is needed: the directory is mounted,
> not copied, which is exactly why a rebuild can never be silently forgotten in
> an image layer — you either see the new files or you see 404.

> **`npm run dev` is not this flow.** It serves the site on :4321 and is useful
> only for fast UI iteration on a page that touches no API. It is *cross-origin*
> from both the relay and the API, so nothing cookie-authenticated works in it —
> never use it to judge whether the dashboard works end to end.

**The migration is applied by `bin/migrate.rs`, never on server boot** — the deploy
order is *migrate → server → health → frontend*, and the server deliberately opens
SQLite with `create_if_missing: false` so a missing database is a loud "you have not
migrated" rather than an empty, schema-less file. The schema lives in
`server/migrations/`; there is no `schema.sql` to apply by hand.

## Resetting the local database

Because the database is a file, a reset is a file operation, not a volume one:

```sh
rm -f data/server.db data/server.db-wal data/server.db-shm
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
```

Delete all three. `-wal` and `-shm` are the write-ahead log and its shared-memory
index; removing only `server.db` leaves a write-ahead log behind with no database to
replay into, and the next `migrate` produces a file whose contents are not what the
log was written against.

`docker compose down -v` does **not** touch any of this — there is no database
volume to remove any more.

## Tests need no database set up

`cargo test` builds its own migrated SQLite file in a temp directory, per test
(`server/src/test_support.rs`). `DATABASE_URL` is not read by the suite, so a
forgotten env var cannot make tests pass against the wrong database. The money
tests — including the real-concurrency overdraw proof — run by default; nothing
is `#[ignore]`d.

To check the ledger invariant against a database you have been hammering on:

```sh
DATABASE_URL=sqlite://data/server.db sh tools/reconcile/reconcile.sh
```

Non-zero exit means drift. See [`tools/reconcile/README.md`](../tools/reconcile/README.md).

## Fakes — the part that makes this workable

**Never develop against real money or real providers.** Two fakes:

### 1. Fake upstream provider

A tiny local HTTP server that speaks the OpenAI-compatible shape and streams a
canned response with a usage block.

| Behaviour | Controlled by |
| --- | --- |
| Normal stream | default |
| Slow first token | query flag |
| **Fail mid-stream** | query flag — tests the case that matters |
| 429 | query flag |
| 500 | query flag |
| Usage numbers | fixed, so billing is deterministic |

**A fake that can fail mid-stream is the single most useful test tool in this
project.** It exercises the failover path that no happy-path test reaches.

### 2. Fake Midtrans

A local endpoint that emits webhook payloads with **correctly computed signatures**
against a known dev server key.

| Scenario | Payload |
| --- | --- |
| Settlement | `transaction_status: settlement` |
| **Wrong signature** | must be rejected — test this first |
| **Wrong amount** | must be rejected |
| **Replay** | same `order_id` twice — must credit once |
| Refund | must debit |

**The replay and wrong-signature cases are the two that protect real money.** If
they are not tested, the webhook is not tested.

## Testing a money system

Ordinary unit tests cover logic. These need integration tests:

| Test | Asserts |
| --- | --- |
| Top-up credits exactly once | Idempotency under a replayed webhook |
| Bad signature rejected | No balance change, no topup status change |
| Wallet debit is atomic with ledger | Crash between them is impossible |
| **Ledger sum == wallet balance** | The reconciliation query returns nothing |
| Concurrent webhooks, same order_id | Exactly one credit |
| Key spend limit blocks | 402 once exhausted |
| Model allowlist blocks | 403 for a disallowed model |
| **Mid-stream upstream failure** | Error emitted, no duplicate answer, billing correct |

**The reconciliation assertion is the highest-value test here.** It is one query
and it catches the entire class of balance bugs.

### Property worth testing explicitly

**Cache-read tokens must never be counted as input tokens.** They differ ~50x in
cost. A test that feeds a payload with cache hits and asserts the billed amount is
the cheapest guard against the most expensive accounting error.

## Environment

| Variable | Local value |
| --- | --- |
| `DATABASE_URL` | `sqlite://data/server.db` — relative to `server/` |
| `MIDTRANS_SERVER_KEY` | a dev constant the fake signs with |
| `MIDTRANS_ENV` | `sandbox` |
| `RUST_LOG` | `debug` |
| `PUBLIC_API_BASE_URL` | **empty** — set at `npm run build` time, makes the bundle same-origin |

**`.env` is gitignored and never contains production values.** See
[`.env.example`](../.env.example).

## Rules

1. **Never point local development at the production database file.** One mistaken
   migration against live data is unrecoverable, and with SQLite "pointing at it"
   is copying a file — which is easier to do by accident than editing a connection
   string, and leaves no connection log behind.
2. **Never use real provider keys locally.** A bug that loops retries produces a
   real bill.
3. **The fake must be able to fail.** A fake that only succeeds tests nothing.
4. **Seed data for the awkward cases** — an account with a negative-trending
   balance, a revoked key, an expired limit.
5. **Back up before a destructive local experiment.** `cp data/server.db …` is now
   a complete backup, provided you also take `-wal` — or better, use
   `VACUUM INTO` (see [`docs/backup-and-restore.md`](backup-and-restore.md)).

## Open items

- [x] Fakes live in the repo: `tools/fake-upstream/` and `tools/fake-midtrans/`.
- [ ] Seed script contents.
- [x] CI needs **no** database service container — the suite builds its own SQLite
      file per test. See [`ci-cd.md`](ci-cd.md).
- [x] Whether the identity service runs as a binary or in a container locally —
      **moot.** Phase 6 landed and there is no separate service: identity is served
      in-process by the Rust crate. See [`architecture/identity.md`](architecture/identity.md).
