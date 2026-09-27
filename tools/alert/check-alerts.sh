#!/bin/sh
# apikita alert checks - the checks docs/observability.md's alert table can run
# with the sqlite3 CLI alone, plus an honest statement of the ones it cannot.
#
# The database is embedded SQLite: a FILE the API opens, named by DATABASE_URL.
# There is no database server, no host and no port, so these checks read the file
# directly with the sqlite3 CLI. See tools/reconcile/README.md.
#
# docs/observability.md:97-107 lists 9 alerts. Several need a metrics backend
# (error rate, relay 5xx, DB disk, all-providers-unhealthy) and one needs a log
# counter (webhook rejection). This script runs the ones that do not, and PRINTS
# the ones it does not - a check that is silently absent is the failure mode this
# whole directory exists to prevent.
#
# Checks run here:
#   1. LEDGER DRIFT      - delegates to tools/reconcile/reconcile.sh and takes ITS
#                          exit code. Deliberately not a second drift query: two
#                          definitions of "drift" is how a detector stops being
#                          trusted (tools/reconcile/README.md).
#   2. BALANCE NEGATIVE  - SELECT count(*) FROM wallets WHERE balance_idr < 0.
#                          The schema has CHECK (balance_idr >= 0), so a non-zero
#                          count is a bug, not a business event.
#   3. STRANDED HOLD     - also from reconcile.sh: it copies the predicate of
#                          server/src/bin/hold-sweep.rs verbatim and reports the
#                          count on every run. The over-bound count is parsed from
#                          its "HOLD SWEEP" line; if that line is missing the
#                          script FAILS rather than assuming zero.
#
# Not run here, and why (printed on every run). Three of these are no longer
# merely unchecked - tools/alert/probe.sh covers them WITHOUT a database, and the
# NOT CHECKED block says so per line rather than dropping them, so the split
# between "this script cannot" and "nothing can" stays legible:
#   - webhook rejection: server/src/routes/webhooks.rs emits it as a structured
#     LOG line (warn! at :116 for a bad signature, error! at :188 for an amount
#     mismatch). There is no counter column anywhere, so "any topup.rejected" is
#     not answerable from SQL. probe.sh counts it in the server's captured stdout.
#   - API down / relay down: external probes, not database checks. probe.sh does
#     both. (Relay down is still unchecked HERE, and always will be: it is not a
#     database fact.)
#   - relay 5xx: nginx status counts. Needs a metrics backend or access logs.
#   - all providers unhealthy: upstream circuit-breaker state, in-process.
#   - DB disk >80%: volume usage, not a SQL-visible fact.
#   - error rate >5%: HTTP counters over a 5-minute window.
#
# Exit codes. 0-4 keep tools/reconcile/reconcile.sh's meanings so an operator
# learns one table; 5 and 6 are additive:
#   0  CLEAN    - every runnable check ran and no threshold was breached
#   1  FIRED    - at least one alert fired and was delivered (or throttled). The
#                 tool worked; something real is wrong. Mirrors tools/backup's 1
#                 ("ran, but the news is not all good").
#   2  CONFIG   - DATABASE_URL is set but is not a sqlite:// URL, or names an
#                 in-memory database (which this script cannot read)
#   3  MISSING  - sqlite3 is not installed / not on PATH
#   4  FAILED   - sqlite3 or reconcile ran and failed (unreadable file, SQL error)
#   5  UNDELIVERED - an alert fired and could NOT be delivered. An alert nobody
#                 receives is not a pass; this code exists so it cannot be one.
#   6  UNKNOWN  - a check could not run (reconcile could not run, its HOLD SWEEP
#                 line was unparseable, the database file is missing, or sqlite3
#                 returned something that is not a count). Unknown is not clean.
#
# 5 outranks 1, and 2/3/4/6 outrank 1: "something is wrong" is only good news if
# you heard about it, and a check that did not run is not a check that passed.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
RECONCILE="$REPO_ROOT/tools/reconcile/reconcile.sh"

