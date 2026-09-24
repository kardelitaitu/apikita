# Provider Documentation

Raw pricing records, one file per upstream provider. These are the **source
of truth**; `apikita.toml` is the compiled output.

| File | Provider | Status |
| --- | --- | --- |
| [provider1.md](provider1.md) | Reseller (DeepSeek models) | **Active** — prices verified |
| [provider2.md](provider2.md) | _unknown_ | Planned — failover slot, unverified |
| [provider3.md](provider3.md) | _unknown_ | Planned — failover slot, unverified |
| [provider4.md](provider4.md) | _unknown_ | Planned — failover slot, unverified |

## Workflow

1. Get the provider's price list and record it here **in its native currency**,
   with the date observed.
2. Note the FX rate if converting. CNY figures convert at **x 2,676.78**.
3. Transcribe the derived IDR into `apikita.toml`.
4. Record the concurrency ceiling per key, if published.

Change the price here first. The TOML bills customers, so a stale number in the
TOML is a billing defect, not a doc drift.

## Rules

- **Never record a price you have not read.** A guess here becomes a wrong rate
  that silently mischarges. Leave the table empty instead.
- **Resale terms are per provider.** A permission at one does not carry to
  another.
- **Record the date observed.** These lists change, often in promotional
  windows.
- **Different providers, not different accounts.** Failover across accounts of
  the same company is one failure, not two.
