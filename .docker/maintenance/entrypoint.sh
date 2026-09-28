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
#   hold-sweep  RUNS HERE, FOR REAL - report-only, and that is the point. A
#               stranded reservation hold is INVISIBLE MONEY: the ledger still
#               balances and reconcile.sh is STRUCTURALLY BLIND to it (its own
#               output says so). The predicate here is deliberately the SAME as
#               server/src/bin/hold-sweep.rs and db::unpaired_hold_rows - the
#               binary warns that a different one "would make the binary and the
#               library disagree about what 'stranded' means, which is how a
#               detector stops being trusted". It uses that binary's default bound
#               (900s = 15 min, ~7.5x the request timeout).
#
#               It NEVER moves money. The binary's --release flag is the
#               deliberate operator action, and the binary explains why it is not
#               the default: "silently correcting a stranded hold is the same
#               invisible-money anti-pattern this sweep exists to catch." So this
#               job counts, names the accounts and refs, and exits non-zero.
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

# The alert checks, mounted read-only by docker-compose.yml. This script is the
# caller; tools/alert/ owns what the checks are and what their exit codes mean.
# Where docker-compose.yml mounts tools/alert, read-only. The checks are the repo's,
# not this image's: check-alerts.sh delegates to tools/reconcile/reconcile.sh, so
# mounting them keeps ONE definition of drift rather than a second copy here.
ALERT_DIR="${ALERT_DIR:-/usr/local/share/alert}"
# The relay service name in docker-compose.yml. probe.sh defaults to 127.0.0.1,
# which INSIDE this container is its own loopback - nothing is listening there, so
# an unconfigured run would report the relay down every night. The service name is
# what is actually resolvable.
RELAY_HOST="${RELAY_HOST:-nginx}"
ALERT_CHECK="$ALERT_DIR/check-alerts.sh"
ALERT_PROBE="$ALERT_DIR/probe.sh"
ALERT_PROBE_OUT="$TMP/maintenance-probes.$.out"
ALERT_OUT="$TMP/maintenance-alerts.$.out"
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
    log "WIRED     hold-sweep - REPORT-ONLY, SQL inline in this entrypoint, using the SAME predicate as server/src/bin/hold-sweep.rs (which warns that a different predicate would make the binary and the library disagree about what 'stranded' means). Bound ${HOLD_SWEEP_BOUND_SECONDS}s. It counts, names and exits non-zero; it NEVER moves money, because silently crediting a hold is the same invisible-money anti-pattern the sweep exists to catch. --release stays a deliberate host action."
    if [ -x "$ALERT_CHECK" ]; then
        if [ -n "${WEBHOOK_URL:-}" ] || [ -n "${TELEGRAM_BOT_TOKEN:-}" ] || [ -n "${ALERT_SINK_FILE:-}" ] || [ -n "${ALERT_SINK_STDOUT:-}" ]; then
            log "WIRED     alerts     - tools/alert/check-alerts.sh, exit code preserved (1=fired 2=config 3=no sqlite3 4=failed 5=undelivered 6=unknown). A channel IS configured, so a breach is delivered."
        else
            log "WIRED     alerts     - tools/alert/check-alerts.sh RUNS nightly, but NO CHANNEL IS CONFIGURED, so a breach is reported as UNMONITORED rather than delivered. That is a deployment decision, not a defect: choose TELEGRAM_BOT_TOKEN+TELEGRAM_CHAT_ID, WEBHOOK_URL, ALERT_SINK_FILE or ALERT_SINK_STDOUT. A clean night still exits 0; a BREACH WITH NO CHANNEL DOES NOT."
        fi
        log "WIRED     alert-probes - tools/alert/probe.sh, exit code preserved (1=fired 2=config 3=no curl 4=failed 5=undelivered 6=unknown). The relay is probed by SERVICE NAME ($RELAY_HOST:8000), because 127.0.0.1 is this container's own loopback. api_down additionally needs PROBE_API_URL, which has no safe default and is skipped with its reason printed when unset."
    else
        log "NOT WIRED alerts     - $ALERT_CHECK is not present, so tools/alert is not mounted into this container. The checks exist and nothing runs them; see docker-compose.yml."
    fi
    log "NOT WIRED two report-only gaps, not silent ones. Run them on the host on the same cadence: DATABASE_URL=... cargo run --manifest-path server/Cargo.toml --bin ip-purge (or --bin usage-purge)"
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
# Job 3 - hold-sweep. REPORT-ONLY, by design.
# -----------------------------------------------------------------------------
# THE PREDICATE IS THE BINARY'S, deliberately. server/src/bin/hold-sweep.rs:157-160
# states the constraint: "The predicate is deliberately the one db::unpaired_hold_rows
# documents... A different predicate here would make the binary and the library
# DISAGREE about what stranded means, which is how a detector stops being trusted."
# So this is the same SQL, and the smoke test asserts agreement with reconcile.sh
# rather than trusting two statements to match by eye.
#
# REPORT-ONLY, matching the binary's default. The binary describes why:
# "silently correcting a stranded hold is the same invisible-money anti-pattern this
# sweep exists to catch." Crediting a hold writes a positive ledger row and is an
# OPERATOR action (--release); it does not belong on a nightly schedule. So this job
# counts, names and exits non-zero, and the money-moving half stays in the tested
# Rust binary.
#
# WHY THE BOUND IS 900s. Same as the binary's DEFAULT_MAX_HOLD_AGE_SECONDS: a hold
# lives as long as one request, so anything under ~7.5x the request timeout is a
# release that is merely late, and alerting on it would be noise.
HOLD_SWEEP_BOUND_SECONDS="${HOLD_SWEEP_BOUND_SECONDS:-900}"

