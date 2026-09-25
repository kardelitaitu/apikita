# Restore drill

The executable form of [`docs/backup-and-restore.md`](../../docs/backup-and-restore.md),
sections **"The restore drill"** (the procedure) and **"Pass criteria"**.

**An untested backup is a belief.** A dump that has never been restored and
reconciled is a file, not a recovery plan. This tool converts the claim *"we can
restore"* into a measured, repeatable, logged result - and the number the doc
actually asks for: **how long we would be down**.

```sh
sh tools/drill/drill.sh --target apikita_drill_scratch
```

## What it does, in the doc's order

| Doc step | What the drill does |
| --- | --- |
| 1. Provision a scratch Postgres | **Refuses to run** unless `--target` names a scratch database (exit 5). See [The safety guard](#the-safety-guard). |
| 2. Restore the most recent backup | `pg_restore --clean --if-exists -d <scratch> <dump>`, **timed** |
| 3. Verify the numbers that matter | `verify.sql`: row counts, money totals, `key_hash` presence |
| 4. **THE check**: wallets must equal the ledger | `tools/reconcile/reconcile.sh`, pointed at the scratch database |
| 5. Spot-check a known account | The largest wallet in the source, compared against the restored copy |
| 6. Row counts plausible vs live | Per-table delta against the source, with a documented tolerance |
| 7. Tear down the scratch instance | `DROP DATABASE ... WITH (FORCE)` - on failure too |
| Record the drill | A timestamped log; see [Where the drill log lives](#where-the-drill-log-lives) |

The dump is either `--dump <file>` (restore a specific artifact, e.g. the one
your backup job produced) or, if omitted, a fresh `pg_dump -Fc` of the source
taken by this run. Both are preflighted with `pg_restore --list` first: **a dump
that cannot be listed is a FAILURE, not a warning**, because a truncated dump is
the classic backup that "succeeds" and restores nothing.

## The safety guard

This tool **creates and DROPs** a database. The single most important property it
has is that it cannot be pointed at production. `--target` (or
`DRILL_TARGET`) is **required**, and the name is rejected unless it looks like a
scratch instance:

| Rejected | Why |
| --- | --- |
| `--target` omitted | exit 2 - a drill without an explicit scratch target is not a drill |
| equals `DRILL_LIVE_DB` (default `apikita`) | exit 5 - **this is the live database** |
| `postgres`, `template0`, `template1` | exit 5 - PostgreSQL maintenance databases |
| contains `prod`, `prd`, `live` | exit 5 - looks live |
| contains none of `scratch`, `drill`, `test`, `tmp`, `temp`, `rehearsal` | exit 5 - cannot be shown to be scratch |
| not a bare SQL identifier | exit 2 - quotes, dashes or a DSN are not a database name |

The guard is **pure string logic and runs before any database is contacted,
created or dropped**. A refusal is **exit 5**, distinct from every other failure,
so "I pointed the drill at production" is unmistakable in a log or a CI job:

```
drill: REFUSING: target 'apikita' IS the live database name (DRILL_LIVE_DB='apikita')
drill:   this drill DROPs and recreates its target. Restoring over production is the one
drill:   thing docs/backup-and-restore.md forbids outright: "never restore over production".
drill:   nothing was contacted, created or dropped.
drill: result       REFUSED (exit 5)
```

## THE check is not reimplemented

Drift is defined in **exactly one place** in this repository:
[`tools/reconcile/reconcile.sql`](../reconcile/reconcile.sql), driven by
[`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh). Step 8 **invokes
that script** with the scratch DSN as `DATABASE_URL` and takes its exit code as
the drill's drift verdict (0 = pass, 1 = drift, 5 = stranded hold).

There is no second copy of the query in this directory, deliberately. A second
copy would be a second definition of "drift", and **two definitions is how a
detector stops being trusted** - the same argument
[`tools/reconcile/README.md`](../reconcile/README.md) makes about the stranded-hold
predicate. `verify.sql` here contains row counts, money totals and `key_hash`
presence, and no drift query at all.

Because the drill calls the gate rather than paraphrasing it, a change to the
drift definition changes the drill's verdict with no edit here.

> **This host has no `psql` on PATH.** `reconcile.sh` calls a bare `psql`, so
> when the host client is absent the drill generates a `psql` shim that execs the
> real `psql` inside the `postgres` compose service and puts it first on
> `PATH` - the same technique `tools/reconcile/README.md` documents for
> verifying that gate on this host. The shim preserves argv exactly, and
> translates `-f <path>` to stdin because the container cannot see the host
> filesystem. **`reconcile.sh` itself is unmodified and unowned by this tool.**

## Exit codes

Codes `3` and `4` keep their meanings from
[`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh).

| Code | Meaning |
| ---- | ------- |
| `0` | **PASS** - restore completed, zero drifting rows, every pass criterion met. |
| `1` | **FAIL** - a check failed: drift, row counts, spot-check, or no usable `key_hash`. |
| `2` | **usage** - no `--target`, an unknown option, or a target that is not a bare identifier. |
| `3` | **missing tool** - `psql`/`pg_restore`/`pg_dump` absent with no container fallback, or `reconcile.sh`/`verify.sql` missing. |
| `4` | **database failure** - a psql command failed (connection, permissions, SQL error). |
| `5` | **REFUSED** - the target looks like the live database. **Nothing was touched.** |
| `6` | **bad dump** - missing, empty, or not a readable `pg_restore` archive. |
| `7` | **restore failed** - `pg_restore` itself failed; the restore is broken. |
| `8` | **teardown failed** - the scratch database could not be dropped and **is still there**. |

`5` and `8` are additive: a consumer that only knows `0`-`4` still treats them
as failure.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--target <db>` | *required* | The scratch database. **Refused unless it is a scratch name.** |
| `--dump <file>` | fresh `pg_dump` | Restore this artifact instead of dumping the source. |
| `--source <dsn>` | local stack DSN | The database to dump and spot-check against. |
| `--live-db <name>` | `apikita` | The name the guard refuses outright. |
| `--spot-account <uuid>` | largest source wallet | Which account step 5 compares. |
| `--keep-scratch` | off | Leave the scratch database in place (doc step 7 skipped). |
| `--verify-only` | off | Skip steps 2-5 and verify an **existing** database. Does not drop it. |
| `--log-dir <dir>` | `.agents/drill-logs` | Where the drill log is written. |

Environment overrides: `DRILL_TARGET`, `DRILL_DUMP`, `DRILL_SOURCE_DSN`,
`DRILL_LIVE_DB`, `DRILL_USER`/`DRILL_PASSWORD`/`DRILL_DB_HOST`,
`DRILL_ROW_TOLERANCE` (default `0`), `DRILL_ROW_TOLERANCE_PCT` (default `5`),
`DRILL_SPOT_ACCOUNT_ID`, `DRILL_KEEP_SCRATCH`, `RTO_BUDGET_SECONDS`
(default `14400`, the documented 4-hour RTO).

## Row-count tolerance, and why it is not zero

The doc's pass criterion is *"within expected range of live"*, not *"equal to
live"*. A source database keeps accepting writes between the dump and the count,
so an exact match is unachievable on a live system and would make the drill red
for the wrong reason. The default allows **`DRILL_ROW_TOLERANCE` (0) + 5% of the
source count** per table, and the **restore may only be BEHIND, never ahead** - a
snapshot cannot contain rows the source does not, so any inflation fails
outright.

5% still catches the failure the doc actually warns about: *"a backup job that
reports success while producing a tiny file is the classic silent failure"*. A
truncated dump loses a whole table or most of its rows, not 5% of them. Every
metric is printed with its allowed delta so a red run is diagnosable without
re-running.

## Where the drill log lives

**This answers the open item "Where the drill log lives" in
`docs/backup-and-restore.md`.**

Logs are written to **`.agents/drill-logs/drill-<UTC timestamp>-<target>.log`**,
one file per run, and the path is printed on stdout **and** on the first lines of
the log itself:

```
drill: log          /c/dev/apikita/.agents/drill-logs/drill-20260925T211629Z-apikita_drill_scratch.log
```

`.agents/` is gitignored (see `AGENTS.md`), which is correct for a log that
contains row counts and account ids: **drill logs are operational records, not
source**. Nothing in this directory commits a dump, and no dump is ever written
anywhere but `.agents/`.

Override the location with `--log-dir` or `DRILL_LOG_DIR` to point it at
whatever the operator's retention policy actually covers - the path is a
parameter precisely because **the right answer depends on where your backups
live**, which is the other open item in that document.

Each log records every field the doc's *"Record the drill"* table asks for:

| Doc field | Log line |
| --- | --- |
| Date | `date_utc` |
| Who ran it | `run_by` |
| Backup age at restore | `backup_age` (seconds before the restore started) |
| **Time to restore** | `restore_ms` - **your real RTO** |
| Result | `result` (PASS/FAIL + exit code) |

plus the dump sha256 and size, the TOC entry count, the row-count table, the
spot-check pair, and the drift verdict with `reconcile.sh`'s exit code.

## Verification status

**Verified** on this host against the local compose stack, with real output - no
fabricated results. Because the host has no `psql`, every run went through the
container path described above, so the SQL, the drift verdict and the exit codes
are PostgreSQL's own.

| Scenario | Result |
| --- | --- |
| Full drill against a scratch database | **exit 0**, restore timed, drift exit 0, spot-check match, scratch dropped |
| Target = `apikita` (the live name) | **exit 5**, refused before contacting anything |
| Target = `postgres`, `template0` | **exit 5**, refused as a maintenance database |
| Target = `apikita_production`, `apikita_live` | **exit 5**, refused as live-looking |
| Target = `mydb` | **exit 5**, refused as not provably scratch |
| One wallet balance corrupted in the scratch db | **exit 1**: `reconcile.sh` exit 1, the offending row named, spot-check mismatch |
| The same scratch db restored clean again | **exit 0** - the check is not stuck red |
| Scratch database after a PASS | dropped; no scratch database left behind |

**Measured restore time on this host: 0.8 s - 11.2 s** across runs, for a
~126 KB `pg_dump -Fc` of the local dev database (466 accounts / 212 api_keys /
16 wallets) - two to three orders of magnitude under the documented 4-hour RTO.
The spread is real and is `docker compose exec` overhead, not database work:
`restore_ms` times the whole `pg_restore` invocation, transport included, which
is the honest number for "how long would we be down".

**Assumed, NOT verified here:**

- **This is a dev-sized database.** ~126 KB and ~750 rows restores in seconds.
  The 4-hour RTO is a claim about a production-sized database, and **this drill
  has not been run against one**. The number above is evidence that the
  *procedure* works and that restore time is measurable - it is **not** evidence
  that production restores inside 4 hours.
- **The restore is local, not offsite.** The dump and the scratch database are on
  the same host. Doc section *"Offsite is not a detail"* is untouched by this
  tool: fetching the offsite copy is not part of the drill and its transfer time
  is not in `restore_ms`.
- **`pg_restore --clean --if-exists` into a freshly created database.** The
  drill `DROP`+`CREATE`s the scratch database first, so the restore never
  actually has anything to clean. Restoring over a *populated* database is not
  what this measures.
- **Encryption is not exercised.** `tools/backup/backup.sh` writes encrypted
  artifacts; the drill restores a plain `.dump`. Decrypting first is the
  operator's step, and the decryption time is not in `restore_ms`.
- **PocketBase has no restore procedure here**, and the doc's open item
  *"Whether PocketBase gets its own tested restore procedure"* stays open. This
  tool covers PostgreSQL only - the doc ranks it first, not only.
- **Nothing schedules this.** No CI workflow and no compose service runs
  `drill.sh`; running it quarterly is a manual step, exactly as
  `tools/reconcile/README.md` says of `hold-sweep`.

## Files

- `drill.sh` - the drill. POSIX shell, no dependencies beyond `psql`/`pg_restore`
  (host or container) and the existing reconciliation gate.
- `verify.sql` - the "numbers that matter" query: row counts per key table, the
  two money totals, and `key_hash` presence. **No drift query** - see
  [THE check is not reimplemented](#the-check-is-not-reimplemented).
