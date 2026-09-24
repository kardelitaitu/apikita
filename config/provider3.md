# Provider 3

**Status:** PLANNED — no pricing data recorded yet

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
| Role | _unknown_ |
| Price list currency | _unknown_ |
| Endpoint URL | _unknown_ |
| Resale terms | **NOT CHECKED** — required before routing any traffic |
| Concurrency per key | _unknown_ |

## Price list

Nothing recorded. This provider exists in the config as a **failover slot**
(`weight = 0.0`), which means it is registered but never routed to.

Do not fill this table with estimates. A guessed price here becomes a wrong rate
in the TOML, and the TOML is what bills customers.

| Model ID | Class | Off-peak | Peak |
| --- | --- | ---: | ---: |
| _none recorded_ | | | |

## Why this provider matters

Failover only works if the providers are **independent**. Multiple accounts or
keys at the same upstream fail together — a termination, an outage, or a
repricing takes them all down at once. If this provider is:
- a **different company** → genuinely useful failover.
- the **same company** as provider 1 under another account → not failover, and
  should be documented as such rather than counted twice.

## Required before enabling

1. **Resale terms.** Read them. This is a per-provider check, not a one-time
   approval — see [`../docs/business/05-risk.md`](../docs/business/05-risk.md) R1.
2. **Price list**, in its native currency, with the date observed.
3. **Concurrency ceiling** per key, if published. If not, leave the config at
   `concurrency_per_key = 0` so the router learns from 429s.
4. **Model IDs** as the provider spells them, plus which of our public names
   they map to. Upstream names do not match ours and are not portable between
   providers.

## Open items

- [ ] Identity and role
- [ ] Resale terms checked
- [ ] Price list recorded with date observed
- [ ] FX basis noted if not CNY
- [ ] Model ID mapping decided
- [ ] Concurrency ceiling recorded
