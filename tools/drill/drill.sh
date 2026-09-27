#!/bin/sh
# Restore drill for apikita - the executable form of docs/backup-and-restore.md,
# sections "The restore drill" (procedure) and "Pass criteria".
#
# WHY THIS EXISTS
#   "An untested backup is a belief." A copy that has never been restored and
#   reconciled is a file, not a recovery plan. This tool turns the claim "we can
#   restore" into a measured, repeatable, logged result:
#     restore time  -> the real RTO, which the doc asks for explicitly
#     integrity     -> PRAGMA integrity_check on the RESTORED file
#     drift rows    -> THE check: wallets must equal the ledger
#     row counts    -> the restored database is plausibly the one we copied
#     spot-check    -> a known account's balance matches the source
#
# WHAT A "DUMP" IS NOW
#   The database is embedded SQLite: a FILE the API opens, named by DATABASE_URL.
#   There is no pg_dump and no server, so the artifact IS the database file, taken
#   with SQLite's own online-copy primitive: .backup, which the CLI implements
#   with the backup API. That matters - it is transactionally consistent against a
#   live writer and it folds in the WAL, which a raw cp of a live WAL database is
#   NOT (cp can miss committed frames still sitting in the -wal file). The restore
#   is .restore, the same API in reverse. Nothing here is a no-op: the artifact is
#   a real second file, the restore really writes it, and every check below runs
#   against the RESTORED file, never against the source.
#
#   The artifact is therefore UNENCRYPTED, exactly as the old pg_dump artifact was
#   (tools/backup/backup.sh encrypts its own artifacts; this drill restores a
#   plain one, and decrypting first remains the operator's step).
#
# SAFETY, FIRST AND LOUDEST
#   This tool creates and DELETES a database FILE, so it must never point at the
#   live one. A --target (or DRILL_TARGET) is REQUIRED, and the tool REFUSES
#   (exit 5) unless the name looks like a scratch instance:
#     * it equals DRILL_LIVE_DB (default "apikita")               -> refuse
#     * it is apikita.db / server.db                              -> refuse
#     * it is the basename of the --source database file          -> refuse
#     * it contains prod / prd / live / production                -> refuse
#     * it contains none of scratch/drill/test/tmp/temp/rehearsal -> refuse
#   The guard runs BEFORE any file is read, created or deleted, and a refusal is
#   exit 5 - distinct from every other failure, so "I pointed it at production" is
#   unmistakable in a log or a CI job.
#
# WHAT IT DOES, IN THE DOC'S ORDER
#    1. guard the target (above)
#    2. obtain an artifact: --dump <file>, else a fresh .backup of the source
#    3. preflight: the artifact must BE a SQLite database (header + integrity_check)
#    4. delete any leftover scratch file, so it is clean by construction
#    5. TIME the restore: .restore the artifact into the scratch file
#    5b. integrity_check the RESTORED file - a byte-perfect copy of a corrupt
#        source is still not a restorable database
#    6. verify.sql against the scratch: row counts, money totals, key hashes
#    7. row counts vs the source, plus the spot-check account's balance
#    8. THE check: tools/reconcile/reconcile.sh with DATABASE_URL=<scratch file>
#    9. delete the scratch file
#   10. write the drill log
#
# Exit codes (3, 4 and 6 keep tools/reconcile/reconcile.sh's meanings):
#   0  PASS    - restore completed, integrity ok, zero drifting rows, every criterion met
#   1  FAIL    - a check failed: drift, integrity, row counts, spot-check, or no key hashes
#   2  usage   - no --target, a bad option, a target that is not a bare *.db
#                 filename, or a knob that is not a usable number
#   3  missing - a required tool is absent (sqlite3, reconcile.sh, verify.sql)
#   4  db      - a sqlite3 command failed (unreadable file, SQL error)
#   5  REFUSED - the target looks like the live database. NOTHING was touched.
#   6  dump    - the artifact is missing, empty, or not a readable SQLite database
#   7  restore - the restore itself failed, or the RESTORED file failed integrity_check
#   8  teardown- the scratch file could not be deleted (it is still there)
#
# 6 means "bad artifact" here and "no such database file" in
# tools/reconcile/reconcile.sh. Deliberately the same news - there is no usable
# database to work with - so the two tables still read as one.
#
# THE CHECK IS NOT REIMPLEMENTED HERE
#   Drift is defined in exactly one place: tools/reconcile/reconcile.sql, driven by
#   tools/reconcile/reconcile.sh. Step 8 INVOKES THAT SCRIPT with the scratch file
#   as DATABASE_URL. Its exit code is the drill's drift verdict (0 pass, 1 drift,
#   5 stranded hold, 6 no such file). A second copy of the query in this directory
#   would be a second definition of "drift", and two definitions is how a detector
#   stops being trusted.
#
# Verified vs assumed: see README.md next to this file.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

DRILL_TARGET="${DRILL_TARGET:-}"
DRILL_DUMP="${DRILL_DUMP:-}"
DRILL_SOURCE_URL="${DRILL_SOURCE_URL:-${DATABASE_URL:-}}"
DRILL_LIVE_DB="${DRILL_LIVE_DB:-apikita}"
DRILL_LOG_DIR="${DRILL_LOG_DIR:-$REPO_ROOT/.agents/drill-logs}"
DRILL_SPOT_ACCOUNT_ID="${DRILL_SPOT_ACCOUNT_ID:-}"
DRILL_KEEP_SCRATCH="${DRILL_KEEP_SCRATCH:-0}"
DRILL_ROW_TOLERANCE="${DRILL_ROW_TOLERANCE:-0}"
# Row counts are compared "within expected range of live" (the doc's pass
# criteria), not exactly: the source keeps accepting writes between the artifact
# and the count, so an exact match is unachievable on a live database and would
# make the drill red for the wrong reason. The default is 5% per metric, which
# still catches the failure the doc actually warns about - a truncated copy loses
# a whole table or most of its rows, not 5% of them.
DRILL_ROW_TOLERANCE_PCT="${DRILL_ROW_TOLERANCE_PCT:-5}"
DRILL_SCRATCH_DIR="${DRILL_SCRATCH_DIR:-${TMPDIR:-/tmp}}"
RECONCILE_SH="${RECONCILE_SH:-$REPO_ROOT/tools/reconcile/reconcile.sh}"
VERIFY_SQL="$SCRIPT_DIR/verify.sql"
RTO_BUDGET_SECONDS="${RTO_BUDGET_SECONDS:-14400}"

