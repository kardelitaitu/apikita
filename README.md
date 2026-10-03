# apikita

**A multi-provider LLM gateway that runs the whole business on one small box.**

Wholesale LLM capacity is cheap, fragmented, and unreliable. Getting it at retail,
as one dependable API, from Indonesia is not. apikita sits in the middle: a
reverse-proxy gateway in **Rust** that unifies **multiple upstream providers** behind
one reliable, prepaid API surface — and does it on a footprint so small the entire
infrastructure bill is a rounding error against the token spend.

This README is for the people deciding whether the idea is interesting. The
engineering detail lives in [`docs/`](docs/).

---

## Why it is worth a look

### Multi-provider by design — one API in front of many suppliers

The gateway is built to sit in front of **many upstream providers at once**, not to
resell one of them. Every upstream is modelled as a `(url + model)` endpoint with its
own key pool, price record, and health, so adding a provider is configuration — not a
rewrite:

- **Provider-agnostic routing** — the router sees endpoints, not vendors. A second,
  third, or fourth supplier slots in beside the first.
- **Per-provider health and failover** — when one supplier degrades, traffic shifts to
  a healthy one; a bad provider stops taking requests without taking the service down.
- **Per-provider price records** — each supplier's rates live in their own file; the
  billing config is compiled from them, so cost basis tracks reality
  ([`config/`](config/README.md)).

The failover slots are provisioned for suppliers 2, 3 and 4 today; they activate as
each is verified. [`docs/failover.md`](docs/failover.md).

### Reliable when upstream is not

Wholesale routes are unstable — timeouts, throttling, and mid-stream drops are
normal. The gateway is built around that, at two levels:

- **Across providers** — an unhealthy supplier is taken out of rotation and traffic
  fails over to a healthy one.
- **Within a provider** — key-pool rotation; a throttled (HTTP 429) key gets a
  per-key cooldown and the pool routes around it, instead of failing the request.
- **Circuit breaking per endpoint** — 3 consecutive failures open the endpoint, a
  half-open probe recovers it, and the cooldown backs off exponentially.

Current honest caveat: **the failover path is architected and wired but not yet
activated** — one supplier is verified and live, so there is nothing to fail over
*to* in production yet. The in-provider resilience above works now.
[`docs/failover.md`](docs/failover.md).

### Fast by construction, not by hardware

The proxy is **I/O-bound, not CPU-bound**. A request spends its life waiting on an
upstream socket, not computing, so a single small instance handles far more traffic
than the business will realistically throw at it. Concurrency is cheap (Tokio green
threads); responses are **streamed, never buffered**, so **memory does not grow with
response size** the way it does in a buffering proxy.

See [`docs/benchmark.md`](docs/benchmark.md) and
[`docs/cost-and-sizing.md`](docs/cost-and-sizing.md).

### Cheap to run on purpose

The cost shape is a design decision, not an accident:

- **Embedded SQLite** — the money ledger is a *file inside the API process*. No
  database server, no network hop, no extra line item.
- **Cloudflare Pages** for the frontend — free at this scale.
- **A cheap edge relay** absorbs TLS, connection churn and floods so the **billed
  backend instance stays at its smallest size**.
- **One instance, not a fleet** — no load balancer, no orchestration.

The baseline is **1 vCPU / 1 GB**, deliberately chosen to start smaller than you
think. [`docs/cost-and-sizing.md`](docs/cost-and-sizing.md).

### Money correctness as a structural property

The wallet is an **append-only ledger**; the balance is derivable
(`wallet = SUM(delta_idr)`) and a negative balance is refused by the database
itself via `CHECK (balance_idr >= 0)`. Reconciliation is a gate, not a hope: the
check must return **zero rows** on production data before launch.
See [`docs/decisions.md`](docs/decisions.md) and
[`docs/launch-checklist.md`](docs/launch-checklist.md).

### Privacy is architecture, not policy

**Prompt and completion content is never logged.** The data model is designed so
there is nowhere for it to land. [`docs/data-retention.md`](docs/data-retention.md).

## The economics in one line

Buy wholesale across providers, resell at a fixed multiplier over *actual* upstream
cost — **`GM% = M − 1`, uniform at any workload mix**. Buying from several
suppliers is what makes the cost basis durable and the moat real.
[`docs/business/00-overview.md`](docs/business/00-overview.md).

## Status

**Built locally; nothing deployed.** The server and website exist and pass their
suites (server: **655 tests** — website: **196 tests**), but no environment is live
and no customer has been served. The operational gates in
[`docs/launch-checklist.md`](docs/launch-checklist.md) — Gate 0 (legal) first —
must clear before taking money.

## How it is put together

| Layer | Runs on | Built with |
| --- | --- | --- |
| Customer site | Cloudflare Pages | **Astro** + islands |
| Edge relay | Cheap Linux VPS | nginx + Docker |
| API + proxy | Northflank | **Rust** |
| Upstreams | Multiple providers | Provider-agnostic endpoint routing |
| Money | *inside the API process* | **SQLite** (embedded — no separate service) |
| Identity | Northflank | **Rust** — the API's own embedded SQLite `accounts` + `identities` tables |

Push to `main` deploys. The reasoning and the tradeoffs:
[`docs/architecture.md`](docs/architecture.md).

## Repository layout

| Folder | Function |
| --- | --- |
| [`server/`](server/README.md) | The Rust API + proxy: sign-in, wallet, keys, limits, webhook, SSE |
| [`website/`](website/README.md) | Signup, API keys, wallet top-up, usage dashboards |
| [`telegram/`](telegram/README.md) | Telegram channel (4 rooms) + bot — design only |
| [`config/`](config/README.md) | Routing and pricing config; per-provider price records |
| [`docs/`](docs/) | Architecture, business plan, website design, whitepaper |

## Start with the docs

| Document | Covers |
| --- | --- |
| [`docs/business/README.md`](docs/business/README.md) | **Does the business work** — pricing, model, risks |
| [`docs/architecture.md`](docs/architecture.md) | The system end to end — the authoritative stack |
| [`docs/decisions.md`](docs/decisions.md) | **The settled-decisions register.** Check here first |
| [`docs/launch-checklist.md`](docs/launch-checklist.md) | **What must be true before taking money** |
| [`docs/cost-and-sizing.md`](docs/cost-and-sizing.md) | What it costs; where the money actually goes |
| [`docs/failover.md`](docs/failover.md) | Multi-provider failover, circuit breaking, key pools |
| [`docs/server/api-spec.md`](docs/server/api-spec.md) | Every endpoint, auth scheme, enforcement order |
| [`docs/whitepaper.md`](docs/whitepaper.md) | The original whitepaper — **retired**, kept as a record of what it got wrong |
| [`tools/README.md`](tools/README.md) | **The verification tools** — what checks the money, the data and the delivery |

**The whitepaper is retired, not authoritative.** It described an architecture that
was never built (Redis, a Postgres document store, hot-reloaded config, weighted
load balancing, mid-stream cuts) and its prices were 2.43x too low. It is kept only
as a record of the founding thesis and of what it got wrong. For anything current,
read [`docs/architecture.md`](docs/architecture.md) and
[`docs/business/`](docs/business/README.md) — they win.

## Secrets

Never commit credentials. `.env` is gitignored; `.env.example` documents the
required variables. Anything prefixed `PUBLIC_*` is **inlined into browser
JavaScript and is not secret**.
