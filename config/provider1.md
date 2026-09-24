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
| Resale terms | **Confirmed permitted** `[per provider]` |
| Account/flat-fee notes | _not yet recorded_ |

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

**In use:** `deepseek-flash` and `deepseek-v4-flash` only. `deepseek-v4-pro` is
priced above the flash models but is **not offered** — see scope note below.

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

`deepseek-v4-pro` is **not offered**. Its output costs ~3.4x flash output, and
at the markup needed to be profitable it may not undercut what customers pay
today. Flash is where the arbitrage is. Revisit only with evidence that a pro
tier sells.

## Open items

- [ ] Record the actual endpoint base URL.
- [ ] Record the concurrency ceiling per key (flash allows 2500 concurrent on
      DeepSeek's own API; a reseller key will differ).
- [ ] Record any account-level fees or minimums.
- [ ] Confirm observed peak/off-peak behaviour against a real invoice.
