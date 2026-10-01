# Bad-migration rollback drill

The executable form of the open item in [`docs/deployment.md`](../../docs/deployment.md):
*"Rollback drill: rehearse a bad deploy and a restore before launch."*

**It rehearses the one recovery path that had no tool.** `tools/drill/drill.sh` proves a
backup restores, but it restores a **matched** source/artifact pair. It never shows that a
snapshot taken *before* a migration still yields a usable database *after* that migration
went wrong -- which is the entire content of the last rollback row in
[`docs/ci-cd.md`](../../docs/ci-cd.md), "Bad migration shipped | Restore from the snapshot;
do not hand-write a reverse migration".

```sh
sh tools/rollback/drill.sh --target rollback_scratch.db
```

## What it does

| Step | What happens |
| --- | --- |
| 1 | Safety guard on the target name, **before anything is read or created** |
| 2 | Build a SOURCE by applying the real `server/migrations/*.sql`, in name order |
| 3a | Seed a consistent ledger (one funded account, one adjustment-only) |
| 3b | **Snapshot** it with `.backup` -- not `cp` -- recording the pre-migration version |
| 4 | Apply a bad migration of the kind that **succeeds and destroys data** |
| 5 | **THE PRECONDITION**: `tools/reconcile/reconcile.sh` must **detect** the damage |
| 6 | Restore the snapshot into a scratch file, timed |
| 7 | Assert integrity, restored drift, schema version, spot-check and row counts |

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | PASS -- restored, intact, reconciles, and back on the pre-migration schema version |
| 1 | FAIL -- an assertion failed |
| 2 | usage -- a bad option or a bad `--target` |
| 3 | missing -- `sqlite3`, `reconcile.sh` or the migrations directory is absent |
| 4 | db -- a `sqlite3` command failed |
| 5 | **REFUSED** -- the target looks live. Nothing was touched |
| 6 | snapshot -- missing, empty, or not a readable SQLite database |
| 7 | restore -- the restore failed, or the restored file failed `integrity_check` |
| 8 | **precondition** -- the bad migration was NOT detectable, so the drill proves nothing |
| 9 | teardown -- a scratch file could not be deleted |

**Exit 8 is not a flaky failure to retry.** It means the fixture was wrong: a migration
whose damage nothing can see would let a clean rollback be reported for a database nobody
noticed was broken.

## The bad migration, and why that one

The drill applies a **mirror-write**: it rewrites `wallets.balance_idr` from the sum of
settled deposits, ignoring debits.

Chosen over a tempting alternative, on measured evidence. The rejected candidate was
`UPDATE topups SET settled_at = NULL WHERE status='settled'`, which destroys the
credit-expiry anchor -- and **`reconcile.sh` cannot see it** (measured: exit 0 against
genuinely damaged data). A drill built on that would report a clean rollback of damage
nobody detected.

The mirror-write is right because it is all three of:

1. **Silent** -- succeeds in DDL terms, no error, schema valid, every statement commits.
   *A migration that fails loudly never ships.*
2. **Detectable** -- `reconcile.sh` exits 1 and names the account.
3. **Plausible** -- anyone consolidating "the balance is the sum of deposits" would write
   exactly this. The ledger is what makes it wrong.

## Two measured caveats

**A database built by the `sqlite3` CLI is `journal_mode=delete`, and `.backup` preserves
the source's mode.** `server/src/bin/migrate.rs` requires `wal` and refuses otherwise,
while the service binary sets WAL but does not refuse on it. A restored snapshot is
therefore a file the *migrations* binary will not touch until WAL is re-established --
which is correct for a rollback, because you restore to run the **old server**. The drill
does not silently "fix" the mode.

**Raw `sqlite3` does not create `_sqlx_migrations`.** The real runner is `sqlx`, and
applying the migration files with the CLI builds the schema only. The drill synthesises a
sqlx-shaped `_sqlx_migrations` (and it must **not** be `STRICT` -- SQLite rejects it with
`unknown datatype ... "BIGINT"`), because comparing schema *versions* is the point.

## Safety

The drill creates and deletes database files, so it must never point at the live one. A
`--target` is **required**, and the name must contain one of `scratch`, `rollback`,
`drill`, `test`, `tmp`, `temp` or `rehearsal`. These are refused outright (exit 5, nothing
touched): the `ROLLBACK_LIVE_DB` name, `apikita.db`, `server.db`, the source's basename,
and anything containing `prod`, `prd` or `live`. A name that is neither live-looking nor
scratch-looking is also refused -- the allow-list is what separates a rehearsal from an
accident.

**The drift check is not reimplemented here.** Drift is defined in exactly one place,
`tools/reconcile/reconcile.sql` driven by `tools/reconcile/reconcile.sh`, and this tool
invokes that script. A second copy of the query would be a second definition of "drift",
and two definitions is how a detector stops being trusted.

## Fault-injection hooks (for the check, not operators)

`ROLLBACK_INJECT_SNAPSHOT_VERSION`, `ROLLBACK_INJECT_RESTORED` and
`ROLLBACK_INJECT_SPOTCHECK` deliberately corrupt inputs so the check can prove the drill
*fails* when it should. They are guarded and confined to scratch targets. See
[`tools/rollback-check/`](../rollback-check/README.md) for which assertions they bind --
and, importantly, for the ones they do **not**.