VERIFY_ONLY=0
USAGE="usage: sh tools/drill/drill.sh --target <scratch-file.db> [--dump <file>] [--verify-only] [--keep-scratch] [--log-dir <dir>]"

# Numeric knobs are refused HERE, before any file is read or written. A knob
# that only fails mid-run is the worst kind of config error: '%' in
# DRILL_ROW_TOLERANCE_PCT is the MODULO operator in shell arithmetic, so the
# step-6b expansion dies, and POSIX says an expansion error exits a
# non-interactive shell - the drill would stop mid-restore with exit 1, which
# this script's own table documents as "a check failed: drift". A typo must
# not be able to page that. tools/alert/probe.sh refuses its knobs for the
# same reason: a threshold in the message must not be a lie.
for DRILL_KNOB in DRILL_ROW_TOLERANCE DRILL_ROW_TOLERANCE_PCT RTO_BUDGET_SECONDS; do
    eval "DRILL_KNOB_VAL=\${$DRILL_KNOB:-}"
    case "$DRILL_KNOB_VAL" in
        ''|*[!0-9]*)
            printf 'drill: %s=%s is not a non-negative integer\n' "$DRILL_KNOB" "$DRILL_KNOB_VAL" >&2
            printf '%s\n' "$USAGE" >&2
            exit 2
            ;;
    esac
done
case "$DRILL_KEEP_SCRATCH" in
    0|1) ;;
    *)
        printf 'drill: DRILL_KEEP_SCRATCH=%s is not 0 or 1\n' "$DRILL_KEEP_SCRATCH" >&2
        printf '%s\n' "$USAGE" >&2
        exit 2
        ;;
esac

while [ $# -gt 0 ]; do
    case "$1" in
        --target)         [ $# -ge 2 ] || { printf '%s\n' 'drill: --target needs a value' >&2; exit 2; }; DRILL_TARGET="$2"; shift 2 ;;
        --target=*)       DRILL_TARGET="${1#--target=}"; shift ;;
        --dump)           [ $# -ge 2 ] || { printf '%s\n' 'drill: --dump needs a value' >&2; exit 2; }; DRILL_DUMP="$2"; shift 2 ;;
        --dump=*)         DRILL_DUMP="${1#--dump=}"; shift ;;
        --source)         [ $# -ge 2 ] || { printf '%s\n' 'drill: --source needs a value' >&2; exit 2; }; DRILL_SOURCE_URL="$2"; shift 2 ;;
        --source=*)       DRILL_SOURCE_URL="${1#--source=}"; shift ;;
        --live-db)        [ $# -ge 2 ] || { printf '%s\n' 'drill: --live-db needs a value' >&2; exit 2; }; DRILL_LIVE_DB="$2"; shift 2 ;;
        --live-db=*)      DRILL_LIVE_DB="${1#--live-db=}"; shift ;;
        --log-dir)        [ $# -ge 2 ] || { printf '%s\n' 'drill: --log-dir needs a value' >&2; exit 2; }; DRILL_LOG_DIR="$2"; shift 2 ;;
        --log-dir=*)      DRILL_LOG_DIR="${1#--log-dir=}"; shift ;;
        --scratch-dir)    [ $# -ge 2 ] || { printf '%s\n' 'drill: --scratch-dir needs a value' >&2; exit 2; }; DRILL_SCRATCH_DIR="$2"; shift 2 ;;
        --scratch-dir=*)  DRILL_SCRATCH_DIR="${1#--scratch-dir=}"; shift ;;
        --spot-account)   [ $# -ge 2 ] || { printf '%s\n' 'drill: --spot-account needs a value' >&2; exit 2; }; DRILL_SPOT_ACCOUNT_ID="$2"; shift 2 ;;
        --spot-account=*) DRILL_SPOT_ACCOUNT_ID="${1#--spot-account=}"; shift ;;
        --keep-scratch)   DRILL_KEEP_SCRATCH=1; shift ;;
        --verify-only)    VERIFY_ONLY=1; shift ;;
        -h|--help)        printf '%s\n' "$USAGE"; exit 0 ;;
        *) printf 'drill: unknown option %s\n' "$1" >&2; printf '%s\n' "$USAGE" >&2; exit 2 ;;
    esac
done

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
SAFE_TARGET=$(printf '%s' "${DRILL_TARGET:-no-target}" | tr -c 'A-Za-z0-9_.-' '_')
LOG="$DRILL_LOG_DIR/drill-$STAMP-$SAFE_TARGET.log"

if ! mkdir -p "$DRILL_LOG_DIR" 2>/dev/null; then
    printf 'drill: cannot create the drill log directory: %s\n' "$DRILL_LOG_DIR" >&2
    exit 2
fi

# say  - stdout AND the log. emit - a file's contents to stdout AND the log.
say()  { printf '%s\n' "$*" | tee -a "$LOG"; }
emit() { tee -a "$LOG" < "$1"; }
fail() { printf 'drill: %s\n' "$*" >&2; printf 'drill: %s\n' "$*" >> "$LOG" 2>/dev/null || :; }

