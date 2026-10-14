# Reconciliation gate check

Verifies [`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh) — the **launch Gate 2
money check**.

**Why it exists.** `docs/launch-checklist.md:277` calls the reconciliation query *"the
single best guard against silently wrong money"*. The script was **mounted** into the
scheduler smoke and therefore *reachable* from CI — which is why a grep for `reconcile`
found matches and it looked covered — but **no step ever ran it**.

> **A mounted tool is not a run tool.**

And a gate that cannot fail is worse than no gate, because the tick beside it means
something. So the check proves the direction that matters:

| Scenario | Expected |
| --- | --- |
| A wallet disagreeing with its ledger sum | exit **1**, and the account is **named** |
| Ledger money with **no** `wallets` row | exit **1**, marked `NO WALLET ROW` |
| A **consistent** ledger | exit **0** — the control |
| A stranded reservation hold | exit **5**, distinct from drift |
| A non-`sqlite` DSN | exit **2** |
| A missing database file | exit **6** |

```sh
sh tools/reconcile-check/check.sh
```

The consistent-must-pass row is not decoration: without it, every other assertion would
be satisfied by a script that always exits 1. A mutation replacing the gate with exactly
that brick **is caught** by this control.

Skips loudly (exit 3) when `sqlite3` is absent, never 0.

## Mutation-tested

| Mutation | Caught by |
| --- | --- |
| `HAVING w.account_id IS NULL` dropped | the `NO WALLET ROW` assertion |
| Drift comparison flipped (`<>` → `=`) | the drift assertion |
| `FULL OUTER JOIN` weakened to `LEFT JOIN` | the `NO WALLET ROW` assertion |
| The gate replaced by an unconditional `exit 1` | the consistent-must-pass control |

## One trap worth recording

An early version of the mutation script **copied the file it was about to mutate as its
"original" restore point**. After one run the saved copy was itself mutated, so every
later measurement was taken against a broken gate and appeared to show the check
missing things it actually caught. Restore with `git checkout --`, never a copy.

That is also why the assertions **guard their own fixtures** — the orphan test counts the
rows it seeded and fails if the seed did nothing, so a test that silently tests nothing
is caught rather than trusted.