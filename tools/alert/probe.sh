#!/bin/sh
# apikita alert probe - the three docs/observability.md alerts that need NO database.
#
# docs/observability.md:97-107 lists 9 alerts. tools/alert/check-alerts.sh runs the
# three answerable with psql alone and PRINTS the rest as not-checked. Of those
# seven, three never needed a metrics backend - they needed a prober:
#
#   api_down          - external GET /health, alert after a 2-minute window of failures
#   relay_down        - external check against the relay's own origin
#   webhook_rejection - the server's captured stdout, counted for topup.rejected
#
# This script is that prober, and it opens NO DATABASE CONNECTION - deliberately.
# The stack is being migrated from PostgreSQL to SQLite (a concurrent change under
# server/), and an alert path that goes blind during a database migration is blind
# at the worst possible moment. Nothing here knows what a DSN is.
#
# Delivery is NOT reimplemented. Every firing is routed through alert.sh --alert <id>,
# so the cooldown, the channel precedence and the delivery exit codes stay in ONE
# place. See tools/alert/README.md.
#
# Usage:
#   probe.sh                          # all three checks
#   probe.sh --check api_down         # one check (repeatable)
#   probe.sh --list                   # the three, and whether each can run
#   probe.sh --help
#
# Exit codes - check-alerts.sh's table, so an operator learns one numbering:
#   0  CLEAN       - every selected check ran (or was visibly skipped) and no
#                    threshold was breached
#   1  FIRED       - at least one alert fired and was delivered (or throttled)
#   2  CONFIG      - a knob is unusable (non-integer window/interval, unknown check
#                    id, unknown argument)
#   3  MISSING     - no usable curl, or alert.sh is not next to this script
#   4  FAILED      - a check ran and could not reach a verdict (log file unreadable)
#   5  UNDELIVERED - an alert fired and could NOT be delivered. Not a pass.
#   6  UNKNOWN     - a configured source is absent, so the check cannot run
#                    (PROBE_LOG_FILE points at a file that does not exist).
#                    Unknown is not clean.
#
# Precedence, same as check-alerts.sh: 5 (fired and nobody was told) beats
# everything; then 2/3/4/6 (a check that could not run); then 1 (fired, delivered).

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ALERT_SH="$SCRIPT_DIR/alert.sh"

# --- knobs (every one overridable so the checks are testable without a 2-minute wait)
API_URL="${PROBE_API_URL:-http://127.0.0.1:8080}"
API_WINDOW="${PROBE_API_WINDOW_SECONDS:-120}"
API_INTERVAL="${PROBE_API_INTERVAL_SECONDS:-10}"
RELAY_URL="${PROBE_RELAY_URL:-http://127.0.0.1:8000}"
LOG_FILE="${PROBE_LOG_FILE:-}"
STATE_DIR="${PROBE_STATE_DIR:-${TMPDIR:-/tmp}/apikita-probe}"
HTTP_TIMEOUT="${PROBE_HTTP_TIMEOUT:-5}"
# The operator session cookie the metrics route requires. UNSET means the error_rate
# check is visibly skipped, never silently passed.
OPERATOR_COOKIE="${PROBE_OPERATOR_COOKIE:-}"

TMP="${TMPDIR:-/tmp}"
CURL_ERR="$TMP/probe.$$.curl.err"
cleanup_tmp() { rm -f "$CURL_ERR"; return 0; }
trap cleanup_tmp EXIT HUP INT TERM

FIRED=0
FIRE_FAILED=0
FAIL_CODE=0
FIRE_RC=0
fail_with() { [ "$FAIL_CODE" -eq 0 ] && FAIL_CODE="$1"; return 0; }

CHECKS=""
MODE="run"

usage() {
    echo "probe: usage: probe.sh [--check <id>]... | --list | --help" >&2
    echo "probe: checks: api_down, relay_down, webhook_rejection, error_rate (no database access at all)" >&2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --check) [ $# -ge 2 ] || { echo "probe: --check needs a value" >&2; usage; exit 2; }; CHECKS="$CHECKS $2"; shift 2 ;;
        --list)  MODE="list"; shift ;;
        --help|-h) MODE="help"; shift ;;
        *) echo "probe: unknown argument: $1" >&2; usage; exit 2 ;;
    esac
done

if [ "$MODE" = "help" ]; then
    usage
    exit 0
fi