TMP="${TMPDIR:-/tmp}"
# $$, not $: a lone dollar is a LITERAL in POSIX sh, so "drill.$" named every
# run's staging directory the SAME - concurrent drills shared OUT/ERR/metrics/
# reconcile.out and one run's cleanup_tmp deleted the tree another was using.
STAGE="$TMP/drill.$$"
if ! mkdir -p "$STAGE"; then
    printf 'drill: cannot create a working directory at %s\n' "$STAGE" >&2
    exit 2
fi
OUT="$STAGE/out"
ERR="$STAGE/err"
SRC_METRICS="$STAGE/src.metrics"
SCR_METRICS="$STAGE/scr.metrics"
REC_OUT="$STAGE/reconcile.out"
REC_ERR="$STAGE/reconcile.err"
DUMP="$DRILL_DUMP"

SCRATCH_CREATED=0
SCRATCH_PATH=""

cleanup_tmp() { rm -rf "$STAGE" 2>/dev/null || :; }
trap 'cleanup_tmp' EXIT HUP INT TERM

# ---------------------------------------------------------------------------
# Tool resolution: the sqlite3 CLI is the ONLY client.
#
# There is no container fallback any more, and adding one would be dead code:
# docker-compose.yml has no database service to exec into (docker compose config
# --services -> nginx, scheduler), so a "docker compose exec postgres ..." path
# could only ever fail at runtime. A missing sqlite3 is exit 3, loudly.
# ---------------------------------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    CLIENT=none
    CLIENT_DESC="none (sqlite3 is not installed or not on PATH)"
else
    CLIENT=sqlite3
    CLIENT_DESC="sqlite3 CLI $(sqlite3 --version 2>/dev/null | cut -d' ' -f1-2)"
fi

abs_path() { # an absolute path for a file that may not exist yet
    _d=$(dirname -- "$1"); _b=$(basename -- "$1")
    _ad=$(cd -- "$_d" 2>/dev/null && pwd) || _ad=""
    if [ -z "$_ad" ]; then printf '%s' "$1"; else printf '%s/%s' "$_ad" "$_b"; fi
}

# ---------------------------------------------------------------------------
# sqlite_dot <db-file> <dot-command> <dir of the command's path argument>
#
# The sqlite3 CLI's dot-commands (.backup/.restore) resolve their path argument
# with the C library, so an MSYS/Git-Bash absolute path like /c/dev/... is NOT
# translated and the command fails with "cannot open". cd'ing into the argument's
# directory and using a bare basename is the one form that works on every host, so
# every dot-command here goes through this wrapper. The database file itself is
# opened by the CLI (which does translate), so it is passed as an absolute path.
# A directory that does not exist makes the subshell fail, which is what we want:
# loud, not a silent no-op.
# ---------------------------------------------------------------------------
sqlite_dot() {
    _db="$1"; _cmd="$2"; _argdir="$3"
    ( cd -- "$_argdir" && sqlite3 -bail "$_db" "$_cmd" )
}

# An artifact/database is only usable if the 16-byte SQLite header is there. A
# zero-length or truncated file fails this before anything is restored from it.
is_sqlite_file() {
    [ -f "$1" ] || return 1
    head -c 16 -- "$1" 2>/dev/null | grep -q 'SQLite format 3' || return 1
    return 0
}

# Integrity is checked with -readonly: the check must not be able to repair the
# thing it is measuring.
integrity_ok() {
    sqlite3 -readonly -bail -noheader -separator '|' "$1" "PRAGMA integrity_check;" 2>/dev/null | head -1
}

# ---------------------------------------------------------------------------
# STEP 1 - the safety guard. Pure string logic: it runs before anything is read,
# created or deleted.
# ---------------------------------------------------------------------------
guard_target() {
    if [ -z "$DRILL_TARGET" ]; then
        fail "no target: the drill creates and DELETES a database file, so it needs an explicit SCRATCH file"
        fail "  $USAGE"
        fail "  docs/backup-and-restore.md: never restore over production"
        return 2
    fi
    # The target is a FILE now, so the check is not "is this a bare SQL identifier"
    # but "is this a bare filename". Anything containing a separator, a drive
    # colon, a space or a quote is a path or a DSN, not a name - refuse it rather
    # than let it escape --scratch-dir. This is also what keeps a leftover
    # postgres:// DSN from being rewritten into a plausible-looking filename.
    if [ -n "$(printf '%s' "$DRILL_TARGET" | tr -d 'A-Za-z0-9_.-')" ]; then
        fail "target '$DRILL_TARGET' is not a bare filename (letters, digits, dot, dash, underscore only)"
        fail "  pass a NAME, not a path or a DSN; the directory is --scratch-dir (default ${TMPDIR:-/tmp})"
        return 2
    fi
    LOW=$(printf '%s' "$DRILL_TARGET" | tr 'A-Z' 'a-z')
    LIVE_LOW=$(printf '%s' "$DRILL_LIVE_DB" | tr 'A-Z' 'a-z')

    if [ "$LOW" = "$LIVE_LOW" ]; then
        fail "REFUSING: target '$DRILL_TARGET' IS the live database name (DRILL_LIVE_DB='$DRILL_LIVE_DB')"
        fail "  this drill DELETES and rewrites its target. Restoring over production is the one"
        fail "  thing docs/backup-and-restore.md forbids outright: never restore over production."
        fail "  nothing was read, created or deleted."
        fail "  name a scratch file instead, e.g. --target apikita_drill_scratch.db"
        return 5
    fi
    # The PostgreSQL maintenance-database names are gone with the server. Their
    # SQLite equivalent is the live database FILENAME: the default local database
    # is server/data/server.db, and apikita.db is what it is called in the docs.
    for reserved in apikita.db server.db; do
        if [ "$LOW" = "$reserved" ]; then
            fail "REFUSING: target '$DRILL_TARGET' is a live-looking database filename ('$reserved')"
            fail "  the drill would DELETE it. nothing was read, created or deleted."
            return 5
        fi
    done
    # And the real thing: the file --source points at. The guard has to know the
    # source's basename, which is why it is computed before the guard runs.
    if [ -n "${SOURCE_BASENAME:-}" ] && [ "$LOW" = "$SOURCE_BASENAME" ]; then
        fail "REFUSING: target '$DRILL_TARGET' IS the source database file (--source names it)"
        fail "  restoring over the source would destroy the database this drill exists to protect."
        fail "  nothing was read, created or deleted."
        fail "  name a scratch file instead, e.g. --target apikita_drill_scratch.db"
        return 5
    fi
    case "$LOW" in
        *prod*|*prd*|*live*)
            fail "REFUSING: target '$DRILL_TARGET' looks like a LIVE database, not a scratch one"
            fail "  nothing was read, created or deleted."
            fail "  name a scratch file instead, e.g. --target apikita_drill_scratch.db"
            return 5
            ;;
    esac
    case "$LOW" in
        *scratch*|*drill*|*test*|*tmp*|*temp*|*rehearsal*) ;;
        *)
            fail "REFUSING: target '$DRILL_TARGET' does not look like a scratch database"
            fail "  the name must contain one of: scratch, drill, test, tmp, temp, rehearsal"
            fail "  (DRILL_LIVE_DB='$DRILL_LIVE_DB' is refused outright; so are apikita.db and server.db)"
            fail "  nothing was read, created or deleted."
            return 5
            ;;
    esac
    # Last, not first: a target that IS the live database must be refused as
    # REFUSED (exit 5) even when it is spelled without the .db suffix, so the
    # "I pointed the drill at production" signal stays unmistakable.
    case "$DRILL_TARGET" in
        *.db) ;;
        *)
            fail "target '$DRILL_TARGET' is not a .db file"
            fail "  the database is a FILE now (embedded SQLite); name it e.g. apikita_drill_scratch.db"
            return 2
            ;;
    esac
    return 0
}

