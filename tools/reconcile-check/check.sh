#!/bin/sh
# apikita reconciliation-gate check.
#
# WHY THIS EXISTS. `tools/reconcile/reconcile.sh` is the LAUNCH GATE 2 money check, and
# docs/launch-checklist.md:277 calls it "the single best guard against silently wrong
# money". It is mounted into the scheduler container and thus REACHABLE from CI, which
# is why a path grep for "reconcile" finds matches and looks covered - but no CI step
# ever RAN it. A MOUNTED TOOL IS NOT A RUN TOOL.
#
# A gate that cannot fail is worse than no gate, because the tick beside it means
# something. So this check proves the DIRECTION that matters:
#
#   A DRIFTED DATABASE MUST FAIL. A wallet whose balance disagrees with the sum of its
#   ledger rows is exactly what the gate exists to catch, and it must exit 1 and NAME
#   the account - a count nobody can act on is half an alert.
#
#   AND A CONSISTENT ONE MUST PASS. Without this control, "it exited 1" would be
#   satisfied by a script that always exits 1, which is a different kind of useless.
#
# It also walks the documented exit codes, because collapsing them would send an
# operator to the wrong fix: 1 = drift, 2 = bad DSN, 5 = stranded hold, 6 = no such
# database file. Each is asserted separately.
#
# MUTATION-TESTED, against the SQL itself. Three mutations are all caught: dropping the
# NO-WALLET-ROW arm (`HAVING w.account_id IS NULL`), flipping the drift comparison, and
# weakening FULL OUTER JOIN to LEFT JOIN. A fourth case - a gate replaced by one that
# exits 1 unconditionally - is caught by the CONSISTENT-MUST-PASS control, which is what
# that control is for.
#
# ONE TRAP WORTH RECORDING, because it made a whole round of measurements meaningless:
# an early mutation script copied the file it was about to mutate as its "original"
# restore point, so after one run the SAVED copy was itself mutated and every later
# result was measured against a broken gate. Restore with `git checkout --` instead of a
# copy. The assertions here also GUARD THEIR OWN FIXTURE (e.g. counting the seeded orphan
# rows) so a seed that silently does nothing fails loudly rather than passing vacuously.
#
# Skips LOUDLY (exit 3) when sqlite3 is absent, never 0.
#
# Usage: sh tools/reconcile-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
WORK="${TMPDIR:-/tmp}/apikita-reconcile-check-$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
mkdir -p "$WORK" || { echo "reconcile-check: cannot create $WORK" >&2; exit 2; }

command -v sqlite3 >/dev/null 2>&1 || {
    echo "reconcile-check: SKIPPED - sqlite3 is not on PATH, so the gate was NOT verified" >&2
    exit 3
}

DB="$WORK/ledger.db"
DSN="sqlite://$DB"

# The minimal shape the gate reads. A full migration would be more faithful but slower,
# and these properties - drift detection, the hold predicate, the exit codes - do not
# depend on the rest of the schema.
sqlite3 "$DB" "
  CREATE TABLE accounts (id TEXT PRIMARY KEY, pb_user_id TEXT);
  CREATE TABLE wallets (account_id TEXT PRIMARY KEY, balance_idr INTEGER NOT NULL, updated_at TEXT);
  CREATE TABLE ledger (id INTEGER PRIMARY KEY, account_id TEXT, delta_idr INTEGER, reason TEXT, ref TEXT, balance_after INTEGER, created_at TEXT);
  INSERT INTO accounts VALUES ('acct-1', 'pb_1');
  INSERT INTO wallets VALUES ('acct-1', 5000, '2026-01-01T00:00:00+00:00');
  INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
    VALUES ('acct-1', 3000, 'topup', 'ref_1', 3000, '2026-01-01T00:00:00+00:00');
" || { echo "reconcile-check: could not seed the ledger" >&2; exit 3; }

FAILED=0
fail() { echo "reconcile-check: FAIL - $1" >&2; FAILED=1; }

run_gate() {
    env DATABASE_URL="$DSN" RECONCILE_DATABASE_URL="$DSN" \
        sh "$REPO/tools/reconcile/reconcile.sh" 2>&1
}