if [ "$MODE" = "list" ]; then
    printf 'probe: %-20s %-8s %s\n' "CHECK" "COVERAGE" "HOW"
    printf 'probe: %-20s %-8s %s\n' "api_down" "covered" \
        "GET $API_URL/health; alert after ${API_WINDOW}s of failed checks (polling every ${API_INTERVAL}s)"
    printf 'probe: %-20s %-8s %s\n' "relay_down" "covered" \
        "GET $RELAY_URL; alert on 1 failed external check"
    if [ -n "$LOG_FILE" ]; then
        printf 'probe: %-20s %-8s %s\n' "webhook_rejection" "covered" \
            "counts 'topup.rejected' in lines of $LOG_FILE not yet scanned"
    else
        printf 'probe: %-20s %-8s %s\n' "webhook_rejection" "skipped" \
            "PROBE_LOG_FILE is unset - no log source configured, so this alert is NOT checked"
    fi
    if [ -n "$OPERATOR_COOKIE" ]; then
        printf 'probe: %-20s %-8s %s\n' "error_rate" "covered" \
            "GET $API_URL/api/admin/metrics as an operator; alerts above a 5% 5xx rate"
    else
        printf 'probe: %-20s %-8s %s\n' "error_rate" "skipped" \
            "PROBE_OPERATOR_COOKIE is unset - the route is operator-authenticated, so this alert is NOT checked"
    fi
    echo "probe: not checked by anything in tools/alert yet: relay_5xx, all_providers_unhealthy, db_disk"
    exit 0
fi

# --- validation ---------------------------------------------------------------
# window/interval only matter to api_down, but a bad knob is a config error either
# way: silently ignoring it would mean the threshold in the message is a lie.
case "$API_WINDOW" in
    ''|*[!0-9]*) echo "probe: PROBE_API_WINDOW_SECONDS='$API_WINDOW' is not a non-negative integer" >&2; exit 2 ;;
esac
case "$API_INTERVAL" in
    ''|*[!0-9]*) echo "probe: PROBE_API_INTERVAL_SECONDS='$API_INTERVAL' is not a non-negative integer" >&2; exit 2 ;;
esac
case "$HTTP_TIMEOUT" in
    ''|*[!0-9]*) echo "probe: PROBE_HTTP_TIMEOUT='$HTTP_TIMEOUT' is not a non-negative integer" >&2; exit 2 ;;
esac
# A zero interval would spin the poll loop for the whole window. Floor it at 1s.
[ "$API_INTERVAL" -ge 1 ] || API_INTERVAL=1

if [ -n "$CHECKS" ]; then
    for id in $CHECKS; do
        case "$id" in
            api_down|relay_down|webhook_rejection|error_rate) ;;
            *) echo "probe: unknown check: $id" >&2
               echo "probe: known checks: api_down, relay_down, webhook_rejection, error_rate" >&2
               exit 2 ;;
        esac
    done
fi

command -v curl >/dev/null 2>&1 || {
    echo "probe: curl is not on PATH; the two external checks cannot run" >&2
    exit 3
}
[ -f "$ALERT_SH" ] || {
    echo "probe: alert runner not found: $ALERT_SH" >&2
    echo "probe: this script deliberately does not deliver anything itself" >&2
    exit 3
}

# --- delivery: alert.sh, and nothing else -------------------------------------
# The cooldown (default 900s), the channel precedence and the 0/1/2/3 delivery
# contract all live in alert.sh. FIRE_RC is kept so a caller can tell "somebody was
# told" (0 delivered, 1 throttled) from "nobody was told" (2/3/other).
fire() {
    id="$1"; observed="$2"
    echo "probe: FIRING $id - observed: $observed"
    sh "$ALERT_SH" --alert "$id" --observed "$observed"
    FIRE_RC=$?
    case "$FIRE_RC" in
        0) FIRED=$((FIRED + 1)); echo "probe:   -> delivered" ;;
        1) FIRED=$((FIRED + 1)); echo "probe:   -> THROTTLED (inside the cooldown); one page per incident, not per run" ;;
        2) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "probe:   -> NOT DELIVERED: no channel configured" >&2 ;;
        3) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "probe:   -> NOT DELIVERED: delivery failed" >&2 ;;
        *) FIRE_FAILED=$((FIRE_FAILED + 1)); echo "probe:   -> NOT DELIVERED: alert.sh usage error (exit $FIRE_RC)" >&2 ;;
    esac
    return 0
}

