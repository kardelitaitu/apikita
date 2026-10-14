# Cache-Heavy Usage — Options

**Status:** AN OPTIONS NOTE FOR A CAPABILITY THAT IS NOT BUILT. `doc_claims.rs` excludes this file
from the citation check as "a design note for a cache that is not built", and that exclusion is only
honest if the file says so itself - it did not, which made a note about an unbuilt feature read as a
description of shipped behaviour. Read the figures below as the arithmetic behind a decision that is
still open, not as what the service does.

The one open commercial decision. Every workload except cache-heavy is profitable at
M = 2.00; this document lays out what can be done about the exception.

> Numbers from [`business/03-financial-model.md`](business/03-financial-model.md) and
> [`business/02-pricing.md`](business/02-pricing.md). Context: [`plan-audit.md`](plan-audit.md).

## The problem, precisely

**Margin per 1M tokens at M = 2.00:**

| Class | Wholesale | Margin |
| --- | ---: | ---: |
| Output | 10,707.12 | **10,707 IDR** |
| Input (cache miss) | 2,676.78 | **2,677 IDR** |
| Cache read | 53.54 | **54 IDR** |

**Output earns 200x what cache earns per token**, and support cost is the same per
customer regardless of what they run. So:

| To cover 25,000 IDR of support cost | Tokens needed/month |
| --- | ---: |
| From output alone | **2.5M** |
| From input alone | **9.8M** |
| From cache alone | **492M** |

**A cache-heavy customer would need ~492M cache tokens/month to pay for themselves.**
That is not a customer segment; it is an impossibility. The class is structurally
loss-making at a uniform markup.

### The realistic case

An agent workload — 2M raw, 40M cache, 0.5M out, 42.5M tokens total:

| | Value |
| --- | ---: |
| Revenue | 25,697 IDR |
| Cost | 12,849 IDR |
| Gross margin | 12,849 IDR |
| **Contribution** | **−12,974 IDR** |

**So the decision is: what do we do with a segment that loses ~13,000 IDR/month per
customer?**

---

## Option A — Do nothing

Accept the loss as a loss-leader.

| | |
| --- | --- |
| **Effort** | None |
| **Effect** | Every cache-heavy customer is a cost centre |
| **Risk** | Growth in this segment *shrinks* margin while customer count looks healthy |

**Verdict: rejected.** There is no mechanism to detect it happening — the plan has no
workload metric, so the loss would be invisible until revenue reconciliation showed
it. Accepting a known-invisible loss is not a strategy.

---

## Option B — A higher multiplier on cache reads

Keep one multiplier for input and output; price cache separately.

**How much is needed?** The agent workload is 12,974 IDR short. Each +1x on the
cache multiplier adds ~4,283 IDR of revenue to it, so:

**a cache multiplier of ~4.2x is the break-even point.** Use 5x for margin.

| Cache multiplier | Billed | Agent contribution |
| ---: | ---: | ---: |
| 1x (current) | 107 IDR/1M | **−12,974** |
| **5x** | **535 IDR/1M** | **+3,183** |
| 10x | 1,071 IDR/1M | +23,378 |

### The effect on every workload

| Workload | Contribution @1x | Contribution @5x |
| --- | ---: | ---: |
| Chat | +55,175 | +55,175 (unchanged — no cache) |
| Mixed | +10,678 | **+15,525** |
| Agent | **−12,974** | **+3,183** |

**Nothing becomes worse; the two cache-using workloads become better.**

### The cost of this option

At 5x, cache reads bill at **535 IDR/1M** — roughly **5x DeepSeek-direct** (their
official cache rate is ~54–108 IDR/1M). It is still only 2x their input and output
rates, so a customer comparing blended prices barely notices. **But a customer who
optimises specifically for cache cost will notice immediately.**

| | |
| --- | --- |
| **Effort** | Small — one config value per model (already per-model pricing) |
| **Cost** | Less competitive on the cache class specifically |
| **Risk** | Invites "why is cache so expensive?" questions |

**Verdict: recommended.** It is one config value, it fixes the segment, and it leaves
every other workload untouched or better.

---

## Option C — Volume floor on output tokens

Require a minimum number of **output** tokens per month — the class that actually
earns.

| | |
| --- | --- |
| **Threshold** | ~2.5M output tokens/month covers support from output alone |
| **Effect** | Excludes the agent workload (it has 0.5M output) |
| **Downside** | Also excludes small legitimate users who happen to be cache-heavy |

**Verdict: rejected as the primary fix.** It does not price the usage, it just refuses
the customer — and refusing a customer is a worse outcome than charging them
correctly. Worth keeping as a *secondary* control if Option B proves insufficient.

---

## Option D — Combination (B + C)

5x cache pricing, plus a modest output floor to catch the pathological case.

**Verdict: the likely end state.** Start with B, add C only if data shows B is not
enough. Adding both immediately makes it impossible to tell which one worked.

---

## Recommendation

**Option B — price cache reads at ~5x.**

1. **It is a config change** — `[[models]] price` becomes a per-class map, or add a
   `cache_price_multiplier` alongside it.
2. **It fixes the segment** rather than refusing it.
3. **It leaves chat and mixed untouched**, and improves mixed.
4. **The cost is honest and defensible**: cache reads cost us almost nothing, so a
   higher markup on them is not exploitative — but it *is* where the arbitrage on
   cache-heavy usage has to come from, or there is none.

### What changes if this is adopted

| Document | Change |
| --- | --- |
| `config/apikita.toml` | Per-class pricing, cache multiplier 5x |
| `decisions.md` | Record the decision and the reasoning |
| `business/02-pricing.md` | Rate card gains a cache row at the new price |
| `business/03-financial-model.md` | Contribution tables recomputed |
| `website/06-api-keys-and-limits.md` | Pricing explanation for customers |
| `server/api-spec.md` | Billing formula becomes per-class |

### The honest caveat

**The support cost of 25,000 IDR/month is what makes cache-heavy unprofitable.** If
real support cost is 5,000 IDR, the agent workload contributes +6,000 at uniform
pricing and the whole problem mostly disappears.

**So: measure support cost first if possible.** If it must be decided now, Option B is
the safe choice — it does not harm any workload, and it becomes unnecessary rather
than wrong if support turns out cheap.
