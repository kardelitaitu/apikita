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

Framework is not yet fixed; Tokio is assumed, with `axum` for HTTP as the
whitepaper implies.

## Full specification

[`docs/server/api-spec.md`](../docs/server/api-spec.md) — every endpoint,
authentication scheme, the proxy enforcement order, and the webhook procedure.

Two authentication schemes, deliberately separate: **cookie sessions** for the
dashboard, **API keys** for `/v1/*`. A cookie is never accepted on `/v1/*`, so a
leaked session cannot spend money.

## Status

Empty scaffolding. Nothing implemented.
