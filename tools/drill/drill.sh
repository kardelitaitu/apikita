#!/bin/sh
# Restore drill for apikita - the executable form of docs/backup-and-restore.md,
# sections "The restore drill" (procedure) and "Pass criteria".
#
# WHY THIS EXISTS
#   "An untested backup is a belief." A dump that has never been restored and
#   reconciled is a file, not a recovery plan. This tool turns the claim "we can
#   restore" into a measured, repeatable, logged result:
#     restore time  -> the real RTO, which the doc asks for explicitly
#     drift rows    -> THE check: wallets must equal the ledger
#     row counts    -> the restored database is plausibly the one we dumped
#     spot-check    -> a known account's balance matches the source
#
# SAFETY, FIRST AND LOUDEST
#   This tool creates and DROPS a database, so it must never point at the live
#   one. A --target (or DRILL_TARGET) is REQUIRED, and the tool REFUSES (exit 5)
#   unless the name looks like a scratch instance:
#     * it equals DRILL_LIVE_DB (default "apikita")             -> refuse
#     * it is postgres / template0 / template1                  -> refuse
#     * it contains prod / prd / live / production              -> refuse
#     * it contains none of scratch/drill/test/tmp/temp/rehearsal -> refuse
#   The guard runs BEFORE any database is contacted, and a refusal is exit 5 -
#   distinct from every other failure, so "I pointed it at production" is
#   unmistakable in a log or a CI job.
#
# WHAT IT DOES, IN THE DOC'S ORDER
#    1. guard the target (above)
#    2. obtain a dump: --dump <file>, else a fresh pg_dump -Fc of the source
#    3. preflight: the archive must be listable (pg_restore --list)
#    4. DROP + CREATE the scratch database, so it is clean by construction
#    5. TIME the restore: pg_restore --clean --if-exists -d <scratch> <dump>
#    6. verify.sql against the scratch: row counts, money totals, key hashes
#    7. row counts vs the source, plus the spot-check account's balance
#    8. THE check: tools/reconcile/reconcile.sh with DATABASE_URL=<scratch dsn>
#    9. DROP the scratch database
#   10. write the drill log
#
# Exit codes (3 and 4 keep tools/reconcile/reconcile.sh's meanings):
#   0  PASS    - restore completed, zero drifting rows, every criterion met
#   1  FAIL    - a check failed: drift, row counts, spot-check, or no key hashes
#   2  usage   - no --target, a bad option, or a target that is not a bare identifier
#   3  missing - a required tool is absent (psql/pg_restore/pg_dump, docker, reconcile.sh)
#   4  db      - a database command failed (connection, permissions, SQL error)
#   5  REFUSED - the target looks like the live database. NOTHING was touched.
#   6  dump    - the dump is missing, empty, or not a readable archive
#   7  restore - pg_restore failed: the restore itself is broken
#   8  teardown- the scratch database could not be dropped (it is still there)
#
# THE CHECK IS NOT REIMPLEMENTED HERE
#   Drift is defined in exactly one place: tools/reconcile/reconcile.sql, driven
#   by tools/reconcile/reconcile.sh. Step 8 INVOKES THAT SCRIPT with the scratch
#   DSN as DATABASE_URL. Its exit code is the drill's drift verdict (0 pass, 1
#   drift). A second copy of the query in this directory would be a second
#   definition of "drift", and two definitions is how a detector stops being
#   trusted.
#
# Verified vs assumed: see README.md next to this file.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

