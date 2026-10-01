#!/bin/sh
# Bad-migration rollback drill for apikita - the executable form of the open item in
# docs/deployment.md:
#
#   "Rollback drill: rehearse a bad deploy and a restore before launch"
#
# and of the last row of the rollback table in docs/ci-cd.md:
#
#   "Bad migration shipped | Restore from the snapshot; do not hand-write a reverse
#    migration"
#
# WHAT THIS ADDS OVER tools/drill/drill.sh
#   The restore drill proves a BACKUP restores. It restores a MATCHED source/artifact
#   pair, so it cannot show the thing a rollback actually depends on: that a snapshot
#   taken BEFORE a migration still yields a usable database AFTER that migration went
#   wrong, on the schema the OLD binary expects.
#
#   That is the whole content of "restore the snapshot", and nothing exercised it.
#
# WHAT IT DOES
#   1. build a SOURCE from the real server/migrations/*.sql, in name order
#   2. seed a consistent ledger and SNAPSHOT it            -> pre_migration_version
#   3. apply a BAD migration of the kind that SUCCEEDS AND DESTROYS DATA
#   4. prove the damage is DETECTABLE (reconcile.sh must exit 1)   <- the precondition
#   5. restore the snapshot into a scratch file, TIMED
#   6. assert: integrity ok, reconcile exit 0, schema version BACK to pre-migration,
#      row/money spot-check
#   7. tear the scratch files down, on failure too
#
# WHY THE BAD MIGRATION IS THE MIRROR-WRITE
#   It rewrites wallets.balance_idr from the sum of settled deposits, ignoring what was
#   actually debited. It is the right fixture because it is all three of:
#
#     * SILENT. It succeeds in DDL terms - no error, exit 0, the schema stays valid,
#       every statement commits. A migration that fails loudly never ships; the
#       dangerous one passes.
#     * DETECTABLE. tools/reconcile/reconcile.sh exits 1 and names the account.
#     * PLAUSIBLE. Anyone consolidating "the balance is the sum of deposits" would
#       write exactly this. The ledger is what makes it wrong.
#
#   A tempting alternative was REJECTED on measured evidence:
#   `UPDATE topups SET settled_at = NULL WHERE status='settled'` destroys the
#   credit-expiry anchor and reconcile.sh CANNOT SEE IT (measured: exit 0 against
#   genuinely damaged data, because it is not wallet/ledger money). A drill built on
#   that would report a clean rollback of damage nobody detected - worse than no drill.
#   Step 4 exists to make exactly that mistake impossible: if the damage is not
#   detectable the drill refuses to certify anything (exit 8).
#
# THE DRIFT CHECK IS NOT REIMPLEMENTED HERE
#   Drift is defined in exactly one place: tools/reconcile/reconcile.sql, driven by
#   tools/reconcile/reconcile.sh. Steps 4 and 6 INVOKE THAT SCRIPT. A second copy of
#   the query would be a second definition of "drift", and two definitions is how a
#   detector stops being trusted.
#
# SAFETY, FIRST AND LOUDEST
#   This tool creates and DELETES database files, so it must never point at the live
#   one. A --target (or ROLLBACK_TARGET) is REQUIRED, and it REFUSES (exit 5) unless
#   the name looks like a scratch instance:
#     * it equals ROLLBACK_LIVE_DB (default "apikita")           -> refuse
#     * it is apikita.db / server.db                             -> refuse
#     * it is the basename of --source                           -> refuse
#     * it contains prod / prd / live                            -> refuse
#     * otherwise the name must contain one of
#       scratch / rollback / test / tmp / temp / rehearsal       -> else refuse
#   Nothing is read, created or deleted before the guard passes.
#
# Exit codes (0/2/3/4/5 deliberately aligned with tools/drill/drill.sh so the two read
# alike):
#   0  PASS         - the snapshot restored, is intact, reconciles, and is back on the
#                     PRE-migration schema version
#   1  FAIL         - an assertion failed: drift, integrity, schema version, or the
#                     row/money spot-check
#   2  usage        - a bad option or a bad --target
#   3  missing      - a required tool is absent (sqlite3, reconcile.sh, migrations)
#   4  db           - a sqlite3 command failed (unreadable file, SQL error)
#   5  REFUSED      - the target looks like the live database. NOTHING was touched.
#   6  snapshot     - the snapshot is missing, empty, or not a readable SQLite database
#   7  restore      - the restore failed, or the RESTORED file failed integrity_check
#   8  precondition - the bad migration was NOT detectable: reconcile.sh did not report
#                     drift, so the drill proves nothing. This is NOT a flavour of
#                     failure to retry until it goes away: it says the FIXTURE was
#                     wrong, and reporting PASS from it would be a false clean sheet.
#   9  teardown     - a scratch file could not be deleted (it is still there)
#
# OBSERVED CAVEAT, MEASURED
#   A database built by the sqlite3 CLI is journal_mode=delete, and .backup PRESERVES
#   the source's mode. server/src/bin/migrate.rs REQUIRES wal and refuses otherwise,
#   while the service binary sets WAL but does not refuse on it. So a restored snapshot
#   is a file the MIGRATIONS binary will not touch until WAL is re-established - which
#   is correct for a rollback, because you restore to run the OLD server, not to migrate
#   forward again. This drill asserts schema version, integrity and reconciliation; it
#   does not silently "fix" the journal mode.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