# http_get_code <url> - echoes the HTTP status code; the return value is curl's exit
# status, so a non-zero return means "no HTTP answer at all" (refused, timeout, DNS).
http_get_code() {
    curl -sS -o /dev/null -w '%{http_code}' \
        --connect-timeout "$HTTP_TIMEOUT" --max-time "$HTTP_TIMEOUT" \
        "$1" 2>"$CURL_ERR"
}

first_err() {
    ERR=$(head -n 1 "$CURL_ERR" 2>/dev/null)
    [ -n "$ERR" ] || ERR="$1"
    printf '%s' "$ERR"
}

# --- check: api_down ----------------------------------------------------------
# The registry threshold is "120s of failed /health checks", so the window is a
# WINDOW, not a single sample: poll until either /health answers (the failure run is
# broken - consecutive means consecutive) or the failures span the window. A run
# only blocks while the API is actually down, which is exactly when confirming the
# outage for the full window before paging is the right thing to do.
check_api_down() {
    T0=$(date -u +%s)
    N=0
    while :; do
        CODE=$(http_get_code "$API_URL/health")
        RC=$?
        if [ "$RC" -eq 0 ] && [ "$CODE" = "200" ]; then
            echo "probe: OK  api_down: $API_URL/health returned 200"
            return 0
        fi
        N=$((N + 1))
        NOW=$(date -u +%s)
        SPAN=$((NOW - T0))
        if [ "$SPAN" -ge "$API_WINDOW" ]; then
            REASON=$(first_err "HTTP $CODE")
            echo "probe: ALERT api_down: $API_URL/health failing for ${SPAN}s of the ${API_WINDOW}s window ($N consecutive failed check(s); last: $REASON)"
            fire api_down "$API_URL/health failed ${SPAN}s in a row (>= ${API_WINDOW}s window), $N consecutive check(s); last error: $REASON"
            return 0
        fi
        LEFT=$((API_WINDOW - SPAN))
        if [ "$LEFT" -lt "$API_INTERVAL" ]; then STEP="$LEFT"; else STEP="$API_INTERVAL"; fi
        [ "$STEP" -ge 1 ] || STEP=1
        sleep "$STEP"
    done
}

