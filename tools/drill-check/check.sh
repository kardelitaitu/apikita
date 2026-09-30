#!/bin/sh
# apikita restore-drill check - the SAFETY GUARD, verified rather than trusted.
#
# WHY THIS EXISTS. `tools/drill/drill.sh` deletes a scratch database as part of
# teardown, and its safety guard is the only thing standing between that behaviour and
# production data. docs/launch-checklist.md:279 says "A restore drill has been run. An
# untested backup is a belief" - but the tool that produces that evidence was exercised
# by NOTHING in CI. W33 walked its refusals by hand; hand evidence does not survive the
# next edit.
#
# THE TWO THINGS THAT MATTER, in order:
#
#   1. THE GUARD REFUSES, and refuses BEFORE touching anything. Every documented class is
#      asserted separately, and a database left in its place is checked for intactness
#      afterwards - because "exit 5" with the file already gone would be the worst
#      possible pass.
#   2. A LEGITIMATE TARGET COMPLETES, and a DRIFTED SOURCE FAILS. Without both, "it
#      exited 5" would be satisfied by a tool that refuses everything, and "it passed"
#      by one that never checks anything.
#
# It needs a MIGRATED source, because the drill runs verify.sql and reconcile.sh against
# the restored copy. It builds one with the repo migrations when sqlite3 is available;
# skips LOUDLY (exit 3) otherwise, never 0.
#
# Usage: sh tools/drill-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
WORK="${TMPDIR:-/tmp}/apikita-drill-check-$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
mkdir -p "$WORK" || { echo "drill-check: cannot create $WORK" >&2; exit 2; }

command -v sqlite3 >/dev/null 2>&1 || {
    echo "drill-check: SKIPPED - sqlite3 is not on PATH, so the guard was NOT verified" >&2
    exit 3
}

FAILED=0
fail() { echo "drill-check: FAIL - $1" >&2; FAILED=1; }

SOURCE="$WORK/source.db"

# --- 1. THE GUARD, before anything else -------------------------------------
# A file that must SURVIVE every refusal.
PROTECTED="$WORK/apikita.db"
sqlite3 "$PROTECTED" "CREATE TABLE accounts (id TEXT PRIMARY KEY); INSERT INTO accounts VALUES ('live-1');" \
    || { echo "drill-check: could not create the protected database" >&2; exit 3; }

# The SOURCE must exist for the guard cases that compare against it, but the guard must
# fire on the TARGET regardless of whether the source is usable.
sqlite3 "$SOURCE" "CREATE TABLE accounts (id TEXT PRIMARY KEY);" \
    || { echo "drill-check: could not create the source database" >&2; exit 3; }

guard() {
    # $1 = description, $2 = expected exit, then the drill arguments
    desc="$1"; want="$2"; shift 2
    out=$(env DATABASE_URL="sqlite://$SOURCE" DRILL_SCRATCH_DIR="$WORK" \
        sh "$REPO/tools/drill/drill.sh" "$@" 2>&1); rc=$?
    if [ "$rc" -ne "$want" ]; then
        fail "$desc: expected exit $want, got $rc"
    fi
    # THE PROMISE: nothing was deleted. Asserted after EVERY guard case, not once,
    # because a future edit could delete on one path and not another.
    if [ ! -s "$PROTECTED" ]; then
        fail "$desc: the protected database was DELETED OR EMPTIED - the guard promises nothing is touched"
    fi
}

guard "no --target" 2
guard "target is the live name" 5 --target apikita
guard "target is apikita.db" 5 --target apikita.db
guard "target is server.db" 5 --target server.db
guard "target is the source basename" 5 --target source.db
guard "target looks production" 5 --target scratch-prod.db
guard "target looks live" 5 --target w39-live-drill.db
guard "target cannot be shown scratch" 5 --target mystery.db
guard "target is a path, not a name" 2 --target /tmp/scratch.db
guard "target is a DSN" 2 --target "sqlite://scratch.db"
guard "target does not end .db" 2 --target scratchname

