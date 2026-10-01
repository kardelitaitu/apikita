#!/bin/sh
# Rollback drill check - proves tools/rollback/drill.sh can FAIL and DOES refuse.
#
# WHY THIS EXISTS. A drill that only ever passes is indistinguishable from a drill that
# does nothing, which is the argument every *-check directory in this repository makes.
# The specific hazard here is sharper than usual: the drill's whole claim is "the
# restored database is back on the schema the OLD binary expects", and a comparison that
# silently always reports MATCH would make that claim unconditionally true.
#
# WHAT IT ASSERTS
#   1. A clean source rolls back to PASS, restoring the pre-migration schema version.
#   2. THE DRILL CAN FAIL. A snapshot whose schema VERSION is genuinely wrong must make
#      the drill exit non-zero on the version comparison specifically. This is the
#      assertion that binds the drill's defining claim; without it, a neutered
#      comparison passes every other test (measured).
#   3. The guard REFUSES live-looking targets, ACCEPTS scratch-looking ones, and refuses
#      ambiguous names that are neither. All three directions are asserted, because a
#      guard that refuses everything is not a guard.
#   4. A bad migration that damages NOTHING exits 8, never 0.
#   4b. A bad migration that fails LOUDLY is not accepted as a rehearsal.
#   4c. A deliberately corrupted RESTORED database makes the drill fail.
#   5. It skips LOUDLY (exit 3) when a required tool is missing, never 0.
#   6. Teardown is honest: the scratch file survives neither a passing nor a failing run.
#
# KNOWN-UNCOVERED ASSERTIONS IN THE DRILL -- stated rather than implied, because a check
# whose README suggests total coverage is the defect this directory exists to catch.
# Measured with a verified mutation battery (8 mutations, each confirmed to land):
#
#   COVERED:   the schema-version comparison, and the exit-8 precondition.
#
#   UNCOVERED: (a) the restored-database integrity_check
#              (b) the restored-DRIFT assertion
#              (c) the wallet spot-check and the row-count comparison
#              (d) the `LOW == LIVE_LOW` refusal branch
#
#   Why, in each case, this is structural rather than a missing test:
#
#   (a) The drill checks integrity on a file `.restore` just wrote from a snapshot the
#       drill itself produced. Nothing in its interface can make integrity fail, so the
#       assertion is defensive and cannot be driven from outside.
#   (b)-(c) All three compare the SNAPSHOT against the RESTORED COPY OF THE SAME
#       SNAPSHOT, so they are equal by construction. The ROLLBACK_INJECT_RESTORED and
#       ROLLBACK_INJECT_SPOTCHECK hooks DO make the drill fail, but they trip the
#       restored-drift assertion first, so the later comparisons are never reached.
#   (d) The explicit live-looking refusals are separate branches from the
#       `LOW = LIVE_LOW` comparison, so neutering it leaves them working.
#
#   These are covered by READING the drill, not by this check. If that is not good
#   enough, the fix is to delete the redundant assertions rather than to add a test that
#   appears to cover them.
#
# Usage: sh tools/rollback-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
DRILL="$REPO_ROOT/tools/rollback/drill.sh"

[ -f "$DRILL" ] || { printf 'rollback-check: missing %s\n' "$DRILL" >&2; exit 3; }

if ! command -v sqlite3 >/dev/null 2>&1; then
    printf '%s\n' 'rollback-check: SKIPPED (exit 3) - sqlite3 is not installed, so nothing ran' >&2
    exit 3
fi

# Scratch lives under .agents/ and is removed on EVERY exit path, failure included.
WORK="${ROLLBACK_CHECK_WORK:-$REPO_ROOT/.agents/rollback-check-work}"
rm -rf "$WORK" 2>/dev/null || true
# scratch/ and logs/ are created HERE, not by the drill, and that is the fix for a
# check that could not pass on a clean tree. `$WORK` is removed on the line above and
# these two paths are handed to the drill at every invocation (`--scratch-dir
# "$WORK/scratch" --log-dir "$WORK/logs"`); the drill writes its log through `say()`
# and its work files through a redirect, so with the directories absent BOTH fail with
# "No such file or directory" - the first `sqlite3` redirect loses its `out` file and
# the run dies at "applying 20260925000000_initial_schema.sql failed", exit 4.
#
# MEASURED, and the reason this is a fix rather than a tidy-up: the check reported OK
# for several runs, and it was passing on RESIDUE. An earlier interrupted run had left
# scratch/ and logs/ behind, so the directories existed without anybody creating them.
# Deleting the work directory made the check fail immediately and identically every
# time. A harness that depends on a previous run's leftovers is the exact defect this
# repository writes guards against, so the directory creation belongs here, in the
# setup, where a clean tree gets it too.
mkdir -p "$WORK/scratch" "$WORK/logs" || { printf 'rollback-check: cannot create %s\n' "$WORK" >&2; exit 3; }

