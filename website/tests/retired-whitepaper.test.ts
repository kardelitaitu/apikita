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

/**
 * The two figures that were uniformly wrong, in every spelling a writer might use.
 *
 * THE FIRST VERSION MATCHED ONLY ONE WORD ORDER, AND THAT WAS A HOLE, MEASURED. It was
 * `/~?1,?100 IDR|~?4,?400 IDR/` - the unit must FOLLOW the number - so a live doc saying
 * "IDR 1,100 per 1M" or "1.100 IDR" (the Indonesian thousand separator, on a page about IDR) was
 * INVISIBLE: a planted citation of each left the suite at 3 pass / 0 fail, while the matched spelling
 * was caught in the same run. The comment above it names the stakes exactly - "quoted without its
 * correction is exactly how a stale price reaches a customer" - and the guard missed two ordinary
 * ways to quote it.
 *
 * So the number is matched independently of the unit, in either order, with either separator:
 *   - the thousands separator is `,` or `.` or absent
 *   - `IDR` or `Rp` may precede or follow
 *   - the leading `~` is optional, since the whitepaper wrote it and a quotation may drop it
 *
 * The alternation is deliberately NOT anchored to a currency word on one side only. A regex that
 * requires a particular word order is a regex that reads one spelling, which is the failure this
 * whole file is about one level up: a claim that looks checked and is not.
 */
const RETIRED_FIGURES =
  /(?:~?\s?(?:IDR|Rp)\s?~?\s?(?:1[.,]?100|4[.,]?400)\b)|(?:\b(?:1[.,]?100|4[.,]?400)\s?(?:IDR|Rp)\b)/;

test('the superseded whitepaper figures appear only where they are being corrected', () => {
  const offenders: string[] = [];
  for (const file of liveDocs()) {
    const rel = relative(root, file).replace(/\\/g, '/');
    if (ALLOWED.includes(rel)) continue;
    const text = readFileSync(file, 'utf8');
    const m = text.match(RETIRED_FIGURES);
    if (m) offenders.push(`${rel} -> ${m[0]}`);
  }
  assert.deepEqual(
    offenders,
    [],
    'these live docs quote the retired whitepaper figures; cite business/02-pricing.md instead',
  );
});

