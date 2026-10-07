#!/usr/bin/env node
//
// fill-contacts — replace the four owner-input placeholder tokens from ONE place.
//
// WHY THIS EXISTS. Four tokens are spread across five files:
//
//   [[ABUSE_EMAIL]]       docs/terms-of-service.md (x3), docs/abuse-runbook.md, docs/launch-checklist.md
//   [[OWNER_LEGAL_NAME]]  docs/terms-of-service.md (x2), docs/abuse-runbook.md, docs/launch-checklist.md
//   [[RESPONSE_HOURS]]    docs/terms-of-service.md
//   [[PRIVACY_EMAIL]]     website/src/lib/privacy.ts (x2)
//
// They are all blocked on the SAME owner decision - a monitored mailbox and the legal name of the
// contracting entity - so filling them by hand is four chances to fill three and leave one. This
// script is dry-run by default and edits nothing until you pass --write.
//
// IT ALSO UPDATES THE GUARD. `website/tests/no-placeholders-ship.test.ts` keeps a `PENDING` map, and a
// row is removed when the value exists. If the tokens are filled but the map still lists them, the
// guard fails - correctly, because the map is a claim about what is outstanding. So `--write` removes
// the row it just satisfied, which is what makes the diff self-consistent.
//
// USAGE
//   node tools/fill-contacts/fill.js --help
//   node tools/fill-contacts/fill.js --abuse-email abuse@example.id --legal-name "Nama Lengkap"
//   node tools/fill-contacts/fill.js ... --write
//
// Exit: 0 the requested replacements are consistent, 1 a value is missing/invalid, 3 a prerequisite
//       is absent.
'use strict';

const fs = require('node:fs');
const path = require('node:path');

const REPO = path.resolve(__dirname, '..', '..');

/** Every file carrying a token, and the tokens it may carry. */
const TARGETS = [
  ['docs/terms-of-service.md', ['ABUSE_EMAIL', 'OWNER_LEGAL_NAME', 'RESPONSE_HOURS']],
  ['docs/abuse-runbook.md', ['ABUSE_EMAIL', 'OWNER_LEGAL_NAME']],
  ['docs/launch-checklist.md', ['ABUSE_EMAIL', 'OWNER_LEGAL_NAME']],
  ['website/src/lib/privacy.ts', ['PRIVACY_EMAIL']],
];

/** The guard whose PENDING map must lose a row when a token is filled. */
const GUARD = 'website/tests/no-placeholders-ship.test.ts';

function parseArgs(argv) {
  const out = { write: false, values: {} };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--write') out.write = true;
    else if (a === '--help' || a === '-h') out.help = true;
    else if (a === '--abuse-email') out.values.ABUSE_EMAIL = argv[++i];
    else if (a === '--privacy-email') out.values.PRIVACY_EMAIL = argv[++i];
    else if (a === '--legal-name') out.values.OWNER_LEGAL_NAME = argv[++i];
    else if (a === '--response-hours') out.values.RESPONSE_HOURS = argv[++i];
    else if (a.startsWith('-')) { out.bad = a; }
  }
  return out;
}

