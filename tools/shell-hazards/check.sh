#!/bin/sh
# shell-hazards - the shell constructs that destroy a measurement.
#
# WHY THIS EXISTS. Rounds 4-10 of this work each found a guard or a harness that FAILED TO MEASURE
# what it claimed, and each wrote the reason into `docs/testing.md`. Notes are not guards: the same
# mistake was made three times in three rounds, in three different forms, by the same author. Three
# of those forms are shell-level and are now checked mechanically here.
#
#   1. A CAPTURE THAT DISCARDS THE VERDICT.
#      `OUT=$(cmd ... | filter)` - `$?` across a pipeline is the LAST stage's, so `tail` succeeding
#      reports success no matter what `cmd` found. MEASURED: `reconcile.sh` exits 6 for a missing
#      database and `... | tail -1` reports 0.
#
#   2. A FLAG SET INSIDE A PIPELINE SUBSHELL.
#      `... | while read -r x; do FAILED=1; done` - the loop runs in a subshell, so the assignment
#      is lost. The message still PRINTS, which is what makes it look like it worked. MEASURED: a
#      guard in this repository named a defect and exited 0.
#
#   3. `set -e`.
#      Every gate here runs detectors whose NON-ZERO EXIT IS THE FINDING. MEASURED: adding `-e`
#      aborts alert-check, reconcile-check and backup-check on a CLEAN tree.
#
# WHAT IT DOES NOT FLAG, which is the whole design. Hazard 1 has FIVE live instances in this
# repository and every one is CORRECT - a `sha256sum | cut`, three `sqlite3 ... | head -1`, and a
# `probe.sh --list | grep -c`. A check that flagged them would be the false-positive machine this
# work has produced repeatedly. The distinguishing property is not whether the exit code was
# discarded but whether the CAPTURED VALUE IS VALIDATED before it is trusted. Each of the five is,
# and each is listed by name in check.js WITH its validation - so removing one of those validations
# FAILS, rather than passing because the site is on an exemption list.
#
# THE FIRST RUN FOUND ITS OWN AUTHOR'S MISTAKE, which is the argument for the tool: the DUMP_SHA
# entry asserted a validation that did not exist where it was claimed. The real guard is a
# `[ -n "${DUMP_SHA:-}" ]` elsewhere in the file, and the pattern was corrected to match it.
#
# Usage: sh tools/shell-hazards/check.sh
# Exit: 0 all hold, 1 a hazard, 3 node is missing or too few scripts were found.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
TOOL="$REPO/tools/shell-hazards/check.js"

if ! command -v node >/dev/null 2>&1; then
    echo "shell-hazards: node is required and was not found on PATH" >&2
    exit 3
fi

if [ ! -f "$TOOL" ]; then
    echo "shell-hazards: $TOOL is missing" >&2
    exit 3
fi

exec node "$TOOL"