// The reader is a regex with a word order and a separator convention in it, so it gets pinned the way
// the citation-extension list is: by round-tripping the spellings it must catch. Without this, a
// future "simplification" back to one word order would restore the hole silently - the live docs
// happen to use none of these spellings, so the test above would keep passing.
test('the retired-figure pattern catches every spelling, not just one word order', () => {
  const catches = (s: string): boolean => RETIRED_FIGURES.test(s);
  const misses = (s: string): boolean => !RETIRED_FIGURES.test(s);

  for (const s of [
    '~1,100 IDR',   // the whitepaper's own spelling
    '~1100 IDR',    // no separator
    '1,100 IDR',
    '1.100 IDR',    // Indonesian separator, on an IDR-denominated page
    'IDR 1,100',    // unit FIRST
    'Rp 1.100',
    '~4,400 IDR',
    '4.400 IDR',
    'IDR 4,400',
  ]) {
    assert.ok(catches(s), `the retired-figure pattern no longer catches ${JSON.stringify(s)}`);
  }

  // And the negative direction, or a regex that matched everything would pass the loop above.
  for (const s of ['1,100 tokens', 'the year 4100', 'IDR 22', '4,401 IDR']) {
    assert.ok(misses(s), `the pattern is matching something it should not: ${JSON.stringify(s)}`);
  }
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

/**
 * The PARAGRAPH containing `at`, rather than a fixed character window around it.
 *
 * The window this replaced was ±260 characters, and it FAILED OPEN - which `server/src/doc_claims.rs`
 * already documents about the same technique on the Rust side:
 *
 *   "A FIXED WINDOW FAILS OPEN, measured: with a ±3 window, a planted `nosuch.rs:12` sitting beside
 *    `pub http_client: reqwest::Client` was waved through because the window contained `reqwest`."
 *
 * MEASURED here the same way. A decoy reading "the old health-check probe was retired in March",
 * placed in ITS OWN paragraph ~55 characters above "the whitepaper describes ...", was blessed at
 * 5 pass / 0 fail by the ±260 window - the word belongs to a sentence about something else, and the
 * window counted it. The paragraph is the unit a reader actually associates a claim with, so it is
 * the unit the guard uses, and the decoy is now caught.
 *
 * LINE ENDINGS ARE NORMALISED FIRST, AND THAT IS NOT COSMETIC - IT IS THE WHOLE FUNCTION. The first
 * version of this helper searched for `\n\n` directly, and these documents are CRLF. In CRLF text two
 * newlines are never adjacent (they are separated by `\r`), so BOTH searches returned -1 and the
 * function returned `text.slice(0, text.length)` - THE ENTIRE DOCUMENT. Every citation in a CRLF file
 * was therefore blessed by any occurrence of "retired" anywhere in it, which is the same fail-open the
 * character window had, one level wider. MEASURED: with the un-normalised helper a decoy in its own
 * paragraph still passed at 5 pass / 0 fail.
 *
 * Normalising inside the helper rather than at each call site, because a caller that forgets is
 * silently back to the whole-document behaviour and nothing would report it.
 */
function paragraphAround(text: string, at: number): string {
  const lf = text.replace(/\r\n/g, '\n');
  // The offset is into `text`; CRLF makes it larger than the LF position, so recompute it rather than
  // trusting the caller's index - a stale offset would split the paragraph in the wrong place.
  const atLf = lf.length === text.length ? at : text.slice(0, at).replace(/\r\n/g, '\n').length;
  const before = lf.lastIndexOf('\n\n', atLf);
  const after = lf.indexOf('\n\n', atLf);
  const body = lf.slice(before === -1 ? 0 : before, after === -1 ? lf.length : after);
  // A paragraph that is the WHOLE DOCUMENT is never a paragraph, and returning one is how this
  // helper fails open. Guarded rather than assumed.
  assert.ok(
    body.length < lf.length || lf.indexOf('\n\n') === -1,
    'paragraphAround returned the entire document, so an unrelated "retired" anywhere in it would bless every citation',
  );
  return body;
}

test('no live doc points at the whitepaper as the design of record', () => {
  // The failure mode this prevents: a doc re-introduces "see the whitepaper" for
  // behaviour, which sends a reader to a document known to be wrong.
  const offenders: string[] = [];
  for (const file of liveDocs()) {
    const rel = relative(root, file).replace(/\\/g, '/');
    if (rel === 'docs/whitepaper.md' || rel === 'docs/failover.md' || rel === 'docs/plan-audit.md') continue;
    const text = readFileSync(file, 'utf8');
    // "the whitepaper describes/specifies/promises" without a retirement marker in the SAME paragraph.
    for (const m of text.matchAll(/whitepaper (describes|specifies|promises|defines)/gi)) {
      const at = m.index ?? 0;
      if (!/retir/i.test(paragraphAround(text, at))) offenders.push(rel + ' -> ' + m[0]);
    }
  }
  assert.deepEqual(offenders, [], 'live docs must mark the whitepaper as retired when citing it');
});

// The paragraph reader is what makes the assertion above mean what it says, so it is pinned - the
// same reason the citation guard pins its extension list. Note the direction: a WIDER unit makes this
// guard MORE permissive, so a reader that swallowed the whole document would make every citation
// pass, and one that returned a single line would report every legitimate note.
test('the paragraph reader stops at blank lines, neither swallowing the document nor a line', () => {
  const doc = 'Intro paragraph.\n\nRetired in March for unrelated reasons.\n\n' +
              'The whitepaper describes billing.\n\nAnother paragraph.\n';
  const at = doc.indexOf('whitepaper describes');
  const para = paragraphAround(doc, at);

  assert.ok(para.includes('whitepaper describes'), 'the reader lost the paragraph it was asked about');
  assert.ok(
    !para.includes('Retired in March for unrelated reasons'),
    'the reader swallowed a DIFFERENT paragraph, so an unrelated "retired" would bless this citation',
  );
  assert.ok(
    !para.includes('Intro paragraph') && !para.includes('Another paragraph'),
    'the reader is not bounded by blank lines',
  );
  // The positive case: a marker in the SAME paragraph must still bless the citation.
  const marked = 'The whitepaper was retired; it describes billing.\n';
  assert.ok(
    /retir/i.test(paragraphAround(marked, marked.indexOf('describes'))),
    'a retirement marker in the same paragraph must still be recognised',
  );
});
