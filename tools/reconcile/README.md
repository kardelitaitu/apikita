# Ledger reconciliation gate (launch Gate 2)

This directory holds the wallet/ledger reconciliation check required by
[Gate 2 - Money correctness](../docs/launch-checklist.md). Gate 2 requires that
**"The reconciliation query returns zero rows on production data"** - it is the
single best guard against silently wrong money.

**Gate 2 runs BOTH money checks, and this script now runs both of them.** One
query cannot see the other's failure:

| # | Check | What it catches | What it cannot see |
| - | ----- | --------------- | ------------------ |
| 1 | wallet/ledger drift (`reconcile.sql`) | `wallets.balance_idr <> SUM(ledger.delta_idr)`, **and** ledger money with no `wallets` row at all | a stranded reservation hold |
| 2 | stranded holds (`reconcile.sh`, predicate of `server/src/bin/hold-sweep.rs`) | a `reserve_<uuid>` debit with no positive row under the same ref | - |

Why both are mandatory is the whole point of the gate: a stranded hold is
**structurally invisible** to check 1 (the debit and its missing release are both
absent from the sum, so `balance_idr = SUM(delta_idr)` still holds and no row
returns), and a missing `wallets` row is invisible to a query anchored on
`wallets`. Either one is money the gate would otherwise report as accounted for.
See [the stranded-hold section](#the-check-reconciliation-cannot-make-stranded-holds).

## The invariant

Every customer's money lives in two places:

- `wallets.balance_idr` - the cached current balance for an account.
- `ledger.delta_idr` - an append-only log of every credit (topup, refund) and
  debit (usage, adjustment). The ledger is **authoritative**; `wallets` is a
  cache of it.

For every account, the cached balance must equal the sum of its ledger deltas:

```
wallets.balance_idr = SUM(ledger.delta_idr)   # per account
```

A mismatch means a credit/debit transaction did not update both consistently -
exactly the failure the append-only ledger exists to detect. No client-reachable
path may write `balance_idr` directly (see `docs/launch-checklist.md`, Gate 2);
it is always derived from ledger mutations.

**Both directions count.** The query is a `FULL OUTER JOIN`, deliberately not a
`LEFT JOIN`. Anchored on `wallets`, an account with ledger money and **no
`wallets` row at all** returned nothing - the cache missing entirely is the
worst case, not an ignorable one - and the gate reported PASSING while the money
sat in the authoritative ledger with nothing to disagree with. A ledger-only
account is reported even when its ledger sums to zero, because the cache row
itself is missing.

Every returned row carries the account id and both figures:

```
account_id|balance_idr|ledger_sum
c8c2859c-fc3b-41f4-a6ce-b0eca89d7900|NO WALLET ROW|250000
f28dae91-f881-4420-860d-69a4e7de86ec|60000|50000
```

`NO WALLET ROW` in the `balance_idr` column means ledger money with no cached
wallet. `balance_idr` is rendered as text so that case is legible rather than an
empty field.

## The check reconciliation cannot make: stranded holds

A reservation writes a negative `-reserved` ledger row, and its release writes a
positive `+reserved` row under the **same** `ref` (`reserve_<uuid>`). When the
release never lands, the debit and its missing offset are both out of the sum, so
check 1 returns **no row** and the money is silently gone.

Detection is a separate predicate with its own invariant, copied verbatim into
`reconcile.sh` from `server/src/bin/hold-sweep.rs` (`stranded_holds`, which
mirrors `db::unpaired_hold_rows`): a `reserve_%` ref with a negative row and
**no positive row under the same ref**. **Zero is the invariant.** If that
predicate ever changes, change the copy in `reconcile.sh` in the same commit -
two definitions of "stranded" is how a detector stops being trusted.

The gate reports the hold count on **every** run, passing or failing, and prints
the exact command for the authoritative sweep:

```
DATABASE_URL='<dsn>' cargo run --manifest-path server/Cargo.toml --bin hold-sweep
```

`hold-sweep` needs `DATABASE_URL` and is **report-only**: it never moves money
unless you pass `--release`, which is an opt-in operator action with an audit
trail. Nothing schedules it yet - no CI workflow, no compose service, no reference
anywhere in `.github/`, `docker-compose.yml` or `server/Dockerfile` (only a
"NOT WIRED" log line in `.docker/maintenance/entrypoint.sh`). Until it is wired,
running it is a manual step of this gate, and **a hold still unpaired at two
consecutive sweeps is an incident**: investigate the release path and credit the
account if the hold is lost.

The bound is `HOLD_MAX_AGE_SECONDS` (default `900`, matching hold-sweep's
`DEFAULT_MAX_HOLD_AGE_SECONDS`). A younger hold may be a request still in flight,
so it is reported but does not fail the gate; an older one does.

## Files

- `reconcile.sql` - the reconciliation query, derived from
  `docs/observability.md` (that document still shows the narrower `LEFT JOIN`
  form; this file is the corrected one), using the real column names from
  `server/migrations/20260925000000_initial_schema.sql`. Returns the drifting
  accounts and ledger-only accounts, with both figures.
- `reconcile.sh` - a POSIX shell runner. Applies both checks against
  `$DATABASE_URL` via `psql`, prints the results, and exits non-zero when either
  check fails, so it can gate CI.

## How to run

```sh
export DATABASE_URL='postgres://user:pass@host:5432/db'
sh tools/reconcile/reconcile.sh
```

In CI (e.g. a launch Gate 2 job), a non-zero exit fails the build.

Rows are counted from psql **stdout** only. psql **stderr** is diagnostics - a
`NOTICE` or `WARNING` on an otherwise healthy database is surfaced as
diagnostic output and never counted as drift, so a chatty Postgres cannot make
the gate red for the wrong reason. A genuinely failed psql run is exit 4, not a
warning.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Passed - zero drifting accounts and zero holds over the bound. |
| `1`  | **Drift detected** - at least one account where `balance_idr <> SUM(ledger.delta_idr)`, or ledger money with no `wallets` row. Investigate before the customer notices. |
| `2`  | `DATABASE_URL` is not set - unset, empty, or whitespace only. |
| `3`  | `psql` is not installed / not on PATH. |
| `4`  | `psql` ran but failed (connection, permissions, or SQL error). |
| `5`  | **Stranded hold** - at least one reservation hold unpaired for more than `HOLD_MAX_AGE_SECONDS` (default 900s): money left a wallet and came back nowhere. Run `hold-sweep`. |

Codes `0`-`4` keep their original meanings; `5` is additive, so an existing
consumer that only knows `0`-`4` still treats it as a failure.

## Verification status

Verified against a live PostgreSQL (the local stack database) with every exit
code exercised for real - no fabricated output. Because this host has no `psql`,
the checks ran through a shim that execs the **real** `psql` inside the
`apikita-postgres` container, so exit codes and error text are psql's own.
Covered: clean DB (0, stderr empty), corrupted wallet balance (1, naming the
account and both figures), ledger row with no wallet row (1, where the previous
`LEFT JOIN` version exited 0), a NOTICE on stderr with zero rows (0, not 1),
unset / empty / whitespace-only `DATABASE_URL` (2), absent `psql` (3),
unreachable host and nonexistent database (4), and a seeded stranded hold
(reported on a pass; exit 5 once it exceeds the bound). The fixtures were removed
in FK order and the drift query returned to zero rows afterwards.
