#!/bin/sh
# fake-check - the two fake services do what their READMEs say.
#
# WHY THIS FILE EXISTS. Until this round, CI invoked the tool directly:
#
#     node tools/fake-check/fake-check.js
#
# and that was the ONLY one of 16 CI checks not run through a `check.sh`. The other fifteen all do, and
# every one of them documents the same contract - a `Usage:` line, an `Exit:` line naming each code, and a
# loud exit 3 when a required tool is absent.
#
# THE WORKINGS ARE UNCHANGED. The assertions, the mutation record and the runtime behaviour live in
# `fake-check.js` beside this file and in its README; this file is the wrapper so the tool is reachable the
# same way as every other gate, and so `mutation-check` - which audits the gates for exactly this shape -
# covers it.
#
# Usage: sh tools/fake-check/check.sh
# Exit: 0 = every documented behaviour holds, 1 = a documented behaviour does not, 3 = node is missing.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
TOOL="$REPO/tools/fake-check/fake-check.js"

if ! command -v node >/dev/null 2>&1; then
    echo "fake-check: node is required and was not found on PATH" >&2
    exit 3
fi

if [ ! -f "$TOOL" ]; then
    echo "fake-check: $TOOL is missing" >&2
    exit 3
fi

exec node "$TOOL"
