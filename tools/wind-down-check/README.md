# Wind-down check

Verifies the **eligibility boundary** in [`tools/wind-down/report.sh`](../wind-down/report.sh)
— the report that decides which closing balances are paid automatically and which are paid
on request.

**Why the boundary first.** `docs/decisions.md:66` settles eligibility as *"STRICTLY
greater than `2 x rate`"*. An off-by-one there does not crash: it quietly pays one account
through the automatic rail that should have been paid on request, or withholds one that
should have been automatic. Nobody notices until a customer does. So the boundary is
asserted from **both sides**, against the real migrated schema, rather than reasoned about
in a comment.

```sh
sh tools/wind-down-check/check.sh
```

## What it proves

| Direction | Assertion |
| --- | --- |
| **The boundary splits strictly** | `32000` is sub-threshold and `32001` is eligible, against the frozen rate — both sides, so a comparison weakened to `>=` or `>` is caught rather than reasoned about. |
| **Stablecoin units floor** | The payout units are floored, not rounded, so the report cannot promise a fraction of a unit it cannot send. |
| **A bad rate is refused** | Missing, garbage and zero `CLOSURE_USD_IDR_RATE` each stop the report. A silently-defaulted rate would classify every balance as eligible. |
| **A bad target is refused** | A missing or garbage `DATABASE_URL` stops it before anything runs. |
| **It changes nothing** | The database is **byte-identical** after the report — the only way to prove "read-only" is a fact rather than an intention. |

The threshold cases and the refusal cases are deliberate opposites: **without the
refusals, *"it classified the balances"* would be satisfied by a report that pays nobody
correctly and everybody wrongly.**

## Mutation-tested

| Mutation | Expected |
| --- | --- |
| The threshold comparison weakened to `>=` | caught |
| Stablecoin units rounded instead of floored | caught |
| A missing rate defaulted instead of refused | caught |
| A zero rate accepted | caught |
| The report writes to the database | caught |

## Notes

It needs a **migrated** database, because the report reads the schema the migrations
produce. It does not need a running service, and it needs no network.

Not yet wired into the five `tools/*-check/check.sh` gates CI runs; it is reproducible on
its own.
