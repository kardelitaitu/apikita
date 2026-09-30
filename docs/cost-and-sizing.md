# Cost & Sizing

What this actually costs to run, and how small it can be. The objective is a
**low-cost server**, so this document exists to keep the infrastructure bill from
quietly exceeding the margin.

> Related: [`docs/business/03-financial-model.md`](business/03-financial-model.md)
> for the revenue side. This document covers the infrastructure cost side.

## The constraint that decides everything

From the financial model: contribution per customer is thin at low volume, and the
break-even target is roughly **100-200 customers** at ~20M tokens/month each. At
~35,000 IDR contribution per customer, **infrastructure must stay under ~3,000,000
IDR/month** or it eats the business.

That is the budget. Everything below is sized against it.

## What actually needs to run

| Component | Resource profile | Notes |
| --- | --- | --- |
| **Rust API + proxy** | CPU-light, memory-light, I/O-bound | Streams bytes; the work is waiting, not computing |
| **SQLite** (embedded) | **No instance, no extra CPU/RAM line item** | The wallet ledger is a file inside the API process; it needs the persistent volume below, not a server |
| **Identity** | **No line item** | Served in-process by the same Rust crate (`accounts` + `identities` in that same SQLite file). There is no separate identity service to run |
| **Frontend** | **Free** | Cloudflare Pages static hosting |
| **Edge relay** | Cheap VPS, ~2 vCPU / 4 GB | Absorbs connection load so Northflank stays small |

**Cloudflare Pages and the relay together keep the platform bill small.** That is a
large part of why the stack is shaped the way it is.

### The relay is a cost *reduction*, not an addition

The relay ([`edge-relay.md`](edge-relay.md)) costs a few dollars a month and
exists so the **billed Northflank instance stays at its smallest size**. TLS
handshakes, connection churn, and floods are absorbed by a box that costs less per
core than the managed platform.

**The tradeoff to watch:** if the relay is oversized and the backend is not actually
protected by it, you pay for both and gain nothing. The backend must only accept
traffic from the relay, or the savings are imaginary.

## Why the Rust server is cheap to run

The proxy is **I/O-bound, not CPU-bound**. A request spends its life waiting on an
upstream socket, not computing. That has direct sizing consequences:

- **Concurrency is cheap.** Tokio tasks are green threads; thousands of idle
  streaming connections cost kilobytes each, not megabytes.

- **Memory does not scale with request size.** Responses are streamed, not
  buffered (`server/src/upstream/client.rs`: the body is "never buffered whole").
  That passthrough is the reason a small box can serve large completions — the
  retired whitepaper proposed it, the code is what guarantees it.

- **The database is off the hot path.** API key metadata is cached with a <=60s
  TTL, so a request does not touch the database. Without that cache, the database
  would be the bottleneck and the instance size would have to grow with traffic.

**The practical implication: one small instance handles far more than this
business will ever throw at it.** Do not size for imagined scale.

## Sizing

| Component | Baseline | Why |
| --- | --- | --- |
| API/proxy instance | **1 vCPU / 1 GB** | I/O-bound; start here and measure |
| Database | **in-process — $0, no separate line item** | Embedded SQLite. Tiny row counts: thousands of rows, not millions, and no server to size |
| Identity | **in-process — $0, no separate line item** | No service to size: `accounts` + `identities` are tables in the same SQLite file |
| Volume for the database file | **10-20 GB** | Ledger + usage; usage dominates |

**Start smaller than you think.** A single 1 vCPU box with 1 GB is plausibly enough
for the entire break-even customer base. Measure before scaling, and let the
metrics decide.

## Where memory actually goes

| Consumer | Approximate | Note |
| --- | --- | --- |
| Streaming buffers in flight | per-connection | The dominant variable cost |
| Key metadata cache | small, fixed | Bounded by key count |
| Connection pools | fixed | Database + upstream HTTP pools |
| Runtime overhead | fixed | Tokio, allocator |

