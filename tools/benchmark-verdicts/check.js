#!/usr/bin/env node
//
// benchmark-verdicts — every `[PASS]` the benchmark prints must be earned by a comparison.
//
// WHY THIS EXISTS. MEASURED over two rounds: THREE of the benchmark's four scenarios printed `[PASS]`
// from a fixed string that no measurement could change.
//
//   scenario 3  `"    Status : [PASS] 100-key router absorbs 10% throttles without client failure"`
//               printed beside `Success Rate: 37.76%`, against a documented target of >= 99.9%.
//               It would have printed PASS at 0%.
//   scenario 2  `"    Status : [PASS] Instantaneous webhook validation"` - "instantaneous" is an
//               adjective doing a number's job.
//   scenario 4  `"    Status : [PASS] 0.2 vCPU easily handles {} concurrent active streams"` - the
//               `{}` was the scenario's INPUT, not its result, so the verdict clause was fixed text.
//
// AND ONE MEASUREMENT WAS NOT ONE:
//
//   `"Estimated Socket RAM: {} MB (well within 256MB limit)", (concurrency * 35) / 1024.0`
//   Both operands are constants, and `35` is the document's own PASS CRITERION. The harness took the
//   target, multiplied it by its input, and printed the product as a passing reading. Nothing about
//   the run entered it.
//
// WHY A GUARD AND NOT A CONVENTION. The `benchmark` binary has NO test coverage - no `#[test]`, and
// the only guard coupling it to anything reads its CLI rather than its output. Three of four
// scenarios drifted the same way with nothing watching, which is what this checks.
//
// THE RULE, and it has to be stated carefully because two of the verdicts are shaped differently:
//
//   A `[PASS]`/`[WARN]`/`[BELOW ...]` line is EARNED if EITHER
//     (a) it interpolates a format placeholder, so the printed text carries a computed value, OR
//     (b) it is one arm of an if/else whose condition compares something numerically - the
//         key-auth scenario prints `[PASS] Exceeds 0.2 vCPU threshold (>10,000 ops/sec)` with no
//         placeholder, and that is correct because line 80 branches on `ops_per_sec`.
//
//   A verdict line with NEITHER is a literal, and fails.
//
// WHAT IT DOES NOT CHECK, and a reader should not over-trust it. FALSIFIED AGAINST ITS OWN SUBJECT:
//
//   * A threshold made vacuous - `if ops_per_sec >= MIN_SIGS_PER_SEC` changed to `>= 0` - is NOT
//     caught. `>= 0` is still a comparison, and this rule cannot tell a meaningful threshold from a
//     meaningless one.
//   * An `else` arm made UNREACHABLE is NOT caught, when that arm's verdict carries its value.
//     MEASURED: barring `[BELOW FLOOR] {ops_per_sec} ...` behind `if false` passes, because rule 1
//     is satisfied by the interpolation. Barring `[WARN] Below target`, which has no value, IS
//     caught. The distinction is the point: this catches a verdict that canNOT CHANGE, not one that
//     can change but can no longer be reported. Reachability is a different defect and would need
//     analysis this tool does not do.
//
// What it DOES catch is the shape that actually occurred three times: a verdict printed from a fixed
// string with no value and no branch anywhere near it. That is a narrow rule, and it is worth being
// narrow, because a rule that guessed at threshold quality would fire on correct code - the failure
// mode this repository has now recorded in four separate tools.
//
// It also cannot tell whether the value a verdict interpolates is the RELEVANT measurement. Scenario
// 4's `[PASS] {concurrency} concurrent streams finished` satisfies this rule by interpolating its
// input; what makes it a verdict is the comparison against `MIN_STREAMS_TARGET` beside it.
//
// Usage: node tools/benchmark-verdicts/check.js
// Exit: 0 every verdict is earned, 1 a literal verdict, 3 the source could not be read.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const REPO = path.resolve(__dirname, '..', '..');
const SRC = path.join(REPO, 'server', 'src', 'bin', 'benchmark.rs');

/** A line that prints a verdict. */
const VERDICT = /\[(PASS|WARN|BELOW [A-Z]+|FAIL)\]/;

/** Below this the file is not what this expects. MEASURED: 8 verdict lines across 4 scenarios. */
const FLOOR_VERDICTS = 6;