MIGRATION_DIR="${MIGRATION_DIR:-$REPO_ROOT/server/migrations}"
RECONCILE="$REPO_ROOT/tools/reconcile/reconcile.sh"

TARGET="${ROLLBACK_TARGET:-}"
SOURCE_URL="${ROLLBACK_SOURCE_URL:-${DATABASE_URL:-}}"
LIVE_DB="${ROLLBACK_LIVE_DB:-apikita}"
BAD_MIGRATION_MODE="default"
SCRATCH_DIR="${ROLLBACK_SCRATCH_DIR:-$REPO_ROOT/.agents/rollback-scratch}"
LOG_DIR="${ROLLBACK_LOG_DIR:-$REPO_ROOT/.agents/rollback-logs}"
KEEP_SCRATCH=0

USAGE='usage: sh tools/rollback/drill.sh --target <scratch-file.db> [--bad-migration <file.sql>] [--log-dir <dir>] [--scratch-dir <dir>] [--keep-scratch]'

while [ $# -gt 0 ]; do
    case "$1" in
        --help|-h) printf '%s\n' "$USAGE"; exit 0 ;;
        --target) [ $# -ge 2 ] || { printf '%s\n' 'rollback: --target needs a value' >&2; exit 2; }; TARGET="$2"; shift 2 ;;
        --target=*) TARGET="${1#--target=}"; shift ;;
        --bad-migration) [ $# -ge 2 ] || { printf '%s\n' 'rollback: --bad-migration needs a value' >&2; exit 2; }; BAD_MIGRATION_MODE="$2"; shift 2 ;;
        --bad-migration=*) BAD_MIGRATION_MODE="${1#--bad-migration=}"; shift ;;
        --log-dir) [ $# -ge 2 ] || { printf '%s\n' 'rollback: --log-dir needs a value' >&2; exit 2; }; LOG_DIR="$2"; shift 2 ;;
        --log-dir=*) LOG_DIR="${1#--log-dir=}"; shift ;;
        --scratch-dir) [ $# -ge 2 ] || { printf '%s\n' 'rollback: --scratch-dir needs a value' >&2; exit 2; }; SCRATCH_DIR="$2"; shift 2 ;;
        --scratch-dir=*) SCRATCH_DIR="${1#--scratch-dir=}"; shift ;;
        --source) [ $# -ge 2 ] || { printf '%s\n' 'rollback: --source needs a value' >&2; exit 2; }; SOURCE_URL="$2"; shift 2 ;;
        --source=*) SOURCE_URL="${1#--source=}"; shift ;;
        --keep-scratch) KEEP_SCRATCH=1; shift ;;
        *) printf 'rollback: unknown option: %s\n%s\n' "$1" "$USAGE" >&2; exit 2 ;;
    esac
done

if [ -z "$TARGET" ]; then
    printf '%s\n' 'rollback: --target is REQUIRED' >&2
    printf '%s\n' 'rollback: this drill CREATES AND DELETES database files, so it refuses to guess one.' >&2
    printf '%s\n' "$USAGE" >&2
    exit 2
fi

LOG_FILE=""
# `.read` resolves through the NATIVE sqlite3 binary's own filesystem view, not the
# shell's. On Git Bash/MSYS a shell path is `/c/dev/...`, which the native client cannot
# open (measured: `Error: cannot open "/c/dev/apikita/..."`) even though the file plainly
# exists. cygpath converts where available; the sed fallback handles a POSIX host, where
# the path is already correct and the substitution is a no-op.
native_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$1" 2>/dev/null || printf '%s' "$1"
    else
        printf '%s' "$1" | sed 's|^/\([a-zA-Z]\)/|\1:/|'
    fi
}
say() {
    printf 'rollback: %s\n' "$*"
    if [ -n "$LOG_FILE" ]; then
        printf 'rollback: %s\n' "$*" >>"$LOG_FILE" 2>/dev/null || true
    fi
}
problem() {
    printf 'rollback: %s\n' "$*" >&2
    if [ -n "$LOG_FILE" ]; then
        printf 'rollback: %s\n' "$*" >>"$LOG_FILE" 2>/dev/null || true
    fi
}

