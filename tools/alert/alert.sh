#!/bin/sh
# apikita alert transport - the delivery half of docs/observability.md.
#
# docs/observability.md:13 states the principle: "Alert on things that cost money
# or lose data. Ignore everything else." and :16 that "anything without an action
# is a dashboard, not an alert". So this is NOT a log shipper. It delivers ONE
# alert, and the message always carries the four things an operator needs to act:
# which alert, the observed value, the threshold, and the doc's stated action. The
# ACTION is the first body line for exactly that reason.
#
# The failure mode that matters is a SILENTLY DROPPED ALERT - the same reasoning as
# tools/backup/backup.sh refusing to write a plaintext dump. An alert nobody
# receives is worse than no alerting, because it is believed. So this script
# REFUSES to run with no channel configured (exit 2), and reports an undeliverable
# alert as a FAILURE (exit 3) - never as a quiet success.
#
# Alert definitions (id, title, condition, threshold, action, coverage, source)
# live in ONE place - alerts.tsv next to this file - so the threshold and the
# action cannot drift from the docs/observability.md table they were copied from,
# and so they can be asserted.
#
# Usage:
#   alert.sh --alert <id> [--observed <value|->] [--key <key>] [--dry-run]
#   echo "<value>" | alert.sh --alert ledger_drift --observed -
#   alert.sh --alert <id> --message-file <path>
#   alert.sh --list
#   alert.sh --check-alerts [--observed <value>]
#
# Channels, in precedence order (the first configured one wins, and it is named in
# the output so the operator knows where the alert went):
#   TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID   real HTTP POST to the Bot API sendMessage
#   WEBHOOK_URL                             any HTTP endpoint
#   ALERT_SINK_FILE                         file channel - appends the payload (tests)
#   ALERT_SINK_STDOUT=1                     stdout channel - local use
#
# Exit codes:
#   0  DELIVERED - the channel accepted the alert
#   1  THROTTLED - the same alert key fired inside ALERT_COOLDOWN_SECONDS and was
#      suppressed on purpose. Nothing was sent. This is neither a failure nor a
#      delivery: a suppressed alert must never look like a delivered one.
#   2  NO CHANNEL CONFIGURED - refusing to drop the alert on the floor
#   3  DELIVERY FAILED - a channel was configured and did not accept the alert
#   4  usage error (unknown alert id, missing argument, bad cooldown)

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
ALERTS_FILE="${ALERT_DEFINITIONS:-$SCRIPT_DIR/alerts.tsv}"

COOLDOWN="${ALERT_COOLDOWN_SECONDS:-900}"
STATE_DIR="${ALERT_STATE_DIR:-${TMPDIR:-/tmp}/apikita-alerts}"
HOSTNAME_TAG="${ALERT_HOST:-$(hostname 2>/dev/null || echo unknown)}"
ENV_TAG="${ALERT_ENV:-}"
TAB=$(printf '\t')

ALERT_ID=""
OBSERVED=""
KEY=""
DRY_RUN=0
MODE="send"
MESSAGE_FILE=""

list_ids() {
    cut -f1 "$ALERTS_FILE" 2>/dev/null | tail -n +2 | sed 's/^/alert:   /'
}

usage() {
    echo "alert: usage: alert.sh --alert <id> [--observed <value|->] [--key <key>] [--dry-run]" >&2
    echo "alert:        alert.sh --list" >&2
    echo "alert:        alert.sh --check-alerts [--observed <value>]" >&2
    echo "alert: known alert ids (see $ALERTS_FILE):" >&2
    list_ids >&2
}

if [ ! -f "$ALERTS_FILE" ]; then
    echo "alert: alert definitions not found: $ALERTS_FILE" >&2
    exit 4
fi

while [ $# -gt 0 ]; do
    case "$1" in
        --alert)        [ $# -ge 2 ] || { echo "alert: --alert needs a value" >&2; exit 4; }; ALERT_ID="$2"; shift 2 ;;
        --observed)     [ $# -ge 2 ] || { echo "alert: --observed needs a value" >&2; exit 4; }; OBSERVED="$2"; shift 2 ;;
        --key)          [ $# -ge 2 ] || { echo "alert: --key needs a value" >&2; exit 4; }; KEY="$2"; shift 2 ;;
        --message-file) [ $# -ge 2 ] || { echo "alert: --message-file needs a value" >&2; exit 4; }; MESSAGE_FILE="$2"; shift 2 ;;
        --dry-run)      DRY_RUN=1; shift ;;
        --list)         MODE="list"; shift ;;
        --check-alerts) MODE="check"; shift ;;
        --help|-h)      usage; exit 4 ;;
        *) echo "alert: unknown argument: $1" >&2; exit 4 ;;
    esac
done

