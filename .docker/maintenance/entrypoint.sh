#!/bin/sh
# apikita maintenance scheduler - the thing that finally runs the jobs.
#
# THE GAP THIS CLOSES. docs/observability.md:161 ("neither has a scheduler
# behind it yet") and docs/ip-tracking.md:203 ("[ ] The purge is a binary with no
# scheduler behind it yet") both admit it: server/src/bin/{ip-purge,hold-sweep}
# and tools/reconcile/reconcile.sh were written and then nothing ever ran them.
# A retention promise with no job behind it is a sentence in a document, not a
# behaviour in production.
#
# =============================================================================
# WHAT THIS IMAGE CAN AND CANNOT RUN. Read before trusting a green run.
# =============================================================================
#
#   retention   RUNS HERE, FOR REAL - once the image can reach the database. The
#               full body of the Rust retention sweep is two statements
#               (server/src/ip_tracking.rs:298-316):
#                   DELETE FROM key_ip_seen  WHERE day <= today - 7   (7d promise)
#                   DELETE FROM key_ip_daily WHERE day <= today - 90  (90d promise)
#               against SEEN_RETENTION_DAYS=7 / DAILY_RETENTION_DAYS=90 in the
#               same file. This job applies exactly that SQL through the sqlite3
#               CLI, reports the same two counts the binary logs, and exits
#               non-zero if either delete fails. The inclusive `<=` cutoff matches
#               the Rust comment at line 281-288: 7 and 90 are days RETAINED, so
#               day <= today - N is deleted.
#
#               The `day` column is RFC3339-date TEXT ('YYYY-MM-DD'), so the
#               cutoff is `date('now', '-N days')` - a string comparison of ISO
#               dates, which is what the schema's GLOB check guarantees. date('now')
#               is UTC, matching the Rust binary.
#
#   reconcile   RUNS HERE, FOR REAL - same condition. tools/reconcile/reconcile.sh
#               needs the sqlite3 CLI and $RECONCILE_DATABASE_URL, and its exit
#               code is preserved verbatim and never swallowed:
#               1 = drift, 2 = bad DATABASE_URL, 3 = no sqlite3, 4 = sqlite3
#               failed, 5 = stranded hold, 6 = no such database file.
#               There is no `|| true` anywhere near it.
#
#   ip-purge    NOT WIRED IN THIS TOPOLOGY. server/src/bin/ip-purge.rs is a Rust
#               binary that reaches the database through sqlx. server/ has no
#               Dockerfile and docker-compose.yml has no Rust build stage, so no
#               image in this file can contain it. This service does not pretend
#               otherwise: it says NOT WIRED at startup and never claims to have
#               run it. The retention WINDOW is still enforced (see above); it is
#               the BINARY that does not run here.
#
#   hold-sweep  NOT WIRED, same reason (server/src/bin/hold-sweep.rs). Nothing
#               sweeps stranded reservation holds in this topology. That matters:
#               a stranded hold is invisible money - the ledger still balances
#               and reconciliation returns nothing - which is exactly why the
#               900s bound exists. This gap is loud, not silent.
#
# Run the two Rust jobs on the HOST, on the same nightly cadence, until a server
# image exists:
#
#   DATABASE_URL='sqlite://data/server.db' cargo run --manifest-path server/Cargo.toml --bin ip-purge
#   DATABASE_URL='sqlite://data/server.db' cargo run --manifest-path server/Cargo.toml --bin hold-sweep
#
# -----------------------------------------------------------------------------
# NOT YET PORTED, AND OUTSIDE THIS FILE: the compose service that runs this script.
#
# docker-compose.yml's `scheduler` service is still `image: postgres:16` and still
# sets postgres:// DSNs for DATABASE_URL and RECONCILE_DATABASE_URL. It was kept for
# its psql client, and there is no Postgres any more. Two things must change there,
# and neither is this script's to change:
#
#   1. the image must stop being postgres:16 and must gain a sqlite3 binary;
#   2. both DSNs must become sqlite:// URLs, and the API's data directory must be
#      mounted, or the container cannot see the database file at all.
#
# Until that happens the nightly run FAILS LOUDLY - which is the point. This script
# refuses a non-SQLite URL by name (exit 2 from the job, reported as a failure) and
# refuses to run at all if sqlite3 is absent, rather than reporting a clean sheet
# against a database it never opened. The banner below says so on every start.
# -----------------------------------------------------------------------------
#
# NO SIMULATED WORK. Every job this service does not run is announced as NOT
# WIRED on startup, in the log, and in .docker/maintenance/README.md. A job that
# fails makes this process exit non-zero; it is never reported as success.