# The stranded-hold predicate, in ONE place so the count and the detail listing
# cannot drift apart. $1 = the bound in seconds.
hold_sweep_query() {
    cat <<SQL
SELECT l.account_id, l.ref, CAST(SUM(l.delta_idr) AS INTEGER) AS amount_idr
FROM ledger l
WHERE l.ref LIKE 'reserve_%' AND l.delta_idr < 0
GROUP BY l.account_id, l.ref
HAVING NOT EXISTS (
    SELECT 1 FROM ledger m
    WHERE m.account_id = l.account_id AND m.ref = l.ref AND m.delta_idr > 0
)
  AND CAST(strftime('%s', 'now') - strftime('%s', MIN(l.created_at)) AS INTEGER) > $1
SQL
}

run_hold_sweep() {
    log "job hold-sweep: start (report-only; a stranded hold is invisible money)"
    if [ -z "${DATABASE_URL:-}" ]; then
        log "job hold-sweep: FAILED - DATABASE_URL is not set (refusing to report a sweep that did not run)"
        return 1
    fi
    if ! have_sqlite3; then
        log "job hold-sweep: FAILED - sqlite3 is not installed in this image"
        return 1
    fi
    if ! DB_FILE=$(db_file_from_url "$DATABASE_URL"); then
        log "job hold-sweep: FAILED - DATABASE_URL is not a sqlite:// URL: $DATABASE_URL"
        return 1
    fi
    if [ ! -f "$DB_FILE" ]; then
        log "job hold-sweep: FAILED - no such database file: $DB_FILE (nothing was swept)"
        return 1
    fi

    ROWS=$(sqlite3 -bail -noheader -separator '|' "$DB_FILE" "$(hold_sweep_query "$HOLD_SWEEP_BOUND_SECONDS")" 2>"$SQL_ERR") || {
        log "job hold-sweep: FAILED - the stranded-hold query did not run (sqlite3 error above)"
        [ -s "$SQL_ERR" ] && while IFS= read -r l; do log "job hold-sweep:   $l"; done < "$SQL_ERR"
        return 1
    }

    if [ -n "$ROWS" ]; then
        N=$(printf '%s\n' "$ROWS" | grep -c .)
        log "job hold-sweep: ALERT - $N stranded reservation hold(s) older than ${HOLD_SWEEP_BOUND_SECONDS}s in $DB_FILE"
        printf '%s\n' "$ROWS" | while IFS='|' read -r acct ref amt; do
            log "job hold-sweep:   account=$acct ref=$ref amount_idr=$amt"
        done
        log "job hold-sweep: this is INVISIBLE MONEY - the ledger balances and reconcile.sh is structurally blind to it. Correct it deliberately, never silently: cargo run --bin hold-sweep -- --release"
        return 1
    fi

    log "job hold-sweep: OK - 0 stranded holds older than ${HOLD_SWEEP_BOUND_SECONDS}s ($DB_FILE)"
    return 0
}


