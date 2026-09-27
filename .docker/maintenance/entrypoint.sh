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
#   ip-purge    THE BINARY IS NOT WIRED; ITS WORK IS. server/src/bin/ip-purge.rs
#               reaches the database through sqlx. server/Dockerfile exists (W17)
#               but ships only `apikita-server` and `migrate`, so no image in THIS
#               container has the binary. The retention WINDOW is still enforced -
#               `run_retention` applies the same two DELETEs through sqlite3 - so
#               the promise is kept; it is the binary that does not run here.
#
#   usage-purge THE BINARY IS NOT WIRED; ITS WORK IS. Same shape as ip-purge:
#               server/src/bin/usage-purge.rs, not shipped in the server image.
#               `run_retention` now applies all THREE of its deletes -
#               usage_events (90d), usage_daily (730d) and expired/revoked
#               sessions (30d) - through sqlite3, so
#               docs/data-retention.md is enforced here.
#
#   hold-sweep  NOT WIRED, and NOT covered by an inline equivalent. Nothing sweeps
#               stranded reservation holds in this topology. That matters: a
#               stranded hold is invisible money - the ledger still balances and
#               reconciliation returns nothing - which is exactly why the 900s
#               bound exists. This gap is LOUD, not silent: the banner below names
#               it on every start.
#
# Run the three Rust jobs on the HOST, on the same nightly cadence, until they are
# wired into a scheduled container. Their WORK is already done in-container for
# ip-purge and usage-purge (see run_retention); hold-sweep has no equivalent yet,
# so it must run on the host:
#
#   DATABASE_URL='sqlite://data/server.db' cargo run --manifest-path server/Cargo.toml --bin ip-purge
#   DATABASE_URL='sqlite://data/server.db' cargo run --manifest-path server/Cargo.toml --bin usage-purge
#   DATABASE_URL='sqlite://data/server.db' cargo run --manifest-path server/Cargo.toml --bin hold-sweep
#
# -----------------------------------------------------------------------------
# THE COMPOSE SERVICE THAT RUNS THIS SCRIPT - PORTED.
#
# docker-compose.yml's `scheduler` service now builds from
# `.docker/maintenance/Dockerfile` (the one-reason sqlite3 image) and mounts the
# API's data directory, so the container opens the SAME file the API writes. It
# sets sqlite:// DSNs for DATABASE_URL and RECONCILE_DATABASE_URL. This replaced
# the earlier `image: postgres:16` service; there is no Postgres any more, and
# psql could never have opened a SQLite file. The host run in docker-compose.yml
# is the authoritative one - this note only records that the port happened.
#
# The script still refuses loudly rather than reporting a clean sheet: a non-SQLite
# URL by name, a missing sqlite3, or a database file it cannot see all FAIL the
# job, and the banner below says what is wired on every start.
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
        log "CLIENT    sqlite3 IS NOT INSTALLED IN THIS IMAGE. Every database job below will FAIL, loudly, rather than report a clean sheet against a database it never opened. The scheduler image (`.docker/maintenance/Dockerfile`) must provide a sqlite3 binary."
    fi
    log "WIRED     retention  - age-based sweep, SQL inline in this entrypoint: key_ip_seen > 7d, key_ip_daily > 90d (docs/ip-tracking.md); usage_events > 90d, usage_daily > 730d, expired/revoked sessions > 30d (docs/data-retention.md)"
    log "WIRED     reconcile  - tools/reconcile/reconcile.sh, exit code preserved (1=drift 2=no DATABASE_URL 3=no sqlite3 4=sqlite3 failed 5=stranded hold 6=no such database file)"
    log "NOT WIRED ip-purge   - server/src/bin/ip-purge.rs is a Rust binary NOT shipped in the server image; it does NOT run here. Its retention window IS enforced inline (see retention above)."
    log "NOT WIRED usage-purge - server/src/bin/usage-purge.rs, same: not shipped, does NOT run here. Its three sweeps ARE enforced inline (see retention above)."
    log "NOT WIRED hold-sweep - server/src/bin/hold-sweep.rs is a Rust binary NOT shipped in the server image; it does NOT run here, and nothing inline replaces it. Nothing sweeps stranded holds in this topology."
    log "NOT WIRED these three are report-only gaps, not silent ones. Run them on the host on the same cadence: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin ip-purge (or --bin usage-purge, --bin hold-sweep)"
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
# Job 1 - retention. The runnable form of the ip-purge binary's SQL, plus the
# age-based sweep `usage-purge` performs.
# -----------------------------------------------------------------------------
# $1 = table, $2 = days retained. -bail makes a SQL failure a non-zero exit
# instead of a silent zero-row success, and `SELECT changes()` returns the deleted
# count in the same round trip - the same counts the Rust binaries log.
#
# `day` is a DATE column ('YYYY-MM-DD' TEXT), so the cutoff is
# `date('now', '-N days')` - an ISO string comparison, which the schema's GLOB
# check guarantees. `date('now')` is UTC, matching the Rust binaries.
retention_delete() {
    sqlite3 -bail -noheader -separator '|' "$1" \
        "DELETE FROM $2 WHERE day <= date('now', '-$3 days'); SELECT changes();" 2>"$SQL_ERR"
}