# The fallback is this repo's local development database, spelled as an absolute
# path on purpose. "sqlite://data/server.db" (docs/local-development.md) is
# relative to the SERVER's working directory; a cron job resolving it against its
# own cwd would open a nonexistent file and report exit 6 forever, or worse,
# create an empty one. A default that depends on the caller's cwd is a default
# that silently checks nothing.
DEFAULT_DATABASE_URL="${ALERT_DEFAULT_DATABASE_URL:-sqlite://$REPO_ROOT/server/data/server.db}"

for arg in "$@"; do
    echo "check-alerts: unknown argument: $arg" >&2
    echo "check-alerts: usage: check-alerts.sh   (configuration is by environment only)" >&2
    exit 2
done

if [ ! -f "$RECONCILE" ]; then
    echo "check-alerts: reconcile runner not found: $RECONCILE" >&2
    echo "check-alerts: the drift and stranded-hold checks delegate to it deliberately" >&2
    exit 6
fi

# --- sqlite3 -----------------------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "check-alerts: sqlite3 is not installed or not on PATH" >&2
    echo "check-alerts: install the SQLite command-line shell (sqlite3) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
# Same policy as tools/backup/backup.sh, for the same reason: this runs from cron
# with no environment, and the alternative to the documented local dev default is
# no check at all. Pointing it at production is an explicit DATABASE_URL.
DSN="${DATABASE_URL:-}"
case "$DSN" in
    *[![:space:]]*) ;;
    *)
        echo "check-alerts: DATABASE_URL is not set (unset, empty or whitespace only)" >&2
        echo "check-alerts: using the local development database instead:" >&2
        echo "check-alerts:   $DEFAULT_DATABASE_URL" >&2
        echo "check-alerts:   (docs/local-development.md) - LOCAL dev database only, never production" >&2
        DSN="$DEFAULT_DATABASE_URL"
        ;;
esac

# A case, not a blind prefix strip: a leftover postgres:// URL must be refused
# loudly rather than rewritten into a relative path that happens to be a
# plausible filename (tools/reconcile/reconcile.sh makes the same argument).
case "$DSN" in
    sqlite://*) DB_PATH=${DSN#sqlite://} ;;
    sqlite:*)   DB_PATH=${DSN#sqlite:} ;;
    *)
        echo "check-alerts: DATABASE_URL is not a sqlite:// URL: '$DSN'" >&2
        echo "check-alerts: expected e.g. sqlite://data/server.db" >&2
        exit 2
        ;;
esac
DB_PATH=${DB_PATH%%\?*}
if [ -z "$DB_PATH" ] || [ "$DB_PATH" = ":memory:" ]; then
    echo "check-alerts: DATABASE_URL does not name a file: $DSN" >&2
    echo "check-alerts: an in-memory database cannot be checked from outside the process" >&2
    exit 2
