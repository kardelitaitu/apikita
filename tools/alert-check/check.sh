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
# Exit: 0 all hold, 1 a violation, 2 the scratch directory could not be created,
#       3 a prerequisite is missing.
#
# 2 IS NOT "USAGE". Nothing on the command line is wrong and there is no flag to correct:
# mkdir failed, so this is a permissions or disk problem. It printed no code at all before,
# which meant an operator seeing a bare failure had to guess whether to re-read their flags
# or look at the filesystem.

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
    # MEASURED, and this branch used to hide a real defect. It assumed exit 0 could only
    # mean "something is listening on the test port". It cannot: exit 0 is ALSO what a tool
    # returns when a FAILED delivery falls through to the success path. Removing the
    # `if [ "$DELIVERED" -ne 1 ]` guard at alert.sh:276 makes a dead-port webhook exit 0 AND
    # write a cooldown -- precisely the "a failed delivery silences its own retry" defect
    # this block exists to catch -- and the old skip branch reported it as a port conflict
    # and skipped the assertions. The whole ordering property was green under that mutation.
    #
    # So a skip is only honest if the port really is occupied. Distinguish them: a live
    # port means a connection is ACCEPTED, so check for a listener before skipping, and
    # treat a delivered-but-failed alert as the failure it is.
    #
    # RE-VERIFIED after that fix, because a guard repaired in response to a mutation is
    # exactly the kind that can be repaired into a different blind spot. Re-applying the
    # same mutation to a clean tree (`if [ "$DELIVERED" -ne 1 ]` -> `if false` at
    # alert.sh:276) now exits 1 and prints the message below, with the port genuinely
    # unoccupied - so the branch was taken on the DEFECT and not on a fixture problem.
    # Restoring the file byte-identically returns exit 0. Both directions, measured.
    if command -v nc >/dev/null 2>&1 && nc -z 127.0.0.1 18999 >/dev/null 2>&1; then
        echo "alert-check: SKIPPED the ordering property - port 18999 accepted a connection" >&2
        echo "alert-check:   the exit-code assertions above DID run" >&2
    else
        fail "a failing webhook exited 0 rather than 3, and nothing is listening on the test port. Exit 0 here means a FAILED delivery took the success path: it reported delivery and recorded a cooldown, so this incident is silenced for the whole window and will not be retried"
    fi
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

# --- a delivery that FAILED must not take the success path ---------------------
# The block above proves the ORDERING (no cooldown after a failure). This proves the
# report: a failed delivery must say so and exit 3, on EVERY channel, not just the webhook
# the block above uses. It uses the FILE channel pointed at a path whose parent does not
# exist, which fails without needing a port or a network.
#
# WHY IT IS SEPARATE. Measured: removing the `if [ "$DELIVERED" -ne 1 ]` guard at
# alert.sh:276 makes a failed file delivery exit 0, print "DELIVERED ... cooldown 900s",
# and WRITE the cooldown. The webhook block above was no protection -- it took its
# "something is listening" skip branch and reported nothing.
BADFILE="$WORK/badfile"
rm -rf "$BADFILE"; mkdir -p "$BADFILE"
badfile_out=$(ALERT_COOLDOWN_SECONDS=900 ALERT_STATE_DIR="$BADFILE" \
    ALERT_SINK_FILE="$WORK/does-not-exist-dir/sink" \
    sh "$REPO/tools/alert/alert.sh" --alert ledger_drift --observed "x" 2>&1)
badfile_rc=$?
[ "$badfile_rc" -ne 0 ] || fail "a FILE-channel delivery that could not be written exited 0. A failed delivery that reports success is the defect this check exists for"
case "$badfile_out" in
    *"DELIVERED"*)
        case "$badfile_out" in
            *"NOT delivered"*) : ;;
            *) fail "a failed file delivery printed DELIVERED without saying the alert was NOT delivered: $badfile_out" ;;
        esac
        ;;