DRILL_TARGET="${DRILL_TARGET:-}"
DRILL_DUMP="${DRILL_DUMP:-}"
DRILL_SOURCE_DSN="${DRILL_SOURCE_DSN:-${DATABASE_URL:-}}"
DRILL_LIVE_DB="${DRILL_LIVE_DB:-apikita}"
DRILL_LOG_DIR="${DRILL_LOG_DIR:-$REPO_ROOT/.agents/drill-logs}"
DRILL_SPOT_ACCOUNT_ID="${DRILL_SPOT_ACCOUNT_ID:-}"
DRILL_KEEP_SCRATCH="${DRILL_KEEP_SCRATCH:-0}"
DRILL_ROW_TOLERANCE="${DRILL_ROW_TOLERANCE:-0}"
# Row counts are compared "within expected range of live" (the doc's pass
# criteria), not exactly: the source keeps accepting writes between the dump and
# the count, so an exact match is unachievable on a live database and would make
# the drill red for the wrong reason. The default is 5% per metric, which still
# catches the failure the doc actually warns about - a truncated dump loses a
# whole table or most of its rows, not 5% of them.
DRILL_ROW_TOLERANCE_PCT="${DRILL_ROW_TOLERANCE_PCT:-5}"
DRILL_USER="${DRILL_USER:-postgres}"
DRILL_PASSWORD="${DRILL_PASSWORD:-dev}"
DRILL_DB_HOST="${DRILL_DB_HOST:-}"
COMPOSE_FILE="${COMPOSE_FILE:-$REPO_ROOT/docker-compose.yml}"
COMPOSE_DIR=$(cd -- "$(dirname -- "$COMPOSE_FILE")" && pwd)
COMPOSE_BASE=$(basename -- "$COMPOSE_FILE")
CONTAINER_SERVICE="${CONTAINER_SERVICE:-postgres}"
RECONCILE_SH="${RECONCILE_SH:-$REPO_ROOT/tools/reconcile/reconcile.sh}"
VERIFY_SQL="$SCRIPT_DIR/verify.sql"
RTO_BUDGET_SECONDS="${RTO_BUDGET_SECONDS:-14400}"

VERIFY_ONLY=0
USAGE="usage: sh tools/drill/drill.sh --target <scratch-db> [--dump <file>] [--verify-only] [--keep-scratch] [--log-dir <dir>]"

