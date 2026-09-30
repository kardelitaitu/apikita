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

/**
 * The website suite's count. A LITERAL, despite what this comment used to say.
 *
 * It said "as the runner just reported it", and it never did: the runner reports the count
 * to the shell, and a test cannot read another run's output without running it, which is the
 * recursion the note above is about. So it is a hand-kept number wearing the clothes of a
 * measurement, and that matters more than being wrong, because the comment tells the next
 * person there is nothing to update.
 */
const WEBSITE_TESTS = 174;

/**
 * The server count, and the same kind of literal for the same reason.
 *
 * Update WITH the run that changes it. Both numbers were bumped together on 2026-09-30,
 * when the identity port finally made them stale in the same direction: the server went
 * 509 -> 556 because Phase 6 added the native auth routes and their tests, and the website
 * went 168 -> 171 with the auth-page migration. THE PREVIOUS NOTE SAID THEY WERE "CURRENTLY
 * BEHIND" and left it there, which is the one state this guard cannot detect: it pins the
 * documents to a number, and if the number itself is wrong then every document agrees on
 * something false and the suite stays green.
 *
 * That is the design working as intended rather than a broken guard: this file exists so
 * that ONE edit cannot update four documents and miss the fifth, and the cost of that is
 * that a stale number here is propagated consistently instead of being corrected
 * independently in five places. Bumping these is five documents and two constants, which is
 * why it has not been done opportunistically and why it should be done deliberately.
 */
const SERVER_TESTS = 582;

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
    '391 tests', '163 tests', '161 tests', '160 tests', '157 tests', '146 tests',
    '338 passed', '134 passed',
    // The pair bumped on 2026-09-30. Without these two the guard only stops a
    // number that was stale when it was WRITTEN from coming back; a number it
    // has itself retired could reappear in one document unnoticed, because
    // every check above asserts the CURRENT constant is PRESENT somewhere and
    // none of them asserts a retired one is ABSENT.
    '509 tests', '168 tests',
    // Retired on 2026-09-30 when the password-change and provider-list tests
    // landed. 556 is the figure the Phase 6 register entry still names as the
    // count AT that phase, which is why it is retired here and annotated rather
    // than rewritten in docs/decisions.md.
    '556 tests', '556 passed', '556 / 0 / 0',
    // Retired within the same session as the bump above, when the test for a
    // password change clearing outstanding reset links landed.
    '565 tests', '565 passed',
    // Retired when the doc-denial guard landed below: it is one test, and it is the
    // one that would otherwise let a document deny a gate CI runs.
    'website: **171 tests**', 'passes **171 tests**', '**171 tests**',
    // Retired when the account-lifecycle tests landed: signup answered a known
    // address differently from an unknown one, the reset pair and resend were
    // undriven, and google_sign_in had no test at all. THIRTEEN tests over the
    // five handlers that carry a new customer from an address to a working
    // sign-in, and back in again when the password is lost.
    '566 tests', '566 passed', '566 / 0 / 0',
    // Retired when the route-list guard landed: it compares the `routes!` macro
    // invocation inside `create_router` against the `ROUTES` inventory, which the
    // sibling check reads. It is the one test that catches a route added to the
    // router and to MOUNTED while `ROUTES` was left alone - a 404 for a path the
    // inventory does not admit exists.
    '579 tests', '579 passed', '579 / 0 / 0',
    // Retired when the retention-sweep gaps were closed. `identity::tokens::purge_expired`
    // had a unit test and NO caller of any kind, so expired links were never deleted; the
    // maintenance entrypoint - the thing that actually runs, since `bin/usage-purge.rs` is
    // not shipped - never mentioned `identity_tokens` OR `link_code_issues` at all, while
    // the Rust sweep deleted from both. One test now drives each end of that pair, and a
    // shell guard compares the two table lists so neither can drift alone again.
    '580 tests', '580 passed', '580 / 0 / 0',
    // Retired when the quickstart's error-code table was pinned to the code. The guard
    // found a real drift on its FIRST run: the page published 14 rows against the
    // contract's 15, so `403 forbidden` - which admin.rs::forbidden_response emits - had
    // no row for a developer to look it up in. The page is a third transcription of the
    // same table, and it was the only one nothing read.
    '581 tests', '581 passed', '581 / 0 / 0',
    // Retired when the error-code CASE SET was pinned to the published contract. Every
    // test in error-model.test.ts drives a code typed into the test, so none of them
    // noticed a code with no `case` in describeError: it falls into `default` and renders
    // "Something was wrong with that request." for a specific, actionable failure. Two
    // tests now compare the case set to the document as a SET, in both directions.
    'website: **172 tests**', 'passes **172 tests**', '**172 tests**',
    '582 tests', '582 passed', '582 / 0 / 0',
  ];
  // AN ENTRY THAT IS NOW A LIVE COUNT IS NOT SUPERSEDED, and this list has just
  // crossed that line: the tables above and below it share a numeric space, so a
  // server figure retired here can be the CURRENT website figure (or the reverse)
  // and the check would demand one document not state the count it is supposed to
  // state. Skipping rather than deleting, because the entry is still right about
  // the pair it was retired from - deleting it would drop the protection for the
  // three documents that no longer say it, to satisfy one that now must.
  const liveCounts = [String(SERVER_TESTS), String(WEBSITE_TESTS)];
  for (const stale of superseded) {
    const digits = stale.match(/\d+/)?.[0];
    if (digits !== undefined && liveCounts.includes(digits)) continue;
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
  // Vacuity: the skip above must not have swallowed the whole list.
  assert.ok(
    superseded.filter((s) => !liveCounts.includes(s.match(/\d+/)?.[0] ?? '')).length > 10,
    'the superseded list is now mostly live counts, so this test is checking almost nothing',
  );
});
/**
 * The format gate is described, not imagined.
 *
 * docs/testing.md claimed for as long as anyone could check that "The crate has no
 * `cargo fmt --check` in CI". It has had one since the workflow was added, and the
 * sentence was load-bearing: it was the stated reason the route inventory's
 * `);`-terminated lines were considered safe from a formatter. A doc that says a check
 * does not exist is a doc that tells the next reader not to run it.
 *
 * So the claim is now pinned against the workflow rather than against memory. This is
 * deliberately a ONE-WAY check: it fails when the doc denies a gate the workflow has.
 * It does not fail when the workflow gains a gate the doc never mentioned, because
 * that is the ordinary case and pinning it would mean editing this test every time a
 * stage is added.
 */
test('no doc denies a check that CI actually runs', () => {
  const workflow = read('.github/workflows/ci.yml');
  const fmtIsGated = workflow.includes('cargo fmt --check');
  const doc = read('docs/testing.md');

  if (fmtIsGated) {
    for (const denial of [
      'has no `cargo fmt --check` in CI',
      'no `cargo fmt --check`',
    ]) {
      assert.ok(
        !doc.includes(denial),
        `docs/testing.md denies a gate ci.yml runs: it still says "${denial}"`,
      );
    }
  }
});
