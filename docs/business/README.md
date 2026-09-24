# Business Documentation

Commercial plan for apikita. Written before any code, deliberately — the
business layer constrains what the system needs to do, and several decisions
here can invalidate the technical plan entirely.

Start with [`00-overview.md`](00-overview.md). If you read only one other, read
[`05-risk.md`](05-risk.md).

| Doc | Covers |
| --- | --- |
| [00-overview.md](00-overview.md) | Vision, thesis, what we sell, why it can fail |
| [01-market.md](01-market.md) | Market frame, ICP, competitors, positioning |
| [02-pricing.md](02-pricing.md) | Pricing model, rate card, unit economics, cache trap |
| [03-financial-model.md](03-financial-model.md) | Parameterized contribution, break-even, sensitivity |
| [04-gtm.md](04-gtm.md) | Launch sequence, channels, milestones, pricing levers |
| [05-risk.md](05-risk.md) | Legal/ToS, repricing, billing, abuse, kill criteria |

## What changed in v2

Two corrections, and the second is serious:

1. v1 claimed cache-heavy workloads realize ~33% margin instead of 50%. Wrong: a
   uniform multiplier yields a **uniform** margin percentage (`GM% = M - 1`).
2. v1 and v2 stated **revenue per customer ~5× too high**, carried over from the
   whitepaper's worked example rather than computed from the rate card. Every
   break-even figure in those versions is void.

**Corrected again in v4** — the earlier corrections used whitepaper rates that were
~2.4x too low at peak. With the verified rates and the decided **M = 2.00**:

| Workload at 20M tokens/month | Contribution @ M=2.00 |
| --- | ---: |
| Chat-shaped | **+55,175 IDR** |
| Mixed | **+10,678 IDR** |
| Cache-heavy | **−14,101 IDR** |

**So the business is viable for chat and mixed workloads, but cache-heavy usage
loses money at any markup.** The decisive remaining input is still **support cost
per customer**, which is an assumption. See
[`03-financial-model.md`](03-financial-model.md).

## Reading the numbers

Every figure carries a confidence tag:

- `[UNVERIFIED]` — from the whitepaper or an assumption; check before deciding.
- `[ASSUMPTION]` — planning input with no source; the model is parameterized on it.
- `[VERIFIED]` — confirmed against a live price list or real invoice. **Nothing
  is currently tagged this way.**

Treat every number in this folder as a placeholder until it is not.

## Status of the gating questions

| Question | Status |
| --- | --- |
| Upstream permits resale? | **Resolved — permitted** (re-check per provider) |
| Payment rail | **Decided — Midtrans, dynamic QRIS, Snap + server webhook** |
| Refund policy | **Decided — prepaid, non-refundable** |
| Deposit floor | Decided — 10,000 IDR; first-deposit minimum still open |
| Entity (personal vs. PT) | **Open** — affects dispute posture and volume ceiling |
| Upstream rates verified | **Open** — every rate-card figure is `[UNVERIFIED]` |
| Region margin ceiling | **Open** — competitor pricing vs. M=2.00 unmeasured |
| **Support cost per customer** | **Open — the decisive input.** A 5× error moves break-even from ~260 customers to unreachable |
| Margin: M=1.50 vs 2.00 | **Open — now a viability question**, not positioning |

Remaining open items are in [`05-risk.md`](05-risk.md) R1 and
[`01-market.md`](01-market.md), not blocking in the way resale terms were.

## Relation to the technical plan

- [`docs/whitepaper.md`](../whitepaper.md) — system design that implements this model.
- [`03-financial-model.md`](03-financial-model.md) drives the server's billing
  requirements: cache-hit accounting is a revenue-correctness feature, not a
  nice-to-have.