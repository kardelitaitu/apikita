#!/bin/sh
# apikita alert-delivery check - the alerting tool must DELIVER, and must not silence itself.
#
# WHY THIS EXISTS. Every other gate in this repository DETECTS a problem; this tool has to
# TELL SOMEONE. It had no CI coverage, and its documentation is dense with deliberate
# distinctions that a tidy-up can silently collapse:
#
#   tools/alert/README.md:111 - "A throttled alert is exit 1, not exit 0. 'Suppressed on
#     purpose' and 'delivered' are different facts and must not be conflated."
#   :114 - "The cooldown is recorded only AFTER the channel accepted the alert. Recording it
#     first would let a failed delivery suppress its own retry for the whole window - the
#     worst of both: nobody was told, and nothing will try again for 15 minutes."
#   :99  - the cooldown exists because "a small business with thin margins cannot afford
#     alert fatigue... one page per incident, not one page per run of the check."
#
# THE ORDERING PROPERTY IS THE HIGH-STORES ONE, because its failure is INVISIBLE in normal
# operation: alerts still arrive, but a single failed delivery silences that incident for
# the whole window and nothing ever says so. It is tested by attempting twice against a
# FAILING channel and asserting that BOTH attempts were made - if the first had recorded a
# cooldown, the second would be suppressed and the tool would look healthy.
#
# Uses the FILE channel (ALERT_SINK_FILE), so it is hermetic: no network, no credentials.
#
# Usage: sh tools/alert-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
WORK="${TMPDIR:-/tmp}/apikita-alert-check-$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
mkdir -p "$WORK" || { echo "alert-check: cannot create $WORK" >&2; exit 2; }

[ -f "$REPO/tools/alert/alert.sh" ] || { echo "alert-check: alert.sh is missing" >&2; exit 3; }

FAILED=0
fail() { echo "alert-check: FAIL - $1" >&2; FAILED=1; }

# A fresh state dir per scenario, so no scenario can be affected by another's cooldown.
# The alert arguments are passed through with "$@": listing $4..$8 explicitly would break
# on any shorter invocation AND trips `set -u` on the first one that runs short.
fire() {
    # $1 = state dir name, $2 = sink file name, then the alert.sh arguments
    sd="$WORK/$1"; sink="$WORK/$2"; shift 2
    mkdir -p "$sd"
    ALERT_SINK_FILE="$sink" ALERT_STATE_DIR="$sd" ALERT_COOLDOWN_SECONDS=900 \
        sh "$REPO/tools/alert/alert.sh" "$@" >/dev/null 2>&1
    printf '%s' "$?"
}

# --- the documented exit distinctions ---------------------------------------
rc=$(fire a a.sink --alert ledger_drift --observed "3 accounts")
[ "$rc" = "0" ] || fail "a delivered alert must exit 0, got $rc"

# THE ONE THAT MATTERS MOST: throttled is exit 1, NOT 0. Collapsing it to 0 would make an
# alert that nobody received indistinguishable from one that was, which is the exact
# conflation the README forbids.
rc=$(fire a a.sink --alert ledger_drift --observed "3 accounts")
[ "$rc" = "1" ] || fail "a THROTTLED alert must exit 1 (not 0 - suppressed and delivered are different facts), got $rc"

# A different key must NOT be throttled by the first.
rc=$(fire a a.sink --alert api_down --observed "down")
[ "$rc" = "0" ] || fail "a different alert key must not be throttled by another key, got $rc"

# An unknown id is a usage error, exit 4.
rc=$(fire b b.sink --alert no_such_alert --observed "x")
[ "$rc" = "4" ] || fail "an unknown alert id must exit 4, got $rc"

# --- THE ORDERING PROPERTY ---------------------------------------------------
# A failing channel must not record a cooldown. Proven by attempting TWICE: if the first
# attempt recorded state, the second would be throttled (exit 1) instead of reaching the
# channel again (exit 3).
ORDER="$WORK/order"; mkdir -p "$ORDER"
BAD_CHANNEL="http://127.0.0.1:18999/hook"

attempt() {
    ALERT_COOLDOWN_SECONDS=900 ALERT_STATE_DIR="$ORDER" WEBHOOK_URL="$BAD_CHANNEL" \
        ALERT_HTTP_TIMEOUT=2 \
        sh "$REPO/tools/alert/alert.sh" --alert ledger_drift --observed "x" >/dev/null 2>&1
    printf '%s' "$?"
}

first=$(attempt)
if [ "$first" = "0" ]; then
    # Something IS listening on the test port, so the scenario cannot run as designed.
    echo "alert-check: SKIPPED the ordering property - port 18999 accepted a connection" >&2
    echo "alert-check:   the exit-code assertions above DID run" >&2
else
    [ "$first" = "3" ] || fail "a failing webhook must exit 3 (delivery failed), got $first"

    if [ -f "$ORDER/ledger_drift.last" ]; then
        fail "a FAILED delivery recorded a cooldown. That silences this incident for the whole window and nothing tells anyone: the alert never arrived AND will not be retried"
    fi

    second=$(attempt)
    if [ "$second" = "1" ]; then
        fail "the second attempt was THROTTLED (exit 1) after a failed delivery, so the failure silenced its own retry"
    fi
    [ "$second" = "3" ] || fail "the second attempt must also reach the channel and fail (exit 3), got $second"

    # And the positive direction: a SUCCESSFUL delivery must record the cooldown, or the
    # throttle never engages and every run pages. Both directions are needed - without
    # this one, "no state file was written" would pass on a tool that never writes state.
    ALERT_COOLDOWN_SECONDS=900 ALERT_STATE_DIR="$ORDER" ALERT_SINK_FILE="$WORK/ok.sink" \
        sh "$REPO/tools/alert/alert.sh" --alert ledger_drift --observed "x" >/dev/null 2>&1
    [ -f "$ORDER/ledger_drift.last" ] || fail "a SUCCESSFUL delivery did not record a cooldown, so the throttle would never engage and every run would page"
fi

if [ "$FAILED" -ne 0 ]; then
    echo "alert-check: the alert delivery contract is BROKEN (see above)" >&2
    exit 1
fi

echo "alert-check: OK - alerts deliver, throttling is exit 1 not 0, and a failed delivery never silences its own retry"
exit 0