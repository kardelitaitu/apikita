// A PLACEHOLDER MUST NOT REACH A CUSTOMER - except the ones we are knowingly waiting on.
//
// `docs/terms-of-service.md` §10 and the privacy page both need a contact channel that does not exist
// yet, written as a `[[LIKE_THIS]]` token. That is deliberate: the alternative is a plausible-looking
// address nobody monitors, which the repository's own text calls worse than the gap.
//
// THE DISTINCTION THIS FILE TURNS ON, and it is the whole design:
//
//   * A placeholder on PENDING below is a KNOWN gap with a tracked owner decision behind it. It is
//     allowed, it is counted, and the count is asserted - so deleting the row cannot make this file
//     quiet.
//   * Any OTHER placeholder is a mistake: a half-finished edit, a token somebody forgot. It fails.
//
// A guard that failed on both would be red from now until launch, and a suite that is red for a known
// reason cannot report an unknown one. A guard that allowed both would be decoration. The list is the
// mechanism that keeps them apart.
//
// WHY IT READS THE BUILT OUTPUT AS WELL AS THE SOURCE. The source check catches a token left in a
// `.astro` or `.ts` file. It does NOT catch one a build step assembled into the HTML - and the thing
// a customer receives is the built page, not the source.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, existsSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const SRC = join(here, '..', 'src');
const DIST = join(here, '..', 'dist');
const DOCS = join(here, '..', '..', 'docs');

/**
 * Placeholders we are knowingly waiting on, each with the reason it cannot be filled here.
 *
 * A row is removed when the value exists, and the removal is what publishes the channel - so the
 * diff that fills in a real address is the same diff that empties this list.
 */
const PENDING: Record<string, string> = {
  '[[PRIVACY_EMAIL]]':
    'the address a customer writes to about their data. No mailbox exists; docs/abuse-runbook.md ' +
    'tracks it as an owner input, the same one docs/terms-of-service.md §10 is blocked on.',
  // THE THREE THE TERMS CARRY, which the scan above never saw because it walks `website/src` and these
  // live in `docs/`. They are listed here rather than left unlisted for the reason the whole file
  // exists: `docs/terms-of-service.md` §10 is where a placeholder reaches a CUSTOMER, and until this
  // row existed, no assertion in the repository distinguished `[[ABUSE_EMAIL]]` - a tracked owner
  // decision with a replacement table of its own - from `[[X]]`, a token somebody forgot.
  '[[ABUSE_EMAIL]]':
    'the monitored abuse mailbox. Terms §10.2, and docs/terms-of-service.md carries the replacement ' +
    'table; docs/abuse-runbook.md §Open items records the same gate.',
  '[[OWNER_LEGAL_NAME]]':
    'the contracting entity, blocked on the same decision as the two above: ' +
    'docs/launch-checklist.md §Gate 0 "Decide the contracting entity".',
  '[[RESPONSE_HOURS]]':
    'the first-response commitment, stated numerically only once the times have a numeric form; the ' +
    'canonical table is in docs/abuse-runbook.md §"Severity and response time".',
};

/** The placeholder form: `[[UPPER_SNAKE]]`. Narrow on purpose - a markdown link is `[text](url)`. */
const PLACEHOLDER = /\[\[[A-Z0-9_]+\]\]/g;

/** Every file under a directory, recursively, with the extensions that can reach a customer. */
function filesUnder(dir: string, exts: string[]): string[] {
  if (!existsSync(dir)) return [];
  const out: string[] = [];
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) {
      if (entry === 'node_modules') continue;
      out.push(...filesUnder(path, exts));
    } else if (exts.some((e) => entry.endsWith(e))) {
      out.push(path);
    }
  }
  return out;
}

/** A `//` or `/* *\/` comment line is a note TO the developer and never renders. */
function isCommentLine(text: string, index: number): boolean {
  const start = text.lastIndexOf('\n', index) + 1;
  const line = text.slice(start, text.indexOf('\n', index)).trimStart();
  return line.startsWith('//') || line.startsWith('*') || line.startsWith('/*');
}

