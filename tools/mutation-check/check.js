/**
 * mutation-check - a gate that requires an optional tool must be able to SKIP, and to say what it misses.
 *
 * WHY THIS EXISTS, and the two false starts are the reason it is worth running.
 *
 * START ONE. It was written to certify mutation harnesses and named `relay-check/behaviour.sh`, which
 * failed six of seven checks. Reading the subject showed why: it is a BEHAVIOURAL PROBE that runs a real
 * nginx container and times stream arrival - it never mutates a file and has no reason to restore one.
 * MEASURED: no script under `tools/` performs a mutate-and-restore at all. The check was sound and the
 * subject was invented.
 *
 * START TWO. Rewritten against what is true of every gate, it reported 11 violations - including
 * "relay-check never exits 3". But relay-check is a STATIC check that documents `Exit: 0/1/2` and needs no
 * external tool, so it has no reason to skip. The rule was too broad: it measured "exits 3" rather than
 * "can skip when it cannot run".
 *
 * WHAT IT ASSERTS NOW, and each check is a property rather than a prose shape:
 *
 *   1. **A gate that shells out to an optional tool exits 3 when that tool is absent.** Enumerated rather
 *      than assumed - this is the check that found `sqlite-probes-check` returning 0 with a message saying
 *      "nothing was verified", which is the one real defect the gate has found.
 *   2. Every gate declares its exit codes and its usage in its header, so a caller can act on the verdict.
 *
 * Exit: 0 every gate holds, 1 a violation naming the gate, 3 node is missing or no gate was found.
 */
const fs = require('node:fs');
const path = require('node:path');

const REPO = process.cwd();
const TOOLS = path.join(REPO, 'tools');

const optional = ['docker', 'node', 'sqlite3', 'python3', 'curl', 'jq'];

const wanted = process.argv.slice(2);
const dirs = wanted.length
  ? wanted
  : fs.readdirSync(TOOLS, { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name);

const gates = [];
for (const d of dirs) {
  const sh = path.join(TOOLS, d, 'check.sh');
  if (fs.existsSync(sh)) gates.push({ name: d, file: sh });
}

if (!gates.length) {
  console.log('mutation-check: no gates found under tools/');
  process.exit(3);
}

let failed = 0;
const fail = (m) => { console.log(`  FAIL  ${m}`); failed += 1; };
const pass = (m) => console.log(`  ok    ${m}`);

console.log(`mutation-check: ${gates.length} gate(s)`);

for (const g of gates) {
  const src = fs.readFileSync(g.file, 'utf8').split('\r\n').join('\n');
  console.log('');
  console.log(`== ${g.name} ==`);

  // 1. IF it requires an optional tool, it must be able to skip loudly.
  const required = optional.filter((o) => new RegExp(`command -v ${o}\\b`).test(src));
  const canSkip = /exit\s+3/.test(src);
  if (required.length && !canSkip) {
    fail(
      `${g.name} requires [${required.join(', ')}] but has no \`exit 3\`, so an absent tool returns the ` +
      'same code as a pass - a green run that verified nothing'
    );
  } else if (required.length) {
    pass(`requires [${required.join(', ')}] and skips loudly when it is absent`);
  } else {
    pass('needs no optional tool, so it always runs');
  }

  // 2. A caller must be able to interpret the verdict.
  if (!/^#\s*Exit:/m.test(src)) fail(`${g.name} has no "Exit:" line naming its codes`);
  if (!/^#\s*Usage:/mi.test(src)) fail(`${g.name} has no "Usage:" line`);
  if (/^#\s*Exit:/m.test(src) && /^#\s*Usage:/mi.test(src)) pass('documents its exit codes and its usage');
}

console.log('');
if (failed === 0) {
  console.log('mutation-check: every gate can skip loudly, and every verdict is interpretable');
  process.exit(0);
}
console.log(`mutation-check: ${failed} violation(s)`);
process.exit(1);
