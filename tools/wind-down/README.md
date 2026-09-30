# Wind-down payout report

The **read-only half** of the wind-down runbook: who is owed what, and in which rail.

`docs/decisions.md:71` promises "Deliverable is `docs/wind-down.md` plus a read-only
report." For a while only the runbook existed, and the runbook's payout SQL was left as
markdown an operator hand-copies under pressure — the one document in this repository
whose SQL is meant to be **run**, and nothing ran it. This is that report.

**It pays nobody.** Every statement it issues is a `SELECT`.

```sh
export DATABASE_URL='sqlite://data/server.db'
export CLOSURE_USD_IDR_RATE='<the frozen JISDOR rate>'
sh tools/wind-down/report.sh
```

## What it produces

| Section | What it answers |
| --- | --- |
| **Pre-flight** | How many balances are in scope, and how much money. The number an operator needs before touching anything. |
| **Step 3, eligibility** | Every wallet above the frozen threshold, with its payout rail and — for a stablecoin rail — the payout units. This is the query `docs/wind-down.md:79-99` asks for. |
| **The remaining steps it covers** | The rest of the runbook that is a read rather than a decision. |

## Why it refuses rather than guesses

A missing, garbage or zero `CLOSURE_USD_IDR_RATE` stops the report. A rate is the one
input where a plausible default is worse than a refusal: it decides who is paid
automatically and who is paid on request, and a report that silently used `0` would
classify every balance as eligible. A bad `DATABASE_URL` is refused for the same reason.

The threshold itself is **strictly** greater than `2 x rate` (`docs/decisions.md:66`).
An off-by-one there does not crash — it quietly pays one account through the automatic
rail that should have been paid on request, or withholds one that should have been
automatic. Nobody notices until a customer does. The boundary is asserted from both
sides by [`tools/wind-down-check/`](../wind-down-check/README.md).

## Notes

Read-only is a claim, not an aspiration: `tools/wind-down-check/check.sh` asserts the
database file is **byte-identical** after the report runs.
