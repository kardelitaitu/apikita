#!/bin/sh
# apikita alert checks - the checks docs/observability.md's alert table can run
# with psql alone, plus an honest statement of the ones it cannot.
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
# Not run here, and why (printed on every run):
#   - webhook rejection: server/src/routes/webhooks.rs emits it as a structured
#     LOG line (warn! at :116 for a bad signature, error! at :188 for an amount
#     mismatch). There is no counter column anywhere, so "any topup.rejected" is
#     not answerable from SQL. Needs a metrics backend or a log counter.
#   - API down / relay 5xx / relay down: external probes, not database checks.
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
#   2  CONFIG   - DATABASE_URL is set but is not a postgres:// / postgresql:// DSN
#   3  MISSING  - no usable psql: not on PATH, and no docker compose fallback
#   4  FAILED   - psql or reconcile ran and failed (connection, permissions, SQL)
#   5  UNDELIVERED - an alert fired and could NOT be delivered. An alert nobody
#                 receives is not a pass; this code exists so it cannot be one.
#   6  UNKNOWN  - a check could not run (reconcile could not run, its HOLD SWEEP
#                 line was unparseable, or psql returned something that is not a
#                 count). Unknown is not clean.
#
# 5 outranks 1, and 2/3/4/6 outrank 1: "something is wrong" is only good news if
# you heard about it, and a check that did not run is not a check that passed.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
RECONCILE="$REPO_ROOT/tools/reconcile/reconcile.sh"

DEFAULT_DATABASE_URL="${ALERT_DEFAULT_DATABASE_URL:-postgres://postgres:dev@localhost:5432/apikita}"
COMPOSE_FILE="${COMPOSE_FILE:-$REPO_ROOT/docker-compose.yml}"
CONTAINER_SERVICE="${CONTAINER_SERVICE:-postgres}"

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

# --- DSN ---------------------------------------------------------------------
# Same policy as tools/backup/backup.sh, for the same reason: this runs from cron
# with no environment, and the alternative to the documented local dev default is
# no check at all. Pointing it at production is an explicit DATABASE_URL.
DSN="${DATABASE_URL:-}"
case "$DSN" in
    *[![:space:]]*) ;;
    *)
        echo "check-alerts: DATABASE_URL is not set (unset, empty or whitespace only)" >&2
        echo "check-alerts: using the documented local development DSN instead:" >&2
        echo "check-alerts:   $DEFAULT_DATABASE_URL" >&2
        echo "check-alerts:   (docs/local-development.md) - LOCAL dev stack only, never production" >&2
        DSN="$DEFAULT_DATABASE_URL"
        ;;
esac
case "$DSN" in
    postgres://*|postgresql://*) ;;
    *)
        echo "check-alerts: DATABASE_URL is not a postgres:// or postgresql:// DSN: '$DSN'" >&2
        exit 2
        ;;
esac
export DATABASE_URL="$DSN"
DSN_DB=$(printf '%s' "$DSN" | sed 's|?.*$||' | sed 's|.*/||')
[ -n "$DSN_DB" ] || DSN_DB="apikita"

TMP="${TMPDIR:-/tmp}"

# --- psql, host or container -------------------------------------------------
# This host may have no psql (tools/reconcile/README.md and tools/backup/README.md
# document the same), so the container is the fallback. The chosen mode is used for
# this script's own query - and, when the host has no psql, a shim goes on PATH so
# the reconcile.sh this script DELEGATES to can run as well. Without that, checks 1
# and 3 would report MISSING/UNKNOWN against a database this script can plainly
# reach: a false alarm dressed as a broken check.
HOST_PSQL=0
command -v psql >/dev/null 2>&1 && HOST_PSQL=1
HAS_DOCKER=0
if command -v docker >/dev/null 2>&1 && [ -f "$COMPOSE_FILE" ]; then
    HAS_DOCKER=1
fi

if [ "$HOST_PSQL" -eq 1 ]; then
    PSQL_MODE="host"
elif [ "$HAS_DOCKER" -eq 1 ]; then
    PSQL_MODE="container"
else
    echo "check-alerts: psql is not on PATH and no docker compose fallback is usable" >&2
    echo "check-alerts: looked for: psql on PATH, docker on PATH + $COMPOSE_FILE" >&2
    exit 3
fi

# The shim forwards to the same psql this script uses, so reconcile.sh's exit codes
# and error text are psql's own. It streams a host -f file to psql's stdin, because
# the container cannot see host paths (reconcile.sh passes -f reconcile.sql).
SHIM_DIR=""
if [ "$HOST_PSQL" -eq 0 ]; then
    SHIM_DIR="$TMP/check-alerts-shim.$$"
    mkdir -p "$SHIM_DIR" || { echo "check-alerts: cannot create $SHIM_DIR" >&2; exit 3; }
    cat > "$SHIM_DIR/psql" <<'SHIM'
