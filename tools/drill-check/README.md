# Restore-drill check

Verifies the **safety guard** in [`tools/drill/drill.sh`](../drill/drill.sh) — the tool
that produces the restore evidence `docs/launch-checklist.md:279` demands:

> *"A restore drill has been run. **An untested backup is a belief.**"*

**Why the guard first.** The drill **deletes a scratch database** during teardown. The
guard is the only thing standing between that behaviour and production data, and it was
exercised by nothing in CI.

```sh
sh tools/drill-check/check.sh
```

## What it proves

| Direction | Assertion |
| --- | --- |
| **The guard refuses** | Every documented class — no `--target`, the live name, `apikita.db`, `server.db`, the source basename, `prod`/`prd`/`live`, not-provably-scratch — returns its documented exit (5, or 2 for a usage error). |
| **And touches nothing** | The protected database is checked for **existence AND its rows** after *every* refusal, not once. `exit 5` with the file already gone would be the worst possible pass. |
| **A scratch target completes** | A consistent, migrated source drills through to `PASS` (exit 0) — including the real `reconcile.sh` drift verdict. |
| **A drifted source FAILS** | Without this, a drill that ignores drift would pass everything else — certifying a restore that does not reconcile. |
| **Teardown is honest** | The scratch file is gone after **both** a passing and a failing drill. |

The guard cases and the positive control are deliberate opposites: **without the control,
*"it exited 5"* would be satisfied by a tool that refuses everything.**

## Mutation-tested

| Mutation | Expected |
| --- | --- |
| The live-name comparison weakened | caught |
| The `prod`/`prd`/`live` substring check dropped | caught |
| The drift verdict no longer fails the drill | caught |
| Teardown stops deleting the scratch file | caught |

## Notes

It needs a **migrated** source, because the drill runs `verify.sql` and `reconcile.sh`
against the restored copy — a partial schema fails at step 6 for an honest reason. When
`sqlite3` is missing it skips **loudly (exit 3)**, never 0.

The guard cases restore via `git checkout --`, never a copied file: an earlier mutation
script copied the file it was about to mutate as its "original" restore point, so after
one run the saved copy was itself mutated and every later measurement was taken against a
broken tool.