# --- drift MUST fail, and must NAME the account -----------------------------
out=$(run_gate); rc=$?
if [ "$rc" -ne 1 ]; then
    fail "a wallet that disagrees with its ledger must exit 1 (drift), got $rc"
fi
case "$out" in
    *acct-1*) ;;
    *) fail "the gate did not NAME the drifting account; a count an operator cannot act on is half an alert" ;;
esac
if printf '%s\n' "$out" | grep -q "5000|3000"; then
    : # both figures present, the operator can see the size of the drift
else
    fail "the gate did not report both the cached balance and the ledger sum"
fi

# --- the SECOND arm: ledger money with NO wallets row -----------------------
# Both the script README and docs/launch-checklist.md call this out as its OWN finding,
# and it is the quieter of the two: there is no wallet to compare against, so an
# account whose ledger money arrived without a wallet row is invisible to any check that
# anchors only on wallets. Dropping this arm was MISSED by the first version of this
# check, which is why it is asserted here rather than assumed.
# FIRST make acct-1 consistent again, so the ONLY thing that can trigger drift below is
# the wallet-less account. Without this, acct-1's own drift would satisfy the assertion
# and the NO-WALLET-ROW arm could be deleted without the check noticing - which is
# exactly what a mutation test caught.
sqlite3 "$DB" "UPDATE wallets SET balance_idr = 3000 WHERE account_id = 'acct-1';"
out=$(run_gate); rc=$?
if [ "$rc" -ne 0 ]; then
    fail "the fixture was not made consistent before the orphan test, so the assertion below would not isolate the NO-WALLET-ROW arm (got exit $rc)"
fi

# The SQL is on ONE logical line via backslash continuations: without them bash ends
# the argument at the newline and sqlite3 never receives the INSERTs, so the assertion
# below would silently test nothing. That mistake was in the first version of this file.
sqlite3 "$DB" "\
  INSERT INTO accounts VALUES ('acct-orphan', 'pb_orphan'); \
  INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) \
    VALUES ('acct-orphan', 7000, 'topup', 'ref_orphan', 7000, '2026-01-01T00:00:00+00:00');"

# If the seed did nothing, the assertion below would pass vacuously. Prove it landed.
if [ "$(sqlite3 "$DB" "SELECT COUNT(*) FROM ledger WHERE account_id = 'acct-orphan';")" != "1" ]; then
    fail "the fixture did not insert the wallet-less ledger account, so the NO-WALLET-ROW assertion would be vacuous"
fi

out=$(run_gate); rc=$?
if [ "$rc" -ne 1 ]; then
    fail "ledger money with NO wallets row must be drift (exit 1), got $rc"
fi
case "$out" in
    *"NO WALLET ROW"*) ;;
    *) fail "the gate did not mark the wallet-less account as NO WALLET ROW" ;;
esac
sqlite3 "$DB" "DELETE FROM ledger WHERE account_id = 'acct-orphan'; DELETE FROM accounts WHERE id = 'acct-orphan';"

# --- the CONTROL: a consistent ledger MUST pass -----------------------------
sqlite3 "$DB" "UPDATE wallets SET balance_idr = 3000 WHERE account_id = 'acct-1';"
out=$(run_gate); rc=$?
if [ "$rc" -ne 0 ]; then
    fail "a CONSISTENT wallet/ledger must exit 0, got $rc. Without this control a gate that always fails would pass the check above"
fi

# --- a stranded hold is its OWN code, not drift -----------------------------
sqlite3 "$DB" "
  INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
    VALUES ('acct-1', -1000, 'usage', 'reserve_check_stranded', 2000, '2020-01-01T00:00:00+00:00');
  UPDATE wallets SET balance_idr = 2000 WHERE account_id = 'acct-1';
"
out=$(run_gate); rc=$?
if [ "$rc" -ne 5 ]; then
    fail "a stranded reservation hold must exit 5 (distinct from drift), got $rc"
fi