# NOTE: `set -u` only, deliberately not `set -e`. This loop must survive a
# failed nightly run so it can retry the next night; a stray `set -e` abort
# would turn one bad night into a permanently dead scheduler, which is a quieter
# version of the bug this service exists to fix. Every fallible step is checked
# explicitly instead.
set -u

SCHEDULE_HOUR_UTC="${SCHEDULE_HOUR_UTC:-3}"
# Reconciliation gets its own variable so a bad DSN can be pointed at this job
# alone without disarming the retention sweep.
RECONCILE_DATABASE_URL="${RECONCILE_DATABASE_URL:-${DATABASE_URL:-}}"
export RECONCILE_DATABASE_URL

# Overridable so this job can be exercised without a container: the default is the
# path the compose service mounts, which is what production uses.
RECONCILE_SH="${RECONCILE_SH:-/usr/local/share/reconcile/reconcile.sh}"

TMP="${TMPDIR:-/tmp}"
SQL_ERR="$TMP/maintenance-sql.$.err"
trap 'rm -f "$SQL_ERR"' EXIT HUP INT TERM

log() {
    printf '%s maintenance: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"
}

# --- the database file -------------------------------------------------------
# The database is embedded SQLite: a FILE, named by DATABASE_URL, in the form
# sqlite://data/server.db or sqlite:///abs/path.db. There is no service, no host
# and no port. A `case`, not a blind prefix strip: a leftover postgres:// URL must
# be refused by name rather than rewritten into a relative path that happens to be
# a plausible filename (tools/reconcile/reconcile.sh makes the same argument).
db_file_from_url() {
    _u="$1"
    case "$_u" in
        sqlite://*) _p=${_u#sqlite://} ;;
        sqlite:*)   _p=${_u#sqlite:} ;;
        *) return 2 ;;
    esac
    _p=${_p%%\?*}
    case "$_p" in
        ''|':memory:') return 2 ;;
    esac
    # Relative to the API's working directory (server/), which is the directory
    # the compose service must mount for this job to see anything at all.
    case "$_p" in
        /*) printf '%s' "$_p" ;;
        *)  printf '%s/server/%s' "${APP_DIR:-/srv/apikita}" "$_p" ;;
    esac
    return 0
}

have_sqlite3() { command -v sqlite3 >/dev/null 2>&1; }

# -----------------------------------------------------------------------------
# Startup banner - states, out loud, what runs and what does not.
# -----------------------------------------------------------------------------
banner() {
    log "scheduler starting - cadence: nightly at ${SCHEDULE_HOUR_UTC}:00 UTC"
    if have_sqlite3; then
        log "CLIENT    sqlite3 $(sqlite3 --version 2>/dev/null | cut -d' ' -f1-2) on PATH"
    else
        log "CLIENT    sqlite3 IS NOT INSTALLED IN THIS IMAGE. Every database job below will FAIL, loudly, rather than report a clean sheet against a database it never opened. The scheduler image must stop being postgres:16 and must provide a sqlite3 binary - docker-compose.yml is still unported and is NOT this script's to change."
    fi
    log "WIRED     retention  - IP-tracking retention sweep, SQL inline in this entrypoint: key_ip_seen > 7d, key_ip_daily > 90d (docs/ip-tracking.md retention promise)"
    log "WIRED     reconcile  - tools/reconcile/reconcile.sh, exit code preserved (1=drift 2=no DATABASE_URL 3=no sqlite3 4=sqlite3 failed 5=stranded hold 6=no such database file)"
    log "NOT WIRED ip-purge   - server/src/bin/ip-purge.rs is a Rust binary and no server image exists in this compose file; it does NOT run here. The retention window above is still enforced."
    log "NOT WIRED hold-sweep - server/src/bin/hold-sweep.rs is a Rust binary and no server image exists in this compose file; it does NOT run here. Nothing sweeps stranded holds in this topology."
    log "NOT WIRED ip-purge/hold-sweep are report-only gaps, not silent ones. Run them on the host on the same cadence: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin ip-purge (or --bin hold-sweep)"
    log "DATABASE_URL=${DATABASE_URL:-<unset>}"
    log "RECONCILE_DATABASE_URL=${RECONCILE_DATABASE_URL:-<unset>}"
    if [ -n "${DATABASE_URL:-}" ]; then
        if DB_FILE=$(db_file_from_url "$DATABASE_URL"); then
            log "database file $DB_FILE"
            [ -f "$DB_FILE" ] || log "database file $DB_FILE DOES NOT EXIST - every job below will fail until it does (create it with: cargo run --bin migrate, from server/)"
        else
            log "DATABASE_URL is NOT a sqlite:// URL - every database job below will refuse it by name rather than guess a filename"
        fi
    fi
}

# -----------------------------------------------------------------------------
# Job 1 - retention. The runnable form of the ip-purge binary's SQL.
# -----------------------------------------------------------------------------
# $1 = table, $2 = days retained. -bail makes a SQL failure a non-zero exit
# instead of a silent zero-row success, and `SELECT changes()` returns the deleted
# count in the same round trip - the same two counts the Rust binary logs.
retention_delete() {
    sqlite3 -bail -noheader -separator '|' "$1" \
        "DELETE FROM $2 WHERE day <= date('now', '-$3 days'); SELECT changes();" 2>"$SQL_ERR"
}

run_retention() {
    log "job retention: start"
    if [ -z "${DATABASE_URL:-}" ]; then
        log "job retention: FAILED - DATABASE_URL is not set (refusing to report a sweep that did not run)"
        return 1
    fi
    if ! have_sqlite3; then
        log "job retention: FAILED - sqlite3 is not installed in this image (psql is no longer the client; there is no database server)"
        return 1
    fi
    if ! DB_FILE=$(db_file_from_url "$DATABASE_URL"); then
        log "job retention: FAILED - DATABASE_URL is not a sqlite:// URL, or names an in-memory database: $DATABASE_URL"
        log "job retention:   expected e.g. sqlite://data/server.db. Nothing was swept."
        return 1
    fi
    if [ ! -f "$DB_FILE" ]; then
        log "job retention: FAILED - no such database file: $DB_FILE (nothing was swept)"
        return 1
    fi

    seen=$(retention_delete "$DB_FILE" key_ip_seen 7) || {
        log "job retention: FAILED - the key_ip_seen delete did not run (sqlite3 error above)"
        [ -s "$SQL_ERR" ] && while IFS= read -r l; do log "job retention:   $l"; done < "$SQL_ERR"
        return 1
    }
    daily=$(retention_delete "$DB_FILE" key_ip_daily 90) || {
        log "job retention: FAILED - the key_ip_daily delete did not run (sqlite3 error above)"
        [ -s "$SQL_ERR" ] && while IFS= read -r l; do log "job retention:   $l"; done < "$SQL_ERR"
        return 1
    }
    # A blank count is not a zero count: `SELECT changes()` always returns a row, so
    # anything non-numeric means the delete did not do what this job claims.
    case "$seen" in ''|*[!0-9]*) log "job retention: FAILED - key_ip_seen returned '$seen', not a count"; return 1 ;; esac
    case "$daily" in ''|*[!0-9]*) log "job retention: FAILED - key_ip_daily returned '$daily', not a count"; return 1 ;; esac

    log "job retention: OK - key_ip_seen deleted=$seen (retain 7d), key_ip_daily deleted=$daily (retain 90d)"
    return 0
}

# -----------------------------------------------------------------------------
# Job 2 - ledger reconciliation. Exit code preserved; never swallowed.
# -----------------------------------------------------------------------------
run_reconcile() {
    log "job reconcile: start - tools/reconcile/reconcile.sh (1=drift 2=no DATABASE_URL 3=no sqlite3 4=sqlite3 failed 5=stranded hold 6=no such database file)"
    if [ ! -f "$RECONCILE_SH" ]; then
        log "job reconcile: FAILED - \$RECONCILE_SH is not mounted at $RECONCILE_SH"
        return 1
    fi

    # reconcile.sh reads plain \$DATABASE_URL - that is its documented contract
    # (tools/reconcile/README.md) and this service does not get to rewrite it.
    # Hand the child the reconciliation DSN under that name so the two jobs can be
    # armed independently. Getting this wrong is silent: reconcile.sh would
    # quietly read the OTHER dsn and still exit 0, which is the same
    # reports-success-while-nothing-ran bug this whole service exists to kill.
    # Errexit is deliberately NEVER enabled in this script (see the top): the exit
    # code is captured explicitly instead, so a drifting ledger is a reported
    # failure and not a dead scheduler.
    DATABASE_URL="$RECONCILE_DATABASE_URL" sh "$RECONCILE_SH"
    rc=$?

    if [ "$rc" -eq 0 ]; then
        log "job reconcile: OK - 0 drifting accounts (exit 0)"
        return 0
    fi

    log "job reconcile: FAILED - exit $rc. This is NOT a success: reconcile.sh is the Gate 2 money-correctness gate and its exit code is reported verbatim, never masked."
    return 1
}

# -----------------------------------------------------------------------------
# Job 3 - hold-sweep. Cannot run here. Says so, every night.
# -----------------------------------------------------------------------------
run_hold_sweep_not_wired() {
    log "job hold-sweep: NOT RUN - server/src/bin/hold-sweep.rs is a Rust binary and this image contains no Rust build or binary. A stranded reservation hold is invisible money, so this gap is announced, never silently skipped."
    log "job hold-sweep: run it on the host: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin hold-sweep"
    return 0
}

run_wired_jobs() {
    rc=0
    run_retention || rc=1
    run_reconcile || rc=1
    run_hold_sweep_not_wired
    return "$rc"
}

# -----------------------------------------------------------------------------
# Schedule
# -----------------------------------------------------------------------------
next_run_epoch() {
    now=$(date -u +%s)
    target=$(date -u -d "today ${SCHEDULE_HOUR_UTC}:00" +%s) || return 1
    if [ "$target" -le "$now" ]; then
        target=$(date -u -d "tomorrow ${SCHEDULE_HOUR_UTC}:00" +%s) || return 1
    fi
    printf '%s' "$target"
}

schedule_loop() {
    while :; do
        next=$(next_run_epoch) || {
            log "FATAL - could not compute the next run time for SCHEDULE_HOUR_UTC=${SCHEDULE_HOUR_UTC}"
            exit 2
        }
        now=$(date -u +%s)
        wait_for=$((next - now))
        log "next run in ${wait_for}s at $(date -u -d "$$next" '+%Y-%m-%dT%H:%M:%SZ')"
        sleep "$wait_for"
        run_wired_jobs || log "nightly run finished with failures - see the job lines above; retrying at the next scheduled time"
    done
}

# -----------------------------------------------------------------------------
# Entry
# -----------------------------------------------------------------------------
case "${SCHEDULE_HOUR_UTC}" in
    '' | *[!0-9]*)
        log "FATAL - SCHEDULE_HOUR_UTC must be an hour 0-23, got '${SCHEDULE_HOUR_UTC}'"
        exit 2
        ;;
esac
if [ "$SCHEDULE_HOUR_UTC" -gt 23 ]; then
    log "FATAL - SCHEDULE_HOUR_UTC must be an hour 0-23, got '${SCHEDULE_HOUR_UTC}'"
    exit 2
fi

banner

case "${1:-schedule}" in
    schedule)
        schedule_loop
        ;;
    once)
        run_wired_jobs
        exit $?
        ;;
    retention)
        run_retention
        exit $?
        ;;
    reconcile)
        run_reconcile
        exit $?
        ;;
    *)
        echo "usage: maintenance-entrypoint.sh [schedule|once|retention|reconcile]" >&2
        exit 2
        ;;
esac