if [ "$MODE" = "list" ]; then
    printf '%-26s %-14s %s\n' "ID" "COVERAGE" "THRESHOLD"
    awk -F"$TAB" 'NR>1 && NF { printf "%-26s %-14s %s\n", $1, $6, $4 }' "$ALERTS_FILE"
    exit 0
fi

if [ "$MODE" = "check" ]; then
    if [ -n "$OBSERVED" ]; then
        exec sh "$SCRIPT_DIR/check-alerts.sh" --observed "$OBSERVED"
    fi
    exec sh "$SCRIPT_DIR/check-alerts.sh"
fi

# --- one alert definition ----------------------------------------------------
if [ -z "$ALERT_ID" ]; then
    echo "alert: no --alert <id> given" >&2
    usage
    exit 4
fi

DEF=$(awk -F"$TAB" -v id="$ALERT_ID" '$1==id { print; exit }' "$ALERTS_FILE")
if [ -z "$DEF" ]; then
    echo "alert: unknown alert id: $ALERT_ID" >&2
    list_ids >&2
    exit 4
fi

TITLE=$(printf '%s' "$DEF" | cut -f2)
CONDITION=$(printf '%s' "$DEF" | cut -f3)
THRESHOLD=$(printf '%s' "$DEF" | cut -f4)
ACTION=$(printf '%s' "$DEF" | cut -f5)
COVERAGE=$(printf '%s' "$DEF" | cut -f6)
SOURCE=$(printf '%s' "$DEF" | cut -f7)

# --- observed value ----------------------------------------------------------
# stdin is read ONLY when --observed - is passed explicitly. An implicit read
# would block forever under a scheduler that hands the process a pipe.
if [ -n "$MESSAGE_FILE" ]; then
    [ -f "$MESSAGE_FILE" ] || { echo "alert: --message-file not found: $MESSAGE_FILE" >&2; exit 4; }
    OBSERVED=$(cat "$MESSAGE_FILE")
elif [ "$OBSERVED" = "-" ]; then
    OBSERVED=$(cat)
fi
[ -n "$OBSERVED" ] || OBSERVED="(no observed value supplied)"

# --- cooldown ----------------------------------------------------------------
# Alert fatigue is the doc's stated margin concern (docs/observability.md:15). The
# same alert KEY inside the window is suppressed: one page per incident, not one
# page per run of the check.
case "$COOLDOWN" in
    ''|*[!0-9]*)
        echo "alert: ALERT_COOLDOWN_SECONDS='$COOLDOWN' is not a non-negative integer" >&2
        exit 4
        ;;
esac

DEDUP_KEY="${KEY:-$ALERT_ID}"
SAFE_KEY=$(printf '%s' "$DEDUP_KEY" | tr -c 'A-Za-z0-9._-' '_')
STATE_FILE="$STATE_DIR/$SAFE_KEY.last"

if [ "$DRY_RUN" -eq 0 ] && [ "$COOLDOWN" -gt 0 ] && [ -f "$STATE_FILE" ]; then
    LAST=$(cat "$STATE_FILE" 2>/dev/null || echo 0)
    case "$LAST" in ''|*[!0-9]*) LAST=0 ;; esac
    NOW=$(date -u +%s)
    AGE=$((NOW - LAST))
    if [ "$AGE" -ge 0 ] && [ "$AGE" -lt "$COOLDOWN" ]; then
        echo "alert: THROTTLED '$ALERT_ID' (key '$DEDUP_KEY'): last delivered ${AGE}s ago, cooldown ${COOLDOWN}s; nothing sent" >&2
        exit 1
    fi
fi

# --- the message -------------------------------------------------------------
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
if [ -n "$ENV_TAG" ]; then
    SUBJECT="[$ENV_TAG] ALERT: $TITLE"
else
    SUBJECT="ALERT: $TITLE"
fi

BODY=$(printf '%s\n%s\n\n%s\n%s\n%s\n%s\n\n%s\n%s\n%s\n%s\n\n%s\n%s' \
    "$SUBJECT" \
    "ACTION: $ACTION" \
    "Alert    : $ALERT_ID - $TITLE" \
    "Observed : $OBSERVED" \
    "Threshold: $THRESHOLD" \
    "Condition: $CONDITION" \
    "Host     : $HOSTNAME_TAG" \
    "Time     : $TS" \
    "Source   : $SOURCE" \
    "Coverage : $COVERAGE (tools/alert/README.md)")

# --- channel selection -------------------------------------------------------
CHANNEL=""
if [ -n "${TELEGRAM_BOT_TOKEN:-}" ] && [ -n "${TELEGRAM_CHAT_ID:-}" ]; then
    CHANNEL="telegram"
elif [ -n "${WEBHOOK_URL:-}" ]; then
    CHANNEL="webhook"
elif [ -n "${ALERT_SINK_FILE:-}" ]; then
    CHANNEL="file"
elif [ "${ALERT_SINK_STDOUT:-}" = "1" ]; then
    CHANNEL="stdout"
fi