# -----------------------------------------------------------------------------
# Alert checks
# -----------------------------------------------------------------------------
# Runs the DATABASE-BACKED alert checks (tools/alert/check-alerts.sh) after the
# sweeps, so the four alerts docs/observability.md defines are actually evaluated
# on a schedule rather than only existing.
#
# WHY THIS IS HERE AT ALL. entrypoint.sh:6-8 states the defect this file was built
# to fix - "server/src/bin/{ip-purge,hold-sweep} and tools/reconcile/reconcile.sh
# were written and then nothing ever ran them. A retention promise with no job
# behind it is a sentence in a document, not a guarantee." tools/alert/ had
# exactly that shape: the checks exist, every alert in alerts.tsv is `covered` by
# one, and nothing invoked them. This is the coupling.
#
# THE RETURN CONTRACT, read from tools/alert/README.md:143-164, and it is NOT
# "non-zero means broken":
#
#   0  CLEAN       - every runnable check ran, nothing breached.
#   1  FIRED       - an alert fired AND was delivered. The tool worked; something
#                    real is wrong. This must reach the operator as a non-zero
#                    exit, or a nightly run that found money drift would look
#                    identical to a clean one in the container's exit code.
#   5  UNDELIVERED - an alert fired and could NOT be delivered.
#   2/3/4/6        - CONFIG / MISSING / FAILED / UNKNOWN: a check could not run.
#                    "a check that did not run is not a check that passed", so
#                    these are not passes either.
#
# ALL NON-ZERO CODES ARE PROPAGATED UNCHANGED. The tool's precedence rule (5
# beats 2/3/4/6, which beat 1) is a decision about which failure to report when
# several are true, and it belongs to the tool that can see all of them. Re-
# mapping the codes here would be a second opinion that can drift from the first.
#
# ⚠ THE ONE CASE THAT NEEDS A DECISION RATHER THAN PROPAGATION. With NO channel
# configured, alert.sh exits 2 - "an alert nobody receives is worse than no
# alerting, because it is believed". A stack that has simply not chosen a channel
# yet must not report a CONFIG failure every night, so this function treats
# "no channel" as a known configuration state and says so plainly:
#
#   no channel, nothing breached -> the checks ran clean. The stack is unmonitored
#                                  BY CHOICE, and the log states that in full.
#   no channel, something breached -> STILL NON-ZERO. Something is wrong and
#                                  nobody can be told; reporting success here is
#                                  the silent downgrade this whole directory
#                                  exists to prevent.
#
# read-only against the database: check-alerts.sh opens it --readonly, so this
# cannot move money even if it wanted to.