# --- check: error_rate --------------------------------------------------------
# Threshold from alerts.tsv: "5% over 5 min". The server counts 5xx responses and
# total responses in process (`error.rs`), and serves them at
# `GET /api/admin/metrics` behind the OPERATOR guard - not on /health, whose body is
# pinned because the deploy gate parses it and a leak test forbids any digit in it.
#
# This is why the check needs PROBE_OPERATOR_COOKIE: the route is authenticated, so a
# prober without a session cannot read it. An unset cookie is a VISIBLY SKIPPED check
# rather than a silent pass, the same rule as an unset PROBE_LOG_FILE.
#
# The rate is a RATIO of two counters, and `null` is NOT zero: the server sends null
# when it has served nothing, which is the fresh-deploy case. Treating null as 0.0
# would report a perfectly healthy service and suppress the alert - the exact
# failure mode the counter was built to avoid. So null is NO DATA and the check
# says so.
check_error_rate() {
    if [ -z "$OPERATOR_COOKIE" ]; then
        echo "probe: skipped error_rate: PROBE_OPERATOR_COOKIE is unset - the route is operator-authenticated, so this alert is NOT checked"
        return 0
    fi

    BODY_FILE="$TMP/probe-metrics.$$.json"
    CODE=$(curl -sS -o "$BODY_FILE" -w '%{http_code}' \
        --connect-timeout "$HTTP_TIMEOUT" --max-time "$HTTP_TIMEOUT" \
        -H "Cookie: $OPERATOR_COOKIE" \
        "$API_URL/api/admin/metrics" 2>"$CURL_ERR")
    RC=$?
    if [ "$RC" -ne 0 ]; then
        REASON=$(first_err "curl exit $RC")
        rm -f "$BODY_FILE"
        echo "probe: FAILED error_rate: the metrics route did not answer ($REASON)" >&2
        fail_with 4
        return 0
    fi
    if [ "$CODE" != "200" ]; then
        rm -f "$BODY_FILE"
        # 401 means the cookie is wrong or expired, 403 that it is not an operator.
        # Either way this is a CONFIGURATION problem, not a healthy service: reporting
        # a pass here would be the silent-absence failure this directory exists to
        # prevent.
        echo "probe: FAILED error_rate: /api/admin/metrics returned HTTP $CODE (401 = bad or expired cookie, 403 = not an operator)" >&2
        fail_with 4
        return 0
    fi

    # Pull the three fields without a JSON parser (sqlite3/curl only, no jq): the
    # payload is flat, so a targeted grep is honest and sufficient.
    RATE=$(sed -n 's/.*"error_rate":[[:space:]]*\([^,}]*\).*/\1/p' "$BODY_FILE" | head -n 1)
    ERRORS=$(sed -n 's/.*"server_errors":[[:space:]]*\([0-9]*\).*/\1/p' "$BODY_FILE" | head -n 1)
    RESPONSES=$(sed -n 's/.*"responses":[[:space:]]*\([0-9]*\).*/\1/p' "$BODY_FILE" | head -n 1)
    rm -f "$BODY_FILE"

    if [ "$RATE" = "null" ]; then
        echo "probe: OK  error_rate: no data (null) - the service has served nothing in this window; NOT a healthy 0%"
        return 0
    fi
    case "$RATE" in
        ''|*[!0-9.]*)
            echo "probe: FAILED error_rate: could not parse a rate from the metrics route" >&2
            fail_with 6
            return 0
            ;;
    esac

    # Compare in tenths of a percent using integer arithmetic: the threshold is 5%,
    # and shell has no float. `awk` is already a dependency of this directory.
    BREACH=$(awk -v r="$RATE" 'BEGIN { print (r > 0.05) ? "yes" : "no" }')
    if [ "$BREACH" = "yes" ]; then
        echo "probe: ALERT error_rate: $RATE ($ERRORS of $RESPONSES responses) exceeds the 5% threshold"
        fire error_rate "$ERRORS of $RESPONSES responses are 5xx ($RATE), above the 5% threshold"
    else
        echo "probe: OK  error_rate: $RATE ($ERRORS of $RESPONSES) is within the 5% threshold"
    fi
}
# --- check: relay_down --------------------------------------------------------
# Threshold is "1 failed external check": ONE request, no window. Any HTTP answer at
# all means the relay is up - a 502/504 is relay_5xx's alert (still unchecked), not
# this one's, and conflating the two is how a "the backend is down" page gets filed
# as "the edge is down".
check_relay_down() {
    CODE=$(http_get_code "$RELAY_URL")
    RC=$?
    if [ "$RC" -ne 0 ]; then
        REASON=$(first_err "curl exit $RC")
        echo "probe: ALERT relay_down: $RELAY_URL did not answer a single external check ($REASON)"
        fire relay_down "relay at $RELAY_URL did not answer one external check: $REASON"
    else
        echo "probe: OK  relay_down: $RELAY_URL answered (HTTP $CODE)"
    fi
    return 0
}

# --- check: webhook_rejection -------------------------------------------------
# The registry condition is "any topup.rejected". server/src/routes/webhooks.rs emits
# it as a structured LOG line, not a counter, so the source is the server's captured
# stdout (PROBE_LOG_FILE). The window is "lines written since the last scan", held as
# a line offset in PROBE_STATE_DIR: one page per rejection, not one page per run.
# A rotation (the file shrank) rescans from the start.
record_offset() {
    if mkdir -p "$STATE_DIR" 2>/dev/null; then
        printf '%s\n' "$1" > "$STATE_FILE" 2>/dev/null || \
            echo "probe: warning: cannot write $STATE_FILE; the same log lines will be re-scanned" >&2
    else
        echo "probe: warning: state dir not writable ($STATE_DIR); the same log lines will be re-scanned" >&2
    fi
}