metric() { grep "^$2|" "$1" 2>/dev/null | head -1 | cut -d'|' -f2-; }

now_ms() {
    n=$(date +%s%N 2>/dev/null || :)
    case "$n" in
        ''|*[!0-9]*) printf '%s000' "$(date +%s)" ;;
        *)           printf '%s' "$((n / 1000000))" ;;
    esac
}

mtime_of() {
    m=$(stat -c %Y "$1" 2>/dev/null) || m=""
    case "$m" in ''|*[!0-9]*) m=$(stat -f %m "$1" 2>/dev/null) || m="" ;; esac
    printf '%s' "$m"
}

RESULTS=""
record() {
    if [ -z "$RESULTS" ]; then
        RESULTS="drill:   $*"
    else
        RESULTS="$RESULTS
drill:   $*"
    fi
}

teardown() {
    if [ "$SCRATCH_CREATED" != 1 ]; then return 0; fi
    if [ "$DRILL_KEEP_SCRATCH" = 1 ]; then
        say "drill: --keep-scratch: LEAVING the scratch database '$SCRATCH_PATH' in place (doc step 7 skipped)"
        SCRATCH_CREATED=0
        return 0
    fi
    # -wal and -shm go with it: leaving a stale WAL next to a deleted database is
    # confusing at best, and a corrupted scratch database at worst.
    rm -f "$SCRATCH_PATH" "$SCRATCH_PATH-wal" "$SCRATCH_PATH-shm" 2>/dev/null || :
    if [ -e "$SCRATCH_PATH" ]; then
        fail "could not delete the scratch database '$SCRATCH_PATH' - IT IS STILL THERE"
        SCRATCH_CREATED=1
        return 8
    fi
    SCRATCH_CREATED=0
    say "drill: step 9 teardown - deleted the scratch database '$SCRATCH_PATH'"
    return 0
}

finish() { # finish <exit code>
    code="$1"
    cleanup_tmp
    exit "$code"
}

# --- the source: a SQLite URL, refused loudly if it is not --------------------
# A case, not a blind prefix strip: a leftover Postgres URL must be refused by
# name rather than rewritten into a relative path that happens to be a plausible
# filename (tools/reconcile/reconcile.sh makes the same argument).
SOURCE_PATH=""
SOURCE_BASENAME=""
if [ -n "$DRILL_SOURCE_URL" ]; then
    case "$DRILL_SOURCE_URL" in
        sqlite://*) SOURCE_PATH=${DRILL_SOURCE_URL#sqlite://} ;;
        sqlite:*)   SOURCE_PATH=${DRILL_SOURCE_URL#sqlite:} ;;
        *)
            printf 'drill: DRILL_SOURCE_URL/DATABASE_URL is not a SQLite URL: %s\n' "$DRILL_SOURCE_URL" >&2
            printf 'drill: expected e.g. sqlite://data/server.db (there is no database server any more)\n' >&2
            exit 2
            ;;
    esac
    SOURCE_PATH=${SOURCE_PATH%%\?*}
    case "$SOURCE_PATH" in
        ''|':memory:')
            printf 'drill: the source URL does not name a file: %s\n' "$DRILL_SOURCE_URL" >&2
            printf 'drill: an in-memory database cannot be copied or restored from outside the process\n' >&2
            exit 2
            ;;
    esac
    SOURCE_BASENAME=$(basename -- "$SOURCE_PATH" | tr 'A-Z' 'a-z')
fi

say "drill: ================= apikita RESTORE DRILL ================="
say "drill: date_utc     $STAMP"
say "drill: run_by       ${USER:-${USERNAME:-$(id -un 2>/dev/null || echo unknown)}} on $(hostname 2>/dev/null || echo unknown-host)"
say "drill: target       ${DRILL_TARGET:-<none>}   (scratch file; refused if it is the live db)"
say "drill: live_db      $DRILL_LIVE_DB"
say "drill: mode         $([ "$VERIFY_ONLY" = 1 ] && echo 'verify-only (no restore, no teardown)' || echo 'full drill')"
say "drill: log          $LOG"
say "drill: doc          docs/backup-and-restore.md - RTO ${RTO_BUDGET_SECONDS}s, RPO 900s"