fi
# A relative path in DATABASE_URL is relative to the SERVER's working directory
# (docs/local-development.md: "relative to server/"), not to this script's cwd.
# Resolving it here means the documented "sqlite://data/server.db" is found
# whether this runs from the repo root, from server/, or from cron.
case "$DB_PATH" in
    /*|?:[\\/]*) ;;
    *) DB_PATH="$REPO_ROOT/server/$DB_PATH" ;;
esac
export DATABASE_URL="$DSN"

# The checks read the file the API owns, so a missing file is UNKNOWN (exit 6),
# not an empty database. Reporting "0 rows, CLEAN" against a file that is not
# there is the exact silent pass this directory exists to prevent.
if [ ! -f "$DB_PATH" ]; then
    echo "check-alerts: no such database file: $DB_PATH" >&2
    echo "check-alerts: create it with 'DATABASE_URL=$DSN cargo run --bin migrate' (from server/)" >&2
    exit 6
fi

TMP="${TMPDIR:-/tmp}"
SQL_ERR="$TMP/check-alerts.$$.sqlite.err"
REC_OUT="$TMP/check-alerts.$$.reconcile.out"
REC_ERR="$TMP/check-alerts.$$.reconcile.err"
cleanup_tmp() { rm -f "$SQL_ERR" "$REC_OUT" "$REC_ERR"; return 0; }
trap cleanup_tmp EXIT HUP INT TERM

# -readonly: an alert check must never write. -bail: stop at the first error and
# exit non-zero for it instead of printing a partial answer. Default list output
# is already pipe-separated with no header, matching the old psql -t -A -F'|'.
sqlite_scalar() {
    sqlite3 -readonly -bail -noheader -separator '|' "$DB_PATH" "$1" 2>"$SQL_ERR"
}

FIRE_FAILED=0
FIRED=0
FAIL_CODE=0
fail_with() { [ "$FAIL_CODE" -eq 0 ] && FAIL_CODE="$1"; return 0; }

fire() {
    # fire <alert-id> <observed value>
    id="$1"; observed="$2"
    echo "check-alerts: FIRING $id - observed: $observed"
    sh "$SCRIPT_DIR/alert.sh" --alert "$id" --observed "$observed"
    rc=$?
    case "$rc" in
        0) FIRED=$((FIRED + 1)); echo "check-alerts:   -> delivered" ;;
        1) FIRED=$((FIRED + 1)); echo "check-alerts:   -> THROTTLED (inside the cooldown); one page per incident, not per run" ;;
        2) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "check-alerts:   -> NOT DELIVERED: no channel configured" >&2 ;;
        3) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "check-alerts:   -> NOT DELIVERED: delivery failed" >&2 ;;
        *) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "check-alerts:   -> NOT DELIVERED: alert.sh usage error (exit $rc)" >&2 ;;
    esac
}

echo "check-alerts: mode=sqlite3 db=$DB_PATH (the API's database file; no server, no DSN)"

# --- Checks 1 + 3: ledger drift and stranded holds, via reconcile.sh ----------
# The exit code is reconcile's own, deliberately: this script does not re-derive
# drift. See tools/reconcile/README.md for its 0-6 contract.
sh "$RECONCILE" >"$REC_OUT" 2>"$REC_ERR"
REC=$?
sed 's/^/check-alerts: | /' "$REC_OUT"
if [ -s "$REC_ERR" ]; then
    sed 's/^/check-alerts: ! /' "$REC_ERR" >&2
fi

# The stranded-hold count is reported by reconcile on EVERY run, pass or fail,
# because it is structurally invisible to the drift query. Parse it; a missing
# line is a FAILURE (unknown), never an assumed zero.
HOLDS_OVER=$(sed -n 's/.*; \([0-9][0-9]*\) older than .*/\1/p' "$REC_OUT" | head -1)
HOLDS_ALL=$(sed -n 's/^reconcile: HOLD SWEEP - \([0-9][0-9]*\) stranded.*/\1/p' "$REC_OUT" | head -1)

case "$REC" in
    0)
        echo "check-alerts: OK  ledger drift: 0 accounts"
        ;;
    1)
        DRIFT_N=$(sed -n 's/^reconcile: DRIFT DETECTED - \([0-9][0-9]*\) account.*/\1/p' "$REC_ERR" | head -1)
        [ -n "$DRIFT_N" ] || DRIFT_N="unknown"
        echo "check-alerts: ALERT ledger drift: $DRIFT_N account(s) where wallets.balance_idr <> SUM(ledger.delta_idr)"
        fire "ledger_drift" "$DRIFT_N drifting account(s); the reconcile rows are printed above"
        ;;
    5)
        echo "check-alerts: OK  ledger drift: 0 accounts"
        ;;
    2)
        echo "check-alerts: UNKNOWN - reconcile could not run: DATABASE_URL is not a usable SQLite URL for it" >&2
        fail_with 6
        ;;
    3)
        echo "check-alerts: MISSING - reconcile could not run: sqlite3 is not on PATH for it" >&2
        fail_with 3
        ;;
    4)
        echo "check-alerts: FAILED - reconcile ran and sqlite3 failed (unreadable file or SQL error)" >&2
        fail_with 4
        ;;
    6)
        echo "check-alerts: UNKNOWN - reconcile could not run: it found no database file at $DB_PATH" >&2
        fail_with 6
        ;;
    *)
        echo "check-alerts: UNKNOWN - reconcile returned an unexpected exit code: $REC" >&2
        fail_with 6
        ;;
esac

if [ -z "$HOLDS_ALL" ] || [ -z "$HOLDS_OVER" ]; then
    echo "check-alerts: UNKNOWN - reconcile printed no parseable HOLD SWEEP line, so the" >&2
    echo "check-alerts:   stranded-hold check did NOT run. Assuming zero would be the silent" >&2
    echo "check-alerts:   pass this whole directory exists to prevent. See tools/reconcile/README.md." >&2
    fail_with 6
