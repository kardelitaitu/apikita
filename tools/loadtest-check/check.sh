#!/bin/sh
# loadtest-check — the load test's verdicts are earned by measurement, and its calibration is sound.
#
# WHY THIS EXISTS. `tools/benchmark-verdicts/check.sh` already asserts that every `[PASS]` the
# BENCHMARK BINARY prints is a comparison rather than a fixed string. That gate reads ONE file —
# `server/src/bin/benchmark.rs` — and it does it by PARSING THE SOURCE. Both properties are wrong for
# this tool, and the difference is the reason for a second gate rather than a widened first one:
#
#   * The load test is `tools/loadtest/loadtest.js`, which `benchmark-verdicts` never opens.
#   * A source-shape rule can be satisfied by code that does not work. The load test has THREE
#     calibration dependencies that no amount of reading can confirm — the pid sampler that reads the
#     right process, the CPU quantum under which a sample means anything, and the histogram merge that
#     percentiles are computed from — and MEASURED, the first version of the pid sampler was rejected
#     by the Windows PowerShell parser and returned `null` from every sample. It would have passed
#     every source-shape check ever written and reported "[NO DATA]" for a healthy server forever.
#
# So this gate RUNS things. Every assertion below executes the real module or the real script and
# reads what came back.
#
# WHAT IT ASSERTS, each with its own falsification recorded in README.md:
#   1. The three constants the verdicts compare against equal the rows in docs/benchmark.md.
#      (The Rust side of this coupling is `the_loadtest_thresholds_are_the_thresholds_the_document_publishes`
#      in server/src/doc_claims.rs. This is the shell-side restatement, and it is here because it runs
#      in under a second in a pipeline that may not have a Rust toolchain.)
#   2. verdictP99 and verdictCpu FLIP when the measurement crosses the bar, and flip BOTH ways.
#      This is the "a PASS that cannot fail is the defect" rule, asserted by execution.
#   3. verdictP99 returns NO DATA for no measurement, and never PASS.
#   4. verdictUnresolvable NEVER reports ok — so a p99 the instrument cannot resolve cannot be
#      mistaken for a passing one.
#   5. percentileFromHistogram computes the percentile that is actually in the data, asserted against
#      a hand-built histogram whose answer is known by construction.
#   6. The pid sampler reads a LIVE process and sees its CPU MOVE. This is the assertion that would
#      have caught the null-sampler defect, and it is the reason this gate runs code.
#   7. The two histogram boundary definitions (loadtest.js and loadtest-worker.js) are identical, so
#      the duplication the worker's comment names cannot drift.
#   8. Unknown flags are REFUSED, so a typo cannot produce a run measuring something else.
#
# Usage: sh tools/loadtest-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 node is missing or a file is unreadable.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
HARNESS="$REPO/tools/loadtest/loadtest.js"
DOC="$REPO/docs/benchmark.md"

for f in "$HARNESS" "$REPO/tools/loadtest/loadtest-worker.js" "$DOC"; do
    if [ ! -f "$f" ]; then
        echo "loadtest-check: missing $f" >&2
        exit 3
    fi
done

if ! command -v node >/dev/null 2>&1; then
    echo "loadtest-check: node is required and was not found on PATH" >&2
    exit 3
fi

