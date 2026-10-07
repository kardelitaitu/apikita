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

/**
 * HAZARD 4: a SINGLETON READ over a file whose match can legitimately occur more than once.
 *
 * `head -n 1` / `head -1` / `grep -m 1` / `... | sort -u | head -1` reduces a multi-match file to one
 * match, and every consumer then reasons about that value as though it were the file's value. It is
 * safe while the match is unique and silently wrong the moment it is not - and NOTHING about the code
 * changes when that happens.
 *
 * MEASURED, three times in this repository, all found by injecting a duplicate:
 *
 *   ci-docs-check  a second `- name: Build website` AFTER the tests step passed, because the FIRST
 *                  is still correctly ordered - so the step that actually runs last was unverified.
 *   alert-check    a second `N of its M alerts are covered` figure was never compared.
 *   relay-check    a zone DECLARED at two different rates was checked at the first. That one had a
 *                  comment claiming the count below would catch it; the count counts COMPARISONS, so
 *                  a duplicate ADDS one rather than removing any, and it can never fire.
 *
 * THE ASSERTION IS SITE-SPECIFIC, and that is a correction to this file's first version. A file-wide
 * pattern was defeated by the very bug it polices: `alert-check` contains a SECOND `"$x" -ne 1` at an
 * unrelated site, so deleting the uniqueness guard this list is about left the pattern matching - one
 * of many, in the tool built to catch one of many. Each entry is therefore anchored to a distinctive
 * fragment of ITS OWN guard.
 *
 * NOTE the distinction each repair makes, because a naive rule gets it wrong: a file stating the SAME
 * value twice is not an error, only one stating two DIFFERENT values is. So the assertion is
 * uniqueness of the VALUE (`sort -u | wc -l`), never uniqueness of the mention.
 */
const UNIQUENESS_ASSERTED = [
  ['tools/ci-docs-check/check.sh', /the workflow has \$n step\(s\) named/, 'the step name must occur exactly once before `head -n 1` reads it'],
  ['tools/alert-check/check.sh', /states an 'N of its M alerts are covered' figure \$N_STATED time/, 'the coverage figure must occur exactly once'],
  ['tools/relay-check/check.sh', /the relay config gives zone=\$z more than one \$label/, 'every distinct rate/burst is returned and more than one FAILS'],
];

/**
 * A read that reduces many matches to one, where the input is a FILE rather than a scalar.
 *
 * THE INPUT TEST WAS TOO NARROW IN THE FIRST VERSION, and it made two of the three entries in
 * `UNIQUENESS_ASSERTED` unreachable - so the falsification of those entries passed for the wrong
 * reason. MEASURED: `alert-check`'s coverage read is `tr ... < "$DOC" | ... | head -1`, where the
 * variable is the SECOND word of a redirect rather than the argument of the reading command, and
 * `probe.sh`'s marker read uses `"$doc"` lower-case. Neither matched a `$VAR_FILE`-shaped pattern.
 *
 * So the test is now: does the line reference ANY shell variable or quoted path at all? That is
 * deliberately loose. Being loose costs a few listed entries with reasons; being narrow cost
 * SILENCE, which is the failure this whole tool exists to prevent.
 */
const SINGLETON_READ = /head\s+(?:-n\s*)?1\b|grep\s+-m\s*1\b/;

const FILE_ISH = /\$[A-Za-z_][A-Za-z0-9_]*|"[^"]*\.[a-z]{2,4}"|'[^']*\.[a-z]{2,4}'|\b[a-z_]+\.(md|tsv|yml|yaml|conf|toml|sql)\b/;

/**
 * Where a singleton read is CORRECT because the input is not a document whose match carries meaning
 * - a JSON body from one API call, a scalar already extracted, or a LIST WHERE ONE IS THE POINT.
 *
 * Each needs its reason. Note the last kind carefully, because it is the one a reader will question:
 * `ls -1t ... | head -n 1` is not taking one of many by accident, it is taking the NEWEST of many on
 * purpose - the value is the ordering, not the match.
 */
