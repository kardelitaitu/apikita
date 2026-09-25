# Ledger reconciliation gate (launch Gate 2)

This directory holds the wallet/ledger reconciliation check required by
[Gate 2 - Money correctness](../docs/launch-checklist.md). Gate 2 requires that
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
- `reconcile.sh` - a POSIX shell runner. Applies the SQL against `$DATABASE_URL`
  via `psql`, prints any drifting accounts, and exits non-zero when rows are
  returned so it can gate CI.

## How to run

```sh
export DATABASE_URL='postgres://user:pass@host:5432/db'
sh tools/reconcile/reconcile.sh
```

In CI (e.g. a launch Gate 2 job), a non-zero exit fails the build.

## Exit codes

| Code | Meaning |
| ---- | ------- |
| `0`  | Passed - zero drifting accounts (wallet balances reconcile). |
| `1`  | **Drift detected** - at least one account where `balance_idr <> SUM(ledger.delta_idr)`. Investigate before the customer notices. |
| `2`  | `DATABASE_URL` is not set. |
| `3`  | `psql` is not installed / not on PATH. |
| `4`  | `psql` ran but failed (connection, permissions, or SQL error). |

## Verification status

The shell script is syntax-checked with `sh -n`. Live verification against a
running PostgreSQL is **pending** - it requires a live database (`psql` and a
running Postgres instance), which was not available at authoring time.
