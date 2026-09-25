# Ledger reconciliation gate (launch Gate 2)

This directory holds the wallet/ledger reconciliation check required by
[Gate 2 - Money correctness](../../docs/launch-checklist.md). Gate 2 requires that
**"The reconciliation query returns zero rows on production data"** - it is the
single best guard against silently wrong money.

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

## Files

- `reconcile.sql` - the reconciliation query, verbatim from
  `docs/observability.md`, using the real column names from
  `server/migrations/20260925000000_initial_schema.sql`. Returns the drifting
  accounts (and their wallet balance vs. ledger sum) when they exist.
- `reconcile.sh` - a POSIX shell runner. Applies the SQL to the database
  `$DATABASE_URL` names via the `sqlite3` CLI, prints any drifting accounts, and
  exits non-zero when rows are returned so it can gate CI.

## How to run

```sh
export DATABASE_URL='sqlite://data/server.db'
sh tools/reconcile/reconcile.sh
```

In CI (e.g. a launch Gate 2 job), a non-zero exit fails the build.

`sqlite3` must be on `PATH`. It is not a dependency of the Rust build, so a CI
image needs it installed explicitly — a missing `sqlite3` is exit `3`, not a
silent pass.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Passed - zero drifting accounts (wallet balances reconcile). |
| `1`  | **Drift detected** - at least one account where `balance_idr <> SUM(ledger.delta_idr)`. Investigate before the customer notices. |
| `2`  | `DATABASE_URL` is not set, is not a SQLite URL, or names an in-memory database. |
| `3`  | `sqlite3` is not installed / not on `PATH`. |
| `4`  | `sqlite3` ran but failed (unreadable file, or a SQL error). |
| `5`  | The database file `DATABASE_URL` names does not exist. |

## Two things the runner does on purpose

**It opens the database `-readonly`.** Reconciliation must never write, so a stray
`UPDATE` in `reconcile.sql` fails rather than silently moving money. On a WAL
database this needs the `-shm` file to be creatable; where the volume forbids
that, run the gate against a backup copy instead (`docs/backup-and-restore.md`).

**It refuses a non-SQLite `DATABASE_URL` loudly.** The prefix is matched with a
`case`, not stripped blindly, because a leftover `postgres://…` URL would
otherwise be rewritten into a relative path that happens to be a plausible
filename — and the gate would quietly check nothing.

## Verification status

Verified by execution, not by inspection:

- `sh -n` clean.
- Exit `0` against a migrated database with 5 wallets and 18 ledger rows.
- Exit `1` with drift injected (`+9` into one wallet) - and the drifting account
  is printed: `3e2bfcd2-…|9|0`. A check that cannot fail is not a check, so the
  detection path is proven, not just the clean path.
- Exits `2`, `3`, `4` and `5` each reproduced (unset URL, `sqlite3` absent from
  `PATH`, a file that is not a database, a missing file).
- A sqlx query string is stripped: `…/server.db?mode=ro` resolves to the same
  file.