# --- the live-looking guard, BEFORE anything is read, created or deleted ------
SOURCE_BASENAME=""
if [ -n "$SOURCE_URL" ]; then
    _p=${SOURCE_URL#sqlite://}
    _p=${_p#sqlite:}
    _p=${_p%%\?*}
    [ -n "$_p" ] && SOURCE_BASENAME=$(basename -- "$_p")
fi

LOW=$(printf '%s' "$TARGET" | tr '[:upper:]' '[:lower:]')
LIVE_LOW=$(printf '%s' "$LIVE_DB" | tr '[:upper:]' '[:lower:]')

guard() {
    if [ "$LOW" = "$LIVE_LOW" ]; then
        problem "REFUSING: target '$TARGET' IS the live database name (ROLLBACK_LIVE_DB='$LIVE_DB')"
        problem "  this drill DELETES and rewrites its target. Restoring over production is the one"
        problem "  thing docs/backup-and-restore.md forbids outright: never restore over production."
        problem "  nothing was read, created or deleted."
        return 5
    fi
    for reserved in apikita.db server.db; do
        if [ "$LOW" = "$reserved" ]; then
            problem "REFUSING: target '$TARGET' is a live-looking database filename ('$reserved')"
            problem "  the drill would DELETE it. nothing was read, created or deleted."
            return 5
        fi
    done
    if [ -n "$SOURCE_BASENAME" ] && [ "$LOW" = "$SOURCE_BASENAME" ]; then
        problem "REFUSING: target '$TARGET' IS the source database file (--source names it)"
        problem "  restoring over the source would destroy the database this drill exists to protect."
        problem "  nothing was read, created or deleted."
        return 5
    fi
    case "$LOW" in
        *prod*|*prd*|*live*)
            problem "REFUSING: target '$TARGET' looks like a LIVE database, not a scratch one"
            problem "  nothing was read, created or deleted."
            return 5
            ;;
    esac
    case "$LOW" in
        *scratch*|*rollback*|*drill*|*test*|*tmp*|*temp*|*rehearsal*) ;;
        *)
            problem "REFUSING: target '$TARGET' does not look like a scratch database"
            problem "  the name must contain one of: scratch, rollback, drill, test, tmp, temp, rehearsal"
            problem "  nothing was read, created or deleted."
            return 5
            ;;
    esac
    return 0
}

# --- required tools ----------------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    printf '%s\n' 'rollback: sqlite3 is not installed or not on PATH' >&2
    printf '%s\n' 'rollback: install the SQLite command-line shell (sqlite3) and retry' >&2
    exit 3
fi
if [ ! -f "$RECONCILE" ]; then
    printf '%s\n' 'rollback: the reconciliation gate is missing, so the rollback rehearsal did NOT run' >&2
    printf '%s\n' "rollback: the reconciliation gate is missing: $RECONCILE" >&2
    printf '%s\n' 'rollback:   steps 4 and 6 ARE tools/reconcile/reconcile.sh; this drill does not reimplement drift' >&2
    exit 3
fi
if [ ! -d "$MIGRATION_DIR" ]; then
    printf '%s\n' 'rollback: SKIPPED - no migrations directory to build a source from' >&2
    printf '%s\n' "rollback: looked for: $MIGRATION_DIR" >&2
    exit 3
fi

# --- the guard, run only once the tools are known to exist -------------------
guard
GUARD_RC=$?
if [ "$GUARD_RC" -ne 0 ]; then
    exit "$GUARD_RC"
fi

# --- scratch area ------------------------------------------------------------
# UNIQUE PER RUN. `$$` alone is NOT enough: the check runs the drill several times in
# quick succession, and a shell's PID can be reused (and on this platform `$$` inside
# `sh -c` collides across invocations). When that happened, one run's cleanup deleted a
# concurrent run's work directory, so a later run lost its log file mid-write and the
# check saw a failure that was not the drill's. The timestamp suffix makes the collision
# impossible in practice.
WORK="$SCRATCH_DIR/rbwork.$$.$(date -u +%H%M%S 2>/dev/null || echo x)"
if ! mkdir -p "$WORK" "$LOG_DIR" 2>/dev/null; then
    printf '%s\n' "rollback: could not create the scratch directory: $WORK" >&2
    exit 2
fi
TARGET_PATH="$SCRATCH_DIR/$TARGET"
rm -f "$TARGET_PATH" 2>/dev/null || true

RUN_ID=$(date -u +%Y%m%dT%H%M%SZ)
LOG_FILE="$LOG_DIR/rollback-$RUN_ID-$TARGET.log"

