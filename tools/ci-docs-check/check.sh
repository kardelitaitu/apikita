#!/bin/sh
# CI documentation check - every workflow step must be named in docs/ci-cd.md.
#
# WHY THIS EXISTS. docs/ci-cd.md is where someone answers "what will stop my PR?" and
# "what is checked before merge?". Its stage table listed TEN stages while the workflow
# ran TWENTY-SIX - the seven tool-contract checks added by W35-W43 were absent entirely,
# each with a careful rationale in its commit message and a README beside its script, but
# not in the one document a reader looks at.
#
# The failure mode of an understated pipeline is not academic: a reader runs the ten
# documented commands locally, sees green, and is surprised by seven failures they did not
# know existed - which is how people start working around CI instead of with it. The table
# also had a "Blocks merge" column, so an omission read as "advisory", the opposite of true.
#
# W30 found a doc OVERSTATING a route. This is the same defect with the sign flipped, and
# like the W41/W43 guards this one ASSERTS IT READ THE FILES: a workflow that failed to
# parse, or a document that failed to open, would otherwise pass over an empty set.
#
# Usage: sh tools/ci-docs-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
WORKFLOW="$REPO/.github/workflows/ci.yml"
DOC="$REPO/docs/ci-cd.md"

[ -f "$WORKFLOW" ] || { echo "ci-docs-check: missing $WORKFLOW" >&2; exit 3; }
[ -f "$DOC" ] || { echo "ci-docs-check: missing $DOC" >&2; exit 3; }

# The step names, read from the workflow. `- name:` lines at the step indent, with the
# leading marker stripped. The unnamed first steps (checkout, setup) are skipped by the
# `name:` filter itself.
STEPS=$(grep -E '^      - name: ' "$WORKFLOW" | sed 's/^      - name: //')

COUNT=$(printf '%s\n' "$STEPS" | grep -c .)
if [ "$COUNT" -lt 15 ]; then
    echo "ci-docs-check: only $COUNT step names found; the workflow was not parsed" >&2
    echo "ci-docs-check:   correctly, so every assertion below would pass vacuously" >&2
    exit 3
fi

# The document must be substantial, for the same reason.
if [ "$(wc -l < "$DOC")" -lt 100 ]; then
    echo "ci-docs-check: $DOC looks truncated; refusing to certify it" >&2
    exit 3
fi

# One name per line, each searched for verbatim. A `while read` in a pipeline runs in a
# SUBSHELL, so the missing names are collected into a file rather than a variable - a
# counter assigned inside the loop would not survive it.
MISSING_LIST="${TMPDIR:-/tmp}/apikita-ci-docs-missing.$$"
printf '%s\n' "$STEPS" | while IFS= read -r step; do
    [ -n "$step" ] || continue
    grep -qF "$step" "$DOC" || echo "$step"
done > "$MISSING_LIST"

if [ -s "$MISSING_LIST" ]; then
    while IFS= read -r step; do
        echo "ci-docs-check: FAIL - the workflow runs a step the CI document never names: $step" >&2
    done < "$MISSING_LIST"
    rm -f "$MISSING_LIST"
    echo "ci-docs-check: the CI document must describe the pipeline that actually runs." >&2
    exit 1
fi
rm -f "$MISSING_LIST"

# Guard the other direction too: the document must not claim a step that does not exist.
# Only the `- name:`-derived contract rows are checked, since the doc also describes
# local commands on purpose.
for claimed in \
    "Check the edge relay streams SSE" \
    "Check the compose deployment definition" \
    "Check the backup contract" \
    "Check the reconciliation gate" \
    "Check the restore drill" \
    "Check the alert delivery contract" \
    "Validate the schema against the plan"; do
    grep -qF "$claimed" "$WORKFLOW" || {
        echo "ci-docs-check: FAIL - $DOC describes a step the workflow does not run: $claimed" >&2
        exit 1
    }
done

echo "ci-docs-check: OK - all $COUNT workflow steps are named in docs/ci-cd.md, and every check it advertises exists"
exit 0