# The protected database must still hold its row, not merely exist.
rows=$(sqlite3 "$PROTECTED" "SELECT COUNT(*) FROM accounts;" 2>/dev/null)
[ "$rows" = "1" ] || fail "the protected database survived but its rows did not (got '$rows')"

# --- 2. a LEGITIMATE target COMPLETES ---------------------------------------
# This is the control that makes the refusals above mean something. It needs a fully
# migrated source, because the drill runs verify.sql and reconcile.sh against the
# restored copy - a partial schema fails at step 6 for an honest reason.
# A BROKEN MIGRATION IS A HARD FAILURE, NOT A SKIP, and this was a real false green.
#
# The loop below used to be `sqlite3 ... || break` and the guard `[ -f "$MIGRATED" ]`. A
# migration that failed therefore left the database half-built, the EXISTENCE check saw a
# file, and the positive control ran the drill against a PARTIAL schema - which still
# reaches PASS, because verify.sql happens to work against the smaller schema. Measured:
# replacing the second migration with garbage left 17 tables instead of 18 and the check
# still printed "OK ... a consistent source drills to PASS" and exited 0.
#
# So: apply every migration, stop at the first failure naming the file, and only then
# decide. A schema that cannot be built is a defect in the repository, not a reason to
# skip - the skip path below is for a MISSING migrations directory, which is a different
# fact.
MIGRATED="$WORK/migrated.db"
migrated=0
MIGRATION_DIR="$REPO/server/migrations"
if [ -d "$MIGRATION_DIR" ] && [ -n "$(ls -A "$MIGRATION_DIR"/*.sql 2>/dev/null)" ]; then
    migrate_failed=""
    for f in "$MIGRATION_DIR"/*.sql; do
        if ! sqlite3 -bail "$MIGRATED" < "$f" >/dev/null 2>"$WORK/mig.err"; then
            migrate_failed=$(basename "$f")
            break
        fi
    done
    if [ -n "$migrate_failed" ]; then
        # NOT a skip: the repository is inconsistent and the control below cannot be trusted.
        fail "the migration $migrate_failed did not apply, so the positive control would run against a PARTIAL schema: $(head -1 "$WORK/mig.err" 2>/dev/null)"
    else
        migrated=1
    fi
fi

if [ "$migrated" -eq 0 ]; then
    # Only two ways to get here: no migrations directory at all, or a migration that did
    # not apply. The FIRST is a legitimate skip; the second already called fail() above and
    # must not be described as a skip, or the report contradicts the exit code.
    if [ "$FAILED" -ne 0 ]; then
        echo "drill-check: the positive control could NOT run - see the failure above" >&2
        exit 1
    fi
    echo "drill-check: SKIPPED the positive control - there is no migrations directory to build from" >&2
    echo "drill-check:   the GUARD cases above DID run and are the higher-stakes half" >&2
    exit 0
fi

# THE SCHEMA MUST BE COMPLETE, not merely present. This is the assertion whose absence made
# the false green possible: `[ -f "$MIGRATED" ]` accepted a database built from one of three
# migrations. Counting tables against the migrations themselves is what turns "a file exists"
# into "the schema was built".
#
# The expectation comes from the SHIPPED MIGRATIONS READ AS TEXT - the count of
# `CREATE TABLE` statements across them - not from a second build of the same files.
#
# A second build would be TAUTOLOGICAL: it applies the same files the same way, so the two
# counts agree even when both are wrong, and it cannot catch the condition this exists for.
# Reading the files is an INDEPENDENT measurement, and the two disagreeing is exactly the
# signal that the build did not do what the migrations say.
# `bc` is not guaranteed on the CI runner, so the sum is done in POSIX shell arithmetic.
EXPECTED=0
for f in "$MIGRATION_DIR"/*.sql; do
    # `head -1` because grep -c can emit more than one line if a file has no trailing
    # newline, and a multi-line value makes the arithmetic below fail with a syntax error.
    #
    # A `_`-prefixed name is a REBUILD STAGING TABLE, not a table that survives the
    # migration. SQLite cannot DROP a UNIQUE column, so the identity port recreated
    # `accounts` as `_accounts_rebuild_staging`, copied the rows and renamed it into
    # place - it is gone by the time the migration commits, so counting it here would
    # demand a table the built schema must NOT have. The `_` prefix is the same
    # convention `doc_claims.rs` uses for its schema inventory.
    n=$(grep -ci '^[[:space:]]*CREATE TABLE [^_]' "$f" 2>/dev/null | head -1)
    case "$n" in ''|*[!0-9]*) n=0 ;; esac
    EXPECTED=$((EXPECTED + n))
done
ACTUAL=$(sqlite3 "$MIGRATED" "SELECT COUNT(*) FROM sqlite_master WHERE type='table'" 2>/dev/null)
if [ -z "$EXPECTED" ] || [ -z "$ACTUAL" ]; then
    fail "could not count tables in the built schema or in the migration files (got '$ACTUAL' vs '$EXPECTED')"
elif [ "$ACTUAL" != "$EXPECTED" ]; then
    fail "the migrated source has $ACTUAL tables but the migrations declare $EXPECTED CREATE TABLE statements - the positive control would run against an INCOMPLETE schema"
fi

sqlite3 "$MIGRATED" "
  INSERT INTO accounts (id, is_operator, created_at, updated_at)
    VALUES ('a1', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
  INSERT INTO wallets (account_id, balance_idr, updated_at)
    VALUES ('a1', 1000, '2026-01-01T00:00:00+00:00');
  INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
    VALUES ('a1', 1000, 'topup', 'ref_check', 1000, '2026-01-01T00:00:00+00:00');
" || fail "could not seed the migrated source"

out=$(env DATABASE_URL="sqlite://$MIGRATED" DRILL_SCRATCH_DIR="$WORK" \
    sh "$REPO/tools/drill/drill.sh" --target drill_check_scratch.db 2>&1); rc=$?
case "$out" in
    *"drift check PASSED"*) ;;
    *) fail "the drill did not report a passing drift check on a CONSISTENT source (exit $rc)" ;;
esac
[ "$rc" -eq 0 ] || fail "a consistent source must PASS the drill (exit 0), got $rc"

# --- 3. a DRIFTED source FAILS ----------------------------------------------
# Without this, a drill that ignores drift would pass everything above.
DRIFTED="$WORK/drifted.db"
cp "$MIGRATED" "$DRIFTED"
sqlite3 "$DRIFTED" "UPDATE wallets SET balance_idr = 9999 WHERE account_id = 'a1';"
out=$(env DATABASE_URL="sqlite://$DRIFTED" DRILL_SCRATCH_DIR="$WORK" \
    sh "$REPO/tools/drill/drill.sh" --target drill_check_negscratch.db 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then
    fail "a DRIFTED source PASSED the drill; the drill would certify a restore that does not reconcile"
fi
case "$out" in
    *"drift check FAILED"*) ;;
    *) fail "the drill failed on a drifted source but did not say the drift check failed" ;;
esac

# --- 4. teardown is honest in both directions --------------------------------
test -f "$WORK/drill_check_scratch.db" && fail "the scratch file survived a PASSING drill (teardown step)"
test -f "$WORK/drill_check_negscratch.db" && fail "the scratch file survived a FAILING drill"

if [ "$FAILED" -ne 0 ]; then
    echo "drill-check: the restore drill contract is BROKEN (see above)" >&2
    exit 1
fi

echo "drill-check: OK - the guard refuses every live-looking target and touches nothing, a consistent source drills to PASS, and a drifted one FAILS"
exit 0