# --- STEP 1: guard ----------------------------------------------------------
say "drill: step 1 - safety guard on the target name"
guard_target
GUARD_CODE=$?
if [ "$GUARD_CODE" != 0 ]; then
    say "drill: step 1 REFUSED - target '${DRILL_TARGET:-<none>}' rejected (exit $GUARD_CODE). No database was touched."
    say "drill: result       REFUSED (exit $GUARD_CODE)"
    finish "$GUARD_CODE"
fi
say "drill: step 1 OK - '$DRILL_TARGET' is a scratch name, not '$DRILL_LIVE_DB'"

SCRATCH_PATH=$(abs_path "$DRILL_SCRATCH_DIR/$DRILL_TARGET")
SCRATCH_DIR=$(dirname -- "$SCRATCH_PATH")
if ! mkdir -p "$SCRATCH_DIR" 2>/dev/null; then
    fail "cannot create the scratch directory: $SCRATCH_DIR"
    say "drill: result       FAIL (exit 2)"
    finish 2
fi

# --- tool availability ------------------------------------------------------
if [ "$CLIENT" = none ]; then
    fail "no SQLite client: sqlite3 is not on PATH"
    fail "  there is no container fallback: docker-compose.yml has no database service"
    fail "  (docker compose config --services -> nginx, scheduler) to exec into"
    say "drill: result       FAIL (exit 3)"
    finish 3
fi
if [ ! -f "$RECONCILE_SH" ]; then
    fail "the reconciliation gate is missing: $RECONCILE_SH"
    fail "  THE check is tools/reconcile/reconcile.sh; the drill does not reimplement it"
    say "drill: result       FAIL (exit 3)"
    finish 3
fi
if [ ! -f "$VERIFY_SQL" ]; then
    fail "verify.sql is missing next to this script: $VERIFY_SQL"
    say "drill: result       FAIL (exit 3)"
    finish 3
fi
say "drill: client       $CLIENT_DESC"

if [ -z "$DRILL_SOURCE_URL" ]; then
    fail "no source: set DATABASE_URL (or --source) to the SQLite database to copy"
    fail "  there is deliberately no local default here: a drill that silently picks a"
    fail "  database can prove a restore of the wrong one. export DATABASE_URL='sqlite://data/server.db'"
    say "drill: result       FAIL (exit 2)"
    finish 2
fi
SOURCE_PATH=$(abs_path "$SOURCE_PATH")
say "drill: source       $DRILL_SOURCE_URL"
say "drill: source_file  $SOURCE_PATH"
if [ ! -f "$SOURCE_PATH" ]; then
    fail "the source database file does not exist: $SOURCE_PATH"
    fail "  create it with 'cargo run --bin migrate' (from server/)"
    say "drill: result       FAIL (exit 6)"
    finish 6
fi
# A source that is not a SQLite database is not a drill: SQLite will happily open a
# zero-length file as a brand-new EMPTY database, and every check downstream would
# then agree - 0 rows against 0 rows, 0 drifting accounts - and report PASS for a
# restore of nothing. That is precisely the silent pass this tool exists to prevent,
# so the source gets the same header check the artifact gets.
if ! is_sqlite_file "$SOURCE_PATH"; then
    fail "the source is not a SQLite database (no 'SQLite format 3' header): $SOURCE_PATH"
    fail "  an empty or non-database file would restore to an empty database and PASS every check"
    say "drill: result       FAIL (exit 6)"
    finish 6
fi
SCRATCH_URL="sqlite://$SCRATCH_PATH"

RESTORE_MS=""
BACKUP_AGE_S=""
INTEGRITY=""