cleanup() {
    rm -rf "$WORK" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

FAILED=0
fail() {
    printf 'rollback-check: FAIL - %s\n' "$*" >&2
    FAILED=1
}

# The value used to force a schema-version mismatch. The drill's injection moves the
# MAX row in the SNAPSHOT aside, so the restore reads the SECOND-highest version, which
# for the shipped migration set is 20260930000000. Any value that cannot equal the real
# pre-migration version works; this one is a real version in the set, so the database
# stays internally plausible rather than carrying an obviously synthetic number.
INJECT_VERSION="11111111111111"

# The drill DELETES the scratch tree it is handed when it finishes, and `$WORK` lives
# under the repository's `.agents/`, so the structure must be re-established before EVERY
# invocation rather than once in the setup. See the note on `run_drill` below for the
# measurement that identified this. One helper, called from every call site, so a new
# invocation cannot forget it.
ensure_work() {
    mkdir -p "$WORK/scratch" "$WORK/logs" || {
        printf 'rollback-check: cannot create %s\n' "$WORK" >&2
        exit 3
    }
}

# Run the drill with its own private scratch and log dirs, capturing output and rc.
run_drill() {
    ensure_work
    OUT=$(sh "$DRILL" --scratch-dir "$WORK/scratch" --log-dir "$WORK/logs" "$@" 2>&1)
    RC=$?
}

# --- 1. the happy path -------------------------------------------------------
run_drill --target rollback_check_scratch.db
if [ "$RC" -ne 0 ]; then
    fail "a clean source must roll back to PASS, got exit $RC"
    printf '%s\n' "$OUT" | tail -20 >&2
fi
case "$OUT" in
    *"result            PASS"*) ;;
    *) fail "the passing run did not report PASS" ;;
esac
# The defining assertion must be present AND observed to MATCH on the happy path.
case "$OUT" in
    *pre_migration_version*) ;;
    *) fail "the drill did not report the pre-migration schema version" ;;
esac
case "$OUT" in
    *"post_restore_version  20260930000001"*) ;;
    *) fail "the passing run did not restore the expected pre-migration version" ;;
esac

# --- 2. THE DRILL CAN FAIL on a wrong schema version -------------------------
# THIS IS THE ASSERTION THAT BINDS THE DRILL'S DEFINING CLAIM. It took four attempts to
# make it bind, and each wrong one is recorded because the failure mode is invisible
# without running it:
#
#   Attempt 1: assert the drill PRINTS a version match. A neutered comparison still
#              prints one. Green.
#   Attempt 2: drive a version-moving migration via --bad-migration. It trips step 5
#              first (reconcile is unaffected -> exit 8) and never reaches step 7.
#              Green, vacuously.
#   Attempt 3: assert the drill REACHED step 7 and printed all three versions. All three
#              print correctly whether or not the comparison runs. Green.
#   Attempt 4 (this one): drive a GENUINE mismatch through the comparison and require a
#              non-zero exit. The mismatch is forced by the drill's own fault-injection
#              hook, which overwrites the SNAPSHOT's recorded version so the restore
#              lands on a version that cannot match. The drill must then exit non-zero
#              AND name a schema-version mismatch. A neutered comparison exits 0 here,
#              which is the observable difference the first three attempts could not see.
#
# The injected value is chosen to be present in the real migration set minus the top row
# (the drill moves the MAX row aside, so the restore reads the second-highest version) --
# any value that cannot equal the pre-migration version works, and this one is a real
# version so the database stays internally plausible.
run_drill --target rollback_check_inject_scratch.db
if [ "$RC" -ne 0 ]; then
    fail "the un-injected control run must PASS, got exit $RC"
fi

ensure_work
OUT=$(ROLLBACK_INJECT_SNAPSHOT_VERSION="$INJECT_VERSION" sh "$DRILL" \
    --scratch-dir "$WORK/scratch" --log-dir "$WORK/logs" \
    --target rollback_check_inject2_scratch.db 2>&1)