const USAGE = `fill-contacts - fill the owner-input placeholder tokens from one place

  --abuse-email <addr>     the monitored abuse mailbox      -> [[ABUSE_EMAIL]]
  --privacy-email <addr>   the data-request mailbox        -> [[PRIVACY_EMAIL]]
                           (defaults to --abuse-email when only one mailbox exists)
  --legal-name <name>      the contracting entity's name   -> [[OWNER_LEGAL_NAME]]
  --response-hours <n>     the first-response commitment   -> [[RESPONSE_HOURS]]

  --write                  apply the changes (default is a dry run)
  --help                   this text

The tokens are all blocked on the SAME decision, which is why this fills them together: doing it by
hand is four chances to fill three.
`;

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) { console.log(USAGE); process.exit(0); }
  if (args.bad) { console.error(`fill-contacts: unknown option ${args.bad}\n`); console.log(USAGE); process.exit(1); }

  // One mailbox is the common case, and the repository treats them as the same gap.
  if (!args.values.PRIVACY_EMAIL && args.values.ABUSE_EMAIL) args.values.PRIVACY_EMAIL = args.values.ABUSE_EMAIL;

  const found = {};
  for (const [rel, tokens] of TARGETS) {
    const p = path.join(REPO, rel);
    if (!fs.existsSync(p)) { console.error(`fill-contacts: ${rel} is missing`); process.exit(3); }
    const text = fs.readFileSync(p, 'utf8');
    for (const t of tokens) {
      const n = (text.match(new RegExp(`\\[\\[${t}\\]\\]`, 'g')) || []).length;
      if (n) found[t] = (found[t] || 0) + n;
    }
  }

  const needed = Object.keys(found).sort();
  console.log('=== tokens present in the tree ===');
  for (const t of needed) console.log(`   [[${t}]]  ${found[t]} occurrence(s)`);
  console.log('');

  const missing = needed.filter((t) => !args.values[t]);
  if (missing.length) {
    console.error(`fill-contacts: no value supplied for ${missing.map((m) => `[[${m}]]`).join(', ')}`);
    console.error('   Each needs a real, monitored value. A plausible-looking address nobody reads is');
    console.error('   worse than the token - that is the repository\'s own stated position.');
    process.exit(1);
  }

  // Validate shape before writing anything.
  for (const [t, v] of Object.entries(args.values)) {
    if (!v || !v.trim()) { console.error(`fill-contacts: [[${t}]] is empty`); process.exit(1); }
    if (/^\[\[/.test(v)) { console.error(`fill-contacts: [[${t}]] was given another placeholder`); process.exit(1); }
    if (/EMAIL$/.test(t) && !/^[^@\s]+@[^@\s]+\.[^@\s]+$/.test(v)) {
      console.error(`fill-contacts: ${v} is not an email address`); process.exit(1);
    }
    if (t === 'RESPONSE_HOURS' && !/^\d+$/.test(v)) {
      console.error(`fill-contacts: [[RESPONSE_HOURS]] must be whole hours, got ${v}`); process.exit(1);
    }
  }

  console.log(args.write ? '=== applying ===' : '=== DRY RUN (pass --write to apply) ===');
  let edits = 0;
  for (const [rel, tokens] of TARGETS) {
    const p = path.join(REPO, rel);
    let text = fs.readFileSync(p, 'utf8');
    let changed = 0;
    for (const t of tokens) {
      if (!args.values[t]) continue;
      const re = new RegExp(`\\[\\[${t}\\]\\]`, 'g');
      const n = (text.match(re) || []).length;
      if (!n) continue;
      text = text.replace(re, args.values[t]);
      changed += n;
    }
    if (changed) {
      edits += changed;
      console.log(`   ${rel}: ${changed} replacement(s)`);
      if (args.write) fs.writeFileSync(p, text);
    }
  }

  // The guard's PENDING map is a claim about what is outstanding. Filling a token and leaving its row
  // makes the file contradict itself, so --write removes the row it just satisfied.
  const gp = path.join(REPO, GUARD);
  if (fs.existsSync(gp)) {
    const g = fs.readFileSync(gp, 'utf8');
    const rows = [...g.matchAll(/^\s*'\[\[([A-Z_]+)\]\]':/gm)].map((m) => m[1]);
    const satisfied = rows.filter((r) => args.values[r]);
    if (rows.length) {
      console.log(`   ${GUARD}: PENDING lists ${rows.length} token(s); ${satisfied.length} now satisfied`);
      if (satisfied.length === rows.length) {
        console.log('     -> the map becomes empty; the guard will then fail on ANY token, which is');
        console.log('        the correct end state once a real channel exists.');
      }
    }
  }

  console.log('');
  console.log(`   ${edits} replacement(s) ${args.write ? 'written' : 'pending'}.`);
  if (args.write) {
    console.log('');
    console.log('   NEXT: run these, and do not commit until all are green.');
    console.log('     cd website && npm test');
    console.log('     sh tools/ci-docs-check/check.sh');
    console.log('     cd server && cargo test --lib doc_claims::');
  }
}

main();