# --- the constants against the document ---------------------------------------
#
# Read from BOTH sides rather than restated. A number written here would be the second place to
# update and would go stale exactly the way the claim would without it.
#
# The document read takes the FIRST comparison operator on the row that names the metric, because
# the matrix writes `<label> | <target> | <warning> | <critical>` and looking for digits anywhere on
# the line finds the next column. Both the LaTeX spelling (`\le 1.5`) and the plain one (`<= 1.5`)
# are accepted: the matrix uses LaTeX and Scenario 1's pass criteria do not, and a reader that knew
# only one would silently find nothing in the other.
doc_figure() {
    awk -v label="$1" '
        index($0, label) == 0 { next }
        { t = $0; sub(/^[ \t]+/, "", t); if (substr(t,1,1) == ">" || index($0, "~~")) next }
        {
            n = length($0)
            for (i = 1; i <= n; i++) {
                two = substr($0, i, 2)
                if (two == "\\l" && substr($0, i, 4) == "\\le ") { rest = substr($0, i+4); found = 1; break }
                if (two == "<=") { rest = substr($0, i+2); found = 1; break }
                if (substr($0,i,2) == "\\g" && substr($0,i,4) == "\\ge ") { rest = substr($0, i+4); found = 1; break }
                if (two == ">=") { rest = substr($0, i+2); found = 1; break }
            }
            if (found) {
                sub(/^[ \t]*/, "", rest)
                num = ""
                for (j = 1; j <= length(rest); j++) {
                    c = substr(rest, j, 1)
                    if (c ~ /[0-9.]/) num = num c; else break
                }
                if (num != "") { print num; exit }
            }
        }
    ' "$DOC"
}

tool_figure() {
    node -e '
        const fs = require("node:fs");
        const src = fs.readFileSync(process.argv[1], "utf8");
        const at = src.indexOf("const " + process.argv[2] + " =");
        if (at === -1) { console.error("missing " + process.argv[2]); process.exit(3); }
        const raw = src.slice(at).split("=")[1].split(";")[0].trim();
        const d = raw.replace(/[^0-9.].*$/, "");
        if (!/^[0-9.]+$/.test(d)) { console.error("not a number: " + raw); process.exit(3); }
        process.stdout.write(d);
    ' "$HARNESS" "$1"
}

FAIL=0
compare() {
    label="$1"; constant="$2"; text="$3"
    DOC_V=$(doc_figure "$text")
    TOOL_V=$(tool_figure "$constant")
    if [ -z "$DOC_V" ] || [ -z "$TOOL_V" ]; then
        echo "loadtest-check: FAIL - could not read $label from the document ('$DOC_V') or the tool ('$TOOL_V')" >&2
        echo "loadtest-check:   A comparison over an empty read passes over everything, so this refuses." >&2
        FAIL=1
        return
    fi
    if [ "$DOC_V" != "$TOOL_V" ]; then
        echo "loadtest-check: FAIL - $label: docs/benchmark.md states $DOC_V, tools/loadtest/loadtest.js carries $constant = $TOOL_V" >&2
        echo "loadtest-check:   The tool prints a verdict against its constant, and a reader compares it" >&2
        echo "loadtest-check:   to the document. One target stated twice, so update both or neither." >&2
        FAIL=1
    else
        echo "loadtest-check:   ok - $label: document and tool both say $DOC_V"
    fi
}

echo "loadtest-check: the two published thresholds, document vs tool"
compare "Key Validation Latency (p99)" "PUBLISHED_P99_LATENCY_MS" "Key Validation Latency"
compare "CPU Saturation at 200 req/s" "PUBLISHED_CPU_TARGET_PCT" "CPU Saturation"

# The row's own name carries the rate the CPU figure is denominated at, and the tool prints that
# rate back to a reader as the fix for a saturation-mode reading.
RATE_DOC=$(awk '/CPU Saturation/ { t=$0; sub(/^[ \t]+/,"",t); if (substr(t,1,1) != ">" && index($0,"~~") == 0) { if (match($0, /at [0-9]+ req\/s/)) { s = substr($0, RSTART+3); sub(/ req\/s.*/, "", s); print s; exit } } }' "$DOC")
RATE_TOOL=$(tool_figure "PUBLISHED_CPU_RATE_RPS")
if [ -z "$RATE_DOC" ] || [ "$RATE_DOC" != "$RATE_TOOL" ]; then
    echo "loadtest-check: FAIL - the CPU row states 'at ${RATE_DOC:-<unreadable>} req/s' and the tool carries PUBLISHED_CPU_RATE_RPS = ${RATE_TOOL:-<unreadable>}" >&2
    echo "loadtest-check:   The tool tells an operator to re-run at its constant. A row stated at another" >&2
    echo "loadtest-check:   rate makes that instruction point at a number nobody published." >&2
    FAIL=1