if [ "$VERIFY_ONLY" != 1 ]; then
    # --- STEP 2: obtain an artifact -----------------------------------------
    if [ -z "$DUMP" ]; then
        DUMP="$REPO_ROOT/.agents/drill-dumps/drill-$STAMP-$SAFE_TARGET.db"
        mkdir -p "$(dirname -- "$DUMP")" || { fail "cannot create $(dirname -- "$DUMP")"; finish 2; }
        say "drill: step 2 - no --dump given, taking a fresh .backup of the source"
        say "drill:   (the SQLite backup API: transactionally consistent against a live writer,"
        say "drill:    and it folds in the WAL - which a raw cp of a live WAL database does not)"
        umask 077
        rm -f "$DUMP"
        if ! sqlite_dot "$SOURCE_PATH" ".backup '$(basename -- "$DUMP")'" "$(dirname -- "$DUMP")" > "$OUT" 2>"$ERR"; then
            fail "the .backup of the source failed:"
            [ -s "$ERR" ] && cat "$ERR" >&2
            rm -f "$DUMP"
            say "drill: result       FAIL (exit 6)"
            finish 6
        fi
    else
        say "drill: step 2 - restoring the supplied artifact $DUMP"
    fi
    DUMP=$(abs_path "$DUMP")
    if [ ! -f "$DUMP" ]; then
        fail "the artifact does not exist: $DUMP"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    DUMP_BYTES=$(wc -c < "$DUMP" | tr -d ' ')
    if [ "$DUMP_BYTES" -eq 0 ]; then
        fail "the artifact is EMPTY ($DUMP_BYTES bytes) - a truncated copy looks successful and restores nothing"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        DUMP_SHA=$(sha256sum "$DUMP" | cut -d' ' -f1)
    else
        DUMP_SHA=$(openssl dgst -sha256 -r "$DUMP" 2>/dev/null | cut -d' ' -f1)
    fi
    MT=$(mtime_of "$DUMP")
    if [ -n "$MT" ]; then
        BACKUP_AGE_S=$(( $(date +%s) - MT ))
    else
        BACKUP_AGE_S="unknown"
    fi
    say "drill: artifact     $DUMP"
    say "drill: artifact_size $DUMP_BYTES bytes  sha256=$DUMP_SHA"
    say "drill: backup_age   ${BACKUP_AGE_S}s at restore time"

    # --- STEP 3: the artifact must be a readable database -------------------
    # This replaces pg_restore --list. It asks the same question - "is this a real,
    # intact database file?" - of a different format: the 16-byte SQLite header,
    # then PRAGMA integrity_check over the whole file. A truncated or garbage
    # artifact fails HERE (exit 6), before anything is restored from it.
    say "drill: step 3 - preflight: SQLite header + PRAGMA integrity_check on the artifact"
    if ! is_sqlite_file "$DUMP"; then
        fail "the artifact is not a SQLite database (no 'SQLite format 3' header): $DUMP"
        fail "  a file that cannot be read is a FAILURE, not a warning"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    PRE_INTEGRITY=$(integrity_ok "$DUMP")
    if [ "$PRE_INTEGRITY" != "ok" ]; then
        fail "PRAGMA integrity_check on the artifact says: '${PRE_INTEGRITY:-<no output>}' (not 'ok')"
        fail "  the artifact is corrupt - restoring it cannot produce a sound database"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    say "drill: preflight    OK - SQLite database, integrity_check=ok"

    # --- STEP 4: clear the scratch file -------------------------------------
    # A leftover scratch database from an aborted run must not be restored over: a
    # restore that "succeeds" onto a stale file proves nothing about the artifact.
    # Delete it first, so the file that exists afterwards is one this run created
    # from the artifact and from nothing else.
    say "drill: step 4 - deleting any leftover scratch database '$SCRATCH_PATH'"
    rm -f "$SCRATCH_PATH" "$SCRATCH_PATH-wal" "$SCRATCH_PATH-shm" 2>/dev/null || :
    if [ -e "$SCRATCH_PATH" ]; then
        fail "a file exists at $SCRATCH_PATH and could not be deleted - refusing to restore over it"
        say "drill: result       FAIL (exit 4)"
        finish 4
    fi

    # --- STEP 5: TIME the restore -------------------------------------------
    # .restore is the SQLite backup API in reverse: it writes the artifact's
    # contents into the target database. That is a real restore operation, not a
    # copy of the file, so the timing is a restore time.
    say "drill: step 5 - RESTORE (timed): .restore <artifact> into <scratch>"
    T0=$(now_ms)
    if ! sqlite_dot "$SCRATCH_PATH" ".restore '$(basename -- "$DUMP")'" "$(dirname -- "$DUMP")" > "$OUT" 2>"$ERR"; then
        RESTORE_MS=$(( $(now_ms) - T0 ))
        fail "the restore FAILED after ${RESTORE_MS}ms - the restore itself is broken:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        [ -s "$OUT" ] && cat "$OUT" >&2
        record "restore_ms    $RESTORE_MS"
        record "result        FAIL (exit 7)"
        say "$RESULTS"
        SCRATCH_CREATED=1
        teardown || :
        say "drill: log          $LOG"
        finish 7
    fi
    SCRATCH_CREATED=1
    RESTORE_MS=$(( $(now_ms) - T0 ))
    RESTORE_S=$((RESTORE_MS / 1000))
    RESTORE_FRAC=$((RESTORE_MS % 1000))
    [ -s "$ERR" ] && { say "drill: restore diagnostics (stderr):"; emit "$ERR"; }
    say "drill: RESTORE TIME $((RESTORE_MS / 1000)).$(printf '%03d' "$RESTORE_FRAC") s  (${RESTORE_MS} ms) - this is the real RTO"
    if [ "$RESTORE_S" -le "$RTO_BUDGET_SECONDS" ]; then
        say "drill:   within the documented RTO budget of ${RTO_BUDGET_SECONDS}s"
    else
        say "drill:   OVER the documented RTO budget of ${RTO_BUDGET_SECONDS}s - the RTO claim in docs/backup-and-restore.md is FALSE"
    fi

    # --- STEP 5b: the RESTORED file must itself be intact -------------------
    # New with the SQLite port, and it earns its place: "the restore ran" is not
    # the same claim as "the result is a sound database". A byte-perfect copy of a
    # corrupt source restores perfectly and is still unrestorable.
    say "drill: step 5b - PRAGMA integrity_check on the RESTORED database"
    INTEGRITY=$(integrity_ok "$SCRATCH_PATH")
    if [ "$INTEGRITY" != "ok" ]; then
        fail "the RESTORED database failed integrity_check: '${INTEGRITY:-<no output>}' (not 'ok')"
        fail "  the restore wrote a database SQLite will not vouch for - do NOT trust it"
        record "integrity     ${INTEGRITY:-<no output>} (FAIL)"
        record "result        FAIL (exit 7)"
        say "$RESULTS"
        teardown || :
        say "drill: log          $LOG"
        finish 7
    fi
    say "drill: integrity    ok (RESTORED database)"
else
    say "drill: --verify-only - skipping steps 2-5 (artifact, preflight, clear, restore)"
    say "drill: verifying the EXISTING database '$DRILL_TARGET' as if it were the restored one"
    if [ ! -f "$SCRATCH_PATH" ]; then
        fail "--verify-only: no such database file: $SCRATCH_PATH"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    INTEGRITY=$(integrity_ok "$SCRATCH_PATH")
    say "drill: integrity    ${INTEGRITY:-<no output>} (existing database, not restored by this run)"
    # SCRATCH_CREATED deliberately stays 0: this mode did not create the database,
    # so it must not delete it either. Teardown owns only what this run made.
    say "drill:   (teardown skipped: this run did not create '$SCRATCH_PATH')"
fi