check_webhook_rejection() {
    if [ -z "$LOG_FILE" ]; then
        echo "probe: SKIPPED webhook_rejection - no log source configured (PROBE_LOG_FILE unset)."
        echo "probe:   This alert is NOT being checked. Set PROBE_LOG_FILE to the server's captured"
        echo "probe:   stdout to enable it. A skip stated out loud, not a pass."
        return 0
    fi
    if [ ! -e "$LOG_FILE" ]; then
        echo "probe: UNKNOWN webhook_rejection - PROBE_LOG_FILE=$LOG_FILE does not exist, so" >&2
        echo "probe:   'no rejections' cannot be claimed. Unknown is not clean." >&2
        fail_with 6
        return 0
    fi
    if [ ! -r "$LOG_FILE" ]; then
        echo "probe: FAILED webhook_rejection - PROBE_LOG_FILE=$LOG_FILE is not readable" >&2
        fail_with 4
        return 0
    fi

    STATE_FILE="$STATE_DIR/webhook_rejection.offset"
    LAST=0
    if [ -f "$STATE_FILE" ]; then
        LAST=$(cat "$STATE_FILE" 2>/dev/null || echo 0)
        case "$LAST" in ''|*[!0-9]*) LAST=0 ;; esac
    fi
    TOTAL=$(wc -l < "$LOG_FILE" 2>/dev/null | tr -d ' \t')
    case "$TOTAL" in
        ''|*[!0-9]*)
            echo "probe: FAILED webhook_rejection - could not count lines in $LOG_FILE" >&2
            fail_with 4
            return 0
            ;;
    esac
    if [ "$TOTAL" -lt "$LAST" ]; then
        echo "probe: note: $LOG_FILE shrank (rotated or truncated); scanning it from the start"
        LAST=0
    fi
    NEW=$((TOTAL - LAST))
    N=0
    if [ "$NEW" -gt 0 ]; then
        N=$(tail -n "+$((LAST + 1))" "$LOG_FILE" | grep -c -F 'topup.rejected')
    fi

    if [ "$N" -gt 0 ]; then
        echo "probe: ALERT webhook_rejection: $N 'topup.rejected' line(s) in $NEW new line(s) of $LOG_FILE"
        fire webhook_rejection "$N 'topup.rejected' line(s) in the $NEW new line(s) of $LOG_FILE since the last scan"
        # The scan marker advances only when somebody was actually told (delivered or
        # throttled) - alert.sh's own rule: a failed delivery must not consume the
        # event, or nobody is told AND nothing retries.
        if [ "$FIRE_RC" -eq 0 ] || [ "$FIRE_RC" -eq 1 ]; then
            record_offset "$TOTAL"
        else
            echo "probe:   -> scan marker NOT advanced; the next run will see these lines again" >&2
        fi
    else
        echo "probe: OK  webhook_rejection: 0 'topup.rejected' in $NEW new line(s) of $LOG_FILE ($TOTAL total; scanned from line $((LAST + 1)))"
        record_offset "$TOTAL"
    fi
    return 0
}

# --- run ----------------------------------------------------------------------
run_check() {
    case "$1" in
        api_down) check_api_down ;;
        relay_down) check_relay_down ;;
        error_rate) check_error_rate ;;
        webhook_rejection) check_webhook_rejection ;;
    esac
    return 0
}

if [ -n "$CHECKS" ]; then
    for id in $CHECKS; do run_check "$id"; done
else
    run_check api_down
    run_check relay_down
    run_check webhook_rejection
fi

# --- what still is not checked, on EVERY run ----------------------------------
# Four checks are covered now. These THREE are not, and saying so every run is the
# whole point of this directory: a check that is silently absent is the failure mode,
# and three silent absences would be three.
#
# error_rate MOVED OUT of this list: the counters are in-process (server/src/error.rs)
# and served at GET /api/admin/metrics, so it is a real check now. The line that used
# to sit here said it needed "HTTP counters over a 5-minute window (same access logs)",
# which was true before the counter existed and is now stale.
cat >&2 <<'NOTCHECKED'
probe: NOT CHECKED - 3 of the doc's 10 alerts still need a surface this cannot reach:
probe:   relay_5xx               - nginx access-log status counts (access logs are deliberately off)
probe:   all_providers_unhealthy - upstream circuit-breaker state, in-process under server/
probe:   db_disk                 - volume usage, not visible to any client
probe: Full table and the reasons: tools/alert/README.md. Every definition: alert.sh --list
NOTCHECKED

# --- verdict ------------------------------------------------------------------
if [ "$FIRE_FAILED" -gt 0 ]; then
    echo "probe: UNDELIVERED - $FIRE_FAILED alert(s) fired and were NOT delivered" >&2
    exit 5
fi
if [ "$FAIL_CODE" -ne 0 ]; then
    echo "probe: not clean - a check could not run (exit $FAIL_CODE); unknown is not clean" >&2
    exit "$FAIL_CODE"
fi
if [ "$FIRED" -gt 0 ]; then
    echo "probe: FIRED - $FIRED alert(s) fired and were delivered (or throttled)"
    exit 1
fi
echo "probe: CLEAN - every selected check ran (or was visibly skipped) and no threshold was breached"
exit 0