while [ $# -gt 0 ]; do
    case "$1" in
        --target)         [ $# -ge 2 ] || { printf 'drill: --target needs a value\n' >&2; exit 2; }; DRILL_TARGET="$2"; shift 2 ;;
        --target=*)       DRILL_TARGET="${1#--target=}"; shift ;;
        --dump)           [ $# -ge 2 ] || { printf 'drill: --dump needs a value\n' >&2; exit 2; }; DRILL_DUMP="$2"; shift 2 ;;
        --dump=*)         DRILL_DUMP="${1#--dump=}"; shift ;;
        --source)         [ $# -ge 2 ] || { printf 'drill: --source needs a value\n' >&2; exit 2; }; DRILL_SOURCE_DSN="$2"; shift 2 ;;
        --source=*)       DRILL_SOURCE_DSN="${1#--source=}"; shift ;;
        --live-db)        [ $# -ge 2 ] || { printf 'drill: --live-db needs a value\n' >&2; exit 2; }; DRILL_LIVE_DB="$2"; shift 2 ;;
        --live-db=*)      DRILL_LIVE_DB="${1#--live-db=}"; shift ;;
        --log-dir)        [ $# -ge 2 ] || { printf 'drill: --log-dir needs a value\n' >&2; exit 2; }; DRILL_LOG_DIR="$2"; shift 2 ;;
        --log-dir=*)      DRILL_LOG_DIR="${1#--log-dir=}"; shift ;;
        --spot-account)   [ $# -ge 2 ] || { printf 'drill: --spot-account needs a value\n' >&2; exit 2; }; DRILL_SPOT_ACCOUNT_ID="$2"; shift 2 ;;
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
STAGE="$TMP/drill.$$"
if ! mkdir -p "$STAGE"; then
    printf 'drill: cannot create a working directory at %s\n' "$STAGE" >&2
    exit 2
fi
SHIM_DIR="$STAGE/shim"
SCRATCH_CREATED=0
OUT="$STAGE/out"
ERR="$STAGE/err"
SRC_METRICS="$STAGE/src.metrics"
SCR_METRICS="$STAGE/scr.metrics"
REC_OUT="$STAGE/reconcile.out"
REC_ERR="$STAGE/reconcile.err"
DUMP="$DRILL_DUMP"

cleanup_tmp() { rm -rf "$STAGE" 2>/dev/null || :; }
trap 'cleanup_tmp' EXIT HUP INT TERM

# ---------------------------------------------------------------------------
# Tool resolution: host client, else the postgres compose service.
# ---------------------------------------------------------------------------
CLIENT=""
if command -v psql >/dev/null 2>&1 && command -v pg_restore >/dev/null 2>&1 \
        && command -v pg_dump >/dev/null 2>&1; then
    CLIENT=host
    [ -n "$DRILL_DB_HOST" ] || DRILL_DB_HOST=127.0.0.1
    CLIENT_DESC="host psql/pg_restore/pg_dump"
elif command -v docker >/dev/null 2>&1 && [ -f "$COMPOSE_FILE" ]; then
    CLIENT=container
    [ -n "$DRILL_DB_HOST" ] || DRILL_DB_HOST=postgres
    CLIENT_DESC="docker compose exec -T $CONTAINER_SERVICE (pg_dump/pg_restore/psql)"
else
    CLIENT=none
    CLIENT_DESC="none"
fi

# ---------------------------------------------------------------------------
# STEP 1 - the safety guard. Pure string logic: it runs before anything is
# contacted, created or dropped.
# ---------------------------------------------------------------------------
guard_target() {
    if [ -z "$DRILL_TARGET" ]; then
        fail "no target: the drill creates and DROPS a database, so it needs an explicit SCRATCH database"
        fail "  $USAGE"
        fail "  docs/backup-and-restore.md: \"Provision a scratch Postgres (never restore over production)\""
        return 2
    fi
    case "$DRILL_TARGET" in
        *[!A-Za-z0-9_]*)
            fail "target '$DRILL_TARGET' is not a bare SQL identifier (letters, digits, underscore only)"
            fail "  a target with quotes, dashes or a DSN in it is not a database name"
            return 2
            ;;
    esac

    LOW=$(printf '%s' "$DRILL_TARGET" | tr 'A-Z' 'a-z')
    LIVE_LOW=$(printf '%s' "$DRILL_LIVE_DB" | tr 'A-Z' 'a-z')

    if [ "$LOW" = "$LIVE_LOW" ]; then
        fail "REFUSING: target '$DRILL_TARGET' IS the live database name (DRILL_LIVE_DB='$DRILL_LIVE_DB')"
        fail "  this drill DROPs and recreates its target. Restoring over production is the one"
        fail "  thing docs/backup-and-restore.md forbids outright: \"never restore over production\"."
        fail "  nothing was contacted, created or dropped."
        fail "  name a scratch database instead, e.g. --target apikita_scratch"
        return 5
    fi
    for reserved in postgres template0 template1; do
        if [ "$LOW" = "$reserved" ]; then
            fail "REFUSING: target '$DRILL_TARGET' is a PostgreSQL maintenance database ('$reserved')"
            fail "  the drill would DROP it. nothing was contacted, created or dropped."
            return 5
        fi
    done
    case "$LOW" in
        *prod*|*prd*|*live*)
            fail "REFUSING: target '$DRILL_TARGET' looks like a LIVE database, not a scratch one"
            fail "  nothing was contacted, created or dropped."
            fail "  name a scratch database instead, e.g. --target apikita_scratch"
            return 5
            ;;
    esac
    case "$LOW" in
        *scratch*|*drill*|*test*|*tmp*|*temp*|*rehearsal*) ;;
        *)
            fail "REFUSING: target '$DRILL_TARGET' does not look like a scratch database"
            fail "  the name must contain one of: scratch, drill, test, tmp, temp, rehearsal"
            fail "  (DRILL_LIVE_DB='$DRILL_LIVE_DB' is refused outright; so are postgres/template0/template1)"
            fail "  nothing was contacted, created or dropped."
            return 5
            ;;
    esac
    return 0
}

# ---------------------------------------------------------------------------
# Database plumbing.
# ---------------------------------------------------------------------------
dsn_for_db() { printf 'postgres://%s:%s@%s:5432/%s' "$DRILL_USER" "$DRILL_PASSWORD" "$DRILL_DB_HOST" "$1"; }

# compose_run - run a command inside the compose service.
#
# Runs from the compose file's own directory with the file's BASENAME, and with
# MSYS_NO_PATHCONV=1. Both matter on Windows: an absolute /c/... path passed to
# docker.exe through -f is reinterpreted as C:\c\... and compose fails with
# "cannot find the path specified" - a path-conversion bug, not a missing file.
compose_run() {
    ( cd -- "$COMPOSE_DIR" && MSYS_NO_PATHCONV=1 docker compose -f "$COMPOSE_BASE" "$@" )
}

