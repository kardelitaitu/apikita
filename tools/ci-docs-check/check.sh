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
#
# THIS USED TO BE A HAND-KEPT LIST OF SEVEN NAMES, and that left a hole measured this
# round: deleting `Check the rollback drill` from the workflow while `docs/ci-cd.md` kept
# describing it left this check GREEN. The workflow runs 28 steps and the list named 7, so
# 21 step names were unprotected - including the three newest tool checks
# (`Check the rollback drill`, `Check the wind-down payout report`, `Check the CI
# documentation`), which are exactly the ones most likely to be renamed or dropped.
#
# The fix derives the names from the DOCUMENT's own stage tables rather than from a list
# beside them. Every row whose first cell is a step name is extracted, and each must appear
# in the workflow.
#
# SCOPE, which the first attempt at this got wrong and the failure was informative. That
# attempt scanned every table and immediately reported eight legitimate rows: "Build the
# release artifact", "Run migrations", "Deploy the server" and friends. Those are real and
# correct - they describe the pipeline that runs ON MERGE TO MAIN, which is not
# `.github/workflows/ci.yml` and is not what this check is about. So the extraction STOPS
# at the deployment heading: only the tables a reader would read as "what runs on my PR"
# are in scope. A blanket scan is not a stricter check, it is a check about a different
# document.
# The vacuity floor is the reason this is safe to derive: if the extraction finds too few
# rows, the tables moved or the parse failed, and the check refuses rather than passing
# over an empty set - the W38/W41/W43 failure.
CLAIMED_LIST="${TMPDIR:-/tmp}/apikita-ci-claimed.$$"
awk '
    # The deployment pipeline is a different document concern; stop there.
    /^### On merge/ { exit }
    # A header row naming a CI table.
    /^\| *(Stage|Setup step) *\|/ { in_table = 1; next }
    in_table && /^\| *---/ { next }
    in_table && /^\| / {
        line = $0
        sub(/^\| */, "", line)
        sub(/ *\|.*$/, "", line)
        gsub(/^\*\*/, "", line)
        gsub(/\*\*$/, "", line)
        if (line != "") print line
        next
    }
    in_table { in_table = 0 }
' "$DOC" > "$CLAIMED_LIST"

CLAIMED_COUNT=$(grep -c . "$CLAIMED_LIST" || true)
if [ "$CLAIMED_COUNT" -lt 15 ]; then
    echo "ci-docs-check: only $CLAIMED_COUNT stage name(s) were extracted from $DOC," >&2
    echo "ci-docs-check:   so the inverse assertion below would pass over almost nothing." >&2
    echo "ci-docs-check:   Expected the stage tables; if their headers changed, update the" >&2
    echo "ci-docs-check:   extractor rather than deleting this floor." >&2
    rm -f "$CLAIMED_LIST"
    exit 3
fi

STALE_LIST="${TMPDIR:-/tmp}/apikita-ci-stale.$$"
while IFS= read -r claimed; do
    [ -n "$claimed" ] || continue
    grep -qF "$claimed" "$WORKFLOW" || echo "$claimed"
done < "$CLAIMED_LIST" > "$STALE_LIST"

if [ -s "$STALE_LIST" ]; then
    while IFS= read -r step; do
        echo "ci-docs-check: FAIL - $DOC describes a step the workflow does not run: $step" >&2
    done < "$STALE_LIST"
    rm -f "$CLAIMED_LIST" "$STALE_LIST"
    echo "ci-docs-check: a stage the document presents as running, and CI does not run, is" >&2
    echo "ci-docs-check:   the reader's map of their own gate. Delete the row or restore the step." >&2
    exit 1
fi
rm -f "$CLAIMED_LIST" "$STALE_LIST"

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

# --- THE ONE ORDERING THAT IS LOAD-BEARING -----------------------------------
#
# Every check above is about MEMBERSHIP: is this step named, does this named check exist, is this tool
# indexed. None of them reads ORDER, and order was the defect one round ago.
#
# MEASURED, and it is why this block exists. The workflow ran `Website contract tests` BEFORE
# `Build website`. One of those tests reads the RENDERED pages under `website/dist`, so with the build
# after them `dist/` did not exist yet and the test reported a SKIP on every CI run - a green tick
# over an assertion that never executed. Membership was satisfied the whole time: both steps were in
# the workflow and both were named in this document, in the same wrong order.
#
# So this pins the PAIR, in both files, against a specific reason. It is deliberately not a general
# ordering check: most CI step order is a preference, and asserting it would be the hand-kept-list
# defect this file has already been rewritten twice to remove. This one earns its place because a
# test's result depends on it.
build_at=$(grep -n '^      - name: Build website$' "$WORKFLOW" | head -n 1 | cut -d: -f1)
tests_at=$(grep -n '^      - name: Website contract tests$' "$WORKFLOW" | head -n 1 | cut -d: -f1)