if [ -z "$CHANNEL" ]; then
    echo "alert: NO CHANNEL CONFIGURED - refusing to drop '$ALERT_ID' silently" >&2
    echo "alert: an alert nobody receives is worse than no alerting, because it is believed" >&2
    echo "alert: configure ONE of:" >&2
    echo "alert:   TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID   (real Bot API sendMessage)" >&2
    echo "alert:   WEBHOOK_URL                            (any HTTP endpoint)" >&2
    echo "alert:   ALERT_SINK_FILE=<path>                  (file channel, for tests)" >&2
    echo "alert:   ALERT_SINK_STDOUT=1                     (stdout, local use)" >&2
    echo "alert: the alert that was NOT delivered:" >&2
    printf '%s\n' "$BODY" >&2
    exit 2
fi

if [ "$DRY_RUN" -eq 1 ]; then
    echo "alert: DRY RUN - channel would be '$CHANNEL'; nothing sent; cooldown not consumed"
    printf '%s\n' "$BODY"
    exit 0
fi

# --- deliver -----------------------------------------------------------------
DELIVERED=0
RESP_FILE="${TMPDIR:-/tmp}/apikita-alert-resp.$$"
TIMEOUT="${ALERT_HTTP_TIMEOUT:-10}"
case "$CHANNEL" in
    telegram)
        # The token is read from the environment and used in the URL only. It is
        # never echoed, never written to the state file, never committed.
        # TELEGRAM_API_BASE exists so the sendMessage REQUEST SHAPE can be tested
        # against a local sink without a real bot; it is not a production knob.
        API="${TELEGRAM_API_BASE:-https://api.telegram.org}/bot$TELEGRAM_BOT_TOKEN/sendMessage"
        HTTP=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
            --connect-timeout "$TIMEOUT" --max-time "$TIMEOUT" \
            -X POST "$API" \
            --data-urlencode "chat_id=$TELEGRAM_CHAT_ID" \
            --data-urlencode "disable_web_page_preview=true" \
            --data-urlencode "text=$BODY" 2>"$RESP_FILE.err")
        CURL_STATUS=$?
        if [ "$CURL_STATUS" -ne 0 ]; then
            echo "alert: telegram delivery FAILED (curl exit $CURL_STATUS):" >&2
            cat "$RESP_FILE.err" >&2 2>/dev/null
        elif [ "$HTTP" = "200" ]; then
            DELIVERED=1
        else
            echo "alert: telegram delivery FAILED (HTTP $HTTP):" >&2
            cat "$RESP_FILE" >&2 2>/dev/null
        fi
        ;;
    webhook)
        JSON=$(printf '%s' "$BODY" | jq -Rs '{text: .}')
        HTTP=$(curl -sS -o "$RESP_FILE" -w '%{http_code}' \
            --connect-timeout "$TIMEOUT" --max-time "$TIMEOUT" \
            -X POST "$WEBHOOK_URL" \
            -H 'Content-Type: application/json' \
            --data "$JSON" 2>"$RESP_FILE.err")
        CURL_STATUS=$?
        if [ "$CURL_STATUS" -ne 0 ]; then
            echo "alert: webhook delivery FAILED (curl exit $CURL_STATUS):" >&2
            cat "$RESP_FILE.err" >&2 2>/dev/null
        else
            case "$HTTP" in
                2??) DELIVERED=1 ;;
                *) echo "alert: webhook delivery FAILED (HTTP $HTTP):" >&2; cat "$RESP_FILE" >&2 2>/dev/null ;;
            esac
        fi
        ;;
    file)
        if printf '%s\n' "$BODY" >> "$ALERT_SINK_FILE" 2>/dev/null; then
            DELIVERED=1
        else
            echo "alert: file delivery FAILED: cannot append to $ALERT_SINK_FILE" >&2
        fi
        ;;
    stdout)
        printf '%s\n' "$BODY"
        DELIVERED=1
        ;;
esac
rm -f "$RESP_FILE" "$RESP_FILE.err" 2>/dev/null || true

if [ "$DELIVERED" -ne 1 ]; then
    echo "alert: DELIVERY FAILED for '$ALERT_ID' on channel '$CHANNEL' - the alert was NOT delivered" >&2
    exit 3
fi

# The cooldown is recorded ONLY after the channel accepted the alert. Recording it
# before would let a failed delivery suppress its own retry for the whole window.
if [ "$COOLDOWN" -gt 0 ]; then
    if mkdir -p "$STATE_DIR" 2>/dev/null; then
        date -u +%s > "$STATE_FILE" 2>/dev/null || true
    else
        echo "alert: warning: state dir not writable ($STATE_DIR); cooldown is not effective" >&2
    fi
fi

echo "alert: DELIVERED '$ALERT_ID' via $CHANNEL (key '$DEDUP_KEY', cooldown ${COOLDOWN}s)"
exit 0
