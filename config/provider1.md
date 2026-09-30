# Provider 1

**Status:** ACTIVE — pricing verified from provider price list

> Provider pricing source of truth. Facts recorded here are transcribed into
> `apikita.toml`. This file is the raw record; the TOML is the compiled
> output. When a price changes, update this file first, then the TOML.
>
> **FX:** the upstream list is quoted in **CNY per 1M tokens**. Convert with
> `IDR = CNY x 2,676.78` (Google Finance, 24 Sept 21:00 UTC). Re-derive on any
> material move. Do not hand-edit derived IDR without updating that rate.

## Identity

| Field | Value |
| --- | --- |
| Role | Reseller (NOT DeepSeek direct) |
| Price list currency | CNY per 1M tokens |
| Endpoint URL | _not yet recorded_ |
| Resale terms | **Confirmed permitted** `[per provider]` — see below |
| Concurrency allowance | **10x an ordinary account**, granted at approval |
| Account/flat-fee notes | _not yet recorded_ |

**How the resale terms were established.** By direct conversation with the
reseller, not by reading a posted document — this provider publishes no
resale-terms page, so the terms were settled in the negotiation itself and the
outcome is the approval. Two things were confirmed:

1. **Resale is permitted.** The `[per provider]` marker is the point that this
   permission is scoped here and carries nowhere else: `config/README.md:28`
   states that a permission at one provider is not a permission at another.
2. **The account is approved for 10x the concurrency limit of an ordinary
   account**, as a starting allowance.

Recorded because a bare "Confirmed permitted" is a claim a reader cannot audit.
The distinction matters for the same reason `weight = 0` does elsewhere in this
tree: what a provider has actually agreed to and what the config assumes must be
the same statement, or the config is asserting a permission nobody granted.

## Price list (CNY per 1M tokens)

Transcribed from the provider's pricing table.

| Model ID | Version | Class | Off-peak | Peak (2x) |
| --- | --- | --- | ---: | ---: |
| `deepseek-flash` | DeepSeek-V4.1-Flash-0910 | Input | ¥0.50 | ¥1.00 |
| | | Output | ¥2.00 | ¥4.00 |
| | | Cache read | ¥0.01 | ¥0.02 |
| `deepseek-v4-flash` | DeepSeek-V4-Flash-0731 | Input | ¥0.50 | ¥1.00 |
| | | Output | ¥2.00 | ¥4.00 |
| | | Cache read | ¥0.01 | ¥0.02 |
| `deepseek-v4-pro` | DeepSeek-V4-Pro-0813 | Input | ¥2.25 | ¥4.50 |
| | | Output | ¥6.75 | ¥13.50 |
| | | Cache read | ¥0.075 | ¥0.15 |

**In use:** `deepseek-flash` and `deepseek-v4-flash` are the routed models.
`deepseek-v4-pro` has **moved to offered** and is now registered in
`apikita.toml` with `weight = 0.0` on every endpoint, so it is exposed but
never routed. See the scope note below, which has been revisited.

### Derived IDR (x 2,676.78)

| Model | Class | Off-peak | Peak |
| --- | --- | ---: | ---: |
| flash / v4-flash | Input | 1,338.39 | 2,676.78 |
| | Output | 5,353.56 | 10,707.12 |
| | Cache read | 26.77 | 53.54 |
| v4-pro | Input | 6,022.76 | 12,045.50 |
| | Output | 18,068.27 | 36,136.53 |
| | Cache read | 200.76 | 401.52 |

## Billing periods

- **Off-peak = exactly half of peak.** The provider's "Peak 2x" label is literal.
- **Peak:** 01:00–04:00 and 06:00–10:00 UTC, Mon–Fri, excluding Chinese public holidays.
- **Everything else is off-peak**, including weekends and Chinese holidays in full.
- Peak is therefore ~35 of 168 weekly hours (~21%).

## Caveats

**The "Official $" comparison column is not trustworthy.** Every ratio in it
resolves to exactly **3.33 CNY/USD** — markets sit near 6.7. It is a fixed
scaling used to render a "Save 50%" badge, not a currency conversion. Ignore the
badge.

Measured against DeepSeek's real published list, the actual discount is
**~46%**, consistent across all three models. That is still a genuine saving;
it is simply 46%, not 50%.

## Scope decision

**Original decision (superseded — kept for the record):** `deepseek-v4-pro` is
**not offered**. Its output costs ~3.4x flash output, and at the markup needed
to be profitable it may not undercut what customers pay today. Flash is where
the arbitrage is. Revisit only with evidence that a pro tier sells.

**Revisited:** the decision has been reopened and `deepseek-v4-pro` is now
**registered** in `apikita.toml` as a real third model. It is priced from the
verified figures in the table above (derived IDR in the table at :44-51). It is
registered with `weight = 0.0` on every endpoint — exposed but never routed —
so the earlier commercial objection is not yet answered by traffic; the model
exists in config and on the marketing page, and routing it remains a separate,
deliberate decision.

## Unverified placeholders — not purchased, not routed

> **These are MOCK placeholders. Nothing in this section is real.** No provider
> was consulted for any of these models, no resale terms were read or agreed,
> and **no price below has been verified against anything**. The rows exist so
> that `apikita.toml` and the website ticker have a legible shape for models
> that are not yet purchasable. They are registered with `weight = 0.0` on
> every endpoint and therefore can never be routed. Do not bill against them,
> do not quote them to a customer, and do not treat any figure here as a cost
> basis. Delete this whole section the moment a real price list arrives.

The columns deliberately match the verified table above so the structure is
legible, but the values are invented:

| Model ID | Version | Class | Off-peak | Peak (2x) |
| --- | --- | --- | ---: | ---: |
| `dummy-glm-5.3-flash` | PLACEHOLDER — invented | Input | ¥0.40 | ¥0.80 |
| | | Output | ¥1.60 | ¥3.20 |
| | | Cache read | ¥0.008 | ¥0.016 |
| `dummy-glm-5.2` | PLACEHOLDER — invented | Input | ¥1.20 | ¥2.40 |
| | | Output | ¥4.80 | ¥9.60 |
| | | Cache read | ¥0.06 | ¥0.12 |
| `dummy-qwen-4-max` | PLACEHOLDER — invented | Input | ¥3.00 | ¥6.00 |
| | | Output | ¥12.00 | ¥24.00 |
| | | Cache read | ¥0.15 | ¥0.30 |

## Open items

- [ ] Record the actual endpoint base URL.
- [ ] Record the concurrency ceiling per key (flash allows 2500 concurrent on
      DeepSeek's own API; a reseller key will differ).
- [ ] Record any account-level fees or minimums.
- [ ] Confirm observed peak/off-peak behaviour against a real invoice.