esac
if [ -f "$BADFILE/ledger_drift.last" ]; then
    fail "a FAILED file delivery recorded a cooldown, so the incident is silenced for the whole window and will not be retried"
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
PROBE="$REPO/tools/alert/probe.sh"
ALERT_CHECK="$REPO/tools/alert/check-alerts.sh"
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

    # -----------------------------------------------------------------------
    # AND THE COVERAGE COUNT THE PROSE STATES, which nothing compared.
    # -----------------------------------------------------------------------
    # `docs/launch-checklist.md` and `todo.md` both publish "10 of its 11 alerts
    # are `covered`", and the checklist's own text records that this sentence was
    # WRONG once before ("This line said '9 of 10' until it was measured") and
    # gives the reason it went stale: "nothing tied the sentence to the file".
    # That reason is still true of the replacement figure - MEASURED: changing
    # both files to "11 of its 11" leaves this gate at exit 0, so the number the
    # checklist offers as evidence of alert coverage is checked by nothing.
    #
    # Both figures are derivable from the file this block already parses, so the
    # comparison is exact rather than a floor. Every document must agree with
    # alerts.tsv: a sentence citing this file as its evidence while disagreeing
    # with it is the failure this whole section exists to catch.
    COVERED=$(grep -v '^#' "$TSV" | grep -v '^id' | cut -f6 | grep -c '^covered$')
    for DOC in "$REPO/docs/launch-checklist.md" "$REPO/todo.md"; do
        if [ ! -f "$DOC" ]; then
            fail "cannot read $DOC, so the published alert-coverage count was not compared"
            continue
        fi
        REL="${DOC#"$REPO"/}"
        # `tr` first, so a sentence broken across two lines is still one string - and the CR is
        # STRIPPED, because this repository is CRLF (130 text files) and `tr '\n' ' '` leaves the
        # `\r` behind. MEASURED: without the second `tr`, the checklist's sentence arrives as
        # "10 of its 11^M alerts are `covered`", so the space between "11" and "alerts" is actually
        # a carriage return and the pattern below matches nothing. todo.md has the same sentence on
        # ONE line, so it matched anyway - which is exactly how one of two documents gets compared
        # while the other silently is not.
        #
        # THE PATTERN STOPS AT THE PHRASE, not at the end of the sentence. The checklist writes
        # ``are `covered`**`` (a bold marker closes right after the term) and todo.md writes
        # ``are `covered```, so anchoring on the closing backtick would miss one of them.
        STATED=$(tr '\n' ' ' < "$DOC" | tr -d '\r' | grep -o '[0-9][0-9]* of its [0-9][0-9]* alerts are `covered' | head -1)
        if [ -z "$STATED" ]; then
            fail "$REL no longer states an 'N of its M alerts are covered' figure. Either the sentence moved or it was deleted - and this block exists because that sentence carries the only evidence the checklist offers for alert coverage."
            continue
        fi
        S_COVERED=$(printf '%s' "$STATED" | sed 's/^\([0-9][0-9]*\) of its.*/\1/')
        S_TOTAL=$(printf '%s' "$STATED" | sed 's/^[0-9][0-9]* of its \([0-9][0-9]*\) alerts.*/\1/')
        if [ "$S_COVERED" != "$COVERED" ] || [ "$S_TOTAL" != "$D_COUNT" ]; then
            fail "$REL publishes '$STATED' but tools/alert/alerts.tsv holds $COVERED covered of $D_COUNT definitions. The checklist cites that file as the evidence the alerts are checked; a count that disagrees with it is a coverage claim nothing supports."
        fi
        echo "alert-check:   $REL states $STATED, which matches alerts.tsv"
    done
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


#   WHAT probe.sh ACTUALLY COVERS vs WHAT check-alerts.sh CLAIMS IT COVERS.
#
#   Two files describe the same fact - which alerts are automated - and they did not
#   agree. check-alerts.sh said "Three of the eight are covered by tools/alert/probe.sh"
#   and annotated three; probe.sh marked SEVEN covered. Four went unmentioned, and two
#   of them (all_providers_unhealthy, db_disk) were listed as though a metrics backend
#   were the gap to close, when probe.sh has been reading the operator metrics route
#   for them since the ServerErrorCounter work landed.
#
#   An operator reading that block would have gone looking for a backend that already
#   exists, and relay_5xx - the ONE real gap - was buried among four already closed.
#   That is the worst shape for a coverage list: it makes closed items look open, so
#   the open one is not acted on.
#
#   Read from probe.sh itself rather than from a list kept here, because a
#   hand-maintained count of what another file covers is the same bug twice over.
COVERED_BY_PROBE=$(grep -o '"[a-z_0-9]*" "covered"' "$PROBE" | sed 's/"//g; s/ covered//' | sort -u)

# The fixture guard: if probe.sh could not be read, COVERED_BY_PROBE is empty and every
# comparison below would vacuously pass, which is the failure this check exists to stop.
if [ -z "$COVERED_BY_PROBE" ]; then
    fail "could not read the covered alerts from $PROBE - the comparison below did not happen"
