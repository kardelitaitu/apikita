# 02 — Pricing & Unit Economics

> **Operative margin: M = 2.00, set per model.** This document analyses both 1.50
> and 2.00 for comparison; the decision is recorded in
> [`docs/decisions.md`](../decisions.md). At 1.50 a 20M-token/month customer is
> contribution-negative, which is why 2.00 is the operating value.

> **Correction notice (v2).** An earlier version of this document claimed that a
> cache-heavy workload realizes ~33% margin instead of 50%, and that revenue mix
> drives the margin *rate*. That is wrong, and the error propagated into
> [`03-financial-model.md`](03-financial-model.md). A uniform multiplier applied to
> all token classes produces a uniform percentage margin on every mix — the
> blend of a constant-factor markup is that same factor. See
> [Margin is mix-invariant](#margin-is-mix-invariant) below for the arithmetic.

## Model

Cost-plus, uniform multiplier, three token classes.

```
Invoice (IDR) = M × [ (RawInput/1e6 × R_in) + (CachedInput/1e6 × R_cache) + (Output/1e6 × R_out) ]

M              = margin multiplier (range under consideration: 1.50–2.00)
R_in           = wholesale input rate, IDR per 1M tokens
R_cache        = wholesale cache-read rate, IDR per 1M tokens
R_out          = wholesale output rate, IDR per 1M tokens
```

Single transparent multiplier, applied identically to input, output, and cache
reads. We do not tier by volume at launch — tiering adds a negotiation surface
and a support burden before we have the volume to justify it.

## Rate card

> **Corrected.** This card previously showed the whitepaper's figures (input 1,100,
> output 4,400, cache 22) tagged `[UNVERIFIED]`. Those were wrong, and the
> doctrine was stale: the provider's real price list was obtained later. The card
> below is the **verified** wholesale cost.

| Token class | Off-peak (IDR/1M) | Peak (IDR/1M) | CNY source |
| --- | ---: | ---: | --- |
| Standard input (cache miss) | 1,338.39 | 2,676.78 | ¥0.50 / ¥1.00 |
| Model output | 5,353.56 | 10,707.12 | ¥2.00 / ¥4.00 |
| Cache read (KV hit) | 26.77 | 53.54 | ¥0.01 / ¥0.02 |

**Converted at 1 CNY = 2,676.78 IDR.** Re-derive on any FX move:
0
```text
IDR_rate = CNY_rate x 2676.78
```

### Peak versus off-peak — the structural change

**The earlier analysis used a single wholesale number. There is no single number.**
The provider charges **exactly double at peak**, and peak is only ~21% of the week
(01:00-04:00 and 06:00-10:00 UTC, Mon-Fri, excluding Chinese holidays).

| Basis | When used |
| --- | --- |
| **Peak** | Reservation and billing, so a request can never lose money |
| Off-peak | The actual cost for ~79% of the week |

**Pricing at peak is the safe default and is what `config/apikita.toml` does**
(`billing_basis = "peak"`). The cost of that safety is competitiveness during
the cheap 79% — the tradeoff is recorded in `decisions.md`.

### Customer prices at the two multipliers

| Token class | Peak cost | Customer @ 1.50 | Customer @ 2.00 |
| --- | ---: | ---: | ---: |
| Input | 2,676.78 | 4,015 | 5,354 |
| Output | 10,707.12 | 16,061 | 21,414 |
| Cache read | 53.54 | 80 | 107 |

**M = 2.00 is the operating value** (see `decisions.md`). At 1.50 a 20M-token/month
customer is contribution-negative against the assumed support cost.

### Why the old figures were wrong, and what it changed

The whitepaper's numbers were ~2.4x too low at peak. Every margin and break-even
figure computed from them was therefore **too optimistic about cost** — though the
error partly cancelled against an independent ARPU mistake in the financial model
(see [`03-financial-model.md`](03-financial-model.md) for its correction notice).

**The lesson worth keeping:** a rate card is not a planning assumption once a real
price list exists. Tag figures `[UNVERIFIED]` only while they are genuinely
unconfirmed, and remove the tag the moment they are checked — otherwise the tag
stops meaning anything.
### Margin is mix-invariant

A uniform factor `M` on every cost component yields a uniform percentage margin,
regardless of mix. This is algebra, not a policy choice:

```
cost  = a + b + c
rev   = M·a + M·b + M·c  =  M·(a + b + c)
GM%   = (rev - cost) / cost = (M - 1)   ← independent of a, b, and c
```

