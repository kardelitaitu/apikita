#!/usr/bin/env node
//
// shell-hazards — a gate for the shell constructs that destroy the measurement.
//
// WHY THIS EXISTS. Rounds 4-10 of this work each found a guard or harness that FAILED TO MEASURE
// what it claimed, and wrote the reason into `docs/testing.md`. Notes are not guards: the same
// mistake was made three times in three rounds by the same author, in three different forms. This
// turns the three shell-level ones into a mechanical check.
//
// THE THREE, and what each destroys:
//
//   1. `OUT=$(cmd ... | filter)` where `cmd`'s EXIT CODE is the verdict.
//      `$?` across a pipeline is the LAST command's. `tail` succeeds, so the harness sees 0 and
//      cannot fail. MEASURED in this repository: `reconcile.sh` exits 6 for a missing database,
//      and `... | tail -1` reports 0.
//
//   2. `... | while read -r x; do COUNTER=...; done`
//      The loop runs in a SUBSHELL, so the counter is lost. The message may still print, which is
//      what makes it look like it worked. MEASURED: a guard named a defect and exited 0.
//
//   3. `set -e` in a gate.
//      Every gate here runs detectors whose NON-ZERO EXIT IS THE FINDING. MEASURED: adding `-e`
//      aborts alert-check, reconcile-check and backup-check on a CLEAN tree.
//
// WHAT IT DOES NOT FLAG, and this is the whole design. Hazard 1 has FIVE live instances in this
// repository and **every one is correct** — `sha256sum | cut`, three `sqlite3 ... | head -1`, and a
// `probe.sh --list | grep -c`. A check that flagged them would be the false-positive machine this
// work has produced twice already. The distinguishing property is not "did it discard the exit code"
// but **"is the captured value validated before it is trusted"** — `[ -z "$X" ]`, `[ "$X" -lt N ]`,
// `[ "$X" != ok ]`. Each of the five does exactly that.
//
// So hazard 1 is reported only when the captured value is used WITHOUT such a check, which is the
// actual defect. The five known-good sites are listed by name, with their validation, so that
// removing a validation from one of them FAILS here rather than passing quietly.
//
// Usage: node tools/shell-hazards/check.js
// Exit: 0 all hold, 1 a hazard, 3 a prerequisite is missing.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const REPO = path.resolve(__dirname, '..', '..');
const TOOLS = path.join(REPO, 'tools');

/** Collect every .sh under tools/, excluding this tool's own directory. */
function shellFiles(dir, base = 'tools') {
  const out = [];
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    const rel = `${base}/${e.name}`;
    if (e.isDirectory()) {
      if (e.name === 'shell-hazards') continue;
      out.push(...shellFiles(p, rel));
    } else if (e.name.endsWith('.sh')) {
      out.push(rel);
    }
  }
  return out;
}

/**
 * Sites where a pipelined capture is KNOWN GOOD, with the validation that makes it so.
 *
 * A site may only be listed here if the captured value is checked before use. The check below
 * verifies that the validation is still present, so this is not an exemption list - it is a claim
 * that can go stale, and the staleness fails.
 */
const VALIDATED_CAPTURES = [
  ['tools/alert-check/check.sh', 'PROBE_NAMED', /\[\s*"\$PROBE_NAMED"\s+-lt\s+\d+\s*\]/, 'a floor, so a failed probe.sh yields 0 and fails the floor'],
  ['tools/drill/drill.sh', 'DUMP_SHA', /\[\s*-n\s+"\$\{DUMP_SHA:-\}"\s*\]/, 'an empty hash is OMITTED from the record rather than written blank'],
  ['tools/drill/drill.sh', 'SRC_SPOT', /\[\s*-z\s+"\$SRC_SPOT"\s*\]/, 'an empty result from a failed sqlite3 fails here'],
  ['tools/drill/drill.sh', 'SCR_SPOT', /\[\s*-z\s+"\$SCR_SPOT"\s*\]/, 'the same, for the restored copy'],
  ['tools/rollback/drill.sh', 'INTEG', /\[\s*"\$INTEG"\s*!=\s*"ok"\s*\]/, 'fails closed: empty is not "ok"'],
];

