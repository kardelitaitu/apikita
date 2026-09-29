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


# --- a cooldown that could not be recorded must not be CLAIMED -----------------
# The state file is written AFTER a successful delivery, and the write was guarded with
# `|| true`. A state directory that exists while the file cannot be written therefore
# produced the worst possible report: "DELIVERED ... cooldown 900s" with NO cooldown on
# disk, so the throttle never engages and every run of the check pages again - the exact
# outcome ALERT_COOLDOWN_SECONDS exists to prevent, attested to as working.
#
# The directory is made WRITABLE and a directory is placed where the FILE belongs, which
# is the case the else branch at alert.sh:286 cannot see (it only checks the directory).
BADSTATE="$WORK/badstate"
rm -rf "$BADSTATE"
mkdir -p "$BADSTATE/ledger_drift.last"
out=$(ALERT_COOLDOWN_SECONDS=900 ALERT_STATE_DIR="$BADSTATE" ALERT_SINK_FILE="$WORK/bad.sink" \
    sh "$REPO/tools/alert/alert.sh" --alert ledger_drift --observed "x" 2>&1)
rc=$?

# The alert WAS delivered, so this must not become a failure exit. The defect is the MESSAGE.
if [ "$rc" -ne 0 ]; then
    fail "a state-write failure must not turn a delivered alert into an error (got exit $rc); the alert reached the channel"
fi

# The claim to catch is the cooldown being reported as RECORDED. The fixed tool still
# names the cooldown, because it was REQUESTED - it says so and marks it NOT RECORDED. So
# matching the bare phrase would fail the honest message too; match the claim instead.
case "$out" in
    *"cooldown 900s)"*)
        fail "the tool claimed a 900s cooldown while the state file was NOT written - the throttle never engages, so every run pages again, and the message tells an operator the opposite" ;;
esac

# Case-insensitive, because the message capitalises for emphasis ("cooldown is NOT
# effective") and a pattern that only matched lowercase would fail the very message it is
# checking for - which is what happened on the first run of this assertion.
case "$out" in
    *[Nn][Oo][Tt]" effective"*|*"cooldown is NOT effective"*)
    : # it said so
    ;;
    *)
        fail "a cooldown that could not be recorded was not reported at all; the run must say the throttle is OFF. Output was: $out" ;;
esac

# And the honest counterpart: with a WORKING state dir the claim must STILL be made, so the
# assertions above cannot be satisfied by a tool that never mentions the cooldown at all.
mkdir -p "$WORK/goodstate"
out=$(ALERT_COOLDOWN_SECONDS=900 ALERT_STATE_DIR="$WORK/goodstate" ALERT_SINK_FILE="$WORK/good.sink" \
    sh "$REPO/tools/alert/alert.sh" --alert ledger_drift --observed "x" 2>&1)
case "$out" in
    *"cooldown 900s"*) : ;;
    *) fail "with a writable state dir the tool must still report the cooldown; got: $out" ;;
esac


# ---------------------------------------------------------------------------
# The alert DEFINITION and the document that specifies it must agree.
# ---------------------------------------------------------------------------
# WHY THIS IS HERE. docs/observability.md:95 says of its Alerts table: "Each has
# a threshold and an action. If you would not act, do not alert." The table IS the
# specification; alerts.tsv IS the machine-readable definition every check reads.
# Measured when this guard was added, they had already drifted TWICE:
#
#   1. The table listed TEN alerts and alerts.tsv had ELEVEN. The extra was
#      stranded_hold - documented as its own section further down the same file,
#      but missing from the table that claims to be the complete set. The scheduler
#      runs that check nightly, so the omission was of a LIVE alert.
#
#   2. NINE of the eleven citations were off by one. Each cited a LINE NUMBER, so
#      adding or removing a row silently re-pointed every citation below it at its
#      neighbour; refund_refusal cited the webhook_rejection row, and so on down the
#      table. This is why the citations now name the ROW instead of a line.
#
# Neither could be caught by reading: line numbers LOOK precise, which is what makes
# a drifted one convincing. So this checks both the set and the references.
OBS="$REPO/docs/observability.md"
TSV="$REPO/tools/alert/alerts.tsv"
if [ ! -f "$OBS" ] || [ ! -f "$TSV" ]; then
    fail "cannot read $OBS and $TSV, so the alert definition was not compared"