SOURCE_DB="$WORK/rb-source.db"
SNAPSHOT="$WORK/rb-snapshot.db"
SEED_SQL="$WORK/seed.sql"
BAD_MIG="$WORK/bad-migration.sql"
BAD_MISSING="$WORK/bad-migration-missing-reconcile.sql"
ERR="$WORK/err"
OUT="$WORK/out"

cleanup() {
    if [ "$KEEP_SCRATCH" -eq 0 ]; then
        rm -f "$TARGET_PATH" "$TARGET_PATH-journal" "$TARGET_PATH-wal" "$TARGET_PATH-shm" 2>/dev/null || true
        rm -rf "$WORK" 2>/dev/null || true
    else
        printf 'rollback: --keep-scratch: leaving %s and %s\n' "$WORK" "$TARGET_PATH"
    fi
}

# The teardown is asserted by the check, so a failure to delete must be VISIBLE and
# must not be masked by a later exit. rc is threaded explicitly rather than relying on
# the trap to preserve $?.
finish() {
    RC="$1"
    cleanup
    if [ "$RC" -eq 0 ]; then
        say "PASS - the snapshot restored, is intact, reconciles, and is back on schema"
        say "       version $PRE_MIGRATION_VERSION (the pre-migration version)"
    fi
    exit "$RC"
}

say "================= apikita BAD-MIGRATION ROLLBACK DRILL ================="
say "date_utc             $RUN_ID"
say "run_by               $(whoami 2>/dev/null || echo unknown) on $(hostname 2>/dev/null || echo unknown)"
say "target               $TARGET   (scratch file; refused if it is the live db)"
say "live_db              $LIVE_DB"
say "scratch_dir          $SCRATCH_DIR"
say "migrations           $MIGRATION_DIR"
say "log                  $LOG_FILE"
say "doc                  docs/ci-cd.md - 'Restore from the snapshot; do not hand-write a reverse migration'"

# --- step 2: build the source from the real migrations -----------------------
say "step 2 - building the SOURCE by applying the real migrations in name order"

# `sqlite3 <file> < migrations.sql` builds the SCHEMA and creates NO `_sqlx_migrations`
# table (measured - `sqlite3` is not sqlx). So the table is synthesised here, because
# the whole point of this drill is to compare schema VERSIONS, which sqlx records and
# this build does not. NOT STRICT: sqlx's own DDL is not, and a STRICT table rejects
# the version column with `unknown datatype ... "BIGINT"` (measured).
sqlite3 -bail "$SOURCE_DB" >"$OUT" 2>"$ERR" <<'SYNTH'
CREATE TABLE IF NOT EXISTS _sqlx_migrations (
    version INTEGER PRIMARY KEY AUTOINCREMENT,
    description TEXT NOT NULL,
    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    success BOOLEAN NOT NULL,
    checksum BLOB NOT NULL,
    execution_time BIGINT NOT NULL
);
SYNTH
if [ $? -ne 0 ]; then
    problem "sqlite3 failed creating the synthetic _sqlx_migrations table"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 4
fi