# ---------------------------------------------------------------------------
# STEP 6 - the numbers that matter, from the RESTORED database.
# ---------------------------------------------------------------------------
FAILED=0
say "drill: step 6 - verify.sql against the scratch database (row counts, money totals, key hashes)"
if ! sqlite3 -readonly -bail -noheader -separator '|' "$SCRATCH_PATH" < "$VERIFY_SQL" > "$SCR_METRICS" 2>"$ERR"; then
    fail "sqlite3 failed reading the scratch database:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    say "drill: result       FAIL (exit 4)"
    teardown || :
    finish 4
fi
[ -s "$ERR" ] && { say "drill: sqlite3 diagnostics (diagnostic, NOT drift):"; emit "$ERR"; }
emit "$SCR_METRICS"

if ! sqlite3 -readonly -bail -noheader -separator '|' "$SOURCE_PATH" < "$VERIFY_SQL" > "$SRC_METRICS" 2>"$ERR"; then
    fail "sqlite3 failed reading the SOURCE database - row counts and the spot-check need it:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    say "drill: result       FAIL (exit 4)"
    teardown || :
    finish 4
fi

say "drill: step 6b - row counts: scratch vs source (a snapshot may only be BEHIND, never ahead)"
say "drill:   metric|source|scratch|delta|allowed   (allowed = ${DRILL_ROW_TOLERANCE} + ${DRILL_ROW_TOLERANCE_PCT}% of source)"
for m in accounts wallets ledger api_keys topups usage_daily api_keys_with_key_hash; do
    s=$(metric "$SRC_METRICS" "$m"); t=$(metric "$SCR_METRICS" "$m")
    [ -n "$s" ] || s="?"; [ -n "$t" ] || t="?"
    if [ "$s" = "?" ] || [ "$t" = "?" ]; then
        say "drill:   $m|$s|$t|?|?"
        fail "row count metric MISSING for $m (source=$s scratch=$t) - a missing table is a broken restore"
        record "row_counts    $m MISSING (source=$s scratch=$t)"
        FAILED=1
        continue
    fi
    d=$((s - t))
    TOL=$(( DRILL_ROW_TOLERANCE + (s * DRILL_ROW_TOLERANCE_PCT + 99) / 100 ))
    say "drill:   $m|$s|$t|$d|$TOL"
    if [ "$t" -gt "$s" ]; then
        fail "row count INFLATED for $m: scratch=$t > source=$s - a snapshot cannot contain rows the source does not"
        FAILED=1
    elif [ "$d" -gt "$TOL" ]; then
        fail "row count SHORT for $m: source=$s scratch=$t (delta $d > allowed $TOL)"
        fail "  more than ${DRILL_ROW_TOLERANCE_PCT}% of this table is missing from the restore - the classic"
        fail "  truncated artifact. Raise DRILL_ROW_TOLERANCE_PCT only with a reason you would defend in a postmortem."
        FAILED=1
    fi
done

# Keys present - docs/backup-and-restore.md pass criteria.
AK=$(metric "$SCR_METRICS" api_keys)
AKH=$(metric "$SCR_METRICS" api_keys_with_key_hash)
if [ -n "$AK" ] && [ "$AK" != 0 ] && { [ -z "$AKH" ] || [ "$AKH" = 0 ]; }; then
    fail "KEYS MISSING: $AK api_keys rows but no usable key_hash - nobody could authenticate after this restore"
    FAILED=1
fi

# Money totals (step 3 of the doc). Global agreement is weaker than per-account
# drift, which is why the gate below is reconcile.sql - this is a sanity total.
say "drill: step 6c - money totals (global; strictly weaker than the per-account gate in step 8)"
say "drill:   total_owed_idr (wallets) source=$(metric "$SRC_METRICS" total_owed_idr) scratch=$(metric "$SCR_METRICS" total_owed_idr)"
say "drill:   ledger_sum_idr (ledger)  source=$(metric "$SRC_METRICS" ledger_sum_idr) scratch=$(metric "$SCR_METRICS" ledger_sum_idr)"

# ---------------------------------------------------------------------------
# STEP 7 - spot-check a known account against the source (doc step 5).
# ---------------------------------------------------------------------------
say "drill: step 7 - spot-check a known account's balance against the source"
# No ::text cast any more: CAST(... AS TEXT) is the SQLite spelling, and both
# values are rendered as TEXT so an integer 1000 and a text '1000' cannot be
# compared as if they were different.
SPOT_SQL="SELECT CAST(w.balance_idr AS TEXT) || '|' || CAST(COALESCE((SELECT sum(delta_idr) FROM ledger l WHERE l.account_id = w.account_id), 0) AS TEXT) FROM wallets w WHERE w.account_id = "
ACCT="$DRILL_SPOT_ACCOUNT_ID"
if [ -z "$ACCT" ]; then
    ACCT=$(printf '%s' "SELECT w.account_id FROM wallets w JOIN accounts a ON a.id = w.account_id ORDER BY w.balance_idr DESC, w.account_id LIMIT 1;" \
        | sqlite3 -readonly -bail -noheader "$SOURCE_PATH" 2>"$ERR" | head -1)
    if [ -z "$ACCT" ]; then
        say "drill:   no wallets row exists in the source - the spot-check has nothing to compare"
        say "drill:   (recorded as NOT APPLICABLE, not as a pass)"
        record "spot_check    N/A (source has no wallets rows)"
    else
        say "drill:   picked the source's largest wallet: $ACCT"
    fi
else
    say "drill:   operator-supplied account: $ACCT"