# Runs the HTTP alert checks (tools/alert/probe.sh) after the database ones.
#
# WHY THE IMAGE NOW CARRIES curl: probe.sh hard-requires it (probe.sh:164) and the
# two checks worth having here are api_down and relay_down - both `covered`, and both
# unreachable before because the binary was absent, not because of configuration.
#
# *** THE URLS ARE THE WHOLE DIFFICULTY. *** probe.sh defaults to 127.0.0.1:8080 and
# 127.0.0.1:8000, which are HOST addresses. Inside this container those are the
# container's own loopback - nothing is listening - so an unconfigured run would poll
# an endpoint that cannot exist, conclude the API is DOWN, and fire a false alarm on
# every scheduled run. That is worse than not checking at all, because it is believed
# and it trains an operator to ignore the alert. So the relay is addressed by its
# COMPOSE SERVICE NAME, which is what makes it resolvable at all.
#
# The API is a separate deployment in this stack (compose holds only nginx and the
# scheduler), so its URL comes from the environment and is NOT defaulted to something
# that cannot work. Unset means the check is skipped with the reason printed - which
# is probe.sh's own SKIPPED semantics, not a silent pass.
run_alert_probes() {
    log "job alert-probes: start (the HTTP checks probe.sh covers)"

    if [ ! -x "$ALERT_PROBE" ]; then
        log "job alert-probes: SKIPPED - $ALERT_PROBE is not present, so tools/alert is not mounted"
        return 0
    fi
    if ! command -v curl >/dev/null 2>&1; then
        log "job alert-probes: SKIPPED - curl is not installed in this image, and probe.sh requires it (probe.sh:164). The database checks are unaffected."
        return 0
    fi

    # The relay answers /healthz from INSIDE the network, so a compose service name
    # resolves where 127.0.0.1 does not. PROBE_RELAY_URL overrides for any other topol-
    # ogy; the default is the service this compose actually defines.
    PROBE_RELAY_URL="${PROBE_RELAY_URL:-http://$RELAY_HOST:8000}"
    export PROBE_RELAY_URL

    # *** SELECT THE CHECKS EXPLICITLY. *** This is not tidiness, it is the safety property
    # of this function. probe.sh does NOT skip api_down when PROBE_API_URL is unset: it falls
    # back to 127.0.0.1:8080 (probe.sh:49), which inside a container is its OWN loopback with
    # nothing listening. MEASURED - an unconfigured run fired a FALSE api_down, and with no
    # channel that became exit 5 UNDELIVERED: the nightly job reporting the API down when the
    # API had never been addressed at all.
    #
    # A false alarm on a schedule is worse than a missing check. It is believed, and it
    # trains an operator to ignore the alert. So a check runs only when the input it needs is
    # present, and everything left out is NAMED with its reason - never silently defaulted
    # and never silently dropped.
    _probe_checks=""
    if [ -n "${PROBE_API_URL:-}" ]; then
        _probe_checks="$_probe_checks api_down"
        log "job alert-probes: api_down enabled - probing the API at $PROBE_API_URL"
    else
        log "job alert-probes:   SKIPPED - api_down (PROBE_API_URL unset; 127.0.0.1 is THIS container, so there is no safe default)"
    fi
    _probe_checks="$_probe_checks relay_down"
    log "job alert-probes: relay_down enabled - probing the relay at $PROBE_RELAY_URL"
    # The log- and cookie-backed checks need inputs this stack does not provide; naming them
    # keeps the omission visible instead of leaving it to be inferred from an absence.
    log "job alert-probes:   SKIPPED - webhook_rejection, refund_refusal (PROBE_LOG_FILE unset)"
    log "job alert-probes:   SKIPPED - error_rate, all_providers_unhealthy, db_disk (PROBE_OPERATOR_COOKIE unset)"

    # A --check per selected id. The unquoted expansion below is deliberate and is why this
    # is a loop rather than one string: each id must become its own word.
    _args=""
    for _c in $_probe_checks; do
        _args="$_args --check $_c"
    done

    # No channel yet is a configuration state, not a failure - the same reasoning as
    # run_alert_checks. probe.sh's own exit codes carry the rest.
    # shellcheck disable=SC2086  # intentional word-splitting: _args is an argument list
    "$ALERT_PROBE" $_args >"$ALERT_PROBE_OUT" 2>&1
    _prc=$?
    while IFS= read -r _line; do log "job alert-probes:   $_line"; done < "$ALERT_PROBE_OUT"

    case "$_prc" in
        0)
            log "job alert-probes: CLEAN - every reachable check answered and nothing was breached"
            return 0
            ;;
        1)
            log "job alert-probes: FIRED - an alert fired and was delivered; see the lines above"
            return 1
            ;;
        5)
            log "job alert-probes: UNDELIVERED - an alert fired and could NOT be delivered. This is NOT a pass"
            return 1
            ;;
        *)
            log "job alert-probes: NOT RUN ($_prc) - a check could not reach a verdict; consult tools/alert/README.md for what $_prc means. Unknown is not clean"
            return 1
            ;;
    esac
}