psql_dsn() { # psql_dsn <dsn> [psql args...]   (SQL on stdin)
    dsn="$1"; shift
    if [ "$CLIENT" = host ]; then
        psql "$dsn" "$@"
    else
        compose_run exec -T "$CONTAINER_SERVICE" psql "$dsn" "$@"
    fi
}

pg_dump_dsn() {
    dsn="$1"; shift
    if [ "$CLIENT" = host ]; then
        pg_dump "$@" -d "$dsn"
    else
        compose_run exec -T "$CONTAINER_SERVICE" pg_dump "$@" -d "$dsn"
    fi
}

pg_restore_stdin() { # stdin is the archive
    if [ "$CLIENT" = host ]; then
        pg_restore "$@"
    else
        compose_run exec -T "$CONTAINER_SERVICE" pg_restore "$@"
    fi
}

# The shim lets tools/reconcile/reconcile.sh - which calls a bare "psql" - run on
# a host with no PostgreSQL client, by execing the real psql inside the compose
# service. This is the same technique tools/reconcile/README.md documents for
# verifying that gate on this host. Argv is preserved exactly; "-f <path>"
# carries a HOST path that does not exist in the container, so it is translated
# to stdin.
make_psql_shim() {
    mkdir -p "$SHIM_DIR" || return 1
    cat > "$SHIM_DIR/psql" <<'SHIM'
#!/bin/sh
# Generated by tools/drill/drill.sh - a psql that runs the real psql inside the
# postgres compose service. Argv order and content are preserved; -f <file> is
# read from a host path and piped in on stdin, because the container cannot see
# the host filesystem.
set -u
AF="$TMPDIR_SHIM/args.$$"
: > "$AF"
FILE=""
while [ $# -gt 0 ]; do
    case "$1" in
        -f)  FILE="$2"; shift 2 ;;
        -f*) FILE=$(printf '%s' "$1" | cut -c3-); shift ;;
        *)   printf '%s\n' "$1" >> "$AF"; shift ;;
    esac
done
set --
while IFS= read -r a; do set -- "$@" "$a"; done < "$AF"
rm -f "$AF"
export MSYS_NO_PATHCONV=1
if [ -n "$FILE" ]; then
    [ -r "$FILE" ] || { echo "drill-psql-shim: cannot read $FILE" >&2; exit 3; }
    exec sh -c 'D=$1; S=$2; shift 2; cd -- "$D" || exit 1; MSYS_NO_PATHCONV=1 exec docker compose -f "'"$DRILL_COMPOSE_BASE"'" exec -T "$S" psql "$@"' _ "$DRILL_COMPOSE_DIR" "$DRILL_SERVICE" "$@" < "$FILE"
fi
exec sh -c 'D=$1; S=$2; shift 2; cd -- "$D" || exit 1; MSYS_NO_PATHCONV=1 exec docker compose -f "'"$DRILL_COMPOSE_BASE"'" exec -T "$S" psql "$@"' _ "$DRILL_COMPOSE_DIR" "$DRILL_SERVICE" "$@"
SHIM
    chmod +x "$SHIM_DIR/psql" 2>/dev/null || :
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
        say "drill: --keep-scratch: LEAVING the scratch database '$DRILL_TARGET' in place (doc step 7 skipped)"
        SCRATCH_CREATED=0
        return 0
    fi
    printf 'DROP DATABASE IF EXISTS "%s" WITH (FORCE);\n' "$DRILL_TARGET" > "$OUT"
    if ! psql_dsn "$(dsn_for_db postgres)" -v ON_ERROR_STOP=1 -q < "$OUT" > "$OUT.o" 2>"$ERR"; then
        fail "could not drop the scratch database '$DRILL_TARGET' - IT IS STILL THERE:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        SCRATCH_CREATED=1
        return 8
    fi
    SCRATCH_CREATED=0
    say "drill: step 9 teardown - dropped the scratch database '$DRILL_TARGET'"
    return 0
}

finish() { # finish <exit code>
    code="$1"
    cleanup_tmp
    exit "$code"
}

say "drill: ================= apikita RESTORE DRILL ================="
say "drill: date_utc     $STAMP"
say "drill: run_by       ${USER:-${USERNAME:-$(id -un 2>/dev/null || echo unknown)}} on $(hostname 2>/dev/null || echo unknown-host)"
say "drill: target       ${DRILL_TARGET:-<none>}   (scratch; refused if it is the live db)"
say "drill: live_db      $DRILL_LIVE_DB"
say "drill: mode         $([ "$VERIFY_ONLY" = 1 ] && echo 'verify-only (no restore, no teardown)' || echo 'full drill')"
say "drill: log          $LOG"
say "drill: doc          docs/backup-and-restore.md - RTO ${RTO_BUDGET_SECONDS}s, RPO 900s"