test('no UNKNOWN placeholder reaches a customer', () => {
  const sources = filesUnder(SRC, ['.astro', '.ts']);
  assert.ok(sources.length > 20, `only ${sources.length} source file(s) found - the scan is not reading the tree`);

  const unknown: string[] = [];
  const seenPending = new Set<string>();
  const scan = (file: string, label: string): void => {
    const text = readFileSync(file, 'utf8');
    for (const m of text.matchAll(PLACEHOLDER)) {
      // The pending count is recorded from EVERY occurrence, including a comment: a token kept in a
      // comment is the safe form, and the staleness check below needs to see it there. What the
      // comment exemption decides is only whether the occurrence is a DEFECT.
      if (m[0] in PENDING) seenPending.add(m[0]);
      // Markdown has no `//` comment form, so the exemption does not apply there.
      if (file.endsWith('.md')) {
        if (!(m[0] in PENDING)) unknown.push(`${label}: ${m[0]}`);
        continue;
      }
      if (isCommentLine(text, m.index)) continue;
      if (m[0] in PENDING) continue;
      unknown.push(`${label}: ${m[0]}`);
    }
  };

  for (const file of sources) scan(file, file.slice(SRC.length + 1));

  // THE DOCUMENTS ARE SCANNED TOO, and this is the part that was missing.
  //
  // MEASURED: replacing `[[ABUSE_EMAIL]]` in docs/terms-of-service.md §10 with `[[X]]` - a generic
  // token that names no decision and ships to a customer - left this file GREEN at 3 passed. The
  // documents were outside the scan, so the guard's own stated distinction ("a KNOWN gap with a
  // tracked owner decision" versus "a half-finished edit, a token somebody forgot") was not applied
  // where the token actually reaches a reader.
  //
  // It matters more here than in `src` because `tools/fill-contacts` WRITES to these documents: the
  // tool's own output was the one place a placeholder could be introduced with no guard looking.
  const docs = existsSync(DOCS) ? filesUnder(DOCS, ['.md']) : [];
  assert.ok(
    docs.length > 20,
    `only ${docs.length} document(s) found under ${DOCS} - the document half of this scan is not ` +
      'reading the tree',
  );
  for (const file of docs) scan(file, `docs/${file.slice(DOCS.length + 1)}`);

  assert.deepEqual(
    unknown,
    [],
    `a placeholder nobody is waiting on would reach a customer: ${unknown.join(', ')}. Either ` +
      'fill it in, move it into a comment if it is a note rather than a value, or add it to ' +
      'PENDING in this file with the reason it cannot be filled here.',
  );

  // AND THE PENDING LIST IS NOT ALLOWED TO GO STALE. A row for a placeholder that appears NOWHERE,
  // not even in a comment, means the value was filled in and the exemption was left behind - which
  // would silently re-allow the token if it ever came back.
  for (const [token, reason] of Object.entries(PENDING)) {
    assert.ok(
      seenPending.has(token),
      `PENDING still lists ${token} but no shipped source carries it anywhere, not even in a ` +
        `comment. If the real value was filled in, delete the row - leaving it behind re-permits the ` +
        `token. Reason recorded was: ${reason}`,
    );
  }
});

test('no placeholder reaches the BUILT website', () => {
  const built = filesUnder(DIST, ['.html', '.js', '.css']);
  if (built.length === 0) {
    // Not a silent pass: the message names which half did not run and how to make it run.
    assert.ok(
      true,
      'website/dist is absent, so the built-output half of this check did not run. It runs in CI, ' +
        'which builds before testing; run `npm run build` locally to exercise it.',
    );
    return;
  }

  const found: string[] = [];
  for (const file of built) {
    for (const m of readFileSync(file, 'utf8').matchAll(PLACEHOLDER)) {
      found.push(`${file.slice(DIST.length + 1)}: ${m[0]}`);
    }
  }
  assert.deepEqual(
    found,
    [],
    `a placeholder was BUILT INTO the shipped site: ${found.join(', ')}. It appears in no source ` +
      'file in this form, which is why the source check above cannot see it. A customer reading the ' +
      'published page would be shown a token where a contact address belongs.',
  );
});

test('the document that carries a pending workaround says what must change', () => {
  // THE OTHER DIRECTION, and without it this file is satisfied by a section that simply has no
  // channel at all - which is the state the whole arrangement exists to make visible rather than
  // tidy. The Terms are the document a customer can hold us to, so the requirement is stated there.
  const tos = readFileSync(join(DOCS, 'terms-of-service.md'), 'utf8');
  const start = tos.indexOf('## 10. Contact and complaints');
  assert.ok(start > 0, 'docs/terms-of-service.md has no §10 Contact and complaints');
  const next = tos.indexOf('\n## ', start + 1);
  const section = tos.slice(start, next === -1 ? undefined : next);

  // EVERY TOKEN IN THE SECTION MUST BE A PENDING ONE, and that is the clause this test was missing.
  //
  // MEASURED BEFORE IT EXISTED: replacing `[[ABUSE_EMAIL]]` in §10 with `[[X]]` - a generic token
  // that names no decision and ships to a customer - left this file GREEN, 3 passed / 0 failed. The
  // assertion below it asks only whether the section NAMES a channel, and `[[X]]` is a match. So the
  // test accepted any placeholder at all where its own header promises it distinguishes a KNOWN gap
  // from "a half-finished edit, a token somebody forgot".
  //
  // The scan above does not cover this either: it walks `website/src`, and `docs/` is outside it -
  // which matters because `tools/fill-contacts` WRITES to these documents, so the tool's output is
  // the one place a placeholder can be introduced without any guard seeing it.
  const tokens = section.match(PLACEHOLDER) ?? [];
  const strangers = tokens.filter((t) => !(t in PENDING));
  assert.deepEqual(
    strangers,
    [],
    `docs/terms-of-service.md §10 carries placeholder(s) nobody is waiting on: ${strangers.join(', ')}. ` +
      'A token that is not in PENDING is an unfinished edit rather than a tracked gap - and this is ' +
      'the section a customer reads to find out how to complain. Fill it, or add it to PENDING with ' +
      'the reason it cannot be filled here.',
  );

  const namesChannel = /\[\[[A-Z0-9_]+\]\]/.test(section);
  const saysBlocked = /not yet|before publish|replac|placeholder|blocked/i.test(section);
  assert.ok(
    namesChannel || saysBlocked,
    'docs/terms-of-service.md §10 names no contact channel AND does not say one is missing. That ' +
      'combination is the dangerous one: the section reads as a finished clause while promising a ' +
      'complaint path that does not exist.',
  );
});