APPLIED=0
POST_MIGRATION_VERSION=""
for m in "$MIGRATION_DIR"/*.sql; do
    [ -f "$m" ] || continue
    base=$(basename -- "$m")
    ver=${base%%_*}
    case "$ver" in
        ''|*[!0-9]*) problem "migration filename does not start with a numeric version: $base"; finish 2 ;;
    esac
    m_native=$(native_path "$m")
    if ! sqlite3 -bail "$SOURCE_DB" ".read $m_native" >"$OUT" 2>"$ERR"; then
        problem "applying $base failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
    if ! sqlite3 -bail "$SOURCE_DB" \
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($ver, '$base', 1, X'00', 0);" \
        >"$OUT" 2>"$ERR"; then
        problem "recording $base in _sqlx_migrations failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
    APPLIED=$((APPLIED + 1))
    say "  applied $ver ($(printf '%s' "$base" | sed 's/^[0-9]*_//; s/\.sql$//'))"
    POST_MIGRATION_VERSION="$ver"
done

if [ "$APPLIED" -eq 0 ]; then
    problem "no migrations were applied, so this drill would prove nothing"
    finish 3
fi
say "source_schema_version $POST_MIGRATION_VERSION  ($APPLIED migrations applied, none of them bad)"

# --- step 3a: seed a consistent ledger ---------------------------------------
say "step 3a - seeding a consistent ledger (wallet == SUM(ledger.delta_idr))"
cat >"$SEED_SQL" <<'SEED'
INSERT INTO accounts (id, created_at, updated_at)
VALUES ('rb-a1', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
INSERT INTO wallets (account_id, balance_idr, updated_at)
VALUES ('rb-a1', 5000, '2026-01-01T00:00:00+00:00');
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('rb-a1', 5000, 'topup', 'rb-t1', 5000, '2026-01-01T00:00:00+00:00');
INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at, settled_at)
VALUES ('rb-t1', 'rb-a1', 5000, 'rb-o1', 'settled', 'midtrans',
        '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
-- An adjustment-only account: money with NO settled deposit. The mirror-write must be
-- wrong for this one too, which is what proves the fixture is not a single lucky case.
INSERT INTO accounts (id, created_at, updated_at)
VALUES ('rb-a2', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
INSERT INTO wallets (account_id, balance_idr, updated_at)
VALUES ('rb-a2', 700, '2026-01-01T00:00:00+00:00');
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES ('rb-a2', 700, 'adjustment', 'rb-adj', 700, '2026-01-01T00:00:00+00:00');
SEED
if ! sqlite3 -bail "$SOURCE_DB" ".read $(native_path "$SEED_SQL")" >"$OUT" 2>"$ERR"; then
    problem "seeding the fixture failed:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 4
fi

RB_RECONCILE_OUT=$(DATABASE_URL="sqlite://$SOURCE_DB" sh "$RECONCILE" 2>&1)
RB_RECONCILE_RC=$?
if [ "$RB_RECONCILE_RC" -ne 0 ]; then
    problem "the SEEDED source does not reconcile (exit $RB_RECONCILE_RC), so the starting"
    problem "state is already damaged and every assertion after this would be meaningless:"
    printf '%s\n' "$RB_RECONCILE_OUT" >&2
    finish 1
fi
say "  seeded source reconciles (reconcile.sh exit 0) - the starting state is clean"

# --- step 3b: snapshot BEFORE the bad migration ------------------------------
say "step 3b - SNAPSHOT the source BEFORE the migration: .backup, not cp"
say "   .backup is SQLite's online copy: transactionally consistent against a live"
say "   writer, and it folds in the WAL. A raw cp can miss committed frames still"
say "   sitting in -wal, which is the exact failure a rollback cannot afford."
if ! sqlite3 -bail "$SOURCE_DB" ".backup '$(native_path "$SNAPSHOT")'" >"$OUT" 2>"$ERR"; then
    problem "taking the pre-migration snapshot failed:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 4
fi
if [ ! -s "$SNAPSHOT" ]; then
    problem "the snapshot is missing or empty: $SNAPSHOT"
    finish 6
fi

PRE_MIGRATION_VERSION=$(sqlite3 -readonly -bail "$SNAPSHOT" \
    "SELECT COALESCE(MAX(version),0) FROM _sqlx_migrations;" 2>"$ERR")
if [ $? -ne 0 ]; then
    problem "the snapshot is not a readable SQLite database:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 6
fi
say "pre_migration_version $PRE_MIGRATION_VERSION  (recorded from the snapshot, before the bad migration)"

# --- step 4: apply the BAD migration ----------------------------------------
if [ "$BAD_MIGRATION_MODE" = "default" ]; then
    say "step 4 - applying the BAD migration to the SOURCE (schema-visible, data-destroying)"
    say "   the mirror-write: rewrites the wallet cache from deposits, ignoring debits."
    say "     * it SUCCEEDS in DDL terms - no error, exit 0, the schema stays valid,"
    say "       every statement commits. A migration that fails loudly never ships."
    say "     * it is DETECTABLE, which is step 5's job to prove."
    cat >"$BAD_MIG" <<'BAD'
-- The bad migration: a plausible consolidation that forgets the ledger.
UPDATE wallets
   SET balance_idr = (
       SELECT COALESCE(SUM(amount_idr), 0)
         FROM topups
        WHERE topups.account_id = wallets.account_id
          AND status = 'settled'
   );
BAD
    # A no-op migration, used by the check to prove step 5 can actually say "not
    # detectable" instead of rubber-stamping. Chosen to be harmless by construction.
    printf 'CREATE INDEX IF NOT EXISTS rb_noop_idx ON wallets (account_id);\n' >"$BAD_MISSING"
else
    BAD_MIG="$BAD_MIGRATION_MODE"
    if [ ! -f "$BAD_MIG" ]; then
        problem "the --bad-migration file does not exist: $BAD_MIG"
        finish 2
    fi
    say "step 4 - applying the SUPPLIED bad migration: $BAD_MIG"
fi

say "   bad migration file: $BAD_MIG"
if ! sqlite3 -bail "$SOURCE_DB" ".read $(native_path "$BAD_MIG")" >"$OUT" 2>"$ERR"; then
    problem "the bad migration FAILED LOUDLY, which is not the scenario this drill rehearses."
    problem "  a migration that errors never ships, so there is nothing to roll back. Choose a"
    problem "  migration that succeeds in DDL terms and damages data silently."
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 4
fi

# THE BAD MIGRATION IS RECORDED, like a real one. This is not bookkeeping: a migration
# that shipped WAS applied by sqlx and IS in _sqlx_migrations, and -- more importantly --
# registering it is the only thing that makes the step-7 version comparison able to fail.
# Without this row the snapshot and the restored copy of it carry the SAME version by
# construction, so the drill's defining assertion could not fire (measured: neutering the
# comparison left the drill passing). With it, the damaged source sits one version AHEAD
# of the snapshot, and the restore must come back DOWN.
BAD_VERSION="${ROLLBACK_BAD_VERSION:-20261231000000}"
if ! sqlite3 -bail "$SOURCE_DB" \
    "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES ($BAD_VERSION, 'bad-migration (rehearsal)', 1, X'00', 0);" \
    >"$OUT" 2>"$ERR"; then
    problem "could not register the bad migration's version ($BAD_VERSION):"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 4
fi
say "   bad migration registered as version $BAD_VERSION (so the source is now AHEAD of the snapshot)"

# The proof that the registration landed, read back rather than assumed: if this equals
# the pre-migration version the whole step-7 comparison is vacuous, which is exactly the
# defect this line exists to prevent.
POST_MIGRATION_VERSION=$(sqlite3 -readonly -bail "$SOURCE_DB" \
    "SELECT COALESCE(MAX(version),0) FROM _sqlx_migrations;" 2>"$ERR")
say "post_migration_version $POST_MIGRATION_VERSION  (the damaged source)"
if [ "$POST_MIGRATION_VERSION" = "$PRE_MIGRATION_VERSION" ]; then
    problem "the bad migration did NOT move the schema version, so the restored database"
    problem "  would match the snapshot trivially and step 7 would prove nothing."
    problem "  pre=$PRE_MIGRATION_VERSION post=$POST_MIGRATION_VERSION"
    finish 8
fi

# --- step 5: THE PRECONDITION - is the damage detectable? --------------------
say "step 5 - THE PRECONDITION: is the damage DETECTABLE?"
say "  invoking the repository's single drift definition: tools/reconcile/reconcile.sh"
say "  DATABASE_URL=sqlite://<damaged source> sh $RECONCILE  (exit 1 required)"
DAMAGED_OUT=$(DATABASE_URL="sqlite://$SOURCE_DB" sh "$RECONCILE" 2>&1)
DAMAGED_RC=$?
printf 'rollback:   reconcile stdout:\n' >>"$LOG_FILE"
printf '%s\n' "$DAMAGED_OUT" >>"$LOG_FILE"
if [ "$DAMAGED_RC" -eq 0 ]; then
    problem "PRECONDITION FAILED - the bad migration is NOT DETECTABLE (reconcile.sh exit 0)."
    problem "  This drill proves nothing: a fixture whose damage no check can see would let"
    problem "  a clean rollback be reported for a database nobody noticed was broken."
    problem "  reconcile.sh output was:"
    printf '%s\n' "$DAMAGED_OUT" >&2
    finish 8
fi
say "  damage DETECTED - reconcile.sh exit $DAMAGED_RC on the damaged source"
say "  (a migration the drift check cannot see is not this drill's scenario: exit 8)"

# --- step 6: restore the snapshot, TIMED -------------------------------------
say "step 6 - RESTORE the snapshot into the scratch file (timed)"

# FAULT INJECTION, for the check only. Deliberately corrupts the SNAPSHOT's recorded
# schema version before it is restored, which is the one way to make step 7's comparison
# come out wrong: the restore then lands on a version that does not match what the drill
# recorded as pre-migration. Without a hook like this the comparison is UNFALSIFIABLE --
# the snapshot and the restored copy of it are the same bytes, so they always agree, and
# a check that only reads the drill's output cannot tell a working comparison from a
# neutered one (measured: a neutered comparison survived three earlier check designs).
#
# It is guarded hard: it refuses unless the target is already a scratch name (checked in
# step 1 above) AND the value is numeric, so it cannot be pointed at anything real.
if [ -n "${ROLLBACK_INJECT_SNAPSHOT_VERSION:-}" ]; then
    case "$ROLLBACK_INJECT_SNAPSHOT_VERSION" in
        ''|*[!0-9]*)
            problem "ROLLBACK_INJECT_SNAPSHOT_VERSION must be numeric, got '$ROLLBACK_INJECT_SNAPSHOT_VERSION'"
            finish 2
            ;;
    esac
    say "FAULT INJECTION - overwriting the snapshot's schema version with $ROLLBACK_INJECT_SNAPSHOT_VERSION"
    say "  (this makes step 7's comparison FALSE on purpose; only the check sets this)"
    # `version` is the PRIMARY KEY, so a bare UPDATE collides with any row already on that
    # value (measured: "UNIQUE constraint failed: _sqlx_migrations.version"). Move the
    # highest row aside instead: that is the row MAX(version) reads, so the injected value
    # is what the comparison sees.
    if ! sqlite3 -bail "$SNAPSHOT" \
        "UPDATE _sqlx_migrations SET version = $ROLLBACK_INJECT_SNAPSHOT_VERSION
          WHERE version = (SELECT MAX(version) FROM _sqlx_migrations);" >"$OUT" 2>"$ERR"; then
        problem "the fault injection itself failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
fi

RB_START=$(date -u +%s%N 2>/dev/null || echo "")
if ! sqlite3 -bail "$TARGET_PATH" ".restore '$(native_path "$SNAPSHOT")'" >"$OUT" 2>"$ERR"; then
    problem "the restore failed:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 7
fi
RB_END=$(date -u +%s%N 2>/dev/null || echo "")
if [ -n "$RB_START" ] && [ -n "$RB_END" ]; then
    RESTORE_MS=$(( (RB_END - RB_START) / 1000000 ))
else
    RESTORE_MS=""
fi
if [ -n "$RESTORE_MS" ]; then
    say "RESTORE TIME ${RESTORE_MS}ms - the RTO for this path"
else
    say "RESTORE TIME <unmeasured: no nanosecond clock>"
fi

# integrity on the RESTORED file
INTEG=$(sqlite3 -readonly -bail "$TARGET_PATH" "PRAGMA integrity_check;" 2>"$ERR" | head -1)
if [ "$INTEG" != "ok" ]; then
    problem "PRAGMA integrity_check on the RESTORED database is not ok: $INTEG"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 7
fi
say "integrity ok (PRAGMA integrity_check on the RESTORED database)"

# --- step 7: the assertions --------------------------------------------------
say "step 7 - asserting the rollback is real"

# FAULT INJECTION, for the check only. Corrupts the RESTORED database AFTER the integrity
# check has passed, so the step-7 assertions (restored drift, schema version, spot-check)
# are exercised against a database that is genuinely wrong. Without this the step-7
# assertions are unfalsifiable in the same way step 7's comparison was: the restored copy
# is by construction a faithful copy of a clean snapshot, so drift is always zero and the
# spot-check always matches. Measured: neutering the restored-drift assertion survived
# every check design until this hook existed.
#
# `corrupt` breaks the wallet/ledger invariant; `drop` removes rows; both are confined to
# the scratch target, whose name the step-1 guard has already vetted.
if [ -n "${ROLLBACK_INJECT_RESTORED:-}" ]; then
    say "FAULT INJECTION - corrupting the RESTORED database ($ROLLBACK_INJECT_RESTORED)"
    say "  (this makes step 7's assertions FALSE on purpose; only the check sets this)"
    case "$ROLLBACK_INJECT_RESTORED" in
        drift)
            INJECT_SQL="UPDATE wallets SET balance_idr = balance_idr + 12345;"
            ;;
        drop)
            INJECT_SQL="DELETE FROM wallets;"
            ;;
        *)
            problem "ROLLBACK_INJECT_RESTORED must be 'drift' or 'drop', got '$ROLLBACK_INJECT_RESTORED'"
            finish 2
            ;;
    esac
    if ! sqlite3 -bail "$TARGET_PATH" "$INJECT_SQL" >"$OUT" 2>"$ERR"; then
        problem "the restored-database fault injection itself failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
fi

RESTORED_OUT=$(DATABASE_URL="sqlite://$TARGET_PATH" sh "$RECONCILE" 2>&1)
RESTORED_RC=$?
if [ "$RESTORED_RC" -ne 0 ]; then
    problem "ASSERTION FAILED: the restored snapshot does not reconcile (exit $RESTORED_RC)."
    problem "  the snapshot was taken from a CLEAN source, so this means the restore or the"
    problem "  snapshot is wrong - one of which is the failure this drill exists to catch:"
    printf '%s\n' "$RESTORED_OUT" >&2
    finish 1
fi
say "drift             reconcile.sh exit 0 on the restored database (zero drifting rows - THE check passes)"

POST_RESTORE_VERSION=$(sqlite3 -readonly -bail "$TARGET_PATH" \
    "SELECT COALESCE(MAX(version),0) FROM _sqlx_migrations;" 2>"$ERR")
if [ $? -ne 0 ]; then
    problem "could not read the schema version from the restored database:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    finish 7
fi
say "pre_migration_version $PRE_MIGRATION_VERSION"
say "post_restore_version  $POST_RESTORE_VERSION"
if [ "$POST_RESTORE_VERSION" != "$PRE_MIGRATION_VERSION" ]; then
    problem "SCHEMA VERSION MISMATCH - the restored database is on $POST_RESTORE_VERSION,"
    problem "  the pre-migration version was $PRE_MIGRATION_VERSION. This is THE assertion"
    problem "  that makes this a ROLLBACK rehearsal rather than a restore: the old binary"
    problem "  cannot run against a schema that moved."
    finish 1
fi

# row/money spot-check against the snapshot
#
# NOTE ON FALSIFIABILITY. `SRC_BAL` reads the SNAPSHOT and `DST_BAL` reads the RESTORED
# COPY OF THAT SAME SNAPSHOT, so on a clean run they are equal by construction and these
# two comparisons cannot fail. Measured: neutering both survived every check design until
# the injection below existed. The `spot` injection desynchronises the comparison inputs
# (it edits the SNAPSHOT after the restore has already been taken from it) so the
# assertions have something that can genuinely differ.
if [ "${ROLLBACK_INJECT_SPOTCHECK:-}" = "desync" ]; then
    say "FAULT INJECTION - desynchronising the snapshot AFTER the restore (spot-check inputs)"
    if ! sqlite3 -bail "$SNAPSHOT" "UPDATE wallets SET balance_idr = balance_idr + 7;" >"$OUT" 2>"$ERR"; then
        problem "the spot-check fault injection itself failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
    if ! sqlite3 -bail "$SNAPSHOT" "DELETE FROM accounts WHERE id = 'rb-a2';" >"$OUT" 2>"$ERR"; then
        problem "the spot-check fault injection itself failed:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        finish 4
    fi
fi

SRC_BAL=$(sqlite3 -readonly -bail "$SNAPSHOT" \
    "SELECT COALESCE(SUM(balance_idr),0) FROM wallets;" 2>"$ERR")
DST_BAL=$(sqlite3 -readonly -bail "$TARGET_PATH" \
    "SELECT COALESCE(SUM(balance_idr),0) FROM wallets;" 2>"$ERR")
say "spot_check        wallets_total source=$SRC_BAL restored=$DST_BAL"

# AN EMPTY READING IS NOT A MISMATCH. If a query fails, the variable is empty and the
# comparison below would report "the totals differ", sending the reader after bad DATA
# when the real fault is a broken MEASUREMENT. Those are different diagnoses and must not
# share a message: the first means the restored database is wrong, the second means this
# drill could not tell. Measured: the empty reading produced
# `wallets_total source=5700 restored=` reported as a spot-check failure.
for pair in "SRC_BAL:$SRC_BAL" "DST_BAL:$DST_BAL"; do
    n=${pair%%:*}
    v=${pair#*:}
    case "$v" in
        ''|*[!0-9-]*)
            problem "SPOT-CHECK COULD NOT MEASURE: $n read '$v', which is not a number."
            problem "  This is a broken MEASUREMENT, not bad data. The query against"
            problem "  $( [ "$n" = "SRC_BAL" ] && printf 'the snapshot' || printf 'the restored database' ) failed or returned nothing."
            [ -s "$ERR" ] && cat "$ERR" >&2
            finish 4
            ;;
    esac
done

if [ "$SRC_BAL" != "$DST_BAL" ]; then
    problem "SPOT-CHECK FAILED: the restored wallet total ($DST_BAL) differs from the snapshot's ($SRC_BAL)"
    finish 1
fi

SRC_ROWS=$(sqlite3 -readonly -bail "$SNAPSHOT" "SELECT COUNT(*) FROM accounts;" 2>"$ERR")
DST_ROWS=$(sqlite3 -readonly -bail "$TARGET_PATH" "SELECT COUNT(*) FROM accounts;" 2>"$ERR")
say "row_counts        accounts source=$SRC_ROWS restored=$DST_ROWS"
for pair in "SRC_ROWS:$SRC_ROWS" "DST_ROWS:$DST_ROWS"; do
    n=${pair%%:*}
    v=${pair#*:}
    case "$v" in
        ''|*[!0-9-]*)
            problem "ROW-COUNT COULD NOT MEASURE: $n read '$v', which is not a number."
            problem "  A broken measurement, not a mismatch. The query returned nothing."
            [ -s "$ERR" ] && cat "$ERR" >&2
            finish 4
            ;;
    esac
done
if [ "$SRC_ROWS" != "$DST_ROWS" ]; then
    problem "ROW-COUNT MISMATCH: accounts source=$SRC_ROWS restored=$DST_ROWS"
    finish 1
fi

say "result            PASS (exit 0)"
finish 0