# --- STEP 1: guard ----------------------------------------------------------
say "drill: step 1 - safety guard on the target name"
guard_target
GUARD_CODE=$?
if [ "$GUARD_CODE" != 0 ]; then
    say "drill: step 1 REFUSED - target '${DRILL_TARGET:-<none>}' rejected (exit $GUARD_CODE). No database was contacted."
    say "drill: result       REFUSED (exit $GUARD_CODE)"
    finish "$GUARD_CODE"
fi
say "drill: step 1 OK - '$DRILL_TARGET' is a scratch name, not '$DRILL_LIVE_DB'"

# --- tool availability ------------------------------------------------------
if [ "$CLIENT" = none ]; then
    fail "no PostgreSQL client: psql/pg_restore/pg_dump are not on PATH, and no docker fallback exists"
    fail "  looked for: psql, pg_restore, pg_dump on PATH; docker + $COMPOSE_FILE"
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
say "drill: client       $CLIENT_DESC  (db host '$DRILL_DB_HOST')"

if [ -z "$DRILL_SOURCE_DSN" ]; then
    DRILL_SOURCE_DSN=$(dsn_for_db "$DRILL_LIVE_DB")
    say "drill: source       $DRILL_SOURCE_DSN (defaulted: local stack, db '$DRILL_LIVE_DB')"
else
    say "drill: source       $DRILL_SOURCE_DSN"
fi
SCRATCH_DSN=$(dsn_for_db "$DRILL_TARGET")

RESTORE_MS=""
BACKUP_AGE_S=""

