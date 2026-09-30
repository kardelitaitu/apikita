# Wind-down payout report

The **read-only half** of the wind-down runbook: who is owed what, and in which rail.

`docs/decisions.md:71` promises "Deliverable is `docs/wind-down.md` plus a read-only
report." For a while only the runbook existed, and the runbook's payout SQL was left as
markdown an operator hand-copies under pressure — the one document in this repository
whose SQL is meant to be **run**, and nothing ran it. This is that report.

**It pays nobody.** Every statement it issues is a `SELECT`. It writes no ledger row,
moves no money and changes no balance. The payout steps are in
[`docs/wind-down.md`](../../docs/wind-down.md) (Steps 5-8); this report stops before
Step 5 and takes no action on what it finds.

```sh
export DATABASE_URL='sqlite://data/server.db'
export CLOSURE_USD_IDR_RATE='<the frozen JISDOR rate>'
sh tools/wind-down/report.sh
```

`sqlite3` must be on `PATH`. It is not a dependency of the Rust build, so a CI image
needs it installed explicitly — a missing `sqlite3` is exit `3`, not a silent pass.

## What it produces

| Section | What it answers |
| --- | --- |
| **Pre-flight** | How many balances are in scope, and how much money. The number an operator needs before touching anything. |
| **Step 3, eligibility** | Every wallet above the frozen threshold, with its payout rail and — for a stablecoin rail — the payout units. This is the query `docs/wind-down.md:79-99` asks for. |
| **Step 4, sub-threshold** | Balances above zero but at or below the threshold. These are **not forfeited**: they are paid on request with the company covering the transfer fee. The threshold decides what is *automatic*, not what is *owed* (`docs/decisions.md:68`). |
| **Rail breakdown** | How the eligible money splits between `bank_transfer` and `stablecoin`, and the total stablecoin units. |

## The rate is supplied, never looked up

`CLOSURE_USD_IDR_RATE` is **required** and has **no default**.

`docs/decisions.md:64` freezes the rate once at wind-down start from Bank Indonesia
JISDOR and reuses it for every payout: balances are frozen at the same instant, so a
per-payout rate would value identical balances differently on the same day, and a live
FX API is "precisely what is unavailable when you are shutting down"
(`docs/wind-down.md:41`). The runbook says *"substitute the captured value, never a live
lookup"*. So the tool takes the number from the environment and refuses loudly when it is
absent or not a positive integer.

A defaulted rate would be worse than a refusal: the rate decides who is paid
automatically and who is paid on request, and a report that silently used `0` would
classify **every** balance as eligible.

The threshold is **strictly** greater than `2 x rate` (`docs/decisions.md:66`). An
off-by-one there does not crash — it quietly pays one account through the automatic rail
that should have been paid on request, or withholds one that should have been automatic.
Nobody notices until a customer does. The boundary is asserted from both sides by
[`tools/wind-down-check/`](../wind-down-check/README.md).

## A worked example

Against a scratch database migrated from `server/migrations/` and seeded with four
accounts, at a frozen rate of **16240 IDR/USD** (so the threshold is `2 * 16240 = 32480`):

