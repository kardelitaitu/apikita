# 01 — Market & Customer

## Market frame

This is not a "capture 1% of the global AI market" plan. The addressable market
is narrow and specific: **Indonesian software developers and small studios who
would use LLM APIs but are priced out of retail or blocked by payment friction.**

The relevant question is not "how big is the AI market" — it is "how many
Indonesian developers would switch for a 40–60% cost reduction and local payment."
That number is findable and should be measured, not assumed.

## Ideal customer profile

**Primary: the price-sensitive Indonesian indie developer or small studio.**

| Attribute | Value |
| --- | --- |
| Team size | 1–10 engineers |
| Current spend | Under ~$200/mo on LLM APIs `[ASSUMPTION]` |
| Workload | Chat, summarization, RAG, coding assistance |
| Sensitivity | High — LLM cost is a visible line item |
| Payment | Wants QRIS / bank transfer / e-wallet, not an international card |
| Tolerance for ops | Low — they will not run their own multi-provider router |

Their alternative today: pay retail with an international card they may not have,
or give up and use a weaker local option.

**Secondary: the AI-wrapper startup with steady volume.**

They care about unit economics because LLM cost is their COGS. They are more
demanding — they will ask about SLA, rate limits, and what happens when a
provider dies. They are worth more revenue but cost more support.

**Explicit non-customers:**

- Enterprises with compliance requirements. We cannot serve them and should not
  pretend otherwise.
- Customers needing frontier-model guarantees. Our supply is commodity-modality;
  the value is price, not capability.
- Anyone whose use case is abuse-adjacent. See [`05-risk.md`](05-risk.md).

## Why a customer switches

Ranked by importance. The first is the only one that reliably closes a deal:

1. **Price.** 40–60% below retail is the entire pitch. Nothing else matters if
   this is not true at the customer's usage mix.
2. **Local payment.** Removes a hard blocker for customers who literally cannot
   use the alternative.
3. **One integration, many suppliers.** Saves work they would otherwise repeat.
4. **Stability.** Failover beats a flaky single upstream — but customers only
   value this *after* they have been burned once.

## Why a customer leaves

- Upstream quality degrades and we cannot fix it.
- A price change erases the savings.
- We cut off an in-flight request for billing reasons and it looks like a bug.
- Retail incumbents drop prices to meet us.

## Competitor landscape

Four categories. We should be honest about where we are weak in each.

**1. Direct retail incumbents (OpenRouter, OpenAI, Anthropic, and regional
resellers).**
They have brand trust, compliance posture, model breadth, and support. We beat
them on price and local payment only. If any of them decides to price-match in
Indonesia, our pitch weakens to "cheaper and also less trusted."

**2. Regional Chinese aggregators.**
Closest to our model and our biggest real threat. Some already resell the same
wholesale capacity, often cheaper, sometimes without the billing correctness.
Our differentiation is reliability, IDR billing, and support — not price.
**This is the binding constraint on M:** if aggregators sit near 1.2× wholesale,
a 2.00 multiplier is priced out and 1.50 becomes the ceiling. Not yet measured.

**3. Self-hosting / the developer's own router.**
The honest competing alternative is "just do it yourself." A competent developer
can write a fallback router in a weekend. We win only if our markup is smaller
than the cost of that weekend plus ongoing maintenance — which means **our
margin ceiling is bounded by the cost of DIY**, and that is a real constraint.

**4. Doing nothing.**
The most common outcome. Many developers simply use a smaller, cheaper model and
never adopt the wholesale tier. This is our largest competitor by volume.

## Positioning statement

> For Indonesian developers who need LLM inference at the lowest possible cost,
> apikita is a prepaid, OpenAI-compatible API that resells wholesale Asian
> capacity at a fixed markup — with local payment and automatic failover.
> Unlike retail providers, we optimize for price and local accessibility rather
> than model breadth or enterprise compliance.

## Open questions to resolve before spending money

These are cheap to answer and expensive to get wrong:

- How many Indonesian developers currently pay for LLM APIs, and at what level?
- What is the actual retail price they face today, in IDR, for an equivalent model?
- What do regional Chinese aggregators charge for the same model? This bounds M.
- Do the target upstream providers permit resale in their terms? **Resolved for
  the current provider**; re-check per provider added (see [05-risk.md](05-risk.md)).
- Would customers accept a 50% markup if it is still below their current cost?
- What is the support cost per customer at this price point?