if [ "$VERIFY_ONLY" != 1 ]; then
    # --- STEP 2: obtain a dump ----------------------------------------------
    if [ -z "$DUMP" ]; then
        DUMP="$REPO_ROOT/.agents/drill-dumps/drill-$STAMP-$SAFE_TARGET.dump"
        mkdir -p "$(dirname -- "$DUMP")" || { fail "cannot create $(dirname -- "$DUMP")"; finish 2; }
        say "drill: step 2 - no --dump given, taking a fresh pg_dump -Fc of the source"
        umask 077
        if ! pg_dump_dsn "$DRILL_SOURCE_DSN" -Fc > "$DUMP" 2>"$ERR"; then
            fail "pg_dump failed:"
            [ -s "$ERR" ] && cat "$ERR" >&2
            rm -f "$DUMP"
            say "drill: result       FAIL (exit 6)"
            finish 6
        fi
    else
        say "drill: step 2 - restoring the supplied artifact $DUMP"
    fi
    if [ ! -f "$DUMP" ]; then
        fail "the dump does not exist: $DUMP"
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    DUMP_BYTES=$(wc -c < "$DUMP" | tr -d ' ')
    if [ "$DUMP_BYTES" -eq 0 ]; then
        fail "the dump is EMPTY ($DUMP_BYTES bytes) - a truncated dump looks successful and restores nothing"
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
    say "drill: dump         $DUMP"
    say "drill: dump_size    $DUMP_BYTES bytes  sha256=$DUMP_SHA"
    say "drill: backup_age   ${BACKUP_AGE_S}s at restore time"

    # --- STEP 3: the archive must be readable -------------------------------
    say "drill: step 3 - preflight: pg_restore --list on the archive"
    if ! pg_restore_stdin --list < "$DUMP" > "$OUT" 2>"$ERR"; then
        fail "the dump is not a readable custom-format archive (pg_restore --list failed):"
        [ -s "$ERR" ] && cat "$ERR" >&2
        say "drill: result       FAIL (exit 6)"
        finish 6
    fi
    TOC=$(grep -c '^[0-9]' "$OUT" 2>/dev/null || echo 0)
    say "drill: preflight    OK - $TOC table-of-contents entries"

    # --- STEP 4: create the scratch database --------------------------------
    say "drill: step 4 - DROP + CREATE the scratch database '$DRILL_TARGET'"
    say "drill:   (a leftover scratch db from an aborted run must not be restored over)"
    {
        printf 'DROP DATABASE IF EXISTS "%s" WITH (FORCE);\n' "$DRILL_TARGET"
        printf 'CREATE DATABASE "%s";\n' "$DRILL_TARGET"
    } > "$OUT"
    if ! psql_dsn "$(dsn_for_db postgres)" -v ON_ERROR_STOP=1 -q < "$OUT" >"$OUT.o" 2>"$ERR"; then
        fail "could not create the scratch database '$DRILL_TARGET':"
        [ -s "$ERR" ] && cat "$ERR" >&2
        say "drill: result       FAIL (exit 4)"
        finish 4
    fi
    SCRATCH_CREATED=1

    # --- STEP 5: TIME the restore -------------------------------------------
    say "drill: step 5 - RESTORE (timed): pg_restore --clean --if-exists -d <scratch> <dump>"
    T0=$(now_ms)
    if ! pg_restore_stdin --clean --if-exists -d "$SCRATCH_DSN" < "$DUMP" > "$OUT" 2>"$ERR"; then
        RESTORE_MS=$(( $(now_ms) - T0 ))
        fail "pg_restore FAILED after ${RESTORE_MS}ms - the restore itself is broken:"
        [ -s "$ERR" ] && cat "$ERR" >&2
        [ -s "$OUT" ] && cat "$OUT" >&2
        record "restore_ms    $RESTORE_MS"
        record "result        FAIL (exit 7)"
        say "$RESULTS"
        teardown || :
        say "drill: log          $LOG"
        finish 7
    fi
    RESTORE_MS=$(( $(now_ms) - T0 ))
    RESTORE_S=$((RESTORE_MS / 1000))
    RESTORE_FRAC=$((RESTORE_MS % 1000))
    [ -s "$ERR" ] && { say "drill: pg_restore diagnostics (stderr):"; emit "$ERR"; }
    say "drill: RESTORE TIME $((RESTORE_MS / 1000)).$(printf '%03d' "$RESTORE_FRAC") s  (${RESTORE_MS} ms) - this is the real RTO"
    if [ "$RESTORE_S" -le "$RTO_BUDGET_SECONDS" ]; then
        say "drill:   within the documented RTO budget of ${RTO_BUDGET_SECONDS}s"
    else
        say "drill:   OVER the documented RTO budget of ${RTO_BUDGET_SECONDS}s - the RTO claim in docs/backup-and-restore.md is FALSE"
    fi
else
    say "drill: --verify-only - skipping steps 2-5 (dump, preflight, create, restore)"
    say "drill: verifying the EXISTING database '$DRILL_TARGET' as if it were the restored one"
    # SCRATCH_CREATED deliberately stays 0: this mode did not create the database,
    # so it must not DROP it either. Teardown owns only what this run made.
    say "drill:   (teardown skipped: this run did not create '$DRILL_TARGET')"
fi

# ---------------------------------------------------------------------------
# STEP 6 - the numbers that matter, from the RESTORED database.
# ---------------------------------------------------------------------------
FAILED=0
say "drill: step 6 - verify.sql against the scratch database (row counts, money totals, key hashes)"
if ! psql_dsn "$SCRATCH_DSN" -v ON_ERROR_STOP=1 -t -A -F'|' < "$VERIFY_SQL" > "$SCR_METRICS" 2>"$ERR"; then
    fail "psql failed reading the scratch database:"
    [ -s "$ERR" ] && cat "$ERR" >&2
    say "drill: result       FAIL (exit 4)"
    teardown || :
    finish 4
fi
[ -s "$ERR" ] && { say "drill: psql diagnostics (diagnostic, NOT drift):"; emit "$ERR"; }
emit "$SCR_METRICS"

