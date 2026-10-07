#!/usr/bin/env node
//
// doc-figures — every number a document states about a config key or a RUST constant, checked
// against the code that defines it.
//
// WHY THIS EXISTS. `docs/benchmark.md` published a capacity figure of
// "connection pool size = 10" while `db::POOL_MAX_CONNECTIONS` is 8, and a pass criterion was built
// on it. Nothing compared the two, and nothing could: the citation guard that watches `docs/` checks
// LINE references, so a document excluded from it for a reason about *citations* has its **numbers**
// unread. MEASURED before this tool existed: `git log -S 'max_connections(10)' -- server/src/db.rs`
// returns no commit, so the published figure was never true.
//
// WHAT IT IS NOT. It does not replace `a_published_pool_size_is_the_pool_size_the_code_opens`, which
// reads one document against one constant. This tool is the SWEEP: it finds statements nobody has
// written a dedicated guard for. A finding here should usually become a guard.
//
// USAGE
//   node tools/doc-figures/check.js          # exit 1 on an unexplained mismatch or a short scan
//
// EXIT CODES
//   0  every comparable statement agrees with the code
//   1  a mismatch, or the scan found fewer statements than the floor
//
// The floor is the point of the exit code: a scan that reads nothing agrees with everything, and
// this tool has a filter that can hide a whole class of statement (see HYPOTHETICAL below).

'use strict';

const fs = require('node:fs');
const path = require('node:path');

const REPO = path.resolve(__dirname, '..', '..');
const CONFIG = path.join(REPO, 'config', 'apikita.toml');
const SRC = path.join(REPO, 'server', 'src');
const DOCS = path.join(REPO, 'docs');

/** Below this many comparable statements the scan is not reading the tree. Measured: 10. */
const FLOOR_COMPARABLE = 8;

/**
 * A statement is a claim about the SHIPPED value only when nothing marks it as a different setting.
 *
 * MEASURED FALSE POSITIVES, and why each is here: `config.rs` treats `0` as "off" for both
 * `credit_expiry_months` and `key_metadata_cache_seconds`, and three documents state
 * `<key> = 0` while describing that disabling value. A sweep that reads every `key = number` as a
 * shipped claim reports three drifts that are not there.
 *
 * SUBSTRINGS, not one alternation. The first version was `/\b(disabl|...)\b/i` and it silently
 * matched nothing for "disables": a trailing `\b` after the stem `disabl` requires a non-word
 * character next, and the next character is `e`.
 */
const HYPOTHETICAL = [
  'disabl', 'turns off', 'turn off', 'turning off',
  'set to 0', 'setting it to', 'if you set', 'is set to 0',
  'with the cache', 'without the cache', 'instead of', 'rather than',
  'for example', 'e.g.', 'hypothetical', 'otherwise',
];

const isHypothetical = (line) => {
  const l = line.toLowerCase();
  return HYPOTHETICAL.some((s) => l.includes(s));
};

function readConfig() {
  const text = fs.readFileSync(CONFIG, 'utf8').split('\r\n').join('\n');
  const values = new Map();
  let section = '';
  for (const line of text.split('\n')) {
    const s = line.match(/^\[([a-z_.]+)\]\s*$/);
    if (s) { section = s[1]; continue; }
    const kv = line.match(/^([a-z_]+)\s*=\s*(\d+)\s*(?:#.*)?$/);
    if (kv) {
      values.set(kv[1], +kv[2]);
      values.set(`${section}.${kv[1]}`, +kv[2]);
    }
  }
  return values;
}

function readConsts() {
  const values = new Map();
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) { if (e.name !== 'target') walk(p); }
      else if (e.name.endsWith('.rs')) {
        const t = fs.readFileSync(p, 'utf8').split('\r\n').join('\n');
        for (const m of t.matchAll(/pub const ([A-Z][A-Z0-9_]*)\s*:[^=]+=\s*([0-9_]+)\s*;/g)) {
          values.set(m[1], +m[2].replace(/_/g, ''));
        }
      }
    }
  };
  walk(SRC);
  return values;
}

function readDocs() {
  const out = [];
  const walk = (dir, base) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      const rel = base ? `${base}/${e.name}` : e.name;
      if (e.isDirectory()) walk(p, rel);
      else if (e.name.endsWith('.md')) out.push(rel);
    }
  };
  walk(DOCS, '');
  return out;
}

function main() {
  const cfg = readConfig();
  const consts = readConsts();
  const docs = readDocs();

  const rows = [];
  for (const rel of docs) {
    const text = fs.readFileSync(path.join(DOCS, rel), 'utf8').split('\r\n').join('\n');
    text.split('\n').forEach((line, i) => {
      const t = line.trimStart();
      // A blockquote or strikethrough is a correction quoting an OLD value; a `//` line is inside a
      // fenced example.
      if (t.startsWith('>') || t.startsWith('~~') || t.startsWith('//')) return;
      for (const m of line.matchAll(/\b([a-z_]{5,})\s*=\s*([0-9][0-9_,]*)/g)) {
        if (!cfg.has(m[1])) continue;
        rows.push({ rel, at: i + 1, key: m[1], stated: +m[2].replace(/[_,]/g, ''), actual: cfg.get(m[1]), line });
      }
      for (const m of line.matchAll(/\b([A-Z][A-Z0-9_]{6,})\b[^\n]{0,24}?=\s*([0-9][0-9_,]*)/g)) {
        if (!consts.has(m[1])) continue;
        rows.push({ rel, at: i + 1, key: m[1], stated: +m[2].replace(/[_,]/g, ''), actual: consts.get(m[1]), line });
      }
    });
  }

  let mismatches = 0;
  let skipped = 0;
  for (const r of rows) {
    if (r.stated === r.actual) { console.log(`   ok    ${r.rel}:${r.at}  ${r.key} = ${r.stated}`); continue; }
    if (isHypothetical(r.line)) {
      skipped += 1;
      console.log(`   hypo  ${r.rel}:${r.at}  ${r.key} ${r.stated} (code: ${r.actual}) — a non-shipped setting`);
      continue;
    }
    mismatches += 1;
    console.log(`   *** MISMATCH ${r.rel}:${r.at}  ${r.key} states ${r.stated}, the code has ${r.actual}`);
    console.log(`       ${r.line.trim().slice(0, 100)}`);
  }

  console.log('');
  console.log(`   ${docs.length} document(s), ${rows.length} comparable statement(s), ${skipped} hypothetical, ${mismatches} mismatch(es)`);

  if (rows.length < FLOOR_COMPARABLE) {
    console.log(`   FAIL: ${rows.length} comparable statement(s), floor is ${FLOOR_COMPARABLE}. A scan that matches nothing passes over everything.`);
    process.exit(1);
  }
  if (mismatches > 0) process.exit(1);
}

main();
