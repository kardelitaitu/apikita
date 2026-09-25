#!/bin/sh
# apikita maintenance scheduler — the thing that finally runs the jobs.
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
#   retention   RUNS HERE, FOR REAL. The full body of the Rust retention sweep
#               is two statements (server/src/ip_tracking.rs:298-316):
#                   DELETE FROM key_ip_seen  WHERE day <= today - 7   (7d promise)
#                   DELETE FROM key_ip_daily WHERE day <= today - 90  (90d promise)
#               against SEEN_RETENTION_DAYS=7 / DAILY_RETENTION_DAYS=90 in the
#               same file. This image is built FROM postgres:16, so it has the
#               psql client those statements need and nothing else. The job
#               below applies exactly that SQL, reports the same two counts the
#               binary logs, and exits non-zero if either delete fails. The
#               inclusive `<=` cutoff matches the Rust comment at line 281-288:
#               7 and 90 are days RETAINED, so day <= today - N is deleted.
#
#   reconcile   RUNS HERE, FOR REAL. tools/reconcile/reconcile.sh only needs a
#               psql client and $RECONCILE_DATABASE_URL, and this image has both.
#               Its exit code is preserved verbatim and is never swallowed:
#               1 = drift, 2 = no DATABASE_URL, 3 = no psql, 4 = psql failed.
#               There is no `|| true` anywhere near it.
#
#   ip-purge    NOT WIRED IN THIS TOPOLOGY. server/src/bin/ip-purge.rs is a Rust
#               binary that reaches Postgres through sqlx. server/ has no
#               Dockerfile and docker-compose.yml has no Rust build stage, so no
#               image in this file can contain it. This service does not pretend
#               otherwise: it says NOT WIRED at startup and never claims to have
#               run it. The retention WINDOW is still enforced (see above); it is
#               the BINARY that does not run here.
#
#   hold-sweep  NOT WIRED, same reason (server/src/bin/hold-sweep.rs). Nothing
#               sweeps stranded reservation holds in this topology. That matters:
#               a stranded hold is invisible money — the ledger still balances
#               and reconciliation returns nothing — which is exactly why the
#               900s bound exists. This gap is loud, not silent.
#
# Run the two Rust jobs on the HOST, on the same nightly cadence, until a server
# image exists:
#
#   DATABASE_URL='postgres://postgres:dev@localhost:5432/apikita' cargo run --manifest-path server/Cargo.toml --bin ip-purge
#   DATABASE_URL='postgres://postgres:dev@localhost:5432/apikita' cargo run --manifest-path server/Cargo.toml --bin hold-sweep
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

RECONCILE_SH=/usr/local/share/reconcile/reconcile.sh

log() {
    printf '%s maintenance: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$*"
}

# -----------------------------------------------------------------------------
# Startup banner — states, out loud, what runs and what does not.
# -----------------------------------------------------------------------------
banner() {
    log "scheduler starting — cadence: nightly at ${SCHEDULE_HOUR_UTC}:00 UTC"
    log "WIRED     retention  — IP-tracking retention sweep, SQL inline in this entrypoint: key_ip_seen > 7d, key_ip_daily > 90d (docs/ip-tracking.md retention promise)"
    log "WIRED     reconcile  — tools/reconcile/reconcile.sh, exit code preserved (1=drift 2=no DATABASE_URL 3=no psql 4=psql failed)"
    log "NOT WIRED ip-purge   — server/src/bin/ip-purge.rs is a Rust binary and no server image exists in this compose file; it does NOT run here. The retention window above is still enforced."
    log "NOT WIRED hold-sweep — server/src/bin/hold-sweep.rs is a Rust binary and no server image exists in this compose file; it does NOT run here. Nothing sweeps stranded holds in this topology."
    log "NOT WIRED ip-purge/hold-sweep are report-only gaps, not silent ones. Run them on the host on the same cadence: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin ip-purge (or --bin hold-sweep)"
    log "DATABASE_URL=${DATABASE_URL:-<unset>}"
    log "RECONCILE_DATABASE_URL=${RECONCILE_DATABASE_URL:-<unset>}"
}