**The only thing that grows with load is in-flight streaming buffers.** That is why
streaming (rather than buffering) is a cost decision, not just a latency one.

## Cost levers, in order of impact

| Lever | Effect |
| --- | --- |
| **Cloudflare Pages** | Frontend hosting is free at this scale |
| **Streaming, not buffering** | Keeps memory flat as concurrency grows |
| **Key metadata cache** | Removes the database from the request path |
| **One instance, not a fleet** | No load balancer, no orchestration cost |
| **Relay absorbs connection churn** | Keeps the billed backend instance small |
| **Off-peak routing** | Upstream cost halves; see below |

## The upstream bill dominates, not the server

**This is the most important number in this document.**

Token cost is not infrastructure cost, and it dwarfs it.

At 20M tokens/month, mixed workload, the **wholesale token cost alone is roughly
30,000-55,000 IDR per customer per month** (see
[`docs/business/02-pricing.md`](business/02-pricing.md)). Across 100 customers that is
**3,000,000-5,500,000 IDR/month of upstream spend** — comparable to or larger than
the infrastructure budget.

**Optimising the server by 50% saves a rounding error. Optimising upstream cost
by 10% saves more than the entire infrastructure bill.**

### The off-peak lever

DeepSeek prices peak and off-peak, with **off-peak at exactly half price**, and
peak covering only ~21% of the week (01:00-04:00 and 06:00-10:00 UTC, Mon-Fri,
excluding Chinese holidays).

Anything that shifts traffic toward off-peak **halves the largest cost line**. This
is a product decision (batch/async tier, delayed processing) rather than a server
tuning one, but it is the single biggest cost lever available.

See [`docs/business/02-pricing.md`](business/02-pricing.md) for the full rate card.

## Scaling triggers

Do not scale on a hunch. Scale when one of these is true:

| Signal | Likely cause | Response |
| --- | --- | --- |
| CPU sustained >70% | Unexpectedly CPU-bound (compression? hashing?) | Find out why before adding CPU |
| Memory growth with traffic | Streaming buffers leaking or accumulating | Fix the leak, not the symptom |
| Database CPU high | Key cache not working, or usage writes are synchronous | Re-check the cache and batch usage writes |
| Latency rising, CPU low | **Upstream**, not us — check the provider | Do not scale the server |

**The last row is the trap.** A proxy's latency is usually the upstream's latency.
Adding server capacity fixes nothing and doubles the bill.

## What would make this expensive

- **Buffering whole responses** instead of streaming — memory scales with request
  size, and the instance size must grow with it.
- **Querying the database per token** — it becomes the bottleneck and forces a
  larger instance. (Embedded SQLite makes this *easier* to hit, not harder: the
  ledger now shares the API process's CPU and disk.)
- **Running the proxy and the API as separate services** before it is necessary —
  two instances, two deploys, no benefit at this scale.
- **Writing `last_used_at` on every request** — turns a read path into a write
  path and pushes load onto the database. This one is deliberately avoided rather
  than hypothetical: the write sits on the key-metadata cache MISS, so it happens
  at most once per `limits.key_metadata_cache_seconds` per key instead of once per
  request. See [website/02-data-model.md](website/02-data-model.md).
- **A load balancer in front of one instance** — cost with no benefit.

## Open items

- [ ] Actual prices from Northflank for the target instance sizes.
- [ ] Confirm the Cloudflare Pages free tier covers this usage.
- [ ] Measure real memory per concurrent stream once implemented.
- [x] ~~Decide whether Postgres runs as a managed add-on or self-hosted.~~ **Moot** —
  the port to embedded SQLite removed the decision along with the service
  ([§Stack](../README.md#stack), [`plans/sqlite-migration.md`](plans/sqlite-migration.md)).
- [ ] Establish the off-peak routing decision (async tier), which dominates cost.