else
    # The table's alert names, from the rows between the header and the next blank line.
    TABLE=$(sed -n '/^| Alert | Condition | Action |/,/^$/p' "$OBS" \
        | grep '^| \*\*' \
        | sed 's/^| \*\*\([^*]*\)\*\*.*/\1/' \
        | sed 's/[[:space:]]*$//')

    # The definition's labels.
    DEFS=$(grep -v '^#' "$TSV" | grep -v '^id' | cut -f2 | sed 's/[[:space:]]*$//')

    # Guard the fixture: a sed that matched nothing would make both sides empty and
    # every assertion below vacuously true.
    T_COUNT=$(printf '%s\n' "$TABLE" | grep -c .)
    D_COUNT=$(printf '%s\n' "$DEFS" | grep -c .)
    if [ "$T_COUNT" -lt 8 ] || [ "$D_COUNT" -lt 8 ]; then
        fail "parsed $T_COUNT table rows and $D_COUNT definitions - too few to be real, so the comparison below would be vacuous"
    fi

    # Set equality, both directions, so neither an alert in the table with no
    # definition nor a definition with no table row can pass.
    MISSING_FROM_TABLE=$(printf '%s\n' "$DEFS" | while IFS= read -r d; do
        [ -z "$d" ] && continue
        printf '%s\n' "$TABLE" | grep -qxF "$d" || printf '%s\n' "$d"
    done)
    MISSING_FROM_TSV=$(printf '%s\n' "$TABLE" | while IFS= read -r t; do
        [ -z "$t" ] && continue
        printf '%s\n' "$DEFS" | grep -qxF "$t" || printf '%s\n' "$t"
    done)

    if [ -n "$MISSING_FROM_TABLE" ]; then
        fail "alerts.tsv defines an alert the observability Alerts table does not list (the table says 'Each has a threshold and an action', so a missing row is a live alert with no specification): $(printf '%s' "$MISSING_FROM_TABLE" | tr '\n' ',')"
    fi
    if [ -n "$MISSING_FROM_TSV" ]; then
        fail "the observability Alerts table lists an alert with no alerts.tsv definition (it would be specified and never checked): $(printf '%s' "$MISSING_FROM_TSV" | tr '\n' ',')"
    fi

    # And every definition must cite the document by NAME, not by line, so that
    # inserting a row cannot silently re-point nine citations at their neighbours.
    STOPPED=$(grep -v '^#' "$TSV" | grep -v '^id' | cut -f7 | grep 'observability' | grep -c 'observability.md:[0-9]')
    if [ "$STOPPED" -gt 0 ]; then
        fail "$STOPPED alerts.tsv citation(s) still use a LINE NUMBER (docs/observability.md:N). A line citation re-points at its neighbour the moment a row is added, which already happened to 9 of 11 of them - cite the row by name instead"
    fi
fi


# ---------------------------------------------------------------------------
# The documented scheduling status must match what the scheduler does.
# ---------------------------------------------------------------------------
# WHY THIS IS HERE, and it is now the fourth time this class has appeared. The
# launch checklist told a reader "nothing invokes the alert checks on a schedule"
# - which was TRUE when written and was invalidated by the two rounds that wired
# the jobs. todo.md told a reader the same thing. Both sentences existed to state
# precisely what remained, and neither was revisited when the remaining thing got
# done. A reader deciding whether alerting is finished was misled in the direction
# that wastes effort.
#
# WHY IT IS NOW A MARKER AND NOT A GREP FOR THE SENTENCE. The previous version
# grepped the checklist for the literal phrase "nothing invokes the alert checks".
# That is exactly how todo.md got through: its sentence read "nothing CURRENTLY
# invokes the alert checks", so the checklist's exact phrase did not match, and the
# one document still carrying the claim was the one the grep could not see. The same
# fragility runs the other way - a correction note cannot safely QUOTE the claim it
# corrects, because the note would trip the very grep meant to catch the claim.
#
# So a document now declares its status with an explicit marker instead:
#
#     <!-- alert-scheduling: wired -->      <!-- alert-scheduling: not-wired -->
#
# which a reader never sees, which a prose paraphrase cannot collide with, and which
# can be asserted EXACTLY: present once, spelled one of two ways, and agreeing with
# the code.
#
# The claim is still mechanical - is run_alert_checks in run_wired_jobs? - so the two
# sides are held together. NEITHER SIDE IS ASSERTED ALONE: the assertion is that they
# AGREE, so it stays true whichever way someone changes it, and it fails in EITHER
# direction rather than pinning today's state.
ENTRYPOINT="$REPO/.docker/maintenance/entrypoint.sh"
# Every document that tells a reader whether the alert checks run on a schedule. A
# NEW document that repeats the claim must be added here, or it repeats todo.md's
# fate: correct in isolation, stale within a fortnight.
SCHEDULING_DOCS="$REPO/docs/launch-checklist.md $REPO/todo.md"