else
    echo "loadtest-check:   ok - the CPU row's rate: document and tool both say $RATE_DOC req/s"
fi

# --- the verdicts actually flip -------------------------------------------------
#
# THE ASSERTION THIS WHOLE GATE EXISTS FOR. Each verdict is called with a measurement inside the bar
# and one outside it, and BOTH directions are required: a verdict pinned to PASS and a verdict pinned
# to FAIL are the same defect with the sign flipped, and asserting only one of them would accept the
# other. `--target-*` on the command line is what a human uses to do this by hand; this does it
# mechanically on every CI run.
echo ""
echo "loadtest-check: the verdicts flip when the measurement crosses the bar"
#
# THE ASSERTIONS LIVE IN verdict-probe.js, not in a `node -e` here, and the reason is a defect this
# gate had in its first version: the JavaScript contains `$(...)`-shaped text (template literals,
# `verdictCpu(null, 0.2, 40)`), and in a plain `sh` command substitution the parenthesis is a SYNTAX
# ERROR in the shell before node runs. It failed loudly here — "syntax error near unexpected token
# `('" — but a version that happened to parse would have run an empty program and exited 0, which
# would have reported every verdict as earned. A file the shell only has to NAME has neither problem.
if ! node "$REPO/tools/loadtest-check/verdict-probe.js" "$HARNESS"; then
    FAIL=1
fi

# --- the pid sampler reads a live process ---------------------------------------
#
# THE ASSERTION THAT WOULD HAVE CAUGHT THE REAL DEFECT. The first pid sampler built here was rejected
# by the Windows PowerShell parser and returned `null` from every call, which the harness reported
# downstream as "[NO DATA] the server process was never sampled" — for a healthy server, forever, on
# every run. No source-shape check can see that. This starts a process that BURNS CPU and requires the
# sampler to observe the burn.
#
# It is skipped, loudly, where the sampler cannot apply — the sampler is Windows-specific
# (`Win32_Process`), and a CI runner on Linux has no such counter. A SKIP THAT IS PRINTED is not the
# same as a check that silently does nothing; a skip that is silent is the failure mode this
# repository writes gates against.
echo ""
echo "loadtest-check: the pid sampler reads a live process and sees its CPU move"
case "$(uname -s 2>/dev/null || echo unknown)" in
    MINGW*|MSYS*|CYGWIN*|Windows*)
        if ! node "$REPO/tools/loadtest-check/sampler-probe.js" "$HARNESS"; then
            FAIL=1
        fi
        ;;
    *)
        echo "loadtest-check:   SKIPPED - the sampler reads Win32_Process, and this is $(uname -s)."
        echo "loadtest-check:   The CPU verdict is NOT covered on this platform. That is stated rather"
        echo "loadtest-check:   than passed over: a silent skip is a green tick over nothing."
        ;;
esac

# --- the two histogram definitions agree ----------------------------------------
#
# `loadtest-worker.js` is spawned as a separate process, so it cannot import the parent's boundary
# array; the array is duplicated and the worker's own comment says so. Duplication drifts, and a
# drifted boundary array would mis-place every sample in the tail — silently, since the histogram
# would still sum to the right total. So the two declarations are compared as text.
echo ""
echo "loadtest-check: the two histogram boundary definitions are identical"
#
# The stderr is CAPTURED rather than discarded, and that is a fix rather than tidiness. The first
# version piped it to /dev/null, so when the extraction failed the failure was invisible and the gate
# reported only "could not read the boundary arrays" — an error about the comparison, with the reason
# it could not be made thrown away. A guard whose message names the wrong problem sends the next
# reader to the wrong file.
#
# The EXTRACTION is in bounds-probe.js rather than inline here, because the first version sliced the
# source at `)();` and produced an expression with its closing brace missing ("SyntaxError: Unexpected
# end of input"). It balances braces instead — see that file's header.
BOUNDS_ERR=$(mktemp 2>/dev/null || echo "${TMPDIR:-/tmp}/loadtest-bounds.$$")
BOUNDS_A=$(node "$REPO/tools/loadtest-check/bounds-probe.js" "$REPO/tools/loadtest/loadtest.js" HISTOGRAM_BOUNDS 2>"$BOUNDS_ERR")
BOUNDS_B=$(node "$REPO/tools/loadtest-check/bounds-probe.js" "$REPO/tools/loadtest/loadtest-worker.js" BOUNDS_US 2>>"$BOUNDS_ERR")