const NOT_A_DOCUMENT = [
  [/BODY_FILE/, 'a JSON response body from one API call; the field is unique by construction'],
  [/header_zones|address_zones/, 'an already-extracted scalar, not a file'],
  [/"\$CURL_ERR"|"\$ERR"/, 'the FIRST LINE of a command\'s stderr, which is what the message wants'],
  [/ls\s+-1t.*\| head -n 1/, 'the newest file of many: the sort order IS the selection, so one of many is the intent'],
];

/**
 * Where a singleton read IS over a document, and the risk is documented rather than removed.
 *
 * This is the weaker category and it is kept separate on purpose. `UNIQUENESS_ASSERTED` means the
 * hazard cannot happen; this means it can, it was measured, and it fails LOUDLY rather than
 * silently - which is the direction that matters, since a gate that cannot see the truth and
 * reports a pass is the serious case.
 *
 * MEASURED for the one entry: `docs/deployment.md` mentions `run_wired_jobs` three times and only
 * the first is the job list. If a summary sentence were added above it, the three-line window would
 * be unrelated prose and EVERY job name would come back missing - a false positive with a message
 * about the scheduler paragraph, not a false pass.
 */
const DOCUMENTED_RISK = [
  ['tools/backup-check/check.sh', /THE ASSUMPTION THIS RESTS ON/, 'the first mention must be the list; a wrong window fails loudly, measured against three mentions'],
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
  let singletonSites = 0;

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

      // --- hazard 4: a singleton read over a file whose match can repeat ---
      if (SINGLETON_READ.test(line) && FILE_ISH.test(line)) {
        singletonSites += 1;
        const excused = NOT_A_DOCUMENT.find(([re]) => re.test(line));
        if (excused) return;
        // Has THIS file asserted uniqueness anywhere? The three known sites do it in different
        // shapes, so the test is per-file and named, not per-line.
        const known = UNIQUENESS_ASSERTED.find(([f]) => f === rel);
        if (known) {
          const [, assertion, why] = known;
          if (!assertion.test(text)) {
            problems.push(
              `${where}  this file is listed as ASSERTING UNIQUENESS (${why}) but the assertion is gone. ` +
              `A singleton read is safe only while the match is unique, and nothing in the code changes ` +
              `when it stops being so.`,
            );
          }
          return;
        }
        // A site whose risk is documented rather than removed: the note must still be there, or the
        // assumption has been dropped silently - which is how this class keeps recurring.
        const documented = DOCUMENTED_RISK.find(([f]) => f === rel);
        if (documented) {
          const [, note, why] = documented;
          if (!note.test(text)) {
            problems.push(
              `${where}  this file is listed as DOCUMENTED RISK (${why}) but the note explaining it is ` +
              `gone. An unenforced assumption that stops being written down is one nobody will check.`,
            );
          }
          return;
        }
        problems.push(
          `${where}  a singleton read over a file whose match can legitimately occur more than once.\n` +
          `       ${shown}\n` +
          `       \`head -1\`/\`grep -m1\` takes one match and every consumer then treats it as THE value ` +
          `- so a duplicate is silently ignored. MEASURED three times here: a second workflow step, a ` +
          `second coverage figure, and a zone declared at two rates were all read as the first.\n` +
          `       Either assert the VALUE is unique before using it (\`... | sort -u | wc -l\` is 1) ` +
          `and add the site to UNIQUENESS_ASSERTED, or establish some other reason the input cannot ` +
          `repeat and add it to NOT_A_DOCUMENT.`,
        );
      }
    });
  }

  for (const p of problems) console.error(`shell-hazards: ${p}`);
  console.log(
    `shell-hazards: ${files.length} script(s); ${captureSites} pipelined capture(s) ` +
      `(${VALIDATED_CAPTURES.length} validated), ${subshellSites} pipeline-subshell(s), ${setSites} set -e, ` +
      `${singletonSites} singleton read(s) over a file (${UNIQUENESS_ASSERTED.length} asserting uniqueness).`,
  );

  if (problems.length) {
    console.error(`shell-hazards: ${problems.length} hazard(s) found.`);
    process.exit(1);
  }
}

main();