# -----------------------------------------------------------------------------
# Job 1 — retention. The runnable form of the ip-purge binary's SQL.
# -----------------------------------------------------------------------------
# $1 = table, $2 = days retained. A data-modifying CTE returns the deleted count
# in one round trip; ON_ERROR_STOP=1 makes a SQL or connection failure a
# non-zero exit instead of a silent zero-row success.
retention_delete() {
    psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -t -A -c \
        "WITH d AS (DELETE FROM $1 WHERE day <= (CURRENT_DATE - $2) RETURNING 1) SELECT count(*) FROM d;"
}

run_retention() {
    log "job retention: start"
    if [ -z "${DATABASE_URL:-}" ]; then
        log "job retention: FAILED — DATABASE_URL is not set (refusing to report a sweep that did not run)"
        return 1
    fi

    seen=$(retention_delete key_ip_seen 7) || {
        log "job retention: FAILED — the key_ip_seen delete did not run (psql error above)"
        return 1
    }
    daily=$(retention_delete key_ip_daily 90) || {
        log "job retention: FAILED — the key_ip_daily delete did not run (psql error above)"
        return 1
    }

    log "job retention: OK — key_ip_seen deleted=${seen} (retain 7d), key_ip_daily deleted=${daily} (retain 90d)"
    return 0
}

# -----------------------------------------------------------------------------
# Job 2 — ledger reconciliation. Exit code preserved; never swallowed.
# -----------------------------------------------------------------------------
run_reconcile() {
    log "job reconcile: start — tools/reconcile/reconcile.sh (1=drift 2=no DATABASE_URL 3=no psql 4=psql failed)"
    if [ ! -f "$RECONCILE_SH" ]; then
        log "job reconcile: FAILED — \$RECONCILE_SH is not mounted at $RECONCILE_SH"
        return 1
    fi

    # reconcile.sh reads plain \$DATABASE_URL — that is its documented contract
    # (tools/reconcile/README.md) and this service does not get to rewrite it.
    # Hand the child the reconciliation DSN under that name so the two jobs can
    # be armed independently. Getting this wrong is silent: reconcile.sh would
    # quietly read the OTHER dsn and still exit 0, which is the same
    # reports-success-while-nothing-ran bug this whole service exists to kill.
    # Errexit is deliberately NEVER enabled in this script (see the top): the
    # exit code is captured explicitly instead, so a drifting ledger is a
    # reported failure and not a dead scheduler.
    DATABASE_URL="$RECONCILE_DATABASE_URL" sh "$RECONCILE_SH"
    rc=$?

    if [ "$rc" -eq 0 ]; then
        log "job reconcile: OK — 0 drifting accounts (exit 0)"
        return 0
    fi

    log "job reconcile: FAILED — exit $rc. This is NOT a success: reconcile.sh is the Gate 2 money-correctness gate and its exit code is reported verbatim, never masked."
    return 1
}

# -----------------------------------------------------------------------------
# Job 3 — hold-sweep. Cannot run here. Says so, every night.
# -----------------------------------------------------------------------------
run_hold_sweep_not_wired() {
    log "job hold-sweep: NOT RUN — server/src/bin/hold-sweep.rs is a Rust binary and this image contains no Rust build or binary. A stranded reservation hold is invisible money, so this gap is announced, never silently skipped."
    log "job hold-sweep: run it on the host: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin hold-sweep"
    return 0
}

# $1 = label, $2 = whether to announce the not-wired hold-sweep.
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
            log "FATAL — could not compute the next run time for SCHEDULE_HOUR_UTC=${SCHEDULE_HOUR_UTC}"
            exit 2
        }
        now=$(date -u +%s)
        wait_for=$((next - now))
        log "next run in ${wait_for}s at $(date -u -d "@$next" '+%Y-%m-%dT%H:%M:%SZ')"
        sleep "$wait_for"
        run_wired_jobs || log "nightly run finished with failures — see the job lines above; retrying at the next scheduled time"
    done
}

# -----------------------------------------------------------------------------
# Entry
# -----------------------------------------------------------------------------
case "${SCHEDULE_HOUR_UTC}" in
    '' | *[!0-9]*)
        log "FATAL — SCHEDULE_HOUR_UTC must be an hour 0-23, got '${SCHEDULE_HOUR_UTC}'"
        exit 2
        ;;
esac
if [ "$SCHEDULE_HOUR_UTC" -gt 23 ]; then
    log "FATAL — SCHEDULE_HOUR_UTC must be an hour 0-23, got '${SCHEDULE_HOUR_UTC}'"
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
