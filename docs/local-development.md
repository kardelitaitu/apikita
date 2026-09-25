# Local Development

Running the whole system on a laptop, and how to test money flows without touching
real money.

> Deployment: [`docs/deployment.md`](deployment.md). This document is for the
> development loop.

## What must run locally

| Component | Required? | Note |
| --- | --- | --- |
| Rust API + proxy | **Yes** | The thing under development |
| SQLite | **No server** | A file at `data/server.db`, created by the migrate binary |
| PocketBase | **Yes** | One binary, no install ceremony — until Phase 6 replaces it |
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
| PocketBase | 8090 |
| Frontend dev server | 4321 (Astro default) |

**Cookie domains are the trap.** Use `localhost` for everything and make the API
reachable at the same origin the frontend uses, or session cookies will be set for
a domain the browser will not send them to. This is the local version of the
production subdomain decision in [`docs/architecture.md`](architecture.md).

## First run

```
# 1. database — a file, created and migrated by the migrate binary
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate

# 2. pocketbase (download the binary, then)
./pocketbase serve --http=127.0.0.1:8090

# 3. api
cp .env.example .env    # fill in what you need; fakes need nothing
cargo run --bin api

# 4. frontend (only for UI work)
cd website && npm run dev
```

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
| `POCKETBASE_URL` | `http://127.0.0.1:8090` |
| `MIDTRANS_SERVER_KEY` | a dev constant the fake signs with |
| `MIDTRANS_ENV` | `sandbox` |
| `RUST_LOG` | `debug` |

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

- [ ] Whether fakes live in the repo or as a separate dev tool.
- [ ] Seed script contents.
- [x] CI needs **no** database service container — the suite builds its own SQLite
      file per test. See [`ci-cd.md`](ci-cd.md).
- [ ] Whether PocketBase runs as a binary or in a container locally — moot after
      Phase 6, which replaces it with a Rust implementation that runs in-process.