if ! psql_dsn "$DRILL_SOURCE_DSN" -v ON_ERROR_STOP=1 -t -A -F'|' < "$VERIFY_SQL" > "$SRC_METRICS" 2>"$ERR"; then
    fail "psql failed reading the SOURCE database - row counts and the spot-check need it:"
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
        fail "  truncated dump. Raise DRILL_ROW_TOLERANCE_PCT only with a reason you would defend in a postmortem."
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
SPOT_SQL='SELECT w.balance_idr::text, COALESCE((SELECT sum(delta_idr) FROM ledger l WHERE l.account_id = w.account_id), 0)::text FROM wallets w WHERE w.account_id = '
ACCT="$DRILL_SPOT_ACCOUNT_ID"
if [ -z "$ACCT" ]; then
    ACCT=$(printf "SELECT w.account_id FROM wallets w JOIN accounts a ON a.id = w.account_id ORDER BY w.balance_idr DESC, w.account_id LIMIT 1;\n" \
        | psql_dsn "$DRILL_SOURCE_DSN" -v ON_ERROR_STOP=1 -t -A 2>"$ERR" | tr -d '\r' | head -1)
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
            printf "%s'%s';\n" "$SPOT_SQL" "$ACCT" > "$OUT"
            SRC_SPOT=$(psql_dsn "$DRILL_SOURCE_DSN" -v ON_ERROR_STOP=1 -t -A -F'|' < "$OUT" 2>"$ERR" | tr -d '\r' | head -1)
            SCR_SPOT=$(psql_dsn "$SCRATCH_DSN" -v ON_ERROR_STOP=1 -t -A -F'|' < "$OUT" 2>"$ERR" | tr -d '\r' | head -1)
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
say "drill:   DATABASE_URL=<scratch dsn> sh $RECONCILE_SH"
REC_STATUS=0
if [ "$CLIENT" = host ]; then
    DATABASE_URL="$SCRATCH_DSN" sh "$RECONCILE_SH" > "$REC_OUT" 2>"$REC_ERR" || REC_STATUS=$?
else
    if ! make_psql_shim; then
        fail "could not generate the psql shim needed to run reconcile.sh without a host client"
        say "drill: result       FAIL (exit 3)"
        teardown || :
        finish 3
    fi
    say "drill:   (no host psql: reconcile.sh runs against a shim that execs psql in the '$CONTAINER_SERVICE' service)"
    # The shim reads these from its environment, so the drill's own
    # configuration reaches it without the shim having to be re-rendered.
    export DRILL_COMPOSE_DIR="$COMPOSE_DIR" DRILL_COMPOSE_BASE="$COMPOSE_BASE" \
           DRILL_SERVICE="$CONTAINER_SERVICE" TMPDIR_SHIM="$SHIM_DIR"
    PATH="$SHIM_DIR:$PATH" DATABASE_URL="$SCRATCH_DSN" sh "$RECONCILE_SH" > "$REC_OUT" 2>"$REC_ERR" || REC_STATUS=$?
fi
[ -s "$REC_OUT" ] && emit "$REC_OUT"
[ -s "$REC_ERR" ] && { say "drill: reconcile stderr:"; emit "$REC_ERR"; }

case "$REC_STATUS" in
    0) say "drill:   drift check PASSED - reconcile.sh exited 0 (zero drifting rows)" ;;
    1) fail "drift check FAILED - reconcile.sh exited 1: the wallet cache disagrees with the authoritative ledger"
       fail "  THE CHECK is the gate: docs/backup-and-restore.md - \"Zero rows from the reconciliation query is the gate\""
       fail "  either the restore is corrupt or the source data has a bug; both block trusting a recovery"
       FAILED=1 ;;
    2) fail "reconcile.sh exited 2: DATABASE_URL was not passed correctly - this is a drill bug"
       FAILED=1 ;;
    3) fail "reconcile.sh exited 3: no psql available to it"
       FAILED=1 ;;
    4) fail "reconcile.sh exited 4: psql failed against the scratch database"
       FAILED=1 ;;
    5) fail "reconcile.sh exited 5: a stranded reservation hold older than the bound - money unaccounted for"
       fail "  structurally invisible to the drift query; see tools/reconcile/README.md"
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
say "drill: dump          ${DUMP:-<none>}"
[ -n "${DUMP_SHA:-}" ] && say "drill: dump_sha256   $DUMP_SHA  ($DUMP_BYTES bytes, $TOC TOC entries)"
say "drill: backup_age    ${BACKUP_AGE_S:-<none: verify-only>} s at restore time"
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
    say "drill: PASS - the backup restored, reconciles with the ledger, and matches the source."
    say "drill: This is measured evidence for the RTO/RPO claim in docs/backup-and-restore.md."
else
    say "drill: FAIL (exit $CODE) - do NOT claim a working restore until this is green."
fi
finish "$CODE"
