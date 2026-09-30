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

Runs in CI (`Check the wind-down payout report` in
[`.github/workflows/ci.yml`](../../.github/workflows/ci.yml)). It needs `sqlite3` on
`PATH` — the same requirement as the report, and the same as `reconcile-check` and
`drill-check`. A missing `sqlite3` **skips loudly with exit 3**, never a silent pass.

## What it proves

It builds a **real** migrated database from `server/migrations/` — a hand-written minimal
schema would let the report pass against a shape the server never produces — then seeds
ten accounts covering every class, and asserts:

| Direction | Assertion |
| --- | --- |
| **The boundary splits strictly** | `32000` (`2 x rate`) is sub-threshold and `32001` is eligible, at a frozen rate of `16000`. Both sides, so a comparison weakened to `>=` is caught rather than reasoned about. |
| **The rail predicate needs `status`** | A **pending** midtrans top-up must *not* make the account Indonesian. The rule is monotone over **settled** top-ups (`docs/decisions.md:65`); a predicate that dropped `t.status = 'settled'` would classify this account as `bank_transfer` and every other assertion would still pass. |
| **The rail predicate needs `rail`** | A crypto-settled account must be `stablecoin`, and an account with **both** a settled crypto and a settled midtrans top-up must be `bank_transfer` ("ever settled midtrans"). |
| **Stablecoin units floor** | `32001 * 1000000 / 16000 = 2000062.5` must be reported as `2000062`, not `2000063`. Rounding up can create money (`docs/decisions.md:67`). |
| **A bad rate is refused** | Missing, empty, garbage (`sixteen-thousand`), decimal (`16000.5`), negative and zero rates each stop the report with exit `7`. A silently-defaulted rate would classify every balance as eligible. |
| **A bad target is refused** | A missing, whitespace-only, non-SQLite (`postgres://`) or in-memory `DATABASE_URL` is exit `2`; a missing file is exit `6`. |
| **It is read-only** | The database file is **byte-identical** after a run, and a write **injected into the report's own SQL** is refused with a read-only error and leaves the file unchanged. |
| **Empty is a report, not an error** | A database whose only balance sits exactly on the threshold exits `0` with an eligible count of `0` — not an error. |
| **It is stable** | Two runs over unchanged data produce identical output, so the report can be diffed against the payout it authorises. |

The threshold cases and the refusal cases are deliberate opposites: **without the
refusals, *"it classified the balances"* would be satisfied by a report that pays nobody
correctly and everybody wrongly.**

## Mutation-tested

Every mutation below was applied to a **copy** of `report.sh` in a scratch tree (never the
real file) and the check was required to fail. All eight are caught:

| Mutation | Result |
| --- | --- |
| Threshold `>` weakened to `>=` | caught |
| Rail predicate: `t.status = 'settled'` dropped | caught |
| Rail predicate: `t.rail = 'midtrans'` dropped | caught |
| Units rounded half-up instead of floored | caught |
| Sub-threshold lower bound `> 0` weakened to `>= 0` | caught |
| **`-readonly` removed from the sqlite3 invocation** | caught |
| Rate guard: zero rate accepted | caught |
| Rate guard: non-numeric rate accepted | caught |

### One trap worth recording

The first version of this check tested the read-only guarantee by running
`sqlite3 -readonly -bail "$DB" "UPDATE ..."` — which tests **the CLI's own flag**, not the
report's invocation of it. Measured: a mutation that deleted `-readonly` from `report.sh`
**SURVIVED**, because the report never tried to write and the file was therefore
byte-identical either way. The probe was true and proved nothing.

The fix is the injection test above: a write is spliced into the report's SQL and the run
must fail *with a read-only error*. Without `-readonly` the write succeeds, the balance
moves, and the check fails loudly. **A test of the flag is not a test of the tool.**

## Notes

It needs a **migrated** database, because the report reads the schema the migrations
produce. A migration that does not apply is a **hard failure, never a skip** — the false
green recorded at [`tools/drill-check/check.sh:92-104`](../drill-check/check.sh), where a
half-built schema still reached PASS. It does not need a running service, and it needs no
network.

Its scratch directory (`${TMPDIR:-/tmp}/apikita-wind-down-check-$$`) is removed by an
`EXIT INT TERM` trap on **every** path — pass, fail and skip (verified for all three).