run_alert_checks() {
    log "job alerts: start (the checks tools/alert/alerts.tsv defines, evaluated nightly)"

    if ! have_sqlite3; then
        log "job alerts: FAILED - sqlite3 is not installed in this image"
        return 1
    fi
    if [ -z "${RECONCILE_DATABASE_URL:-}" ]; then
        log "job alerts: FAILED - RECONCILE_DATABASE_URL is not set (refusing to report checks that did not run)"
        return 1
    fi
    if [ ! -x "$ALERT_CHECK" ]; then
        log "job alerts: FAILED - $ALERT_CHECK is missing or not executable"
        return 1
    fi

    # The alert checks read DATABASE_URL, NOT RECONCILE_DATABASE_URL - they are
    # deliberately separate variables (see the compose comment), and check-alerts.sh
    # resolves a RELATIVE sqlite:// path against its OWN REPO_ROOT, which is wherever
    # tools/alert happens to be mounted - not the server working directory the DSN is
    # written for. So a relative DSN that reconcile.sh resolves correctly would make the
    # alert checks open a nonexistent file and report exit 6 forever. Resolving it here,
    # with the same db_file_from_url the retention sweep uses, is what keeps the two jobs
    # pointed at ONE database. This is the trap entrypoint.sh:116-123 documents for
    # reconcile and it applies verbatim here.
    if ! _alert_db=$(db_file_from_url "$RECONCILE_DATABASE_URL"); then
        log "job alerts: FAILED - RECONCILE_DATABASE_URL is not a sqlite:// URL: $RECONCILE_DATABASE_URL"
        return 1
    fi
    if [ ! -f "$_alert_db" ]; then
        log "job alerts: FAILED - no such database file: $_alert_db (nothing was checked; unknown is not clean)"
        return 1
    fi
    ALERT_DATABASE_URL=sqlite://"$_alert_db"
    export ALERT_DATABASE_URL
    DATABASE_URL="$ALERT_DATABASE_URL"
    export DATABASE_URL

    # A channel is configured iff one of the four the README documents is set.
    if [ -n "${WEBHOOK_URL:-}" ] || [ -n "${TELEGRAM_BOT_TOKEN:-}" ] \
        || [ -n "${ALERT_SINK_FILE:-}" ] || [ -n "${ALERT_SINK_STDOUT:-}" ]; then
        _have_channel=1
    else
        _have_channel=0
    fi

    # ALERT_STATE_DIR must PERSIST or the cooldown resets every run and pages
    # repeatedly for one incident - README.md:121-122. It is pinned in the compose
    # file onto the data volume; this only warns if someone unset it to a tmpfs.
    if [ -z "${ALERT_STATE_DIR:-}" ]; then
        log "job alerts: WARNING - ALERT_STATE_DIR is unset, so the cooldown falls back to /tmp and does not survive a restart; one incident may page once per run"
    fi

    # Run it and take the code DIRECTLY. This was originally `if ! "$ALERT_CHECK"; then
    # _rc=$?; fi`, which is WRONG in a way that matters here: inside `if ! cmd`, `$?`
    # is the result of the NEGATION, so it is always 0 - every failure would have been
    # reported as CLEAN, which is precisely the silent downgrade this function exists
    # to avoid. Verified with a throwaway `(exit 5)` before shipping.
    "$ALERT_CHECK" >"$ALERT_OUT" 2>&1
    _rc=$?
    while IFS= read -r _line; do log "job alerts:   $_line"; done < "$ALERT_OUT"

    case "$_rc" in
        0)
            if [ "$_have_channel" = "0" ]; then
                log "job alerts: CLEAN - every runnable check ran and nothing was breached, but NO CHANNEL IS CONFIGURED, so had one fired nobody would have been told"
            else
                log "job alerts: CLEAN - every runnable check ran and nothing was breached"
            fi
            return 0
            ;;
        1)
            log "job alerts: FIRED - an alert fired and was delivered; see the lines above. The tool worked, something real is wrong"
            return 1
            ;;
        5)
            log "job alerts: UNDELIVERED - an alert fired and could NOT be delivered. This is NOT a pass"
            return 1
            ;;
        *)
            log "job alerts: NOT RUN ($_rc) - a check could not reach a verdict; consult tools/alert/README.md:143-164 for what $_rc means. Unknown is not clean"
            return 1
            ;;
    esac
}

run_wired_jobs() {
    rc=0
    run_retention || rc=1
    run_reconcile || rc=1
    run_hold_sweep || rc=1
    run_alert_checks || rc=1
    run_alert_probes || rc=1
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
    hold-sweep)
        run_hold_sweep
        exit $?
        ;;
    alerts)
        run_alert_checks
        exit $?
        ;;
    alert-probes)
        run_alert_probes
        exit $?
        ;;
    *)
        echo "usage: maintenance-entrypoint.sh [schedule|once|retention|reconcile|hold-sweep|alerts|alert-probes]" >&2
        exit 2
        ;;
esac
