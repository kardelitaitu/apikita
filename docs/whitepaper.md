# Whitepaper: the original design — RETIRED

> **Status: retired. Do not read this as a description of the system.**
>
> The 2026 founding whitepaper ("Co-Plus LLM Arbitrage Routing Engine") described a
> system that was then designed but **never built this way**. It was reduced to this
> record because it could not be kept accurate: the current system is described by
> [architecture.md](architecture.md) (what shipped), [business/](business/README.md)
> (the economics), and [failover.md](failover.md) (provider behaviour).

## Why it was retired

[plan-audit.md](plan-audit.md) §6 reached this first: *"It is labelled as superseded
at the top, but it is 146 lines of authoritative-looking detail. **Consider deleting
it** rather than keeping a corrected-in-footnotes version."*

That advice was right, and the reason is structural, not editorial. Every other
document in [docs/](.) is **coupled to code** — pricing to `config/apikita.toml`, the
API to the router, the schema to the migrations — so a test can fail when it drifts.
The whitepaper's claims were narrative prose about an architecture that does not
exist, so **nothing could check them**. Patching them in place (two rounds of it)
could not converge: each audit round found more.

It was retired rather than corrected when the **sixth** material error was found.

## What it was for

It is the only place the **founding commercial thesis** is stated in full: that a
wholesale market fragmentation exists (regional providers well below global retail
baselines) and that a routing gateway could capture the spread with a cost-plus
margin. That thesis survived; the architecture proposed to deliver it did not.

Two things it recorded are preserved elsewhere and were **not** lost with it:

- **The crypto rail intent** (NOWPayments, stablecoins) — the only other written
  record, and deliberately kept: [billing-system.md](billing-system.md). The rail
  itself is **not built and not promised** ([`decisions.md`](decisions.md) §Money).
- **The peak/off-peak pricing frame** — [business/02-pricing.md](business/02-pricing.md).

## What it got wrong

Recorded because a future reader may find the whitepaper quoted, and because the
failure mode is worth not repeating. Each of these was found by checking the prose
against the code:

| It said | What is true |
| --- | --- |
| Prices ~1,100 / ~4,400 / ~22 IDR per 1M | **2.43x below** the rate the system bills on. Correct card: [business/02-pricing.md](business/02-pricing.md) |
| "guaranteed **risk-free** profit margin" | No such thing. Cross-provider failover is **not usable** with one provider ([failover.md](failover.md)) |
| Cost deducted from a **Redis** wallet | **Redis is not used.** Embedded SQLite (WAL); `routes/proxy.rs` records that this build has no shared-state dependency |
| Parse length with **`tiktoken-rs`** | **Not a dependency.** The estimate is a documented approximation |
| Crossing the balance **CUTS** the stream mid-answer | `mid_stream_cutoff = false`, deliberately: a truncated non-refundable answer is the likeliest delivery dispute |
| Config is **hot-reloadable** (`Arc<RwLock<T>>`) | `Arc<AppConfig>`, parsed **once at startup**. A config change is a restart |
| "**randomized or weighted**" load balancing | Deterministic **config order**. Weight is a gate (0 = never routed), not a probability |
| Trips after **>3** consecutive errors | Trips **at 3** (`failure_threshold = 3`, compared `>=`) |

The stack it named — Redis, a Postgres/JSON document store, a planned crypto rail —
was replaced wholesale by embedded SQLite during the migration. See
[plans/sqlite-migration.md](plans/sqlite-migration.md).

## The one part that was right

The **three-phase money lifecycle** it specified is substantially what shipped: a
worst-case pre-flight reservation, an unbuffered stream, and a post-stream
settlement that releases the unused hold in the same transaction as the debit. The
phases were kept; the mechanisms it named for them were replaced. See
[decisions.md](decisions.md) §Money and `docs/server/api-spec.md`.

---

*Retired 2026-09-28. The original 205-line document is in git history
(`git log -- docs/whitepaper.md`), and is not to be restored: see
[`website/tests/retired-docs.test.ts`](../website/tests/retired-docs.test.ts),
which fails if a live document starts quoting its superseded figures again.*
