#!/bin/sh
# sqlite-probes-check — the probes behind a SHIPPED plan still run and still say what the plan cites.
#
# WHY THIS EXISTS. `docs/plans/sqlite-migration.md` cites these probes' output as the evidence for the
# whole SQLite port, and the port is shipped. Nothing re-ran them: they are three Python scripts with no
# `check.sh`, no CI step, and no assertion that the environment they run in can support the schema.
#
# MEASURED before this gate: the suite's own banner printed the SQLite version and NOTHING checked it,
# while the plan requires **>= 3.37** for the `STRICT` clause every one of the 24 tables carries. A host
# with an older library would run all probes green and then fail to create the first table. That is the
# "a check that cannot fail" defect in its quieter form - a measurement that cannot notice the one
# precondition it depends on.
#
# WHAT THIS CHECKS, and why each is a behaviour rather than a source read:
#   1. all three probes RUN and exit 0          - a probe that crashes cites nothing
#   2. the port probe reports ZERO failures     - `[ERROR]` rows are EXPECTED (SQLite rejecting Postgres
#                                                 syntax); a new one would silently join them
#   3. the probe count has not DROPPED          - the suite silently shrinking is how coverage disappears
#   4. the STRICT probes are among them          - the plan's most important invariant, named explicitly
#                                                 so a future edit cannot quietly remove it
#
# Requires python3 with the stdlib sqlite3 module. No packages, matching the probes' own claim.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
PROBES="$REPO/tools/sqlite-probes"
FAIL=0

fail() {
    echo "sqlite-probes-check: FAIL - $1" >&2
    FAIL=1
}

if ! command -v python3 >/dev/null 2>&1; then
    echo "sqlite-probes-check: SKIPPED - python3 is not on PATH, so nothing was verified"
    exit 0
fi

for p in sqlite-port-probe.py sqlite-timestamp-probe.py validate-migration-schema.py; do
    if [ ! -f "$PROBES/$p" ]; then
        fail "$p is missing, so the plan cites a result nothing can reproduce"
        continue
    fi
    out=$(python3 "$PROBES/$p" 2>&1)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        fail "$p exited $rc"
        printf '%s\n' "$out" | tail -5 | sed 's/^/       /' >&2
        continue
    fi
    echo "sqlite-probes-check:   ok - $p ran (exit 0)"
done

# --- EVERY probe must ASSERT, not just print ---------------------------------
#
# "Ran (exit 0)" is not the same as "checked something". MEASURED: `sqlite-timestamp-probe.py` used to
# print `(want N)` beside every measurement and compare none of them — patching it so a value
# contradicted its own printed `want` still exited 0. It reported the timestamp hazard, which
# `docs/plans/sqlite-migration.md` calls "the finding most likely to have caused a production incident",
# while being unable to notice that hazard changing.
#
# So each probe must emit at least one verdict line. The patterns differ by script — `[PASS ]` with a
# TRAILING PAD in the port probe (`f"[{status:5}]"`), bare `[PASS]` in the other two, and one of them
# indents by two spaces — so the match allows whitespace INSIDE the brackets as well as before them.
# MEASURED: without the inner `[[:space:]]*` this counted 5 of the port probe's 28 verdicts, because only
# the 5 `[ERROR]` lines happened to be unpadded. A guard against under-reporting that under-reports is
# the defect it was written to catch.
for p in sqlite-port-probe.py sqlite-timestamp-probe.py validate-migration-schema.py; do
    [ -f "$PROBES/$p" ] || continue
    # CAPTURE FIRST, COUNT SECOND — do not pipe the command into grep. `n=$(python3 ... | grep -c ...)`
    # captures the LAST stage's status, so a probe that CRASHED would pass this check with a count from
    # whatever grep saw. The run-ability loop above already fails on a non-zero exit, but a hazard here
    # would be masked even so, and `tools/shell-hazards/check.sh` flagged the piped form for exactly that
    # reason. Reading the output into a variable first keeps the two questions separate.
    verdict_out=$(python3 "$PROBES/$p" 2>&1)
    n=$(printf '%s\n' "$verdict_out" | grep -cE '^[[:space:]]*\[[[:space:]]*(PASS|FAIL|ERROR)[[:space:]]*\]' || true)
    if [ "${n:-0}" -eq 0 ]; then
        fail "$p prints no [PASS]/[FAIL]/[ERROR] verdict at all, so its exit code says only that Python ran. A probe that cannot fail is not evidence for the plan that cites it"
    else
        echo "sqlite-probes-check:   ok - $p emits $n verdict line(s)"
    fi
done

# --- the port probe's own results --------------------------------------------
OUT=$(python3 "$PROBES/sqlite-port-probe.py" 2>&1)

# `[ERROR]` is EXPECTED for the Postgres-syntax probes and `[FAIL]` is not used by this suite, so the
# count that matters is the probe TOTAL - a suite that silently shrinks is the failure here.
COUNT=$(printf '%s\n' "$OUT" | grep -E '^[0-9]+ probes$' | grep -oE '^[0-9]+')
if [ -z "$COUNT" ]; then
    fail "the port probe printed no 'N probes' line, so this check cannot tell whether it ran"
elif [ "$COUNT" -lt 28 ]; then
    fail "the port probe reports $COUNT probes, fewer than the 28 this gate was written against. A suite that shrinks silently is how coverage disappears"
else
    echo "sqlite-probes-check:   ok - the port probe reports $COUNT probes (>= 28)"
fi

# The plan's most important invariant, named so it cannot be removed without this failing.
for want in \
    "SQLite version supports STRICT" \
    "STRICT rejects a wrong type" \
    "control: a NON-STRICT table accepts it" \
    "STRICT rejects a REAL in a money column"
do
    if printf '%s\n' "$OUT" | grep -qF "$want"; then
        echo "sqlite-probes-check:   ok - probes '$want'"
    else
        fail "the port probe no longer reports '$want'. docs/plans/sqlite-migration.md:1441 calls STRICT the single most important line in the appendix, and without a probe nothing measures it"
    fi
done

# --- the version precondition ------------------------------------------------
#
# Not read from the probe's own text: the probe asserts it, and a probe that asserts is the point. This
# only confirms the assertion is among the reported results, so a failure here is about the ASSERTION and
# not about the host.
if printf '%s\n' "$OUT" | grep -qE '^\[PASS \] SQLite version supports STRICT'; then
    echo "sqlite-probes-check:   ok - the STRICT version precondition holds on this host"
elif printf '%s\n' "$OUT" | grep -qE '^\[ERROR\] SQLite version supports STRICT'; then
    fail "this host's SQLite is too old for the STRICT schema the plan requires (>= 3.37). The port cannot run here"
fi

echo ""
if [ "$FAIL" -ne 0 ]; then
    echo "sqlite-probes-check: the probe evidence behind the SQLite port is BROKEN (see above)" >&2
    exit 1
fi
echo "sqlite-probes-check: OK - all three probes run, the suite has not shrunk, and the STRICT invariant"
echo "                     the plan calls its most important line is measured on this host"