```
$ export DATABASE_URL='sqlite://.agents/wd-demo/server.db'
$ export CLOSURE_USD_IDR_RATE=16240
$ sh tools/wind-down/report.sh
wind-down: CLOSURE PAYOUT REPORT (READ-ONLY)
wind-down: ==========================================================================
wind-down: THIS TOOL PAYS NOBODY. It reads and prints. It writes no ledger row, moves
wind-down: no money and changes no balance. The payout steps are in docs/wind-down.md
wind-down: (Steps 5-8); this report stops before Step 5 and takes no action on what
wind-down: it finds.
wind-down:
wind-down: FROZEN RATE
wind-down:   closure_usd_idr_rate = 16240 IDR per USD
wind-down:   source: SUPPLIED by the operator via $CLOSURE_USD_IDR_RATE. It was NOT
wind-down:     looked up. docs/decisions.md:64 freezes the rate once at wind-down start
wind-down:     (Bank Indonesia JISDOR on the wind-down date); a live FX lookup is
wind-down:     exactly what is unavailable when you are shutting down.
wind-down:   automatic threshold = balance_idr > 2 * 16240 = 32480 IDR (STRICTLY greater,
wind-down:     so a balance of exactly 32480 is sub-threshold and is NOT paid automatically)
wind-down:
wind-down: PRE-FLIGHT
wind-down:   wallets rows                        : 4
wind-down:   total balance_idr                   : 351000 IDR
wind-down:   ELIGIBLE (automatic payout)         : 2 account(s), 339000 IDR
wind-down:   SUB-THRESHOLD (on request, not lost): 1 account(s), 12000 IDR
wind-down:   zero balance (in neither list)      : 1 account(s)
wind-down:
wind-down: PAYOUT RAIL BREAKDOWN (eligible rows only)
wind-down:   bank_transfer : 1 account(s), 275000 IDR - pay balance_idr whole, no conversion
wind-down:   stablecoin    : 1 account(s), 64000 IDR - 3940886 USDC units in total
wind-down:   Every customer is Indonesian today, so this normally reads bank_transfer for
wind-down:   everyone. That is the CORRECT answer, not a stub: no crypto rail is
wind-down:   implemented, so no account has ever settled a non-midtrans top-up
wind-down:   (docs/wind-down.md:101-122).
wind-down:
wind-down: STEP 3 - ELIGIBLE FOR AUTOMATIC PAYOUT (balance_idr > 32480)
wind-down:   columns: account_id|balance_idr|payout_rail|payout_units
wind-down:   payout_units is '-' for bank_transfer (IDR is already whole) and is
wind-down:   floor(balance_idr * 1000000 / 16240) for stablecoin, rounded DOWN so a
wind-down:   payout can never create money.
a1b2c3d4-0001|275000|bank_transfer|-
a1b2c3d4-0002|64000|stablecoin|3940886
wind-down:
wind-down: STEP 4 - SUB-THRESHOLD (0 < balance_idr <= 32480)
wind-down:   columns: account_id|balance_idr
wind-down:   These are NOT forfeited and are NOT held back for a fee. Pay them on
wind-down:   request and cover the transfer fee: the threshold decides what is
wind-down:   AUTOMATIC, not what is OWED (docs/decisions.md:68). Anything genuinely
wind-down:   unclaimed stays a retained liability -- never recognised as revenue.
a1b2c3d4-0003|12000
wind-down:
wind-down: SENSITIVITY - THIS OUTPUT IS OPERATIONAL DATA, NOT SOURCE
wind-down:   It contains account ids and balances. Whether the database is encrypted
wind-down:   at rest or plaintext makes NO difference to that: this is decrypted, live
wind-down:   data the moment it is read. tools/drill/README.md frames the drill log the
wind-down:   same way ('drill logs are operational records, not source'). Treat this
wind-down:   output the way you treat the database itself: keep it out of git, out of
wind-down:   tickets and out of chat, and delete it when the payout is done.
wind-down:
wind-down: NEXT: docs/wind-down.md. Step 5 pays (bank transfer whole, stablecoin at the
wind-down:   units above); Step 6 writes one ledger row per paid account AND zeroes the
wind-down:   wallet -- both are mandatory or wallet = SUM(delta_idr) breaks; Step 7 runs
wind-down:   tools/reconcile/reconcile.sh and must return zero rows.
wind-down: OK - report complete: 2 eligible, 1 sub-threshold, 0 accounts changed
```

Three details worth reading in that output:

- **The rail is `bank_transfer` for the midtrans-settled account and `stablecoin` for the
  crypto one.** The predicate is *"ever settled a midtrans top-up"*
  (`docs/decisions.md:65`), so a **pending** midtrans top-up does not count. Today every
  customer is Indonesian and the split degenerates to `bank_transfer` for everyone; that
  is the correct answer, not a stub (`docs/wind-down.md:101-122`).
- **`payout_units` is floored, not rounded.** `64000 * 1000000 / 16240 = 3940886.7` is
  reported as `3940886`. Rounding **down** means a payout can never create money
  (`docs/decisions.md:67`).
- **The exit code is `0` even though there is money to pay out.** There being balances to
  refund is not a failure. This is a **report, not a gate**.

## This output is operational data

It contains **account ids and balances**. Whether the database is encrypted at rest or
plaintext makes no difference to that: it is decrypted, live data the moment it is read.
[`tools/drill/README.md`](../drill/README.md) frames the drill log the same way — "drill
logs are operational records, not source" — and `.agents/` is gitignored for exactly this
reason. Treat this output the way you treat the database itself: **keep it out of git,
out of tickets and out of chat, and delete it when the payout is done.**

## What it does NOT do

- **It pays nobody.** No ledger row, no wallet update, no `accounts.status` change.
- **No HTTP endpoint.** There is no `POST /api/admin/.../refund`, deliberately — see
  `docs/wind-down.md:194-200`.
- **No automation of the transfer.** Nothing initiates a payment without a human.
- **It does not look up the rate.** The rate is an input, because a live FX API is what
  is unavailable when you are shutting down.
- **It is not a gate.** It exits `0` whenever it manages to print, including when
  eligible balances exist.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Report produced. **Including** when eligible balances exist — there being money to pay out is not a failure. |
| `2`  | `DATABASE_URL` is not set (unset, empty, or whitespace only), is not a SQLite URL, or names an in-memory database. |
| `3`  | `sqlite3` is not installed / not on `PATH`. |
| `4`  | `sqlite3` ran but failed (unreadable file, or a SQL error). |
| `6`  | The database file `DATABASE_URL` names does not exist. |
| `7`  | `CLOSURE_USD_IDR_RATE` is not set, or is not a positive integer. |
| `8`  | The report is **internally inconsistent**: the SQL aggregates and the rows printed disagree, so one of the two figures would be wrong. It refuses to print a number an operator would act on rather than guessing which one. |

`1` and `5` are deliberately **unassigned**. `1` is "a check failed" in this repo's house
convention ([`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh)), and this is not
a gate, so nothing here may ever be read as one. `5` is `reconcile.sh`'s stranded-hold
code, left free so the two tools cannot be confused if an operator wraps both in one
script.

## Notes

Read-only is a claim, not an aspiration: `tools/wind-down-check/check.sh` asserts the
database file is **byte-identical** after the report runs, and injects a write into the
report's own SQL to prove the write is refused rather than merely absent.
