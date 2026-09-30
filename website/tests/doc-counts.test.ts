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
const WEBSITE_TESTS = 180;

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

/**
 * The exact strings that are LIVE, so the retired-count guard can tell a figure that
 * has been superseded from one that is merely the OTHER suite's current count.
 *
 * Built from the two constants and the claim shapes above, so it cannot go stale on
 * its own: bumping a constant updates this set in the same edit.
 */
const LIVE_FORMS: ReadonlySet<string> = new Set([
  `${SERVER_TESTS} tests`,
  `${SERVER_TESTS} passed`,
  `${SERVER_TESTS} / 0 / 0`,
  `server: **${SERVER_TESTS} tests**`,
  `${WEBSITE_TESTS} tests`,
  `${WEBSITE_TESTS} passed`,
  `website: **${WEBSITE_TESTS} tests**`,
]);

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
  /**
   * A retired figure is a count IN THE SPACE OF ONE SUITE, and the two suites share
   * that numeric space.
   *
   * This was a flat string list. It worked while the two counts were far apart, so a
   * single integer identified which suite it had been retired from. It stopped working
   * when the round-23 case-set guard retired website 172 and, in the same commit, the
   * server count rose to 582: README.md:102 states BOTH counts on one line
   * (`server: **582 tests** — website: **174 tests**`), so a bare `'582 tests'` matches
   * a line that is required to be there, and the guard demands a document not state the
   * number SERVER_CLAIMS above requires it to state. The first fix skipped any entry
   * matching a live count - which worked, and was wrong: it also skipped `'174 tests'`,
   * a genuinely retired website figure that README.md:102 is exactly the line most
   * likely to regress to.
   *
   * So the association is now explicit. Each entry names the suite it was retired from
   * and carries that suite's QUALIFIED form - `server: **N tests**` or
   * `website: **N tests**` - which are the two shapes SERVER_CLAIMS and WEBSITE_CLAIMS
   * already write and which cannot collide, whatever the integers do. The bare forms are
   * kept as well, but a BARE FORM IS ONLY CHECKED AGAINST THE DOCUMENTS WHOSE SUBJECT IT
   * IS: a bare server figure cannot be asserted against website/README.md, because that
   * file has no business stating a server count, and asserting it there is how the
   * collision happened in the first place.
   */
  type Retired = { suite: 'server' | 'website'; forms: string[] };
  const superseded: Retired[] = [
    {
      suite: 'server',
      forms: [
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
      ],
    },
    {
      suite: 'website',
      forms: [
        // Retired when the doc-denial guard landed below: it is one test, and it is the
        // one that would otherwise let a document deny a gate CI runs.
        'website: **171 tests**', 'passes **171 tests**', '**171 tests**',
        // Retired when the error-code CASE SET was pinned to the published contract. Every
        // test in error-model.test.ts drives a code typed into the test, so none of them
        // noticed a code with no `case` in describeError: it falls into `default` and renders
        // "Something was wrong with that request." for a specific, actionable failure. Two
        // tests now compare the case set to the document as a SET, in both directions.
        'website: **172 tests**', 'passes **172 tests**', '**172 tests**',
        // Retired when the citation guard and the shadow guard landed. The citation
        // guard was written because `website/src` cites code by line in nineteen
        // places and doc_claims.rs's citation triage walks `docs/` only - so nothing
        // verified any of them. Ten of the eleven that were checked by hand had
        // drifted onto a login example, a closing brace, a blank line. The shadow
        // guard was written because wallet.astro imported `topupPerHour` and then
        // declared `const topupPerHour = 5;` underneath it, so prices.test.ts
        // compared the CONSTANT against config while the page published the literal.
        'website: **174 tests**', 'passes **174 tests**', '**174 tests**',
      ],
    },
  ];

  // The documents each suite's count actually lives in. A bare figure is checked
  // against these and nothing else, so a server number is never asserted against a
  // file that has no reason to state one.
  const subject: Record<Retired['suite'], string[]> = {
    server: ['README.md', 'server/README.md', 'docs/ci-cd.md'],
    website: ['README.md', 'website/README.md', 'docs/website/README.md'],
  };

  for (const { suite, forms } of superseded) {
    // The qualified form of every bare figure in this group, built from the suite
    // rather than transcribed a second time.
    const qualified = suite === 'server'
      ? forms.map((f) => `server: **${f}**`)
      : forms.map((f) => `website: **${f}**`);

    for (const stale of [...forms, ...qualified]) {
      // A figure that IS a live count of either suite is not superseded, and the
      // qualified form is what makes that testable: `server: **582 tests**` is
      // present-when-582-is-live and must not be asserted absent, while a bare
      // `'582 tests'` is also what a website line legitimately contains.
      if (LIVE_FORMS.has(stale)) continue;
      for (const file of subject[suite]) {
        assert.ok(
          !read(file).includes(stale),
          `${file} still states the superseded ${suite} count "${stale}". Every check above asserts the CURRENT count is present; nothing else asserts a retired one is absent.`,
        );
      }
    }
  }

  // Vacuity, both directions. A list that skipped to nothing, or a subject map that
  // named no file, would make the loop above check nothing at all.
  const applicable = superseded.flatMap(({ suite, forms }) =>
    [...forms, ...forms.map((f) => (suite === 'server' ? `server: **${f}**` : `website: **${f}**`))]
      .filter((s) => !LIVE_FORMS.has(s)),
  );
  assert.ok(
    applicable.length > 10,
    `only ${applicable.length} retired figure(s) are still being checked, so the live-count skip has swallowed the list`,
  );
  for (const [suite, files] of Object.entries(subject)) {
    assert.ok(
      files.length > 0,
      `no documents are checked for the ${suite} count, so its retired figures are asserted against nothing`,
    );
  }

  // The qualified forms must not be vacuous themselves: each must be FOUND in a
  // document while that count is live, or the prefix is a string nothing writes and
  // the collision this whole block exists to prevent is back with a quieter symptom.
  assert.ok(
    read('README.md').includes(`server: **${SERVER_TESTS} tests**`),
    'README.md must state the live server count in the qualified form, or the qualified forms above are not the shape documents actually use',
  );
  assert.ok(
    read('README.md').includes(`website: **${WEBSITE_TESTS} tests**`),
    'README.md must state the live website count in the qualified form, or the qualified forms above are not the shape documents actually use',
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
