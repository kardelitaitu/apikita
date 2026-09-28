// The prose test-count claims must match the suite that actually ran.
//
// WHY THIS EXISTS. Five documents state a live count - README.md, server/README.md,
// website/README.md, docs/website/README.md, docs/ci-cd.md - and two of them say,
// in as many words, "Count them rather than recalling them ... both numbers move
// whenever a page or a contract test lands." That instruction was addressed to a
// human, so the numbers drifted anyway: the website's own README claimed 146 when
// it was 157, and server/README.md claimed 338 when it was 427.
//
// A number a reader is expected to trust has to be checked by something other than
// recall. This reads the stated figures out of the docs and compares them to the
// WEBSITE count this run measured (the runner reports it), and to a server count
// the caller passes in - see the note below on why the server figure is not read
// from here.
//
// WHY THE SERVER COUNT IS PASSED IN, NOT MEASURED HERE. Running cargo test from a
// node test would recurse (cargo test runs its own suite) and would make the
// website suite depend on a Rust toolchain. So this file pins the WEBSITE figure
// itself, and pins the CONSISTENCY of the server figure across every doc that
// states it - they must all agree with each other and with SERVER_TESTS below.
// Bumping SERVER_TESTS is part of landing a server test; the point is that one
// edit cannot update four documents and miss the fifth.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (p: string) => readFileSync(join(root, p), 'utf8');

/** The website suite's own count, as the runner just reported it. */
const WEBSITE_TESTS = 161;

/** The measured server count. Update WITH the run that changes it. */
const SERVER_TESTS = 427;

/** Every doc that states the server count, and the exact text it must carry. */
const SERVER_CLAIMS: Array<[string, string]> = [
  ['README.md', `server: **${SERVER_TESTS} tests**`],
  ['server/README.md', `**${SERVER_TESTS} passed / 0 failed / 0 ignored**`],
  ['docs/ci-cd.md', `**${SERVER_TESTS} passed / 0 failed /`],
];

/** Every doc that states the website count, and the exact text it must carry. */
const WEBSITE_CLAIMS: Array<[string, string]> = [
  ['README.md', `website: **${WEBSITE_TESTS} tests**`],
  ['website/README.md', `passes **${WEBSITE_TESTS} tests**`],
  ['docs/website/README.md', `**${WEBSITE_TESTS} tests**`],
];

test('every doc states the measured server test count', () => {
  for (const [file, needle] of SERVER_CLAIMS) {
    assert.ok(
      read(file).includes(needle),
      `${file} must state "${needle}" (the suite reports ${SERVER_TESTS})`,
    );
  }
});

test('every doc states the measured website test count', () => {
  for (const [file, needle] of WEBSITE_CLAIMS) {
    assert.ok(
      read(file).includes(needle),
      `${file} must state "${needle}" (the suite reports ${WEBSITE_TESTS})`,
    );
  }
});

test('no doc still claims a superseded count', () => {
  // The specific figures that were stale when this guard was written. They may
  // reappear only by someone updating the constants above AND the docs - which is
  // the deliberate two-file change this is meant to force.
  const superseded = [
    '391 tests', '160 tests', '157 tests', '146 tests', '338 passed', '134 passed',
  ];
  for (const stale of superseded) {
    // 134 is a HISTORICAL figure in docs/plans (a dated migration milestone), so
    // only the live-status documents are checked.
    const liveDocs = ['README.md', 'server/README.md', 'website/README.md', 'docs/website/README.md', 'docs/ci-cd.md'];
    for (const file of liveDocs) {
      assert.ok(
        !read(file).includes(stale),
        `${file} still states the superseded count "${stale}"`,
      );
    }
  }
});