else
    echo "check-alerts: OK  stranded holds: $HOLDS_ALL unpaired, $HOLDS_OVER over the bound"
    if [ "$HOLDS_OVER" -gt 0 ]; then
        echo "check-alerts: ALERT stranded hold: $HOLDS_OVER hold(s) unpaired past the bound"
        fire "stranded_hold" "$HOLDS_OVER reservation hold(s) unpaired past the bound (of $HOLDS_ALL unpaired)"
    fi
fi

# --- Check 2: balance negative ----------------------------------------------
# -bail makes a SQL error a non-zero exit, so a failed query can never be read as
# a zero count. (psql needed ON_ERROR_STOP=1 for the same reason.)
NEG=$(sqlite_scalar "SELECT COUNT(*) FROM wallets WHERE balance_idr < 0;")
SQL_STATUS=$?
if [ "$SQL_STATUS" -ne 0 ]; then
    echo "check-alerts: FAILED - the balance-negative check could not run (sqlite3 exit $SQL_STATUS):" >&2
    if [ -s "$SQL_ERR" ]; then
        sed 's/^/check-alerts:   /' "$SQL_ERR" >&2
    fi
    fail_with 4
else
    case "$NEG" in
        ''|*[!0-9]*)
            echo "check-alerts: UNKNOWN - the balance-negative check returned '$NEG', not a count" >&2
            fail_with 6
            ;;
        0)
            echo "check-alerts: OK  balance negative: 0 rows"
            ;;
        *)
            echo "check-alerts: ALERT balance negative: $NEG wallet row(s) with balance_idr < 0"
            fire "balance_negative" "$NEG wallet row(s) with balance_idr < 0"
            ;;
    esac
fi

# --- the alerts this cannot deliver, stated on EVERY run ---------------------
# Three of the eight are covered by tools/alert/probe.sh, which needs no database
# at all. They stay listed, marked, so nobody reads "not checked here" as "not
# checked anywhere" - and so nobody claims this script covers them.
cat >&2 <<'NOTCOVERED'
check-alerts: NOT CHECKED HERE (needs a metrics backend, an external probe or a log counter):
check-alerts:   webhook_rejection  - COVERED BY tools/alert/probe.sh (counts the log line). No SQL
check-alerts:                        can answer it: server/src/routes/webhooks.rs emits it as a
check-alerts:                        structured LOG line (warn! at :116, error! at :188) and no
check-alerts:                        counter column exists.
check-alerts:   api_down           - COVERED BY tools/alert/probe.sh (external GET /health probe).
check-alerts:   relay_down         - COVERED BY tools/alert/probe.sh (external probe of the relay).
check-alerts:   relay_5xx          - nginx access-log status counts; access logs are disabled for
check-alerts:                        privacy (docs/edge-relay.md). Needs a metrics backend.
check-alerts:   all_providers_unhealthy - upstream circuit-breaker state, in-process
check-alerts:   db_disk            - volume usage
check-alerts:   error_rate         - HTTP counters over a 5-minute window
check-alerts: Full table: tools/alert/README.md. Every definition: tools/alert/alert.sh --list
NOTCOVERED

# --- verdict -----------------------------------------------------------------
# Precedence: "fired and nobody was told" (5) is the worst outcome; then a check
# that could not run (2/3/4/6); then fired-and-delivered (1); then clean (0).
if [ "$FIRE_FAILED" -gt 0 ]; then
    echo "check-alerts: UNDELIVERED - $FIRE_FAILED alert(s) fired and were NOT delivered" >&2
    exit 5
fi
if [ "$FAIL_CODE" -ne 0 ]; then
    echo "check-alerts: not clean - a check could not run (exit $FAIL_CODE); unknown is not clean" >&2
    exit "$FAIL_CODE"
fi
if [ "$FIRED" -gt 0 ]; then
    echo "check-alerts: FIRED - $FIRED alert(s) fired and were delivered (or throttled)"
    exit 1
fi
echo "check-alerts: CLEAN - every runnable check passed and no threshold was breached"
exit 0
