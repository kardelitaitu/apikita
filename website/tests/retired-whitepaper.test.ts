// No live document may quote the RETIRED whitepaper's figures as fact.
//
// WHY THIS REPLACED A GUARD RATHER THAN KEEPING ONE. The previous test pinned the
// whitepaper's rate card to config/apikita.toml. It worked, and it still missed the
// point: the document it guarded was a 205-line narrative about an architecture
// that was never built, so the figures could be correct while six other claims
// were false. Nothing could check prose against code, and two rounds of patching
// kept finding more. It was retired (docs/whitepaper.md) and this guard replaces
// the old one.
//
// WHAT IT PINS NOW. The retired figures live on in two legitimate places - the
// retired record itself, and the correction notes in the business docs that explain
// what was wrong. Everywhere ELSE they must not appear, because "~1,100 / ~4,400"
// quoted without its correction is exactly how a stale price reaches a customer.
//
// It also fails if a live doc re-adopts the retired document's framing, which is
// the real risk once the source is gone: people cite it from memory.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { join, relative } from 'node:path';

const root = fileURLToPath(new URL('../..', import.meta.url));

/** Every markdown file under docs/ and the top level, excluding plans (history). */
function liveDocs(dir = root, out: string[] = [], top = true): string[] {
  // website/ holds one real doc (README.md); everything else there is code.

  for (const entry of readdirSync(dir)) {
    // Scratch and build output: not documents anyone reads. .agents/ is
    // throwaway space per AGENTS.md; target/ and dist/ are artifacts.
    if (['node_modules', 'target', '.git', 'dist', '.agents'].includes(entry)) continue;
    if (entry === 'plans') continue; // archived phase plans: dated history, not live claims

    const full = join(dir, entry);
    // website/ is almost entirely code, so only its markdown is a document. Do
    // NOT skip the directory wholesale: website/README.md states figures too.
    if (entry === 'website') {
      const readme = join(full, 'README.md');
      try { if (statSync(readme).isFile()) out.push(readme); } catch { /* none */ }
      continue;
    }
    if (statSync(full).isDirectory()) liveDocs(full, out, false);
    else if (entry.endsWith('.md')) out.push(full);
  }
  return out;
}

/** Files allowed to mention the retired figures, and why. */
const ALLOWED = [
  'docs/whitepaper.md',            // the retired record: it exists to record them
  'docs/business/02-pricing.md',   // the correction note that explains them
  'docs/business/03-financial-model.md', // ditto
  'docs/business/README.md',       // ditto
  'docs/plan-audit.md',            // the audit that recommended retiring the doc
];

test('the superseded whitepaper figures appear only where they are being corrected', () => {
  const offenders: string[] = [];
  for (const file of liveDocs()) {
    const rel = relative(root, file).replace(/\\/g, '/');
    if (ALLOWED.includes(rel)) continue;
    const text = readFileSync(file, 'utf8');
    // The two figures that were uniformly wrong. "22 IDR" is too generic to match
    // safely; these two are distinctive enough to be unambiguous.
    if (/~?1,?100 IDR|~?4,?400 IDR/.test(text)) offenders.push(rel);
  }
  assert.deepEqual(
    offenders,
    [],
    'these live docs quote the retired whitepaper figures; cite business/02-pricing.md instead',
  );
});

test('the retired record names what it got wrong rather than describing the system', () => {
  const retired = readFileSync(join(root, 'docs/whitepaper.md'), 'utf8');
  // It must say it is retired, and must carry the correction table.
  assert.match(retired, /RETIRED|retired/i, 'the record must declare itself retired');
  for (const claim of ['Redis', 'hot-reload', 'tiktoken', '2.43x', 'risk-free']) {
    assert.ok(
      retired.includes(claim),
      `the retired record must name the "${claim}" error so the next reader does not repeat it`,
    );
  }
  // And it must NOT still be presenting a live rate card as fact.
  assert.ok(
    !/Consumer Price \(\+50%/.test(retired),
    'the retired record must not carry a live consumer price card',
  );
});

test('no live doc points at the whitepaper as the design of record', () => {
  // The failure mode this prevents: a doc re-introduces "see the whitepaper" for
  // behaviour, which sends a reader to a document known to be wrong.
  const offenders: string[] = [];
  for (const file of liveDocs()) {
    const rel = relative(root, file).replace(/\\/g, '/');
    if (rel === 'docs/whitepaper.md' || rel === 'docs/failover.md' || rel === 'docs/plan-audit.md') continue;
    const text = readFileSync(file, 'utf8');
    // "the whitepaper describes/specifies/promises" without a retirement marker nearby
    for (const m of text.matchAll(/whitepaper (describes|specifies|promises|defines)/gi)) {
      const at = m.index ?? 0;
      const window = text.slice(Math.max(0, at - 260), at + 260);
      if (!/retir/i.test(window)) offenders.push(rel + ' -> ' + m[0]);
    }
  }
  assert.deepEqual(offenders, [], 'live docs must mark the whitepaper as retired when citing it');
});
