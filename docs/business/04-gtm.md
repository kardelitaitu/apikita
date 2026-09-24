# 04 — Go-To-Market

## Constraint

Contribution per customer is roughly 15,000 IDR/month in the base case. Any paid
acquisition channel with a CAC above ~45,000 IDR is structurally unaffordable.
**This plan is organic and community-led by necessity, not by preference.**

## Sequence

**Phase 0 — credibility before code.**
Do not launch on word of mouth alone. Before the first paying customer:

- Confirm upstream resale terms and that we can legally bill for it
  ([05-risk.md](05-risk.md) — this gates everything).
- Publish a real rate card with verified rates.
- Have one working integration guide and a live status page.

**Phase 1 — first 10 customers, hand-recruited.**
Target Indonesian developer communities directly: Telegram and Discord groups,
local dev meetups, university CS groups, indie hacker circles. The pitch is a
price comparison, not a product tour: show the same workload on retail vs. us, in
IDR. Sign people up personally.

Success criterion: 10 customers with real traffic, so `arpu_volume`, workload
mix, and **support cost per customer** stop being guesses. The last of those is
the input the whole model turns on.

**Phase 2 — 10 to 100, content-led.**
The reusable asset is a public cost-comparison calculator: input a workload, get
the IDR delta. It converts skeptics and it is the only marketing artifact worth
building. Pair with short technical posts on how the routing and failover work —
the engineering is the credibility.

**Phase 3 — proving the unit economics.**
Break-even is **not** a customer-count target yet: at M=1.50 and 20M tokens/month
per customer, contribution is negative and no customer count works. This phase is
about establishing that a typical customer is contribution-positive at all —
which requires measuring real support cost and real consumption
(see [`03-financial-model.md`](03-financial-model.md)). Do not scale spend until
a single customer is proven profitable.

## Channels, ranked by expected value per unit of effort

1. **Direct outreach in local dev communities.** Slow, unscalable, and the only
   channel with a reliable conversion. Do it first.
2. **The cost-comparison content.** Compounds, no ongoing cost, directly on-message.
3. **Open-source the routing layer.** Positions us as engineers rather than
   resellers, and the credibility is worth more than the code. **Check this
   against the resale-terms question first** — publishing the mechanism makes it
   trivially easy for an upstream to identify us. `[FLAG]`
4. **Referral credit.** Cheap and aligned, once retention is proven. Not before.

## What we deliberately do not do

- **No paid ads.** CAC math does not permit it at this ARPU.
- **No enterprise sales motion.** We cannot serve compliance requirements and a
  sales cycle would burn cash we do not have.
- **No free tier at launch.** Free users consume upstream tokens we pay for. A
  small signup credit is the most we can offer, and only if abuse is contained.
- **No model breadth race.** We resell commodity capacity. Breadth is a cost.

## Payment and top-up flow

Collection runs inside the Telegram bot: dynamic QRIS via **Midtrans Snap**,
crediting the wallet only from the **server webhook** (verify `signature_key`;
never credit from the client callback or from the amount in the payload). This
makes `website/` optional at launch — the bot is signup, wallet, and top-up.

**Deposit floor:** 10,000 IDR is a re-top-up minimum, not a first deposit. A
customer whose monthly activity is a single 10k top-up nets 3,263 IDR against an
assumed 25,000 IDR support cost. Set the first deposit at 50,000-100,000 IDR; the
first deposit is where support cost is incurred and it needs no rate-card change.
See [`03-financial-model.md`](03-financial-model.md).

## Pricing as a GTM lever

The 1.50 multiplier is a starting point, not a commitment. It is bounded above by
regional aggregator pricing, which is not yet measured.

- If conversion is weak but customers say the product is good → the multiplier is
  too high; test 1.35.
- If conversion is strong and support load is low → test 1.65.
- If conversion is strong and support load is high → **do not raise price; raise
  the minimum top-up.** It filters payment nuisance without changing the
  published rate.
- If measured contribution per customer is negative → **the multiplier is too
  low, not the volume.** At M=1.50 with 20M-token customers, no workload covers a
  25,000 IDR support cost; M=2.00 roughly halves the required volume
  (see [`03-financial-model.md`](03-financial-model.md)).
- If onboarding skews to cache-heavy agent traffic → absolute margin per customer
  falls ~7×. Address it with a volume floor or separate cache pricing.

That last one is the cheapest available fix for the thin-contribution problem in
[`03-financial-model.md`](03-financial-model.md).

## Milestones

| Milestone | Gate to pass |
| --- | --- |
| M0 — Rate card published | Upstream rates verified; resale terms confirmed ✔ |
| M1 — 10 paying customers | Real `arpu_volume`, workload mix, and support cost measured |
| M2 — Support cost measured | Contribution/customer confirmed positive |
| M3 — 100 customers | Infrastructure holding under real load |
| M4 — Unit economics proven | A typical customer is contribution-positive; break-even count then derives from it |

Each gate can fail. Failing M2 means changing the price floor or the support
model before continuing to M3, not pushing through and hoping volume fixes it.
