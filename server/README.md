# server

The Rust API and routing proxy. **This is the entire backend.**

**Responsibility:** auth exchange, wallet, API keys and limits, Midtrans webhook,
SSE live updates, and the LLM proxy — plus the reverse-proxy behaviour from the
whitepaper (endpoint resolution, streaming passthrough, wallet reservation,
usage settlement).

**Not this folder:** the customer-facing UI (see [`website/`](../website/README.md))
and operator alerts (see [`telegram/`](../telegram/README.md)).

## Stack

**Rust, deployed on Northflank.** Money lives in embedded SQLite — a file, not a
service — and identity comes from PocketBase until Phase 6 replaces it in Rust. See
[`docs/architecture.md`](../docs/architecture.md) and
[`docs/plans/sqlite-migration.md`](../docs/plans/sqlite-migration.md).

**Tokio + `axum`** for HTTP, **`sqlx`** (SQLite feature) for storage — the
framework is fixed by the code, not assumed.

## Full specification

[`docs/server/api-spec.md`](../docs/server/api-spec.md) — every endpoint,
authentication scheme, the proxy enforcement order, and the webhook procedure.

Two authentication schemes, deliberately separate: **cookie sessions** for the
dashboard, **API keys** for `/v1/*`. A cookie is never accepted on `/v1/*`, so a
leaked session cannot spend money.

## Status

**Implemented and green.** `cargo test --lib` → **427 passed / 0 failed / 0 ignored**
(measured 2026-09-27),
against a migrated temp SQLite file per test (`src/test_support.rs`) — no database
server to start and no `DATABASE_URL` needed. Nothing is `#[ignore]`d any more: the
former live-PocketBase exchange test was replaced by a loopback stub, so the whole
suite runs by default. Identity still enters through PocketBase at runtime, where a
live `POCKETBASE_URL` is required (migration Phase 6 moves it into Rust).

Migrations are applied by the `migrate` binary, never on boot:

```bash
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
```

It creates the file, applies `./migrations`, and exits non-zero unless
`journal_mode` is `wal` and `foreign_keys` is on. Nothing is deployed yet.