if [ ! -f "$ENTRYPOINT" ]; then
    fail "cannot read $ENTRYPOINT, so the documented scheduling status was not compared"
else
    WIRED=no
    sed -n '/^run_wired_jobs()/,/^}/p' "$ENTRYPOINT" | grep -q 'run_alert_checks' && WIRED=yes
    # THE SAME VOCABULARY as the marker, so the comparison below is a real equality
    # and not a yes/no compared against wired/not-wired - which compares unequal
    # ALWAYS and fails on a tree where the two sides already agree.
    [ "$WIRED" = yes ] && CODE_MARKER=wired || CODE_MARKER=not-wired

    for doc in $SCHEDULING_DOCS; do
        rel="${doc#"$REPO"/}"
        if [ ! -f "$doc" ]; then
            fail "cannot read $rel, so its documented scheduling status was not compared"
            continue
        fi
        # ANCHORED TO THE FULL MARKER FORM, `<!-- alert-scheduling: X -->`, and not to the
        # bare string. A document that EXPLAINS the marker necessarily writes the words
        # "alert-scheduling:" in prose - this file's own history, and todo.md's correction
        # note, both do - and a bare grep counts those as markers, so a document that
        # documents the convention fails the check for doing exactly that. Found by
        # running it: the note explaining the marker made todo.md look ambiguous.
        count=$(grep -c '<!-- alert-scheduling:' "$doc")
        case "$count" in
            '' | *[!0-9]*)
                fail "could not count the alert-scheduling markers in $rel - the comparison did not actually happen"
                continue
                ;;
        esac
        if [ "$count" -ne 1 ]; then
            fail "$rel carries $count 'alert-scheduling:' markers; exactly one is required, or its documented status is ambiguous"
            continue
        fi
        MARKER=$(sed -n 's/.*<!-- alert-scheduling: *\([a-z-]*\) *-->.*/\1/p' "$doc" | head -n 1)
        case "$MARKER" in
            wired | not-wired) ;;
            *)
                fail "$rel has an unreadable alert-scheduling marker '$MARKER'; use exactly 'wired' or 'not-wired'"
                continue
                ;;
        esac
        if [ "$MARKER" != "$CODE_MARKER" ]; then
            if [ "$WIRED" = yes ]; then
                fail "run_wired_jobs schedules run_alert_checks, but $rel still declares the alert checks '$MARKER' - a reader is misled about whether alerting runs on its own"
            else
                fail "run_wired_jobs does NOT schedule run_alert_checks, but $rel declares them '$MARKER' - a reader would believe the alerts run themselves"
            fi
        fi
    done

    # Guard the fixture: if the nightly job list could not be read, WIRED would be
    # "no", and only the second branch above would be live.
    sed -n '/^run_wired_jobs()/,/^}/p' "$ENTRYPOINT" | grep -q 'run_retention' || {
        fail "could not read the run_wired_jobs job list from $ENTRYPOINT - the comparison above did not actually happen"
    }
fi


if [ "$FAILED" -ne 0 ]; then
    echo "alert-check: the alert delivery contract is BROKEN (see above)" >&2
    exit 1
fi

echo "alert-check: OK - alerts deliver, throttling is exit 1 not 0, and a failed delivery never silences its own retry"
exit 0