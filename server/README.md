# server

The Rust API and routing proxy. **This is the entire backend.**

**Responsibility:** native auth (signup, login, Google sign-in, verification,
password reset), wallet, API keys and limits, Midtrans webhook,
SSE live updates, and the LLM proxy — plus the reverse-proxy behaviour the retired
whitepaper first sketched (endpoint resolution, streaming passthrough, wallet
reservation, usage settlement). The code, not that document, defines what they do
now: see [`docs/architecture.md`](../docs/architecture.md).

**Not this folder:** the customer-facing UI (see [`website/`](../website/README.md))
and operator alerts (see [`telegram/`](../telegram/README.md)).

## Stack

**Rust, deployed on Northflank.** Money and identity both live in embedded SQLite —
a file, not a service. Identity is served natively by this crate: the `accounts` and
`identities` tables, with Argon2id password hashing owned by Rust. There is no
PocketBase service and no `POCKETBASE_URL`. See
[`docs/architecture.md`](../docs/architecture.md) and
[`docs/architecture/identity.md`](../docs/architecture/identity.md).

**Tokio + `axum`** for HTTP, **`sqlx`** (SQLite feature) for storage — the
framework is fixed by the code, not assumed.

## Full specification

[`docs/server/api-spec.md`](../docs/server/api-spec.md) — every endpoint,
authentication scheme, the proxy enforcement order, and the webhook procedure.

Two authentication schemes, deliberately separate: **cookie sessions** for the
dashboard, **API keys** for `/v1/*`. A cookie is never accepted on `/v1/*`, so a
leaked session cannot spend money.

## Status

**Implemented and green.** `cargo test --lib` → **660 passed / 0 failed / 0 ignored**
(measured 2026-09-30),
against a migrated temp SQLite file per test (`src/test_support.rs`) — no database
server to start and no `DATABASE_URL` needed. Nothing is `#[ignore]`d any more: the
former live-PocketBase exchange tests were deleted with the provider they exercised,
so the whole suite runs by default. Identity is served natively by this crate — the
`accounts` and `identities` tables in embedded SQLite — so there is no PocketBase to
reach and no `POCKETBASE_URL` to set.

Migrations are applied by the `migrate` binary, never on boot:

```bash
DATABASE_URL=sqlite://data/server.db cargo run --bin migrate
```

It creates the file, applies `./migrations`, and exits non-zero unless
`journal_mode` is `wal` and `foreign_keys` is on. Nothing is deployed yet.
