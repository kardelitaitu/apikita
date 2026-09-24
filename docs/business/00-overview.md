# 00 — Business Overview

## One-line thesis

Asian wholesale LLM capacity is cheap and structurally hard for outsiders to
consume. We buy it at wholesale, absorb the operational difficulty, and resell it
to Indonesian developers as one reliable prepaid API at a fixed margin.

We are not building a better model. We are building a better *pipe*.

## The problem

Three frictions keep Indonesian developers off wholesale upstream capacity:

1. **Access friction** — onboarding, KYC, and payment rails on these providers
   assume a local entity. An Indonesian solo developer often cannot pay them at all.
2. **Reliability friction** — cross-border routes are unstable. Timeouts and
   mid-stream drops are normal, and a chat app cannot tolerate them.
3. **Integration friction** — every provider has its own auth, schema quirks, and
   billing surface. Supporting three of them is three integrations.

The result: developers pay retail (or Chinese retail with a VPN and a prayer)
purely to avoid operational work they have no time for.

## What we sell

One OpenAI-compatible endpoint, prepaid in IDR, that:

- routes across multiple upstream suppliers transparently
- fails over automatically when one degrades
- bills per token at a published rate, with no seat fees and no minimums
- lets a developer top up with a local payment method

The developer gets one integration and a local invoice. We get the spread.

## The economic engine

Cost-plus pricing at a fixed multiplier over **actual** upstream cost — including
the prompt-cache discount, which is where most of the margin actually lives.

```
Invoice (IDR) = M × [ (RawInput/1e6 × R_in) + (CachedInput/1e6 × R_cache) + (Output/1e6 × R_out) ]
```

where `M` is the margin multiplier (1.50–2.00) and `R_*` are wholesale rates
per million tokens. Because the multiplier is uniform, the gross margin *rate* is
uniform too — `GM% = M − 1` at any workload mix. What varies enormously is
margin *per token*: at M=2.00, output earns 10,707 IDR/1M, input 2,677, and cache reads **54**.
Correctly tracking cache hits is therefore a billing-correctness requirement, not
a margin lever — and per-customer revenue swings ~4× on identical margins.

Pricing detail and the full matrix live in [`02-pricing.md`](02-pricing.md).

## Commercial model

Payments are collected inside the Telegram bot via **dynamic QRIS through
Midtrans** (Snap, server webhook), settling to a bank account. Deposits are
**prepaid and non-refundable**, with **10,000 IDR as the re-top-up minimum** and
a higher first deposit. Supply is a confirmed-resale-permitted Chinese provider,
funded prepaid.

## Why this can work

- **Cost basis is the moat.** Our wholesale input cost is far below global
  retail. A 50% markup still lands under what the customer would otherwise pay.
- **The hard part is operational, not intellectual.** Routing, failover, and
  billing correctness are unglamorous engineering problems that competitors
  correctly assess as not worth their time.
- **Prepaid removes credit risk.** Customers fund their wallet before consuming.
  We never extend credit, so a bad month cannot turn into a receivable we cannot collect.

## What would make this fail

Stated up front, because the plan is only credible if it names its own
disqualifiers. The dominant one is that **this business is built on other
companies' tolerance of resale**, and that tolerance can end without notice.

Full treatment in [`05-risk.md`](05-risk.md). Short version:

1. **Support cost per customer may exceed the margin some workloads generate.**
   At M=2.00 a chat-shaped 20M-token/month customer contributes ~55,000 IDR, but a
   **cache-heavy one is negative at any markup**. Support cost is still unmeasured.
2. Wholesale pricing is promotional and can be repriced upward at will.
3. A price war against well-funded retail incumbents could compress the spread
   to nothing while we carry the operational cost.

## Document map

| Doc | Covers |
| --- | --- |
| [01-market.md](01-market.md) | Market size, ICP, competitor landscape |
| [02-pricing.md](02-pricing.md) | Pricing model, margin matrix, unit economics |
| [03-financial-model.md](03-financial-model.md) | Parameterized break-even and scenario model |
| [04-gtm.md](04-gtm.md) | Launch sequence, channels, milestones |
| [05-risk.md](05-risk.md) | Legal, ToS, operational, and financial risk |

## Confidence labels

Numbers in these docs carry an explicit tag:

- `[UNVERIFIED]` — sourced only from the whitepaper or an assumption; must be
  checked before it is used to make a decision.
- `[ASSUMPTION]` — a planning input with no source; the model is parameterized
  on it and it is expected to change.
- `[VERIFIED]` — confirmed against a provider's live price list or a real
  invoice. Nothing in this doc set is currently tagged this way.