#!/bin/sh
# Generated by tools/alert/check-alerts.sh. Forwards to the real psql inside the
# compose service so tools/reconcile/reconcile.sh can run on a host without psql.
set -u
args=""
stdin_file=""
skip_next=0
for a in "$@"; do
    if [ "$skip_next" -eq 1 ]; then
        stdin_file="$a"; skip_next=0; continue
    fi
    case "$a" in
        postgres://*|postgresql://*) continue ;;
        -f) skip_next=1; continue ;;
    esac
    args="$args '$a'"
done
if [ -n "$stdin_file" ]; then
    eval "exec docker compose -f \"$SHIM_COMPOSE\" exec -T \"$SHIM_SERVICE\" psql -U postgres -d \"$SHIM_DB\" $args -f -" < "$stdin_file"
else
    eval "exec docker compose -f \"$SHIM_COMPOSE\" exec -T \"$SHIM_SERVICE\" psql -U postgres -d \"$SHIM_DB\" $args"
fi
SHIM
    chmod +x "$SHIM_DIR/psql" || { echo "check-alerts: cannot chmod $SHIM_DIR/psql" >&2; exit 3; }
    PATH="$SHIM_DIR:$PATH"
    export PATH
    SHIM_COMPOSE="$COMPOSE_FILE"; SHIM_SERVICE="$CONTAINER_SERVICE"; SHIM_DB="$DSN_DB"
    export SHIM_COMPOSE SHIM_SERVICE SHIM_DB
    echo "check-alerts: no host psql; reconcile.sh runs through a shim onto docker compose exec -T $CONTAINER_SERVICE psql"
fi

PSQL_ERR="$TMP/check-alerts.$$.psql.err"
REC_OUT="$TMP/check-alerts.$$.reconcile.out"
REC_ERR="$TMP/check-alerts.$$.reconcile.err"
cleanup_tmp() {
    rm -f "$PSQL_ERR" "$REC_OUT" "$REC_ERR"
    [ -n "$SHIM_DIR" ] && rm -rf "$SHIM_DIR"
    return 0
}
trap cleanup_tmp EXIT HUP INT TERM

# ON_ERROR_STOP=1 matters: without it psql prints a SQL error and still exits 0.
psql_scalar() {
    if [ "$PSQL_MODE" = "host" ]; then
        psql "$DSN" -v ON_ERROR_STOP=1 -t -A -c "$1" 2>"$PSQL_ERR"
    else
        docker compose -f "$COMPOSE_FILE" exec -T "$CONTAINER_SERVICE" \
            psql "$DSN" -v ON_ERROR_STOP=1 -t -A -c "$1" 2>"$PSQL_ERR"
    fi
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

DSN_REDACTED=$(printf '%s' "$DSN" | sed 's|://[^@/]*@|://***@|')
echo "check-alerts: mode=$PSQL_MODE dsn=$DSN_REDACTED (password never printed)"

# --- Checks 1 + 3: ledger drift and stranded holds, via reconcile.sh ----------
# The exit code is reconcile's own, deliberately: this script does not re-derive
# drift. See tools/reconcile/README.md for its 0-5 contract.
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
        echo "check-alerts: UNKNOWN - reconcile could not run: DATABASE_URL is not set for it" >&2
        fail_with 6
        ;;
    3)
        echo "check-alerts: MISSING - reconcile could not run: psql is not on PATH for it" >&2
        fail_with 3
        ;;
    4)
        echo "check-alerts: FAILED - reconcile ran and psql failed (connection, permissions or SQL error)" >&2
        fail_with 4
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
NEG=$(psql_scalar "SELECT COUNT(*) FROM wallets WHERE balance_idr < 0;")
PSQL_STATUS=$?
if [ "$PSQL_STATUS" -ne 0 ]; then
    echo "check-alerts: FAILED - the balance-negative check could not run (psql exit $PSQL_STATUS):" >&2
    if [ -s "$PSQL_ERR" ]; then
        sed 's/^/check-alerts:   /' "$PSQL_ERR" >&2
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
cat >&2 <<'NOTCOVERED'
check-alerts: NOT CHECKED HERE (needs a metrics backend or an external probe):
check-alerts:   webhook_rejection  - server/src/routes/webhooks.rs emits it as a structured LOG line
check-alerts:                        (warn! at :116 for a bad signature, error! at :188 for an amount
check-alerts:                        mismatch). No counter column exists, so "any topup.rejected" is
check-alerts:                        not answerable from SQL.
check-alerts:   api_down           - external GET /health probe, 2-minute window
check-alerts:   relay_5xx          - nginx access-log status counts
check-alerts:   relay_down         - external probe
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
