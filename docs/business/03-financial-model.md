# 03 — Financial Model

> **Correction notice (v4) — significant.** Every figure in v1–v3 was computed on
> **whitepaper rates that were ~2.4x too low at peak**, and predating the
> peak/off-peak discovery. The verified rates are in
> [`02-pricing.md`](02-pricing.md): input 2,676.78, output 10,707.12, cache 53.54
> IDR/1M at peak.
>
> **Two things change, in opposite directions:**
>
> 1. **Costs rise ~2.4x** at peak — the old cost basis understated spending badly.
> 2. **The cache-read rate is proportionally cheaper than assumed**, and **M = 2.00
>    is now the operating value** rather than an option, which roughly doubles
>    margin per token.
>
> **The net effect is not uniformly worse — it is workload-dependent**, and the
> earlier blanket conclusion ("no workload covers its own support cost") no longer
> holds at M = 2.00. See [The finding](#the-finding) below, recomputed.
>
> Cite nothing from v1–v3 without checking it against the tables here.

Every number here is an input, not a finding. Nothing has been validated against
real traffic, real invoices, or a real customer. It exists to make the
assumptions visible and swappable — not to predict an outcome.

> **Correction notice (v3).** Two errors were found and corrected. (1) v1 used a
> 35% "blended margin"; a uniform multiplier actually yields a uniform margin
> percentage (`GM% = M - 1`), so the true rate at M=1.50 is 50%. (2) More
> seriously, **v1 and v2 both stated revenue per customer roughly 5× too high**,
> because it was carried over from the whitepaper's worked example rather than
> computed from the rate card. Every break-even figure in those versions is void.
>
> The corrected model is materially worse than what v1 and v2 described: at
> M=1.50 and 20M tokens/month, **no workload covers its own support cost**, and
> break-even was described as unreachable. Both v1–v3 rate bases were wrong. See
> [The finding](#the-finding) and [Break-even](#break-even).

Subscription-free, prepaid, usage-based, **non-refundable**. Revenue is a
function of tokens, so the model is built bottom-up from token volume rather
than top-down from market size.

**Headline:** the decisive input is not price or margin — it is **support cost
per customer**, which is set by how support is delivered — see
[`docs/support-model.md`](../support-model.md). At 20M tokens/month, M=1.50 customers are contribution-negative
against a 25,000 IDR support cost. See [Volume, not deposit size](#volume-not-deposit-size-is-the-real-gate).

## Input parameters

| Param | Meaning | Base | Range | Status |
| --- | --- | --- | --- | ---: |
| `M` | Margin multiplier | **2.00** | 1.50–2.00 | Decided — see `docs/decisions.md` |
| `gm_rate` | Gross margin rate (= M − 1) | 50% | 50–100% | Derived |
| `arpu_volume` | Tokens/customer/mo | 20M | 5–200M | `[ASSUMPTION]` |
| `workload` | Output share of token spend | mixed | chat↔agent | `[ASSUMPTION]` |
| `customers_0` | Launch customers | 5 | — | `[ASSUMPTION]` |
| `growth_mo` | MoM customer growth | 15% | 5–40% | `[ASSUMPTION]` |
| `churn_mo` | Monthly logo churn | 8% | 3–15% | `[ASSUMPTION]` |
| `infra_fixed` | Edge + DB + monitoring | 3,000,000 IDR | — | `[ASSUMPTION]` |
| `cost_support` | Support per customer/mo | 25,000 IDR | 5k–50k | `[ASSUMPTION]` — **most sensitive input**. Delivery model: [`docs/support-model.md`](../support-model.md) |
| `qris_fee` | Top-up processing | 0.7% | 0.7–2.0% | `[ASSUMPTION]` |
| `settle_lag` | Settlement delay | T+1 | T+1/T+2 | `[ASSUMPTION]` |
| `wastage` | Upstream spend not billed to anyone | 5% of GM | 2–15% | `[ASSUMPTION]` |

## Contribution per customer

**All figures below use the verified peak rates** — input 2,676.78, output 10,707.12,
cache 53.54 IDR/1M (see [`02-pricing.md`](02-pricing.md)). Earlier versions used
whitepaper rates ~2.4x too low; see the v4 notice at the top.

Cost is derived from token counts x rate. Gross margin is `cost x (M - 1)`.
At 20M tokens/month, 25,000 IDR assumed support cost, 5% wastage:

| Workload (raw / cache / out) | Cost | Contrib @1.50 | Contrib @2.00 |
| --- | ---: | ---: | ---: |
| Chat (16M / 0 / 4M) | 85,657 | **+14,788** | **+55,175** |
| Mixed (6M / 12M / 2M) | 38,117 | **−7,294** | **+10,678** |
| Cache-heavy (2M / 17.5M / 0.5M) | 11,644 | **−19,591** | **−14,101** |

### The finding

**At M = 2.00, chat and mixed workloads are profitable; cache-heavy is not.**

- **Chat-shaped** contributes ~55,000 IDR/month — comfortable.
- **Mixed** contributes ~10,700 — thin but positive.
- **Cache-heavy** loses ~14,100 even at 2.00, and would lose more at 1.50.

**This supersedes the earlier blanket claim that no workload covers its own support
cost.** That conclusion was an artefact of the wrong rate base, not a property of
the business.

**Cache-heavy usage cannot be fixed by raising the markup.** Cache reads earn ~54
IDR per million — the cheapest class by 200x. No multiplier makes a token worth 54
IDR cover a fixed per-customer support cost.

### Required volume to clear support cost

| Shape | @M=1.50 | @M=2.00 |
| --- | ---: | ---: |
| Chat-heavy | ~14M tokens/month | **~7M** |
| Mixed | ~26M | **~13M** |
| Cache-heavy | never | **never** |

**M = 2.00 roughly halves the volume each customer must consume.** That is the
strongest argument for 2.00 over 1.50, and it is why the config sets it.

### What this means

| Question | Answer |
| --- | --- |
| Is the business viable? | **Yes, for chat and mixed workloads at M = 2.00** |
| Viable at M = 1.50? | Only chat-shaped usage |
| Is every customer profitable? | **No** — cache-heavy loses money at any markup |
| Decisive unknown | **Support cost per customer** — still unmeasured. **Placeholder in use: 25,000 IDR/customer/month**, used throughout this model. The model is therefore usable now, and the placeholder is the number to replace with real data |

**Segment by workload, not by spend.** A cache-heavy customer at 20M tokens/month
is a liability; a chat-shaped one at the same volume is profitable. Volume alone
does not distinguish them, and a per-customer support cost is what makes the
difference matter.

## Volume, not deposit size, is the real gate

Contribution scales with tokens consumed; support cost is fixed per customer. This
inverts the deposit-floor discussion below: **the filter that matters is consumption
rate, not deposit amount.**

A 100,000 IDR deposit consumed in a month is a good customer. The same deposit
sitting idle for six months is a pure support cost with no offsetting margin.

## Break-even

Infrastructure is 3,000,000 IDR/month (assumption). Break-even is the customer count
whose combined contribution covers it — which depends entirely on workload mix,
because contribution per customer varies by ~5x between chat and cache-heavy at
identical token counts.

| Workload considered | Contribution/customer | Customers for 3M IDR/month |
| --- | ---: | ---: |
| Chat-heavy | ~55,175 | **~55** |
| Mixed | ~10,678 | **~281** |
| Cache-heavy | ~−14,101 | **never** |

**The honest reading: a mixed-workload customer base needs ~280 customers to cover
infrastructure; a chat-heavy one needs ~55.** With no cache-heavy customers, break-even
is reachable. Adding cache-heavy customers moves it *further away*, not closer.

**This is why workload mix is a first-class metric.** The same customer count can be
comfortably profitable or structurally loss-making depending on what those customers
actually run.

## Deposit economics

Prepaid, non-refundable, QRIS-only. Deposit tiers matter because per-customer costs
are roughly fixed while revenue scales with deposit size.

| Deposit | GM @ M=2.00 | QRIS 0.7% | Net |
| ---: | ---: | ---: | ---: |
| 10,000 | 5,000 | 70 | **4,930** |
| 50,000 | 25,000 | 350 | **24,650** |
| 100,000 | 50,000 | 700 | **49,300** |

**The 10k minimum is viable as a re-top-up floor, not as a first deposit.** A
customer whose entire monthly activity is one 10k deposit generates 4,930 IDR
against an assumed 25,000 IDR support cost — structurally negative.

**Deposit size is a weak proxy for what matters.** Tokens consumed per month is what
generates margin. A 10k deposit consumed immediately by a heavy user is a better
customer than a 300k deposit parked for a year.

Recommendation: **keep 10,000 IDR as the re-top-up minimum; set the first deposit
at 50,000 IDR, and pair it with a consumption expectation** (monthly minimum volume
or dormancy handling). The deposit floor filters payment nuisance; only a volume
rule filters the negative-contribution customer.

## Settlement float

Wallet credit is instant on webhook; Midtrans settlement to the bank lags. The float
is the gap between money owed to customers and money actually in hand:

| Scale | Deposits/day | Avg deposit | Float @ T+1 | Float @ T+2 |
| --- | ---: | ---: | ---: | ---: |
| Launch (10 cust) | ~0.1 | 50,000 | ~3,333 | ~6,667 |
| Phase 2 (100 cust) | ~5.0 | 50,000 | ~250,000 | ~500,000 |
| Scale (200 cust) | ~10.0 | 50,000 | ~500,000 | ~1,000,000 |

**The float is not a material constraint** — around a million IDR at T+2. A
working-capital detail to track, not a funding requirement.

## Liquidity of the upstream float

The genuine capital exposure is the prepaid balance held with the upstream:

```
upstream_float ~= (customers x 20M tokens x blended_wholesale_rate) per month
```

At 100 customers and a ~2,700 IDR/1M blended rate that is roughly **5.4M IDR/month**
of replenishment, before any buffer. Reserve policy should hold about one settlement
cycle of unpurchased liability so a provider outage does not stall paid customers.

**Resale is confirmed permitted**, so termination-for-resale is not the threat. The
remaining exposure is promotional repricing or an outage while wallets are funded —
see [`docs/business/05-risk.md`](05-risk.md) R2.

## What this model does not include

- **Refunds.** Policy is non-refundable, which scopes liability to undelivered
  service — not to zero. See [`docs/business/05-risk.md`](05-risk.md) R1.
- **Customer acquisition cost.** Absent entirely. At a mixed-workload contribution
  of ~10,700 IDR/month, a paid channel above ~30,000 IDR CAC is a three-month
  payback before churn. GTM must stay organic-first.
- **Currency exposure.** Pay upstream in CNY, collect IDR. Unhedged.
- **Tax and entity structure.** Business income into an entity that is not yet
  decided. Unmodeled and mandatory.

## What would change the conclusion

1. **Measure support cost per customer.** The most sensitive input — a 5x error
   moves break-even from reachable to unreachable.
2. **Measure real workload mix.** It drives contribution more than volume does:
   chat is +55k, cache-heavy is −14k at identical token counts.
3. **Watch for cache-heavy acquisition.** It is loss-making at any markup; acquiring
   it on volume alone would quietly subsidise it.
4. **Confirm the rates against a live invoice.** The card is derived from a
   screenshot and an FX rate, both of which move.