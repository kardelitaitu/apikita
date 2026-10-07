#!/bin/sh
# benchmark-verdicts - every `[PASS]` the benchmark prints must be earned by a comparison.
#
# WHY THIS EXISTS. MEASURED over two rounds: THREE of the benchmark's four scenarios printed `[PASS]`
# from a fixed string that no measurement could change.
#
#   scenario 3  printed beside `Success Rate: 37.76%`, against a documented target of `>= 99.9%`.
#               It would have printed PASS at 0%.
#   scenario 2  `[PASS] Instantaneous webhook validation` - "instantaneous" is an adjective doing a
#               number's job.
#   scenario 4  `[PASS] 0.2 vCPU easily handles {} concurrent active streams` - the `{}` was the
#               scenario's INPUT, not its result, so the verdict clause was fixed text.
#
# AND ONE "MEASUREMENT" WAS NOT ONE. `Estimated Socket RAM: {} MB`, computed as
# `(concurrency * 35) / 1024.0` - both operands constants, and `35` is the document's own PASS
# CRITERION. The harness took the target, multiplied it by its input, and printed the product as a
# passing reading. Nothing about the run entered it.
#
# WHY A GUARD RATHER THAN A CONVENTION. The `benchmark` binary has NO test coverage - no `#[test]` -
# and the only check coupling it to anything reads its CLI, never its output. Three of four scenarios
# drifted the same way with nothing watching.
#
# WHAT IT DOES NOT CATCH, stated because the tool was falsified against its own subject and two
# mutations survive: a threshold changed to a vacuous one (`>= 10000` to `>= 0`) still looks like a
# comparison, and deleting an `else` arm is missed when another `else` is nearby. What it does catch
# is the shape that actually occurred three times: a verdict with no value and no branch at all.
#
# Usage: sh tools/benchmark-verdicts/check.sh
# Exit: 0 every verdict is earned, 1 a literal verdict, 3 node is missing or the source is unreadable.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
TOOL="$REPO/tools/benchmark-verdicts/check.js"

if ! command -v node >/dev/null 2>&1; then
    echo "benchmark-verdicts: node is required and was not found on PATH" >&2
    exit 3
fi

if [ ! -f "$TOOL" ]; then
    echo "benchmark-verdicts: $TOOL is missing" >&2
    exit 3
fi

exec node "$TOOL"