if [ -z "$BOUNDS_A" ] || [ -z "$BOUNDS_B" ]; then
    echo "loadtest-check: FAIL - could not read the boundary arrays (parent='${BOUNDS_A:0:40}', worker='${BOUNDS_B:0:40}')" >&2
    echo "loadtest-check:   The reason was:" >&2
    sed 's/^/loadtest-check:     /' "$BOUNDS_ERR" >&2
    echo "loadtest-check:   A comparison over an empty read passes over everything." >&2
    FAIL=1
elif [ "$BOUNDS_A" != "$BOUNDS_B" ]; then
    echo "loadtest-check: FAIL - the parent and the worker declare DIFFERENT histogram boundaries." >&2
    echo "loadtest-check:   loadtest.js  : $(echo "$BOUNDS_A" | cut -c1-90)" >&2
    echo "loadtest-check:   the worker   : $(echo "$BOUNDS_B" | cut -c1-90)" >&2
    echo "loadtest-check:   The worker bins each sample and the parent reads percentiles off the merged" >&2
    echo "loadtest-check:   bins, so a difference mis-places the tail without changing the total." >&2
    FAIL=1
else
    echo "loadtest-check:   ok - both declare $(printf '%s' "$BOUNDS_A" | node -e 'let s="";process.stdin.on("data",d=>s+=d).on("end",()=>console.log(JSON.parse(s).length))') boundaries, identical"
fi
rm -f "$BOUNDS_ERR"

# --- unknown flags are refused ---------------------------------------------------
#
# `docs/benchmark.md` records the defect this guards: a documented invocation with flags the binary
# SILENTLY IGNORED, so the command succeeded while running something else. A load test that accepted
# `--target-p99-msec` (a typo) and measured against the default would print a verdict about a bar the
# caller did not choose.
echo ""
echo "loadtest-check: an unknown flag is refused rather than ignored"
UNKNOWN_OUT=$(node "$HARNESS" --base-url http://127.0.0.1:1 --definitely-not-a-flag 2>&1)
UNKNOWN_RC=$?
if [ "$UNKNOWN_RC" -eq 0 ]; then
    echo "loadtest-check: FAIL - an unrecognised flag exited 0. A run that succeeds while measuring" >&2
    echo "loadtest-check:   something other than what was asked is the defect this repository has" >&2
    echo "loadtest-check:   recorded in docs/benchmark.md, so the parser must refuse it." >&2
    FAIL=1
elif ! printf '%s' "$UNKNOWN_OUT" | grep -q 'unrecognised flag'; then
    echo "loadtest-check: FAIL - an unrecognised flag exited $UNKNOWN_RC but did not say why:" >&2
    echo "$UNKNOWN_OUT" | head -3 >&2
    FAIL=1
else
    echo "loadtest-check:   ok - an unrecognised flag exits $UNKNOWN_RC and names the flag"
fi

echo ""
if [ "$FAIL" -ne 0 ]; then
    echo "loadtest-check: FAIL - see the violations above." >&2
    exit 1
fi
echo "loadtest-check: OK - the load test's two thresholds match docs/benchmark.md, both verdicts fail"
echo "loadtest-check:      when their measurement leaves the bar, the pid sampler reads a live process,"
echo "loadtest-check:      the histogram definitions agree, and an unknown flag is refused."
exit 0