fi
if [ -n "$ACCT" ]; then
    # The account id reaches SQL by interpolation: validate it as a UUID first.
    case "$ACCT" in
        *[!0-9a-fA-F-]*|'')
            fail "refusing to interpolate '$ACCT' into SQL - it is not a UUID"
            FAILED=1 ;;
        *)
            printf "%s'%s';" "$SPOT_SQL" "$ACCT" > "$OUT"
            SRC_SPOT=$(sqlite3 -readonly -bail -noheader "$SOURCE_PATH" < "$OUT" 2>"$ERR" | head -1)
            SCR_SPOT=$(sqlite3 -readonly -bail -noheader "$SCRATCH_PATH" < "$OUT" 2>"$ERR" | head -1)
            say "drill:   account $ACCT  source='$SRC_SPOT'  scratch='$SCR_SPOT'   (balance_idr|ledger_sum)"
            if [ -z "$SRC_SPOT" ]; then
                fail "spot-check: the account has NO wallets row in the SOURCE - cannot compare"
                FAILED=1
            elif [ -z "$SCR_SPOT" ]; then
                fail "spot-check: the account has NO wallets row in the RESTORED database - the restore lost it"
                FAILED=1
            elif [ "$SRC_SPOT" != "$SCR_SPOT" ]; then
                fail "spot-check MISMATCH: source='$SRC_SPOT' scratch='$SCR_SPOT'"
                FAILED=1
            else
                say "drill:   spot-check MATCH"
            fi
            record "spot_check    $ACCT source='$SRC_SPOT' scratch='$SCR_SPOT'"
            ;;
    esac
fi

# ---------------------------------------------------------------------------
# STEP 8 - THE check. Not reimplemented: reconcile.sh, pointed at the scratch.
# ---------------------------------------------------------------------------
say "drill: step 8 - THE CHECK: wallets must equal the ledger"
say "drill:   invoking the repository's single drift definition: tools/reconcile/reconcile.sh"
say "drill:   DATABASE_URL=<scratch file> sh $RECONCILE_SH"
# No psql shim any more, and none is needed: reconcile.sh calls the same sqlite3
# CLI this script just used, against the same file.
REC_STATUS=0
DATABASE_URL="$SCRATCH_URL" sh "$RECONCILE_SH" > "$REC_OUT" 2>"$REC_ERR" || REC_STATUS=$?
[ -s "$REC_OUT" ] && emit "$REC_OUT"
[ -s "$REC_ERR" ] && { say "drill: reconcile stderr:"; emit "$REC_ERR"; }

case "$REC_STATUS" in
    0) say "drill:   drift check PASSED - reconcile.sh exited 0 (zero drifting rows)" ;;
    1) fail "drift check FAILED - reconcile.sh exited 1: the wallet cache disagrees with the authoritative ledger"
       fail "  THE CHECK is the gate: docs/backup-and-restore.md - zero rows from the reconciliation query is the gate"
       fail "  either the restore is corrupt or the source data has a bug; both block trusting a recovery"
       FAILED=1 ;;
    2) fail "reconcile.sh exited 2: DATABASE_URL was not a usable SQLite URL - this is a drill bug"
       FAILED=1 ;;
    3) fail "reconcile.sh exited 3: no sqlite3 available to it"
       FAILED=1 ;;
    4) fail "reconcile.sh exited 4: sqlite3 failed against the scratch database"
       FAILED=1 ;;
    5) fail "reconcile.sh exited 5: a stranded reservation hold older than the bound - money unaccounted for"
       fail "  structurally invisible to the drift query; see tools/reconcile/README.md"
       FAILED=1 ;;
    6) fail "reconcile.sh exited 6: no such database file - the restored file is GONE"
       FAILED=1 ;;
    *) fail "reconcile.sh exited $REC_STATUS"
       FAILED=1 ;;
esac

# ---------------------------------------------------------------------------
# STEP 9/10 - teardown, verdict, log.
# ---------------------------------------------------------------------------
say "drill: step 9 - teardown (doc step 7)"
TEARDOWN_CODE=0
teardown
TEARDOWN_CODE=$?

RESULT="PASS"
CODE=0
if [ "$FAILED" != 0 ]; then RESULT="FAIL"; CODE=1; fi
if [ "$TEARDOWN_CODE" != 0 ]; then RESULT="FAIL (teardown)"; CODE=8; fi

say "drill: ---------------- drill log ----------------"
say "drill: date_utc      $STAMP"
say "drill: run_by        ${USER:-${USERNAME:-$(id -un 2>/dev/null || echo unknown)}} on $(hostname 2>/dev/null || echo unknown-host)"
say "drill: target        ${DRILL_TARGET:-<none>}   (live_db=$DRILL_LIVE_DB, refused if equal)"
say "drill: artifact      ${DUMP:-<none>}"
[ -n "${DUMP_SHA:-}" ] && say "drill: artifact_sha256 $DUMP_SHA  (${DUMP_BYTES} bytes)"
say "drill: backup_age    ${BACKUP_AGE_S:-<none: verify-only>} s at restore time"
say "drill: integrity     ${INTEGRITY:-<none>} (PRAGMA integrity_check on the RESTORED database)"
if [ -n "$RESTORE_MS" ]; then
    say "drill: restore_ms    $RESTORE_MS  ($((RESTORE_MS / 1000)).$(printf '%03d' "$((RESTORE_MS % 1000))") s) - the real RTO"
else
    say "drill: restore_ms    <none: verify-only>"
fi
say "drill: drift         reconcile.sh exit $REC_STATUS  ($([ "$REC_STATUS" = 0 ] && echo 'zero drifting rows - THE check passes' || echo 'NOT zero - see above'))"
say "$RESULTS"
say "drill: result        $RESULT (exit $CODE)"
say "drill: log           $LOG"
say "drill: ------------------------------------------"

if [ "$CODE" = 0 ]; then
    say "drill: PASS - the artifact restored, is intact, reconciles with the ledger, and matches the source."
    say "drill: This is measured evidence for the RTO/RPO claim in docs/backup-and-restore.md."
else
    say "drill: FAIL (exit $CODE) - do NOT claim a working restore until this is green."
fi
finish "$CODE"
