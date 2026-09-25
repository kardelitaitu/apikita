# apikita

A high-throughput LLM arbitrage proxy gateway.

Wholesale LLM API capacity is fragmented and cheap; retail access to it is
neither. This project sits in between: a reverse-proxy gateway that unifies
volatile multi-endpoint upstream suppliers behind one reliable, prepaid API
surface, priced at a fixed margin over wholesale cost.

## Documentation

### Start here

| Document | Covers |
| --- | --- |
| [`docs/decisions.md`](docs/decisions.md) | **Settled decisions — the register.** Check here first |
| [`docs/plan-audit.md`](docs/plan-audit.md) | **Plan review — what holds, what doesn't** |
| [`docs/cache-pricing-options.md`](docs/cache-pricing-options.md) | **The open commercial decision — cache-heavy pricing** |
| [`docs/launch-checklist.md`](docs/launch-checklist.md) | **Launch tasks, gated.** What must be true before taking money |
| [`docs/architecture.md`](docs/architecture.md) | The system end to end — the authoritative stack |
| [`docs/topology.md`](docs/topology.md) | The triangle: Cloudflare, relay, Northflank, failover |
| [`docs/server/api-spec.md`](docs/server/api-spec.md) | Every endpoint, auth scheme, enforcement order |
| [`docs/website/02-data-model.md`](docs/website/02-data-model.md) | SQLite schema (parser-validated) |
| [`docs/business/README.md`](docs/business/README.md) | Does the business work — pricing, model, risks |

### Building it

| Document | Covers |
| --- | --- |
| [`docs/local-development.md`](docs/local-development.md) | Running the stack locally, fakes, testing money |
| [`docs/deployment.md`](docs/deployment.md) | Deploy pipeline, migrations, rollback |
| [`docs/ci-cd.md`](docs/ci-cd.md) | CI stages, the money tests, migration gate |
| [`docs/error-model.md`](docs/error-model.md) | Error codes and responses |
| [`docs/realtime.md`](docs/realtime.md) | SSE contract for live balance/usage |
| [`docs/website/README.md`](docs/website/README.md) | Website pages, flows, states |
| [`docs/website/06-api-keys-and-limits.md`](docs/website/06-api-keys-and-limits.md) | API keys, model access, limits |

### Running it

| Document | Covers |
| --- | --- |
| [`docs/observability.md`](docs/observability.md) | Logging, metrics, alerts, reconciliation |
| [`docs/backup-and-restore.md`](docs/backup-and-restore.md) | Backup strategy and the restore drill |
| [`docs/failover.md`](docs/failover.md) | Circuit breaking, key pools, mid-stream failure |
| [`docs/abuse-runbook.md`](docs/abuse-runbook.md) | What to do when someone abuses the platform |
| [`docs/admin-surface.md`](docs/admin-surface.md) | Operator capabilities, audit trail, money actions |
| [`docs/ip-tracking.md`](docs/ip-tracking.md) | Abuse signals without storing IPs |
| [`docs/edge-relay.md`](docs/edge-relay.md) | The relay itself: TLS, SSE passthrough, hardening |
| [`docs/cost-and-sizing.md`](docs/cost-and-sizing.md) | What it costs; where the money actually goes |

### Policy and legal

| Document | Covers |
| --- | --- |
| [`docs/terms-of-service.md`](docs/terms-of-service.md) | ToS draft — disclosure, refunds, acceptable use |
| [`docs/data-retention.md`](docs/data-retention.md) | What is stored, for how long, what is never stored |
| [`docs/architecture/identity.md`](docs/architecture/identity.md) | Accounts, linking, sessions |
| [`docs/website/04-payments.md`](docs/website/04-payments.md) | Midtrans QRIS end to end |

### Surfaces

| Document | Covers |
| --- | --- |
| [`docs/telegram/README.md`](docs/telegram/README.md) | Channel rooms, top-up feed, review bot |
| [`docs/website/03-functional-spec.md`](docs/website/03-functional-spec.md) | Login, logout, reset, dashboard states |
| [`docs/whitepaper.md`](docs/whitepaper.md) | The **original** design — partly superseded |
The whitepaper is the **original** design. Parts have since been corrected —
notably its pricing figures were ~2.5x too low, and the stack changed. Where
these disagree, `docs/architecture.md` and the business docs win.

## Stack

| Component | Where | Language |
| --- | --- | --- |
| Frontend | Cloudflare Pages | **Astro** + islands |
| Edge relay | Linux VPS (2 vCPU / 4 GB) | nginx + Docker |
| API + proxy | Northflank | Rust |
| Money | Northflank | SQLite (embedded — no separate service) |
| Identity | Northflank | PocketBase, until Phase 6 replaces it in Rust |

Push to `main` deploys. Reasoning and consequences:
[`docs/architecture.md`](docs/architecture.md).

## Layout

| Folder | Function |
| --- | --- |
| [`server/`](server/README.md) | Rust API + proxy: auth exchange, wallet, keys, limits, webhook, SSE |
| [`website/`](website/README.md) | Signup, API keys, wallet top-up, usage dashboards |
| [`telegram/`](telegram/README.md) | Telegram channel (4 rooms) + bot |
| [`config/`](config/README.md) | Routing and pricing config; provider price records |
| [`docs/`](docs/) | Architecture, business plan, website design, whitepaper |
| [`.agents/skills/`](.agents/skills/) | Agent skill definitions |

## Status

**Planning complete; implementation not started.** Nothing is deployed and no
application code exists.

Settled (41 documents): the stack and topology, the identity model, the SQLite
schema (validated with a SQL parser), the full HTTP API, payments, API keys and
limits, realtime, failover, the relay, deployment, cost, observability, backup,
abuse handling, data retention, and the Terms of Service outline.

Not yet: any application code.

## Secrets

Never commit credentials. `.env` is gitignored; `.env.example` documents the
required variables. Anything prefixed `PUBLIC_*` is **inlined into browser
JavaScript and is not secret**.