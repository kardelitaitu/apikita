#!/bin/sh
# mutation-check - every gate must be able to FAIL to run, and must document what it does not catch.
#
# WHY THIS EXISTS. This gate began as a different one and the mistake is the point.
#
# It was written to certify mutation harnesses - scripts that break a subject and restore it - because round
# 42 believed a mutation result that the checks would have rejected. It named one subject,
# `relay-check/behaviour.sh`, and that script failed six of its seven checks. Reading the subject showed
# why: it is a BEHAVIOURAL PROBE, running a real nginx container and timing stream arrival. It never mutates
# a file and has no reason to restore one. MEASURED too: no script under `tools/` performs a
# mutate-and-restore at all. The check was sound and the subject was invented.
#
# A gate built on a guessed subject reports false violations - the same defect it was built to catch. So
# this gate asserts only what is true of every gate here:
#
#   1. it exits 3, not 0, when it cannot run. A silent pass on a machine that ran nothing is the failure
#      this repository's tooling exists to avoid.
#   2. it has a Usage line and an Exit line naming every code it can return.
#   3. it states what it does NOT catch. That sentence has caught more drift in this repo than any
#      assertion, because it is the part an editor has to think about.
#
# STILL NOT ENFORCED, stated rather than hidden: no script under tools/ mutates its subject, so round 43's
# mechanized checklist has no committed home. `.agents/` is gitignored scratch per AGENTS.md - correct, and
# the reason those checks live in prose. This gate covers round 43's OTHER lesson instead.
#
# Usage: sh tools/mutation-check/check.sh [tool-dir...]
# Exit: 0 every gate holds, 1 a violation naming the gate, 3 node is missing or no gates found.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
TOOL="$REPO/tools/mutation-check/check.js"

if ! command -v node >/dev/null 2>&1; then
    echo "mutation-check: node is required and was not found on PATH" >&2
    exit 3
fi

if [ ! -f "$TOOL" ]; then
    echo "mutation-check: $TOOL is missing" >&2
    exit 3
fi

exec node "$TOOL" "$@"