# --- the code-shape refusals -----------------------------------------------
out=$(env DATABASE_URL="postgres://nope" RECONCILE_DATABASE_URL="postgres://nope" \
    sh "$REPO/tools/reconcile/reconcile.sh" 2>&1); rc=$?
[ "$rc" -eq 2 ] || fail "a non-sqlite DSN must exit 2, got $rc"

out=$(env DATABASE_URL="sqlite://$WORK/absent.db" RECONCILE_DATABASE_URL="sqlite://$WORK/absent.db" \
    sh "$REPO/tools/reconcile/reconcile.sh" 2>&1); rc=$?
[ "$rc" -eq 6 ] || fail "a missing database file must exit 6, got $rc"


# ---------------------------------------------------------------------------
# The printed scheduling claim must match what the scheduler actually does.
# ---------------------------------------------------------------------------
# WHY THIS IS HERE. reconcile.sh prints a HOLD SWEEP block on EVERY run, and that
# block used to say "Nothing schedules it yet (no CI workflow, no compose service)".
# That had been FALSE since the detector was wired, and -- the reason this is a
# guard and not a one-off fix -- THE CLAIM IS EMITTED BY THE SCHEDULED RUN ITSELF.
# The nightly log therefore contained, a few lines apart:
#
#   maintenance: job hold-sweep: OK - 0 stranded holds older than 900s
#   reconcile:   Nothing schedules it yet (no CI workflow, no compose service).
#
# An operator reading that is told to run by hand a job that just ran, or concludes
# the automation is broken and stops trusting the lines around it. docs/ and
# tools/reconcile/README.md both repeated the claim, so code and doc agreed with
# each other and disagreed with the system.
#
# The claim is MECHANICAL, so a check can hold the two together: if the entrypoint
# wires run_hold_sweep into the nightly job list, no tool may tell the operator it
# is unscheduled, and vice versa. Neither side is asserted alone -- the assertion is
# that they AGREE, so it stays true whichever way someone changes it.
ENTRYPOINT="${ENTRYPOINT:-$REPO/.docker/maintenance/entrypoint.sh}"
if [ ! -f "$ENTRYPOINT" ]; then
    fail "cannot find the maintenance entrypoint at $ENTRYPOINT, so the scheduling claim cannot be checked"
else
    # What the scheduler does. run_wired_jobs is the nightly sequence.
    if sed -n '/^run_wired_jobs()/,/^}/p' "$ENTRYPOINT" | grep -q 'run_hold_sweep'; then
        SCHEDULED=yes
    else
        SCHEDULED=no
    fi

    # What the tool says. Both the script's printed text and its README.
    CLAIMS_UNSCHEDULED=no
    for f in "$REPO/tools/reconcile/reconcile.sh" "$REPO/tools/reconcile/README.md"; do
        if [ -f "$f" ] && grep -qi 'nothing schedules it' "$f"; then
            CLAIMS_UNSCHEDULED=yes
        fi
    done

    if [ "$SCHEDULED" = yes ] && [ "$CLAIMS_UNSCHEDULED" = yes ]; then
        fail "run_wired_jobs schedules run_hold_sweep, but reconcile.sh/README still tell the operator \"nothing schedules it\" - and that text is printed BY the scheduled run, next to the job's own success line"
    fi
    if [ "$SCHEDULED" = no ] && [ "$CLAIMS_UNSCHEDULED" = no ]; then
        fail "run_wired_jobs does NOT schedule run_hold_sweep, yet reconcile.sh/README no longer say so - the operator would be told the sweep is automatic when it is not"
    fi

    # Guard the fixture: if the entrypoint were not read at all, SCHEDULED would be
    # "no" and the second branch above would be the only live one.
    if ! grep -q 'run_wired_jobs()' "$ENTRYPOINT"; then
        fail "run_wired_jobs was not found in $ENTRYPOINT - the scheduling claim was not actually compared"
    fi
fi

if [ "$FAILED" -ne 0 ]; then
    echo "reconcile-check: the reconciliation gate is BROKEN (see above)" >&2
    exit 1
fi

echo "reconcile-check: OK - the gate detects drift and names the account, passes a consistent ledger, and keeps 1/2/5/6 distinct"
exit 0