function main() {
  if (!fs.existsSync(SRC)) {
    console.error(`benchmark-verdicts: ${SRC} is missing`);
    process.exit(3);
  }
  const text = fs.readFileSync(SRC, 'utf8').split('\r\n').join('\n');
  const lines = text.split('\n');

  // The verdict lines, excluding comments (the file documents its own past defects in prose, and
  // those sentences contain `[PASS]` too).
  const verdicts = [];
  lines.forEach((l, i) => {
    if (/^\s*(\/\/|\/\*|\*)/.test(l)) return;
    if (VERDICT.test(l)) verdicts.push({ at: i + 1, line: l });
  });

  if (verdicts.length < FLOOR_VERDICTS) {
    console.error(
      `benchmark-verdicts: found ${verdicts.length} verdict line(s), floor is ${FLOOR_VERDICTS}. ` +
        `A scan that matches nothing reports every verdict as earned.`,
    );
    process.exit(3);
  }

  const problems = [];
  for (let idx = 0; idx < verdicts.length; idx += 1) {
    const v = verdicts[idx];
    const interpolates = /\{[a-zA-Z_][a-zA-Z0-9_]*/.test(v.line);

    // Is this verdict one arm of a numeric branch?
    //
    // FOUR CORRECTIONS. The first three were window-tuning; the fourth removed the need for it, and
    // the reason is worth keeping because each earlier version LOOKED right:
    //
    //   1. `/if\s+[^;{}]*[<>]=?[^;{}]*\{/` - `[^;{}]*` excludes the `{` that `{:.2?}` puts in the
    //      println! ABOVE the branch, so the scan never reached the `if` and flagged the key-auth
    //      `[PASS]`, which IS earned.
    //   2. The `else` was sought only in the BACKWARD window, but for a PASS arm it is on the line
    //      AFTER. A window ending at the verdict cannot contain it.
    //   3. A symmetric window then found the PASS arm and flagged the WARN arm, whose `if` is
    //      further behind than the window reached.
    //   4. A symmetric window ALSO let a surviving mutation through: deleting one arm's `else` was
    //      missed because ANOTHER `else` sat within reach - and in a file of if/else prints, one
    //      usually does. MEASURED: that mutation was recorded as a known gap in this tool's README.
    //
    // THE FIX IS THE BOUNDARY, not a wider window. A verdict's `else` can only lie between it and the
    // NEXT verdict - because the next verdict is by definition the sibling arm, and any later `else`
    // belongs to a block this verdict is not in. So the interval tested is the verdict's OWN block
    // plus the one before it, which covers both arms of a pair:
    //
    //   PASS arm   `if ... { println!(PASS) } else {`   -> the `else` is in its own block  (ahead)
    //   failure arm `} else { println!(FAIL) }`         -> the `else` is in the PREVIOUS block
    //
    // MEASURED across all eight verdicts in the file: each PASS arm has its `else` ahead of it and no
    // later `else` is needed; each failure arm has it in the preceding block. A window that reached
    // further was satisfied by an unrelated `else`, which is exactly the mutation that survived.
    //
    // The `if` is still sought backwards over a bounded window, because a verdict's condition sits
    // above it by however many lines the output block takes.
    const nextAt = idx + 1 < verdicts.length ? verdicts[idx + 1].at : lines.length + 1;
    const prevStart = idx > 0 ? verdicts[idx - 1].at : 0;
    const ownBlock = lines.slice(v.at - 1, nextAt - 1).join('\n');
    const prevBlock = lines.slice(prevStart - 1, v.at - 1).join('\n');
    const back = lines.slice(Math.max(0, v.at - 20), v.at).join('\n');
    const hasComparison =
      /if\s+[^\n]*[<>]=?\s*[0-9_]/.test(back) || /if\s+[^\n]*[<>]=?\s*[a-z_]/i.test(back);
    const hasSiblingArm = /else\s*(\{|if)/.test(ownBlock) || /else\s*(\{|if)/.test(prevBlock);
    const inBranch = hasComparison && hasSiblingArm;

    if (!interpolates && !inBranch) {
      problems.push(
        `${v.at}  a verdict that nothing can change:\n` +
          `       ${v.line.trim().slice(0, 100)}\n` +
          `       It interpolates no value AND sits in no numeric branch, so the same text prints ` +
          `whatever the run measured. Give it a placeholder carrying the value, or put it in an ` +
          `if/else that compares the measurement against a named threshold.`,
      );
    }
  }

  // AND THE OTHER HALF: a printed NUMBER that is arithmetic on constants is not a measurement.
  //
  // This is the shape the RAM line had. It is harder to detect in general, so this looks for the
  // specific one: a print whose placeholder is fed only by literals and the function's arguments,
  // with no variable that the run produced. The check is deliberately narrow - it names the
  // identifiers in the expression and asks whether ANY of them appears in a `let` bound from a
  // measurement (`elapsed`, `_bytes`, `_per_sec`, `_pct`).
  const suspicious = [];
  lines.forEach((l, i) => {
    if (/^\s*(\/\/|\/\*|\*)/.test(l)) return;
    // A print with an arithmetic expression whose operands are all digits or identifiers.
    const m = l.match(/""?[^"]*\{\s*[^}]*\}[^"]*"/);
    if (!m) return;
    const expr = l.match(/,\s*\(?\s*([a-z_][a-z0-9_]*)\s*\*\s*([0-9_]+)\s*\)?\s*(?:\/|\*)/i);
    if (!expr) return;
    const [, ident, num] = expr;
    const isParam = new RegExp(`\\b${ident}\\b`).test(lines.slice(0, i).join('\n'));
    const boundFromMeasurement = new RegExp(`let\\s+${ident}\\s*=\\s*[^;]*(elapsed|bytes|per_sec|pct|measured)`, 'i').test(text);
    if (isParam && !boundFromMeasurement) {
      suspicious.push(`${i + 1}  ${ident} * ${num} - if this is printed AS a measurement, both operands are constants or inputs and nothing about the run is in it.`);
    }
  });

  for (const s of suspicious) console.log(`benchmark-verdicts: note - ${s}`);
  for (const p of problems) console.error(`benchmark-verdicts: FAIL - ${p}`);

  console.log(
    `benchmark-verdicts: ${verdicts.length} verdict line(s) checked, ${problems.length} literal, ` +
      `${suspicious.length} note(s).`,
  );
  if (problems.length) process.exit(1);
}

main();