# EACH STEP NAME MUST OCCUR EXACTLY ONCE, and this is the assertion that was missing.
#
# MEASURED: with a SECOND `- name: Build website` inserted AFTER the tests step, this script exited 0.
# The `head -n 1` above takes the first occurrence - the legitimate early one - so the ordering
# assertion compared a pair that was still correctly ordered while the workflow's LAST build ran
# after the tests. The guard verified the wrong occurrence of the thing it was guarding.
#
# Neither of the two checks that follow could catch it: `-z` fires only on a MISSING step, and `-ge`
# only on an INVERTED pair. A duplicate is neither. And `head -n 1` is what makes it invisible, so
# the uniqueness has to be asserted rather than assumed.
#
# This is the same shape as `alert-check`'s count guard, which asked whether the right number
# appeared SOMEWHERE while four other occurrences could be wrong. A check that reads one match out of
# many must first establish that there is only one.
for pair in "Build website:$WORKFLOW" "Website contract tests:$WORKFLOW"; do
    step=${pair%%:*}
    n=$(grep -c "^      - name: ${step}\$" "$WORKFLOW" || true)
    if [ "$n" -ne 1 ]; then
        echo "ci-docs-check: FAIL - the workflow has $n step(s) named '$step', and the ordering check" >&2
        echo "ci-docs-check:   below reads only the FIRST with \`head -n 1\`. With more than one, it" >&2
        echo "ci-docs-check:   compares that pair and not the one that actually runs last, so a" >&2
        echo "ci-docs-check:   duplicate placed after 'Website contract tests' passes while leaving the" >&2
        echo "ci-docs-check:   rendered-output test skipping on every run." >&2
        exit 1
    fi
done

# Both must exist, or the comparison below is vacuously true.
if [ -z "$build_at" ] || [ -z "$tests_at" ]; then
    echo "ci-docs-check: FAIL - cannot find both '- name: Build website' (at '${build_at:-none}') and" >&2
    echo "ci-docs-check:   '- name: Website contract tests' (at '${tests_at:-none}') in $WORKFLOW, so" >&2
    echo "ci-docs-check:   the ordering assertion below would pass over a missing step rather than a" >&2
    echo "ci-docs-check:   wrong order." >&2
    exit 1
fi

if [ "$build_at" -ge "$tests_at" ]; then
    echo "ci-docs-check: FAIL - the workflow runs 'Website contract tests' (line $tests_at) BEFORE" >&2
    echo "ci-docs-check:   'Build website' (line $build_at). One of those tests reads the rendered" >&2
    echo "ci-docs-check:   pages under website/dist, so with the build after them it SKIPS on every" >&2
    echo "ci-docs-check:   run and verifies nothing - the failure mode looks like a green tick." >&2
    echo "ci-docs-check:   Move the build above the tests, and keep the table in docs/ci-cd.md in" >&2
    echo "ci-docs-check:   the same order." >&2
    exit 1
fi

# AND THE DOCUMENT, in the same order, because a reader follows the table rather than the YAML.
doc_build=$(grep -n '^| Build website ' "$DOC" | head -n 1 | cut -d: -f1)
doc_tests=$(grep -n '^| Website contract tests ' "$DOC" | head -n 1 | cut -d: -f1)
if [ -n "$doc_build" ] && [ -n "$doc_tests" ] && [ "$doc_build" -ge "$doc_tests" ]; then
    echo "ci-docs-check: FAIL - docs/ci-cd.md lists 'Website contract tests' (line $doc_tests) before" >&2
    echo "ci-docs-check:   'Build website' (line $doc_build), which is the order that made the" >&2
    echo "ci-docs-check:   rendered-output test skip. A reader reproduces the document's order." >&2
    exit 1
fi

echo "ci-docs-check: OK - all $COUNT workflow steps are named in docs/ci-cd.md, every check it advertises exists,"
echo "ci-docs-check:      the build precedes the website contract tests in both files, and all $REAL_COUNT tool"
echo "ci-docs-check:      directories are indexed in tools/README.md"
exit 0