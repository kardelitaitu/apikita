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

**Implemented and green.** `cargo test --lib` → **297 passed / 0 failed / 2 ignored**
(measured 2026-09-26 at commit 0488577),
against a migrated temp SQLite file per test (`src/test_support.rs`) — no database
server to start and no `DATABASE_URL` needed. The 2 ignored tests drive a real
PocketBase auth exchange and need a live `POCKETBASE_URL`; identity is still
PocketBase until migration Phase 6.

Migrations are applied by the `migrate` binary, never on boot:

```bash
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
```

It creates the file, applies `./migrations`, and exits non-zero unless
`journal_mode` is `wal` and `foreign_keys` is on. Nothing is deployed yet.