So `M = 1.50` **always** yields 50% gross margin, and `M = 2.00` always yields
100%, whatever the workload looks like. The earlier "33% blended margin" claim
was wrong.

### Absolute margin is not uniform — and this is the real constraint

What *does* vary by class is margin **per token**, and the spread is enormous:

| Class | Wholesale rate 1M | Margin @ M=2.00 | Relative |
| --- | ---: | ---: | ---: |
| Model output | 10,707.12 | 10,707 IDR | 200× |
| Standard input | 2,676.78 | 2,677 IDR | 50× |
| Cache read | 53.54 | 54 IDR | 1× |

Cache reads earn **54 IDR per million tokens**. That is, for practical purposes,
nothing — the ratio to output is unchanged at ~200x, only the absolute scale moved. The consequence is not a lower margin rate — it is that **per-customer
revenue swings enormously at identical margins**:

| Workload | Tokens | Invoice @ M=2.00 |
| --- | ---: | ---: |
| Chat (10k raw in, 2k out) | 12,000 | 96.36 IDR |
| RAG, 90% cache hit (50k ctx, 2k out) | 52,000 | 74.41 IDR |
| Agent loop (2k raw, 40k cache, 500 out) | 42,500 | 25.70 IDR |

A cache-heavy agent user consumes 3.5x the tokens of a chat user and pays ~73%
less. Margin percentage is identical; **ARPU is not**. Model customers, not
"revenue per customer."

## The cache exposure (corrected)

The earlier version framed cache misclassification as a margin-rate problem. It
is a **billing-accuracy** problem, and it is still the sharpest financial hazard
in the model.

A cached token costs 53.54 IDR per 1M and bills at 107. A raw input token costs
2,676.78 and bills at 5,353.56 — **a 50x spread on the same underlying unit**:

- Misclassifying raw input as cached -> we bill 107 instead of 5,354 and eat the
  2,677 cost ourselves. **We lose money on the transaction.**
- A ~2% misclassification rate in the wrong direction exceeds the entire markup
  on the affected tokens.

The dependency that matters: **we bill using the usage numbers the upstream
reports to us.** We do not tokenize independently. If the upstream calls a token
cached and it was raw — or simply under-reports — the customer is billed
correctly by our books and we absorb the gap silently. See
[`05-risk.md`](05-risk.md) R3. Monthly reconciliation of upstream invoices
against our billed totals is the control.

## Costs excluded from the multiplier

The multiplier covers upstream token cost **only**. These are real costs and they
are where the margin actually goes:

| Cost | Nature | Notes |
| --- | --- | --- |
| Edge hosting (HK/SG) | Fixed monthly `[ASSUMPTION]` | Required for route stability |
| Payment processing (QRIS) | ~0.7–2.0% of top-up | Negligible at the deposit sizes planned |
| Payment processing (card) | 2.9% + flat fee | **Excluded by design — see below** |
| Settlement float | Working capital | Wallet credits instantly; settlement lags T+1/T+2 |
| Failed/retried upstream calls | Variable | We pay for upstream work even when the customer is not billed |
| Support | Per-customer `[ASSUMPTION]` | The likely killer at low ARPU |
| Fraud / abuse losses | Variable | Prepaid limits but does not remove this |

### QRIS only — card is excluded deliberately

| Top-up channel | Fee on a 10k deposit | Net margin left (M=1.50, GM 3,333 IDR) |
| --- | ---: | ---: |
| QRIS @ 0.7% | 70 IDR | 3,263 IDR (97.9%) |
| QRIS @ 2.0% | 200 IDR | 3,133 IDR (94.0%) |
| Card @ 2.9% + 2,000 | 2,290 IDR | 1,043 IDR (31.3%) |

A flat per-transaction card fee of 2,000 IDR is **20% of a 10k top-up**, and it
does not scale down with the deposit. Card acceptance must not be enabled.
QRIS-linked e-wallets (GoPay, ShopeePay, DANA) ride the QRIS rail and are fine.

**A 50% gross margin is not a 50% net margin.** At the rate card above, a
20M-token/month customer generates ~17,600 IDR of gross margin at M=1.50 against
an assumed 25,000 IDR support cost — **the customer does not cover themselves.**
M=2.00 roughly doubles the margin and is closer to viable. See
[`03-financial-model.md`](03-financial-model.md).