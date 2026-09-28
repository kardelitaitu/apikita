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

# --- the tools INDEX ---------------------------------------------------------
# tools/README.md is the only page that says what the tools ARE, and an unchecked index is
# the next thing to drift - the same argument as the stage table above. Every directory
# under tools/ must be listed, and every `tools/<name>/` link it contains must exist.
TOOLS_DIR="$REPO/tools"
INDEX="$TOOLS_DIR/README.md"
[ -f "$INDEX" ] || { echo "ci-docs-check: missing $INDEX" >&2; exit 3; }

# The real directory listing, excluding README.md itself and any hidden entries.
REAL_TOOLS=$(find "$TOOLS_DIR" -mindepth 1 -maxdepth 1 -type d -exec basename {} \; | sort)

REAL_COUNT=$(printf '%s\n' "$REAL_TOOLS" | grep -c .)
if [ "$REAL_COUNT" -lt 8 ]; then
    echo "ci-docs-check: only $REAL_COUNT tool directories found; the listing was not read" >&2
    echo "ci-docs-check:   correctly, so the index assertions would pass vacuously" >&2
    exit 3
fi

# The index links RELATIVELY from inside tools/ (`[reconcile/](reconcile/README.md)`),
# which is what markdown requires of a file in that directory. So match the directory
# NAME inside a link target, not the literal string "tools/name/" - an index that only
# passed because it prefixed every link would be a worse index.
TOOL_MISSING="${TMPDIR:-/tmp}/apikita-tools-missing.$$"
printf '%s\n' "$REAL_TOOLS" | while IFS= read -r name; do
    [ -n "$name" ] || continue
    grep -qE "\]\($name/README\.md\)" "$INDEX" || echo "$name"
done > "$TOOL_MISSING"

if [ -s "$TOOL_MISSING" ]; then
    while IFS= read -r name; do
        echo "ci-docs-check: FAIL - tools/$name/ exists but is not indexed in tools/README.md" >&2
    done < "$TOOL_MISSING"
    rm -f "$TOOL_MISSING"
    echo "ci-docs-check: the index must list every tool, or the tools are undiscoverable." >&2
    exit 1
fi
rm -f "$TOOL_MISSING"

# The other direction: a link to a tool that does not exist.
for linked in $(grep -oE '\]\([a-z0-9-]+/README\.md\)' "$INDEX" | sed 's/^](//; s|/README.md)$||'); do
    [ -d "$TOOLS_DIR/$linked" ] || {
        echo "ci-docs-check: FAIL - tools/README.md links to tools/$linked/, which does not exist" >&2
        exit 1
    }
    [ -f "$TOOLS_DIR/$linked/README.md" ] || {
        echo "ci-docs-check: FAIL - tools/$linked/ is indexed but has no README" >&2
        exit 1
    }
done

echo "ci-docs-check: OK - all $COUNT workflow steps are named in docs/ci-cd.md, every check it advertises exists,"
echo "ci-docs-check:      and all $REAL_COUNT tool directories are indexed in tools/README.md"
exit 0