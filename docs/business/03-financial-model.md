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
> break-even is unreachable by adding customers. See
> [The finding](#the-finding) and [Break-even](#break-even).

Subscription-free, prepaid, usage-based, **non-refundable**. Revenue is a
function of tokens, so the model is built bottom-up from token volume rather
than top-down from market size.

**Headline:** the decisive input is not price or margin — it is **support cost
per customer**. At 20M tokens/month, M=1.50 customers are contribution-negative
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
| `cost_support` | Support per customer/mo | 25,000 IDR | 5k–50k | `[ASSUMPTION]` — **most sensitive input** |
| `qris_fee` | Top-up processing | 0.7% | 0.7–2.0% | `[ASSUMPTION]` |
| `settle_lag` | Settlement delay | T+1 | T+1/T+2 | `[ASSUMPTION]` |
| `wastage` | Upstream spend not billed to anyone | 5% of GM | 2–15% | `[ASSUMPTION]` |

## Contribution per customer

> **Correction notice (v3).** v1 and v2 of this document both stated revenue per
> customer that was roughly 5× too high. v1's ~200-customer break-even and v2's
> ~84-customer figure were derived from those revenues, not from the rate card.
> The figures below are computed directly from the rate card in
> [`02-pricing.md`](02-pricing.md). **The v1 and v2 break-even numbers are void.**

Cost per token is derived from the rate card; gross margin is `cost × (M − 1)`.
Token splits are by *count*, which is what determines cost.

At 20M tokens/month, support 25,000 IDR, wastage 5% of GM:

| Workload (raw / cache / out) | Cost | Revenue @1.5 | GM @1.5 | Contribution @1.5 | GM @2.0 | Contribution @2.0 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Chat (16M / 0 / 4M) | 35,200 | 52,800 | 17,600 | **−8,526** | 35,200 | **+8,900** |
| Mixed (6M / 12M / 2M) | 15,664 | 23,496 | 7,832 | **−17,669** | 15,664 | **+3,155** |
| Cache-heavy (2M / 17.5M / 0.5M) | 4,785 | 7,178 | 2,393 | **−22,761** | 4,785 | **−1,200** |

### The finding (recomputed with verified rates)

At 20M tokens/month, peak basis, 25,000 IDR assumed support cost:

| Workload (raw/cache/out, millions) | Cost | GM @1.50 | Contrib @1.50 | GM @2.00 | Contrib @2.00 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Chat (16 / 0 / 4) | 85,657 | 42,828 | **+14,788** | 85,657 | **+55,175** |
| Mixed (6 / 12 / 2) | 38,117 | 19,059 | **−7,294** | 38,117 | **+10,678** |
| Cache-heavy (2 / 17.5 / 0.5) | 11,644 | 5,822 | **−19,591** | 11,644 | **−14,101** |

**This supersedes the earlier blanket claim that no workload covers its own support
cost.** At **M = 2.00**, a chat-shaped customer contributes ~55,000 IDR/month and a
mixed one ~10,700. Both are viable.

**Cache-heavy remains negative even at M = 2.00 (−14,101).** That is the honest
conclusion: the workload that generates almost no absolute margin per token cannot
carry a fixed support cost, regardless of markup.

### Required volume to clear support cost

| Shape (raw:out by count) | @M=1.50 | @M=2.00 |
| --- | ---: | ---: |
| Chat-heavy | ~14M tokens/month | ~7M |
| Mixed | ~26M | ~13M |
| Cache-heavy | never | never |

**M = 2.00 roughly halves the volume each customer must consume.** That is the
strongest argument for 2.00 over 1.50, and it is why the config sets it.

### What this means for the business

| Question | Answer |
| --- | --- |
| Is the business viable? | **Yes, for chat and mixed workloads at M = 2.00** |
| Is it viable at M = 1.50? | **Only for chat-shaped usage** |
| Is every customer profitable? | **No** — cache-heavy usage loses money at any markup |
| What is the decisive unknown? | **Support cost per customer**, still unmeasured |

**The actionable consequence: segment by workload, not just by spend.** A
cache-heavy customer at 20M tokens/month is a liability; a chat-shaped one at the
same volume is profitable. Volume alone does not distinguish them.
## Volume, not deposit size, is the real gate

Contribution scales with tokens consumed; support cost is fixed per customer.
This inverts the deposit-floor discussion below: **the filter that matters is
consumption rate, not deposit amount.**

A 100,000 IDR deposit consumed in a month is a good customer. The same deposit
sitting idle for six months is a pure support cost with no offsetting margin.
There is currently no policy or mechanism for this, and it needs one — a monthly
minimum, an inactivity fee, or dormancy handling. Deposits alone do not
distinguish the two.

## Deposit economics

Prepaid, non-refundable, QRIS-only. Deposit tiers matter because per-customer
costs are roughly fixed while revenue scales with deposit size.

| Deposit | GM @ M=1.50 | QRIS 0.7% | Net |
| ---: | ---: | ---: | ---: |
| 10,000 | 3,333 | 70 | **3,263** |
| 50,000 | 16,667 | 350 | **16,317** |
| 100,000 | 33,333 | 700 | **32,633** |

**The 10k minimum is viable as a re-top-up floor, not as a first deposit.** A
customer whose entire monthly activity is one 10k deposit generates 3,263 IDR
against an assumed 25,000 IDR support cost — structurally negative.

But note the frame change from the section above: **deposit size is a weak proxy
for the thing that actually matters.** What matters is tokens consumed per month,
because that is what generates margin. A 10k deposit consumed immediately by a
heavy user is a better customer than a 300k deposit parked for a year.

Recommendation: **keep 10,000 IDR as the re-top-up minimum; set the first deposit
at 50,000–100,000 IDR, and pair it with a consumption expectation** (monthly
minimum volume, or dormancy handling). The deposit floor filters payment
nuisance; only a volume rule filters the negative-contribution customer that
[Volume, not deposit size](#volume-not-deposit-size-is-the-real-gate) describes.

## Settlement float

Wallet credit is instant on webhook; Midtrans settlement to the bank lags. The
float is the gap between money owed to customers and money actually in hand:

| Scale | Deposits/day | Avg deposit | Float @ T+1 | Float @ T+2 |
| --- | ---: | ---: | ---: | ---: |
| Launch (10 cust) | ~0.1 | 50,000 | ~3,333 | ~6,667 |
| Phase 2 (100 cust) | ~5.0 | 50,000 | ~250,000 | ~500,000 |
| Break-even (~84 cust) | ~4.2 | 50,000 | ~210,000 | ~420,000 |

**The float is not a material constraint** — under a million IDR even at T+2. It
is a working-capital detail to track, not a funding requirement.

## Liquidity of the upstream float — the real capital constraint

The genuine capital exposure is not settlement lag. It is the prepaid balance
held with the upstream provider:

```
upstream_float ≈ (customers × arpu_volume/1e6 × blended_wholesale_rate) × buffer
```

At 100 customers x 20M tokens x a ~2,700 IDR/1M blended wholesale rate ≈ **5.4M IDR/month** of
upstream replenishment at the mixed workload, before any buffer. Reserve policy
should hold roughly one settlement cycle of unpurchased liability so a provider
outage does not stall paid customers. **Note the customer count here is
illustrative only** — it is not a break-even target, and at M=1.50 no count is
viable (see above).

Resale is confirmed permitted, so termination-for-resale is not the threat. The
remaining exposure is ordinary: promotional repricing, or a provider outage while
customer wallets are funded. See [`05-risk.md`](05-risk.md) R1 and R2.

## What this model does not include

- **Refunds.** Policy is non-refundable. This scopes liability to undelivered
  service, which is *not* eliminated — see [`05-risk.md`](05-risk.md) R1.
- **Customer acquisition cost.** Entirely absent. At mixed-workload contribution
  of ~35,840 IDR/month, a paid channel above ~100,000 IDR CAC is a three-month
  payback before churn. GTM must stay organic-first.
- **Currency exposure.** Pay upstream in one currency, collect IDR. Unhedged.
- **Tax and entity structure.** Business income into an entity that is not yet
  decided. Unmodeled and mandatory.

## What would change the conclusion

1. **Measure support cost per customer.** It is the most sensitive input in the
   model by an order of magnitude — a 5× error moves break-even from ~260
   customers to unreachable. Measure before pricing.
2. **Verify the rate card against the live price list.** The whitepaper's IDR
   figures are `[UNVERIFIED]` and produced a 5× ARPU error when used as
   delivered. Every figure here depends on them.
3. **Measure real `arpu_volume` and workload mix.** Both invented; mix drives
   absolute margin per customer by ~7×.
4. **Decide the entity.** Personal vs. PT changes the volume ceiling, tax
   treatment, and dispute posture. Unmodeled.
5. **Price is chosen: M=2.00 per model.** At plausible support cost, 1.50 has no
   viable configuration at 20M tokens/month. See [`docs/decisions.md`](../decisions.md).
   Revisit downward once real support cost is measured.