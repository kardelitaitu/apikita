# Restore drill

The executable form of [`docs/backup-and-restore.md`](../../docs/backup-and-restore.md),
sections **"The restore drill"** (the procedure) and **"Pass criteria"**.

**An untested backup is a belief.** A copy that has never been restored and
reconciled is a file, not a recovery plan. This tool converts the claim *"we can
restore"* into a measured, repeatable, logged result - and the number the doc
actually asks for: **how long we would be down**.

```sh
export DATABASE_URL='sqlite://data/server.db'   # or sqlite:///abs/path.db
sh tools/drill/drill.sh --target apikita_drill_scratch.db
```

## What it does, in the doc's order

| Doc step | What the drill does |
| --- | --- |
| 1. Provision a scratch database | **Refuses to run** unless `--target` names a scratch file (exit 5). See [The safety guard](#the-safety-guard). |
| 2. Restore the most recent backup | `.restore <artifact>` into the scratch file, **timed** |
| 3. Verify the numbers that matter | `verify.sql`: row counts, money totals, `key_hash` presence |
| 4. **THE check**: wallets must equal the ledger | `tools/reconcile/reconcile.sh`, pointed at the scratch file |
| 5. Spot-check a known account | The largest wallet in the source, compared against the restored copy |
| 6. Row counts plausible vs live | Per-table delta against the source, with a documented tolerance |
| 7. Tear down the scratch instance | The scratch `.db` file is deleted - on failure too |
| Record the drill | A timestamped log; see [Where the drill log lives](#where-the-drill-log-lives) |

## What a "dump" is now: a file copy, taken with `.backup`

The database is **embedded SQLite** - a file the API opens, named by
`DATABASE_URL`. There is no `pg_dump` and no database server, so the artifact
**is** the database file, produced with SQLite's own online-copy primitive:

```sh
sqlite3 "$SOURCE"  ".backup  '$ARTIFACT'"   # the drill does this
sqlite3 "$SCRATCH" ".restore '$ARTIFACT'"   # and this, to restore
```

**`.backup`, not `cp`.** The CLI implements `.backup` with SQLite's online backup
API, which is transactionally consistent against a live writer **and reads through
the WAL** - it sees committed transactions still sitting in the `-wal` file. A raw
`cp` of a live WAL database does not: it copies the main file, which may be
missing committed frames, and either drops the `-wal` or leaves a mismatched
pair. The drill therefore never copies with `cp`.

**`.restore`, not a second copy.** `.restore` writes the artifact's contents into
the target database - the same API in reverse. That is a real restore operation,
so `restore_ms` is a real restore time, not a file copy time. Nothing here is a
no-op: the artifact is a genuine second file, and **every check below runs against
the RESTORED file, never against the source**.

The artifact is **unencrypted**, exactly as the old `pg_dump` artifact was.
`tools/backup/backup.sh` encrypts its own artifacts; this drill restores a plain
one, and decrypting first remains the operator's step.

## The safety guard

This tool **creates and deletes** a database file. The single most important
property it has is that it cannot be pointed at production. `--target` (or
`DRILL_TARGET`) is **required**, and the name is rejected unless it looks like a
scratch instance:

| Rejected | Why |
| --- | --- |
| `--target` omitted | exit 2 - a drill without an explicit scratch target is not a drill |
| equals `DRILL_LIVE_DB` (default `apikita`) | exit 5 - **this is the live database** |
| `apikita.db`, `server.db` | exit 5 - the live database's filenames |
| the basename of the `--source` file | exit 5 - **the drill would delete the database it exists to protect** |
| contains `prod`, `prd`, `live` | exit 5 - looks live |
| contains none of `scratch`, `drill`, `test`, `tmp`, `temp`, `rehearsal` | exit 5 - cannot be shown to be scratch |
| a path, a DSN, or a name not ending `.db` | exit 2 - a target is a bare filename, not a path |

The guard is **pure string logic and runs before any file is read, created or
deleted**. A refusal is **exit 5**, distinct from every other failure, so "I
pointed the drill at production" is unmistakable in a log or a CI job:

```
drill: REFUSING: target 'apikita.db' IS the live database name (DRILL_LIVE_DB='apikita')
drill:   this drill DELETES and rewrites its target. Restoring over production is the one
drill:   thing docs/backup-and-restore.md forbids outright: never restore over production.
drill:   nothing was read, created or deleted.
drill: result       REFUSED (exit 5)
```

The PostgreSQL maintenance-database names (`postgres`, `template0`, `template1`)
are gone with the server. Their SQLite equivalent is the live database's
**filename**, which is why the source's basename is checked too. The order
matters: the live-name refusal is evaluated **before** the `.db`-suffix usage
check, so `--target apikita` is still exit 5 and not exit 2.

## THE check is not reimplemented

Drift is defined in **exactly one place** in this repository:
[`tools/reconcile/reconcile.sql`](../reconcile/reconcile.sql), driven by
[`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh). Step 8 **invokes
that script** with the scratch file as `DATABASE_URL` and takes its exit code as
the drill's drift verdict (0 = pass, 1 = drift, 5 = stranded hold, 6 = no such
file).

There is no second copy of the query in this directory, deliberately. A second
copy would be a second definition of "drift", and **two definitions is how a
detector stops being trusted** - the same argument
[`tools/reconcile/README.md`](../reconcile/README.md) makes about the stranded-hold
predicate. `verify.sql` here contains row counts, money totals and `key_hash`
presence, and no drift query at all.

Because the drill calls the gate rather than paraphrasing it, a change to the
drift definition changes the drill's verdict with no edit here.

> **The old `psql` shim is gone.** The previous version generated a `psql` that
> exec'd into the `postgres` compose service, because `reconcile.sh` called a bare
> `psql` and this host had none. `reconcile.sh` now calls the same `sqlite3` CLI
> this script uses, against the same file, so no shim and no container are needed.
> **`reconcile.sh` itself is unmodified and unowned by this tool.**

## Exit codes

Codes `3`, `4` and `6` keep their meanings from
[`tools/reconcile/reconcile.sh`](../reconcile/reconcile.sh).

| Code | Meaning |
| ---- | ------- |
| `0` | **PASS** - restore completed, `integrity_check` ok, zero drifting rows, every pass criterion met. |
| `1` | **FAIL** - a check failed: drift, integrity, row counts, spot-check, or no usable `key_hash`. |
| `2` | **usage** - no `--target`, an unknown option, a target that is not a bare `*.db` filename, a numeric knob that is not a usable number (`DRILL_ROW_TOLERANCE`, `DRILL_ROW_TOLERANCE_PCT`, `RTO_BUDGET_SECONDS` must be non-negative integers; `DRILL_KEEP_SCRATCH` must be `0` or `1`), a non-SQLite `DATABASE_URL`, or no source at all. |
| `3` | **missing tool** - `sqlite3` not on `PATH`, or `reconcile.sh`/`verify.sql` missing. |
| `4` | **database failure** - a `sqlite3` command failed (unreadable file, SQL error). |
| `5` | **REFUSED** - the target looks like the live database. **Nothing was touched.** |
| `6` | **bad artifact / no database** - the artifact or the source is missing, empty, or not a readable SQLite database. |
| `7` | **restore failed** - the restore itself failed, **or the RESTORED file failed `PRAGMA integrity_check`**. |
| `8` | **teardown failed** - the scratch file could not be deleted and **is still there**. |

`6` means "bad artifact" here and "no such database file" in `reconcile.sh` -
deliberately the same news (there is no usable database to work with), so the two
tables still read as one.

**Changed from the PostgreSQL version:** `2` now also covers a non-SQLite
`DATABASE_URL` and a missing source (there is no local default any more, see
below); `3` is about `sqlite3` rather than `psql`/`pg_restore`/`pg_dump`; `6` is
about the SQLite header and `integrity_check` rather than `pg_restore --list`;
and `7` now also covers a restored database that fails `integrity_check` - a check
the file-copy world needs and the `pg_dump` world did not.

## Options

| Option | Default | Meaning |
| --- | --- | --- |
| `--target <file.db>` | *required* | The scratch **filename** (no path). **Refused unless it is a scratch name.** |
| `--dump <file>` | fresh `.backup` | Restore this artifact instead of copying the source. |
| `--source <url>` | `DATABASE_URL` | The SQLite database to copy and spot-check against. |
| `--scratch-dir <dir>` | `${TMPDIR:-/tmp}` | Where the scratch file is created. |
| `--live-db <name>` | `apikita` | The name the guard refuses outright. |
| `--spot-account <uuid>` | largest source wallet | Which account step 5 compares. |
| `--keep-scratch` | off | Leave the scratch file in place (doc step 7 skipped). |
| `--verify-only` | off | Skip steps 2-5 and verify an **existing** database. Does not delete it. |
| `--log-dir <dir>` | `.agents/drill-logs` | Where the drill log is written. |

Environment overrides: `DRILL_TARGET`, `DRILL_DUMP`, `DRILL_SOURCE_URL` (or
`DATABASE_URL`), `DRILL_LIVE_DB`, `DRILL_SCRATCH_DIR`, `DRILL_ROW_TOLERANCE`
(default `0`), `DRILL_ROW_TOLERANCE_PCT` (default `5`),
`DRILL_SPOT_ACCOUNT_ID`, `DRILL_KEEP_SCRATCH`, `RTO_BUDGET_SECONDS`
(default `14400`, the documented 4-hour RTO).

### There is no source default any more

`tools/backup/backup.sh` and `tools/alert/check-alerts.sh` fall back to the local
development database when `DATABASE_URL` is unset, because they run from cron and
the alternative to the default is *no backup* / *no check*. **The drill does not**:
`DRILL_SOURCE_URL`/`DATABASE_URL` is required, and unset is exit 2. A drill that
silently picks a database can prove a restore of the wrong one, and unlike a
backup it produces a green result that means nothing.

## Row-count tolerance, and why it is not zero

The doc's pass criterion is *"within expected range of live"*, not *"equal to
live"*. A source database keeps accepting writes between the artifact and the
count, so an exact match is unachievable on a live system and would make the drill
red for the wrong reason. The default allows **`DRILL_ROW_TOLERANCE` (0) + 5% of
the source count** per table, and the **restore may only be BEHIND, never ahead** -
a snapshot cannot contain rows the source does not, so any inflation fails
outright.

5% still catches the failure the doc actually warns about: *"a backup job that
reports success while producing a tiny file is the classic silent failure"*. A
truncated artifact loses a whole table or most of its rows, not 5% of them. Every
metric is printed with its allowed delta so a red run is diagnosable without
re-running.

## Where the drill log lives

**This answers the open item "Where the drill log lives" in
`docs/backup-and-restore.md`.**

Logs are written to **`.agents/drill-logs/drill-<UTC timestamp>-<target>.log`**,
one file per run, and the path is printed on stdout **and** on the first lines of
the log itself. `.agents/` is gitignored (see `AGENTS.md`), which is correct for a
log that contains row counts and account ids: **drill logs are operational
records, not source**. Nothing in this directory commits an artifact, and no
artifact is ever written anywhere but `.agents/`.

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

plus the artifact's sha256 and size, the `integrity_check` verdict on the
restored file, the row-count table, the spot-check pair, and the drift verdict
with `reconcile.sh`'s exit code.

## Verification status

**Verified by execution on 2026-09-26** against scratch SQLite databases built
from `server/migrations/20260925000000_initial_schema.sql` (fixtures under
`.agents/sqlite-port/`). Every exit code below was produced for real; no output
is fabricated.

| Scenario | Result |
| --- | --- |
| Full drill against a scratch file (real `.backup` + `.restore`) | **exit 0**, restore 0.170 s, `integrity_check=ok`, drift exit 0, spot-check match, scratch file deleted |
| Target = `apikita`, `apikita.db`, `server.db` | **exit 5**, refused before touching anything |
| Target = the `--source` file's own basename | **exit 5**, refused |
| Target = `apikita_production_drill.db`, `apikita_live_drill.db` | **exit 5**, refused as live-looking |
| Target = `mydb.db` | **exit 5**, refused as not provably scratch |
| Target = `../escape_drill.db` | **exit 2**, refused as a path |
| Target omitted | **exit 2** |
| Artifact = 9 KB of random bytes | **exit 6**, no SQLite header |
| Artifact = the first 4000 bytes of a real database | **exit 6**, `integrity_check` = `database disk image is malformed` |
| Artifact = a real database with 16 bytes overwritten mid-file | **exit 6**, `integrity_check` = malformed |
| Artifact = zero-length | **exit 6** |
| Artifact = a **valid** database with injected drift (wallet 1009 vs ledger 1000) | **exit 1**: `reconcile.sh` exit 1, the offending account named, spot-check mismatch |
| Artifact = a valid database with `key_hash=''` | **exit 1**: "KEYS MISSING ... nobody could authenticate after this restore" |
| The clean artifact again | **exit 0** - the check is not stuck red |
| `DATABASE_URL` = a leftover `postgres://` URL | **exit 2**, refused by name |
| `DATABASE_URL` unset / no `--source` | **exit 2** |
| Source file missing | **exit 6** |
| Source = a zero-length file | **exit 6**, "not a SQLite database" (see below) |
| `sqlite3` off `PATH` | **exit 3** |
| `--verify-only` on an existing scratch file | **exit 0**, and the file is **not** deleted |
| The normal run afterwards | **exit 0**, scratch file deleted |

### Mutation-checked

Each guard was removed and the bad behaviour reproduced, to prove the guard - not
something incidental - is what stops it. Every mutant ran as a **copy under
`.agents/`**; the shipped `drill.sh` sha256 was
`450e51a62808e1ce32b3abdacc3e2fbaae19e5a282c172061d512288a73da7e8` **before and
after**, so "restored" is verified, not claimed.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| The restore's `.restore` argument directory is pointed at a path that does not exist | exit 0, restore timed | **exit 7**, "the restore FAILED after 68ms" - the exit-7 path is wired, not decorative |
| `integrity_ok` returns "malformed" for the **restored** file only | exit 0 PASS | **exit 7**, "the RESTORED database failed integrity_check" - step 5b is a real gate |

### The source is header-checked, and why that matters

SQLite will happily open a **zero-length file** as a brand-new *empty* database.
Without the header check, a drill pointed at a truncated or empty source would
copy nothing, restore nothing, and then report **PASS**: 0 rows against 0 rows,
0 drifting accounts, spot-check N/A. That is exactly the silent pass this tool
exists to prevent, so the source gets the same 16-byte header check the artifact
gets. Verified: a zero-length source is **exit 6**.

### Assumed, NOT verified here

- **A production-sized database.** The fixture is ~237 KB and restores in 0.17 s.
  The 4-hour RTO is a claim about a production-sized database, and **this drill
  has not been run against one**. The number above is evidence that the
  *procedure* works and that restore time is measurable - it is **not** evidence
  that production restores inside 4 hours.
- **The restore is local, not offsite.** The artifact and the scratch file are on
  the same host. Doc section *"Offsite is not a detail"* is untouched by this tool.
- **Encryption is not exercised.** `tools/backup/backup.sh` writes encrypted
  artifacts; the drill restores a plain `.db`. Decrypting first is the operator's
  step, and the decryption time is not in `restore_ms`.
- **Identity has no separate restore procedure, because it has no separate store.**
  The identity port moved identity into the same SQLite file as the money, so this
  tool's restore of that file *is* the identity restore path - there is no second
  database to rank below the first.
- **Nothing schedules this.** No CI workflow and no compose service runs
  `drill.sh`; running it quarterly is a manual step, exactly as
  `tools/reconcile/README.md` says of `hold-sweep`.

## Files

- `drill.sh` - the drill. POSIX shell, no dependencies beyond the `sqlite3` CLI
  and the existing reconciliation gate.
- `verify.sql` - the "numbers that matter" query: row counts per key table, the
  two money totals, and `key_hash` presence. **No drift query** - see
  [THE check is not reimplemented](#the-check-is-not-reimplemented). Ported to
  SQLite: every `x::text` became `CAST(x AS TEXT)`.