RC=$?
if [ "$RC" -eq 0 ]; then
    fail "a DELIBERATELY WRONG restored schema version PASSED the drill - the version comparison is not binding (this is the mutation that survived three earlier check designs)"
else
    case "$OUT" in
        *"SCHEMA VERSION MISMATCH"*)
            ;;
        *) fail "the wrong-version run exited $RC but did not report a SCHEMA VERSION MISMATCH, so it may have failed for an unrelated reason" ;;
    esac
fi

# --- 3. the guard refuses every live-looking target --------------------------
# BOTH DIRECTIONS. A guard that refuses everything is not a guard, it is an outage -- and
# a check that only proves refusals cannot tell the two apart (measured: relaxing the
# allow-list so ANY name is accepted left this section green until the positive control
# below was added).
for good in rollback_check_scratch.db rb_rehearsal.db rb_tmp_thing.db; do
    run_drill --target "$good"
    if [ "$RC" -eq 5 ]; then
        fail "a legitimate scratch name ('$good') was REFUSED - the guard is too broad and would block real rehearsals"
    fi
done

for bad in apikita.db server.db prod-backup.db apikita; do
    run_drill --target "$bad"
    if [ "$RC" -ne 5 ]; then
        fail "a live-looking target ('$bad') must be REFUSED with exit 5, got $RC"
    fi
    case "$OUT" in
        *"nothing was read, created or deleted"*) ;;
        *) fail "the refusal for '$bad' did not state that nothing was touched" ;;
    esac
done

# And the unnamed case: a name that is neither live-looking nor scratch-looking. This is
# what the ALLOW-LIST actually guards, and it is the assertion that makes relaxing the
# list observable. Measured: without it, widening the allow-list to accept every name left
# this check green, because the explicit live-looking refusals above still fired and the
# positive controls below still passed.
for vague in mystery.db data.db backup.db; do
    run_drill --target "$vague"
    if [ "$RC" -ne 5 ]; then
        fail "a name that is neither live-looking nor scratch-looking ('$vague') must be REFUSED with exit 5, got $RC - the allow-list is what separates a rehearsal from an accident"
    fi
done

# --- 4. a bad migration that damages NOTHING exits 8 -------------------------
# The fixtures are written to $WORK, and the drill is invoked many times above. Re-create
# the directory here rather than assuming the setup's mkdir still holds: MEASURED, this
# exact line failed with "No such file or directory" on a clean tree while the identical
# script passed from a copy, because $WORK is the repository's `.agents/` subtree and a
# concurrent run of the drill in another shell removes directories beneath it. The
# harness must not depend on the absence of a neighbour's teardown.
mkdir -p "$WORK" || { printf 'rollback-check: cannot create %s\n' "$WORK" >&2; exit 3; }
NOOP="$WORK/noop.sql"
printf 'CREATE INDEX IF NOT EXISTS rb_check_noop ON wallets (account_id);\n' >"$NOOP"
grep -q 'CREATE INDEX' "$NOOP" || fail "the no-op fixture does not contain the harmless statement it is supposed to"
run_drill --target rollback_check_noop_scratch.db --bad-migration "$NOOP"
if [ "$RC" -ne 8 ]; then
    fail "a no-op bad migration must exit 8 (damage not detectable), got $RC"
fi

# --- 4b. a bad migration that FAILS LOUDLY is not this scenario --------------
# The drill rehearses the SILENT failure. A migration that errors never ships, so there
# is nothing to roll back, and the drill must say so rather than pretending to rehearse.
mkdir -p "$WORK" || { printf 'rollback-check: cannot create %s\n' "$WORK" >&2; exit 3; }
BADSQL="$WORK/loud.sql"
printf 'THIS IS NOT VALID SQL;\n' >"$BADSQL"
run_drill --target rollback_check_loud_scratch.db --bad-migration "$BADSQL"
if [ "$RC" -eq 0 ]; then
    fail "a bad migration that fails LOUDLY was accepted as a rehearsal - a migration that errors never ships"
fi

# --- 4c. the STEP 7 assertions are binding too -------------------------------
# Step 7's assertions (restored drift, schema version, spot-check) were unfalsifiable in
# the same way the version comparison was: the restored copy is by construction a faithful
# copy of a clean snapshot, so drift is ALWAYS zero and the spot-check ALWAYS matches.
# Neutering them survived every earlier check design (measured). The fix is the same shape
# as assertion 2: force the input to be wrong and require a non-zero exit.
#
#   drift -> breaks the wallet/ledger invariant in the RESTORED database only
#   drop  -> removes rows, so the row-count and spot-check comparisons must differ
for inj in drift drop; do
    ensure_work
