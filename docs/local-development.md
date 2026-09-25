# Local Development

Running the whole system on a laptop, and how to test money flows without touching
real money.

> Deployment: [`docs/deployment.md`](deployment.md). This document is for the
> development loop.

## What must run locally

| Component | Required? | Note |
| --- | --- | --- |
| Rust API + proxy | **Yes** | The thing under development |
| PostgreSQL | **Yes** | Docker or a local install |
| PocketBase | **Yes** | One binary, no install ceremony |
| Frontend dev server | Only for UI work | Pages Functions are not used |
| Midtrans | **No** | Faked locally — see below |
| Upstream providers | **No** | Faked locally |

## Ports

| Service | Port |
| --- | --- |
| Rust API | 8080 |
| PostgreSQL | 5432 |
| PocketBase | 8090 |
| Nginx edge relay | 8000 |
| Frontend dev server | 4321 (Astro default) |

**Cookie domains are the trap.** Use `localhost` for everything and make the API
reachable at the same origin the frontend uses, or session cookies will be set for
a domain the browser will not send them to. This is the local version of the
production subdomain decision in [`docs/architecture.md`](architecture.md).

## First run

```
# 1. database
docker run -d --name apk-pg -e POSTGRES_PASSWORD=dev -e POSTGRES_DB=apikita \
  -p 5432:5432 postgres:16

# 2. schema
psql postgresql://postgres:dev@localhost:5432/apikita -f server/migrations/20260925000000_initial_schema.sql

# 3. pocketbase (download the binary, then)
./pocketbase serve --http=127.0.0.1:8090

# 4. api
cp .env.example .env    # fill in what you need; fakes need nothing
cargo run --bin apikita-server

# 5. frontend (only for UI work)
cd website && npm run dev
```

**`schema.sql` is generated from the documents**, not hand-written elsewhere — see
[`docs/website/02-data-model.md`](website/02-data-model.md). Keep one source.

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
| `DATABASE_URL` | `postgres://postgres:dev@localhost:5432/apikita` |
| `POCKETBASE_URL` | `http://127.0.0.1:8090` |
| `MIDTRANS_SERVER_KEY` | a dev constant the fake signs with |
| `MIDTRANS_ENV` | `sandbox` |
| `RUST_LOG` | `debug` |

**`.env` is gitignored and never contains production values.** See
[`.env.example`](../.env.example).

## Rules

1. **Never point local development at production Postgres.** One mistaken migration
   against live data is unrecoverable.
2. **Never use real provider keys locally.** A bug that loops retries produces a
   real bill.
3. **The fake must be able to fail.** A fake that only succeeds tests nothing.
4. **Seed data for the awkward cases** — an account with a negative-trending
   balance, a revoked key, an expired limit.

## Open items

- [x] Fakes live in the repo: `tools/fake-upstream/` and `tools/fake-midtrans/`.
- [ ] Seed script contents.
- [x] CI uses a real Postgres service container — see [`ci-cd.md`](ci-cd.md).
- [ ] Whether PocketBase runs as a binary or in a container locally.