# The same sweep for a table whose age column is a full RFC3339 TIMESTAMP
# (`usage_events.created_at`, `sessions`), not a DATE.
#
# THIS IS NOT THE SAME QUERY, and using the date form would silently fail: a bare
# 'YYYY-MM-DD' cutoff compares as a STRING against 'YYYY-MM-DDTHH:MM:SS+00:00', and
# the shorter string sorts FIRST - the DELETE would match nothing and rows would
# survive forever, which is a retention failure in the direction that KEEPS data.
# So the cutoff is a datetime and the comparison is on the full instant. This
# mirrors server/src/db.rs `purge_expired_usage`, which binds an instant for the
# same reason.
#
# `$4` is a full SQL predicate on the timestamp column, so the sessions case can
# express its extra rule (a session is swept from the instant it STOPPED being
# usable - `revoked_at` when logged out early, else `expires_at`).
retention_delete_instant() {
    sqlite3 -bail -noheader -separator '|' "$1" \
        "DELETE FROM $2 WHERE $3 <= datetime('now', '-$4 days'); SELECT changes();" 2>"$SQL_ERR"
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
    # --- The age-based tables, mirroring server/src/bin/usage-purge.rs --------
    # usage_daily.day is a DATE; usage_events.created_at and sessions.* are
    # TIMESTAMPs, so they use the instant helper.
    usage_daily=$(retention_delete "$DB_FILE" usage_daily 730) || {
        log "job retention: FAILED - the usage_daily delete did not run (sqlite3 error above)"
        return 1
    }
    usage_events=$(retention_delete_instant "$DB_FILE" usage_events created_at 90) || {
        log "job retention: FAILED - the usage_events delete did not run (sqlite3 error above)"
        return 1
    }
    # A session is swept from the instant it stopped being usable: revoked_at for
    # an early logout, else expires_at. COALESCE picks whichever governs.
    sessions=$(retention_delete_instant "$DB_FILE" sessions "COALESCE(revoked_at, expires_at)" 30) || {
        log "job retention: FAILED - the sessions delete did not run (sqlite3 error above)"
        return 1
    }

    # A blank count is not a zero count: `SELECT changes()` always returns a row, so
    # anything non-numeric means the delete did not do what this job claims.
    case "$seen" in ''|*[!0-9]*) log "job retention: FAILED - key_ip_seen returned '$seen', not a count"; return 1 ;; esac
    case "$daily" in ''|*[!0-9]*) log "job retention: FAILED - key_ip_daily returned '$daily', not a count"; return 1 ;; esac
    case "$usage_daily" in ''|*[!0-9]*) log "job retention: FAILED - usage_daily returned '$usage_daily', not a count"; return 1 ;; esac
    case "$usage_events" in ''|*[!0-9]*) log "job retention: FAILED - usage_events returned '$usage_events', not a count"; return 1 ;; esac
    case "$sessions" in ''|*[!0-9]*) log "job retention: FAILED - sessions returned '$sessions', not a count"; return 1 ;; esac

    log "job retention: OK - key_ip_seen=$seen (7d), key_ip_daily=$daily (90d), usage_daily=$usage_daily (730d), usage_events=$usage_events (90d), sessions=$sessions (30d)"
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
# Midnight UTC of "today", as an epoch. $1 = the epoch to take the day of.
#
# WHY THIS IS ARITHMETIC AND NOT `date -d`. The image is Alpine, so `date` is
# BUSYBOX date, which does NOT accept GNU's `-d` relative forms. The original
# implementation called `date -u -d "today 3:00" +%s`, and BusyBox answers
# `date: invalid date 'today 3:00'` - so the nightly loop exited 2 on its FIRST
# iteration, Compose restarted it, and it restarted forever. The one-shot verbs
# (retention/reconcile/once) never touch this function, which is exactly why the
# bug survived: every test used a verb that skipped the loop.
#
# `%s` is seconds since the epoch, always UTC, and `days * 86400` is exact because
# epoch seconds ignore leap seconds. So floor-divide to the day, then add the
# scheduled hour. No timezone handling is needed or wanted: every value here is
# already UTC by definition.
utc_midnight() {
    printf '%s' "$(( $1 - ($1 % 86400) ))"
}

next_run_epoch() {
    now=$(date -u +%s) || return 1
    # Guard the one input that would make the arithmetic silently wrong: an hour
    # outside 0-23. The entry block checks this too, but this function is the one
    # whose output a caller sleeps on, so it refuses rather than returning a
    # plausible-looking wrong instant.
    case "${SCHEDULE_HOUR_UTC}" in
        '' | *[!0-9]*) return 1 ;;
    esac
    if [ "$SCHEDULE_HOUR_UTC" -gt 23 ]; then
        return 1
    fi

    today=$(utc_midnight "$now")
    target=$((today + SCHEDULE_HOUR_UTC * 3600))
    if [ "$target" -le "$now" ]; then
        target=$((target + 86400))
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
        log "next run in ${wait_for}s (at UTC epoch $next; nightly ${SCHEDULE_HOUR_UTC}:00 UTC)"
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