/** Below this the scan is not reading the tree. Measured: 19 scripts. */
const FLOOR_SCRIPTS = 15;

function main() {
  const files = shellFiles(TOOLS).sort();
  if (files.length < FLOOR_SCRIPTS) {
    console.error(`shell-hazards: found ${files.length} script(s), floor is ${FLOOR_SCRIPTS}. A scan that reads nothing passes over everything.`);
    process.exit(3);
  }

  const problems = [];
  let captureSites = 0;
  let subshellSites = 0;
  let setSites = 0;

  for (const rel of files) {
    const text = fs.readFileSync(path.join(REPO, rel), 'utf8').split('\r\n').join('\n');
    const lines = text.split('\n');

    lines.forEach((line, i) => {
      if (/^\s*#/.test(line)) return;
      const where = `${rel}:${i + 1}`;
      const shown = line.trim().slice(0, 96);

      // --- hazard 1: a pipelined capture, unless the value is validated before use ---
      const m = line.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)=\$\((.*)\)\s*$/);
      if (m && /\|/.test(m[2])) {
        const lead = m[2].split('|')[0].trim();
        if (/^(sh|bash|python3?|node|sqlite3|docker|\.\/|sha256sum|\$)/.test(lead)) {
          captureSites += 1;
          const varName = m[1];
          const known = VALIDATED_CAPTURES.find(([f, v]) => f === rel && v === varName);
          if (known) {
            // The site is only "known good" while its validation survives.
            const [, , validation, why] = known;
            if (!validation.test(text)) {
              problems.push(
                `${where}  ${varName} is listed as a VALIDATED capture (${why}) but the validation is gone. ` +
                `A captured value whose exit code was discarded must be checked before it is trusted, ` +
                `or the check cannot fail.`,
              );
            }
            return;
          }
          problems.push(
            `${where}  the capture of ${varName} pipes a command, so $? is the LAST stage's, not the detector's.\n` +
            `       ${shown}\n` +
            `       Either capture without the pipe (out=$(cmd 2>&1); rc=$?), or validate the VALUE before ` +
            `trusting it ([ -z ], a floor, or a comparison) and add the site to VALIDATED_CAPTURES with the reason.`,
          );
        }
      }

      // --- hazard 2: a flag set inside a pipeline subshell ---
      if (/\|\s*while\s+(IFS=\S+\s+)?read\b/.test(line)) {
        subshellSites += 1;
        problems.push(
          `${where}  \`| while read\` runs the loop in a SUBSHELL, so any counter or flag assigned inside is lost.\n` +
          `       ${shown}\n` +
          `       Use a here-document (\`done <<EOF\`) so the loop stays in the current shell.`,
        );
      }

      // --- hazard 3: set -e ---
      if (/^\s*set\s+-[a-z]*e/.test(line)) {
        setSites += 1;
        problems.push(
          `${where}  \`set -e\` aborts the gate the first time a detector reports what it looks for.\n` +
          `       ${shown}\n` +
          `       Every gate here runs detectors whose non-zero exit IS the finding; use \`|| true\` or an ` +
          `explicit code comparison at the call site instead.`,
        );
      }
    });
  }

  for (const p of problems) console.error(`shell-hazards: ${p}`);
  console.log(
    `shell-hazards: ${files.length} script(s); ${captureSites} pipelined capture(s) ` +
      `(${VALIDATED_CAPTURES.length} validated), ${subshellSites} pipeline-subshell(s), ${setSites} set -e.`,
  );

  if (problems.length) {
    console.error(`shell-hazards: ${problems.length} hazard(s) found.`);
    process.exit(1);
  }
}

main();