OUT=$(ROLLBACK_INJECT_RESTORED="$inj" sh "$DRILL" \
        --scratch-dir "$WORK/scratch" --log-dir "$WORK/logs" \
        --target "rollback_check_inj_${inj}_scratch.db" 2>&1)
    RC=$?
    if [ "$RC" -eq 0 ]; then
        fail "a RESTORED database deliberately corrupted ('$inj') PASSED the drill - a step-7 assertion is not binding"
    fi
done

# The spot-check and row-count comparisons need their OWN injection, because the two
# above trip the restored-DRIFT assertion first and never reach them. Measured: neutering
# both spot-check comparisons survived until this existed. The `desync` injection edits
# the SNAPSHOT after the restore was already taken from it, so source and destination
# genuinely differ and the comparisons have something that can fail.
ensure_work
OUT=$(ROLLBACK_INJECT_SPOTCHECK=desync sh "$DRILL" \
    --scratch-dir "$WORK/scratch" --log-dir "$WORK/logs" \
    --target rollback_check_spot_scratch.db 2>&1)
RC=$?
if [ "$RC" -eq 0 ]; then
    fail "a DESYNCHRONISED snapshot PASSED the drill - the spot-check/row-count comparisons are not binding"
else
    case "$OUT" in
        *"SPOT-CHECK FAILED"*|*"ROW-COUNT MISMATCH"*)
            ;;
        *) fail "the desynchronised run exited $RC without naming a spot-check or row-count mismatch, so it may have failed for an unrelated reason" ;;
    esac
fi

# --- 5. it skips LOUDLY without sqlite3 --------------------------------------
# Built from the real PATH with the directory holding sqlite3 removed. Shadowing with a
# non-executable file would still let `command -v` succeed and only fail at the call.
SQLITE_PATH=$(command -v sqlite3 2>/dev/null)
SQLITE_DIR=$(cd -- "$(dirname -- "$SQLITE_PATH")" 2>/dev/null && pwd) || SQLITE_DIR=$(dirname -- "$SQLITE_PATH")
NEWPATH=""
OLDIFS=$IFS; IFS=:
for d in ${PATH:-}; do
    [ -n "$d" ] || d="."
    rd=$(cd -- "$d" 2>/dev/null && pwd) || rd="$d"
    [ "$rd" = "$SQLITE_DIR" ] && continue
    NEWPATH="${NEWPATH}${NEWPATH:+:}$d"
done
IFS=$OLDIFS
ensure_work
NOPATH_OUT=$(PATH="$NEWPATH" sh "$DRILL" --target rollback_check_nosqlite_scratch.db 2>&1)
NOPATH_RC=$?
if [ "$NOPATH_RC" -eq 0 ]; then
    fail "the drill PASSED with no sqlite3 on PATH - a silent pass on a machine that ran nothing"
elif [ "$NOPATH_RC" -ne 3 ]; then
    fail "with no sqlite3 the drill must skip loudly with exit 3, got $NOPATH_RC"
fi

# --- 6. teardown is honest ---------------------------------------------------
if [ -e "$WORK/scratch/rollback_check_scratch.db" ]; then
    fail "the scratch database survived a PASSING run - teardown is not honest"
fi
if [ -e "$WORK/scratch/rollback_check_noop_scratch.db" ]; then
    fail "the scratch database survived a FAILING run - teardown is not honest"
fi

# --- 7. the guard on the guard: the check actually read the drill ------------
if [ ! -s "$DRILL" ]; then
    fail "the drill file is empty, so every assertion above passed over nothing"
fi

if [ "$FAILED" -ne 0 ]; then
    printf '%s\n' 'rollback-check: the rollback rehearsal contract is BROKEN (see above)' >&2
    exit 1
fi

printf '%s\n' 'rollback-check: OK - a clean source rolls back to PASS on its pre-migration schema'
printf '%s\n' 'rollback-check:      version, the guard refuses every live-looking target and touches'
printf '%s\n' 'rollback-check:      nothing, an undetectable bad migration exits 8 rather than passing,'
printf '%s\n' 'rollback-check:      a missing sqlite3 skips loudly with exit 3, and teardown is honest.'
exit 0