else
    for id in $COVERED_BY_PROBE; do
        grep -q "$id.*COVERED BY tools/alert/probe.sh" "$ALERT_CHECK" || {
            fail "check-alerts.sh does not say that $id is COVERED BY tools/alert/probe.sh, but probe.sh marks it covered. An alert that is automated but listed as needing a metrics backend sends an operator to build one that already exists."
        }
    done
fi

#   alerts.tsv's COVERAGE COLUMN vs WHAT probe.sh ACTUALLY COVERS.
#
#   The third leg of the same chain. observability.md and alerts.tsv are already
#   compared above; probe.sh is compared to check-alerts.sh by the guard above. This
#   closes the loop between the machine-readable DEFINITION and the thing that runs.
#
#   A coverage column that is wrong is worse than a missing one: an operator reading
#   it either trusts an alert that does not fire or stops watching one that does. The
#   rows are:
#
#     7 covered by probe.sh    (the operator metrics route, the log counters, the
#                               external /health and relay probes)
#     3 covered by the SQL path in check-alerts.sh (ledger drift, negative balance,
#                               stranded hold)
#     1 genuinely uncovered    (relay_5xx, which needs a metrics backend)
#
#   So the two independent statements - the tsv's column and probe.sh's own list -
#   must agree about which is which, and probe.sh names its single gap in its output.
COVERED_TSV=$(grep -v '^#' "$TSV" | grep -v '^id' | awk -F'	' '$6 == "covered" {print $1}' | sort -u)
NEEDS_METRICS_TSV=$(grep -v '^#' "$TSV" | grep -v '^id' | awk -F'	' '$6 == "needs-metrics" {print $1}' | sort -u)
COVERED_PROBE=$(grep -o '"[a-z_0-9]*" "covered"' "$PROBE" | sed 's/"//g; s/ covered//' | sort -u)
# The trailing quote matters: the line in probe.sh is an echo, so the captured
# text ends in a double quote, the value came out as relay_5xx-with-a-quote, and this
# guard failed on its own output.
# @ as the sed delimiter, NOT /: the pattern contains tools/alert, and a bare
# slash inside a slash-delimited s command silently ends the pattern early. The first
# version of this line did exactly that and the guard failed on its own empty output.
GAP_PROBE=$(sed -n 's@^.*not checked by anything in tools/alert yet: @@p' "$PROBE" | tr -d ' ' | tr -d '"' | tr ',' ' ' | sort -u)

if [ -z "$COVERED_TSV" ] || [ -z "$COVERED_PROBE" ] || [ -z "$GAP_PROBE" ]; then
    fail "could not read the coverage columns from $TSV or the covered list from $PROBE (tsv_covered=$COVERED_TSV probe_covered=$COVERED_PROBE probe_gap=$GAP_PROBE) - the comparison below did not happen"
else
    for id in $COVERED_PROBE; do
        printf '%s
' "$COVERED_TSV" | grep -qx "$id" || {
            fail "probe.sh marks $id covered, but alerts.tsv does not. A coverage column an operator reads is a claim about whether an alert fires; if the two files disagree, one of them is lying and nobody can tell which."
        }
    done
    for id in $GAP_PROBE; do
        printf '%s
' "$NEEDS_METRICS_TSV" | grep -qx "$id" || {
            fail "probe.sh names $id as not covered by anything, but alerts.tsv does not mark it needs-metrics. An alert nothing checks must be the one the table singles out, or it is invisible."
        }
    done
    for id in $COVERED_TSV; do
        printf '%s
' "$COVERED_PROBE" | grep -qx "$id" && continue
        # Not covered by the prober: it must be one of the SQL-path alerts, which
        # check-alerts.sh runs against the database.
        case "$id" in
            ledger_drift | balance_negative | stranded_hold) ;;
            *)
                fail "alerts.tsv marks $id covered, but neither probe.sh nor the known SQL-path alerts account for it. Either a check was removed and the table was not updated, or a new alert was added to the table with nothing implementing it."
                ;;
        esac
    done
fi

if [ "$FAILED" -ne 0 ]; then
    echo "alert-check: the alert delivery contract is BROKEN (see above)" >&2
    exit 1
fi

echo "alert-check: OK - alerts deliver, throttling is exit 1 not 0, and a failed delivery never silences its own retry"
exit 0