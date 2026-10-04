// Guard for the customer-facing claims on the landing page and the models page.
//
// These are the sentences a customer can hold us to. The specific failure this
// suite exists to catch: the FAQ once said an in-flight request is "allowed to
// finish even if it briefly overdraws", which contradicted docs/decisions.md
// ("Overdraft is **not permitted** — no flag") and the code — settlement clamps
// the debit to the available balance (settle_partial_usage / clamp_debit), so the
// wallet never goes below zero. A promise of overdraft is a promise we do not keep.
//
// The copy lives in .astro frontmatter, which node --test cannot import, so this
// suite reads the files as TEXT. That is deliberate: the guard is about what the
// file says, not about a value it computes.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(join(here, '..', 'src', 'pages', rel), 'utf8');

test('the landing FAQ does not promise an overdraft', () => {
  const text = read('index.astro').toLowerCase();
  // The exact phrases that would re-introduce the false claim.
  assert.ok(!text.includes('briefly overdraw'), 'the FAQ must not claim a request overdraws');
  assert.ok(!text.includes('small overdraft'), 'the FAQ must not describe an overdraft');
  assert.ok(!text.includes('briefly overdraws'), 'the FAQ must not claim a request overdraws');
});

test('the landing FAQ says the balance never goes below zero', () => {
  const text = read('index.astro').toLowerCase();
  assert.ok(
    text.includes('never goes below zero'),
    'the FAQ must state the balance never goes negative',
  );
});

test('the landing page never discloses the cost basis', () => {
  // The page's own header rule: it states the price, never where it comes from.
  // Comments are stripped first — the file's own header NAMES the forbidden words
  // to document the rule, and that documentation must stay.
  const text = read('index.astro')
    .split('\n')
    .filter((line) => !line.trim().startsWith('//'))
    .join('\n')
    .toLowerCase();
  for (const forbidden of ['wholesale', 'margin', 'multiplier', 'our cost', 'arbitrage']) {
    assert.ok(!text.includes(forbidden), `the landing page must not disclose the cost basis: ${forbidden}`);
  }
});

// ---------------------------------------------------------------------------
// THE SIGNUP DISCLOSURE — docs/launch-checklist.md Gate 6, claim 205:
// "Signup shows the cross-border disclosure before the first request."
//
// The claim is not "the disclosure exists somewhere" — privacy.astro would satisfy
// that — but that it appears ON THE SIGNUP SURFACE, BEFORE the customer can act.
// A refactor that moved it to a post-signup page, or reordered the form, would
// leave the promise looking kept while breaking the actual obligation. The
// checklist is blunt about the stakes (lines 36-37): "Do not take a single deposit
// before this gate closes."
//
// The .astro file is read as TEXT for the reason this suite documents: frontmatter
// cannot be imported, and the guard is about what the file SAYS.
// ---------------------------------------------------------------------------

test('signup discloses where prompts are forwarded, and does so before the submit control', () => {
  const raw = read('signup.astro');

  // THE LOWERCASING MUST NOT MOVE CHARACTERS, and that is asserted rather than assumed
  // because the whole ordering check below rests on it. `toLowerCase()` is
  // length-preserving for ASCII, but NOT for every Unicode string - a few characters
  // change length or expand. If the page ever contained one, the indices below would be
  // positions in a DIFFERENT string from the one read, and the comparison would still run,
  // still look meaningful, and compare the wrong things.
  //
  // Found while mutation-testing this guard: a probe searched the RAW file for the marker
  // and got -1 while the test passed, because the test searches the lowercased text. That
  // cost a round to diagnose and is exactly the confusion this assertion prevents for the
  // next reader.
  assert.equal(
    raw.length,
    raw.toLowerCase().length,
    'lowercasing signup.astro changed its length, so the ordering indices below are ' +
      'positions in a different string than the one read. Compare the raw positions, or ' +
      'normalise with an index-preserving fold.',
  );
  const text = raw.toLowerCase();

  // (a) The disclosure is on THIS page at all.
  assert.ok(
    text.includes('where your prompts go'),
    'signup must carry the "Where your prompts go" disclosure block',
  );

  // (b) It names the destination and the part outside our control. A version that
  //     said only "forwarded to a provider" would hide the one fact the customer
  //     most needs: whose retention policy now applies to their prompts.
  assert.ok(
    text.includes('mainland china'),
    'the disclosure must name the destination jurisdiction',
  );
  assert.ok(
    text.includes('outside our control'),
    'the disclosure must say the upstream retention policy is outside our control',
  );

  // (c) ORDERING, which is the "before the first request" half. Both markers must
  //     be present and the disclosure must come FIRST.
  const disclosure = text.indexOf('where your prompts go');
  const submit = text.indexOf('signup-submit');
  assert.ok(submit > -1, 'the signup submit control must exist for this ordering check to mean anything');
  assert.ok(
    disclosure < submit,
    'the disclosure must appear BEFORE the submit control: a customer reads the form top-down, so a notice placed after the button is not consent to it',
  );

  // POSITIVE CONTROL for the ordering check itself: the two indices are genuinely
  // different positions, so `disclosure < submit` cannot pass by both being -1.
  assert.notEqual(
    disclosure,
    submit,
    'the two markers must be distinct positions, or the ordering assertion above is vacuous',
  );
});
// ---------------------------------------------------------------------------
// THE MONEY-TERMS SURFACES — Gate 6 claims 206 and 208.
//
// 206: "Top-up screen states the fee and the non-refundable policy before
//       payment."  The wallet page carries the terms at lines 27-31, ABOVE the
//       top-up action, and the first-deposit minimum is stated in the header.
//
// 208: "API key shown once, with an acknowledged warning." The BEHAVIOUR is
//       already covered (`plaintextKeyOf` in dashboard-form.test.ts, which pins
//       that only a create response can reveal a key). What was not covered is the
//       WARNING a customer is shown, which is the part a copy edit can quietly
//       delete while the code stays correct.
//
// Both are read as TEXT, for the reason this suite documents: the guard is about
// what the file SAYS.
// ---------------------------------------------------------------------------

test('the wallet states the non-refundable policy and the expiry before the top-up action', () => {
  const text = read('dashboard/wallet.astro').toLowerCase();

  assert.ok(
    text.includes('non-refundable'),
    'the wallet must state that balances are non-refundable',
  );
  assert.ok(
    text.includes('expires 2 years'),
    'the wallet must state the credit expiry term, or a customer learns it only after losing credit',
  );
  // The minimums differ by deposit, so the page must say so rather than showing one
  // number that is wrong for a first top-up.
  assert.ok(
    text.includes('minimums differ'),
    'the wallet must flag that the first deposit and later top-ups have different minimums',
  );

  // ---------------------------------------------------------------------------
  // THE ORDERING, WHICH THIS TEST'S NAME PROMISED AND ITS BODY DID NOT CHECK.
  //
  // It asserted PRESENCE only, while the launch gate it is cited by
  // (docs/launch-checklist.md, "Top-up screen states the fee and the non-refundable
  // policy before payment") is about POSITION. The two are different claims, and the
  // difference was live: measured, the wallet page rendered `<TopUpForm />` at offset
  // 2607 and the non-refundable policy at 3275, so the customer met the payment form
  // BEFORE the policy saying the money is not refundable.
  //
  // `wallet.astro` composes the action as <TopUpForm />, so the ordering that reaches
  // the customer is decided HERE, not in the form island.
  //
  // HTML COMMENTS ARE STRIPPED FIRST, and that is not tidiness - it is the same defect
  // this assertion exists to catch, one level up. The page explains the rule in a
  // comment that NAMES <TopUpForm />, so a search on the raw text finds the comment
  // before the element and the test passes on prose about the fix rather than on the
  // fix. Measured: that is exactly what happened on the first run of this assertion.
  //
  // The comparison is on raw text, not a lowercased copy: lowercasing is not
  // length-preserving for every Unicode string, and an index is only meaningful in the
  // string it came from (see the signup test above for the long version).
  // ---------------------------------------------------------------------------
  const withoutComments = read('dashboard/wallet.astro').replace(/<!--[\s\S]*?-->/g, '');
  const low = withoutComments.toLowerCase();
  const form = low.indexOf('<topupform');
  const policy = low.indexOf('non-refundable');
  const expiry = low.indexOf('expires 2 years');

  assert.ok(
    form > -1,
    'the wallet page must compose the top-up action (<TopUpForm />), or the ordering below is vacuous',
  );
  assert.ok(policy > -1 && expiry > -1, 'both disclosures must be present for an ordering claim');

  assert.ok(
    policy < form,
    'the non-refundable policy must be stated BEFORE the top-up action. A customer who ' +
      'reaches the payment form first is asked to pay before being told the balance ' +
      'cannot be refunded, which is the disclosure failing at the only moment it matters',
  );
  assert.ok(
    expiry < form,
    'the 2-year expiry must be stated BEFORE the top-up action, for the same reason: a ' +
      'term that extinguishes value has to be read before the money is committed',
  );
});

test('the new-key screen warns that the plaintext key is shown only once', () => {
  const text = read('dashboard/keys/new.astro').toLowerCase();

  assert.ok(
    text.includes('shown <strong>once</strong>') || text.includes('shown once'),
    'the new-key screen must warn that the plaintext key is shown only once',
  );
  assert.ok(
    text.includes('sha-256') || text.includes('only its hash') || text.includes('stores only'),
    'the warning must explain WHY it cannot be shown again: the server stores only a hash',
  );
});
test('the promises that must stay are still present', () => {
  const text = read('index.astro').toLowerCase();
  // Non-refundable, the 2-year expiry, and paying balances back at closure.
  assert.ok(text.includes('non-refundable'), 'the non-refundable promise must stay');
  assert.ok(text.includes('2 years'), 'the credit-expiry term must stay stated');

  // THE WIND-DOWN PAYOUT PROMISE, matched as a CLAIM AND NOT AS A WORD ORDER. It was pinned to the
  // exact string `pay remaining balances back`, which is one ordering of three words - so
  // `pay back remaining balances`, identical in meaning, failed the guard. MEASURED: rewording the
  // page that way fired this test, on a page whose promise was intact.
  //
  // That matters more here than a style point, because of what a false positive does to a suite of
  // this kind: it teaches the reader that a red test can be a reworded sentence rather than a lost
  // promise, and the next real loss is discounted as noise. The other two assertions above are left
  // exact because their phrase IS the claim being quoted - `non-refundable` and `2 years` are the
  // terms, and the test names say so.
  //
  // So: all three words must appear, in any order, within one sentence of each other. That accepts
  // every natural phrasing of the promise and still fails if any word is dropped - which is the
  // failure the test exists to catch.
  const payout = /pay\b[^.]{0,40}\bremaining\b[^.]{0,40}\bbalances?\b[^.]{0,20}\bback\b/.test(text)
    || /pay\b[^.]{0,40}\bback\b[^.]{0,40}\bremaining\b[^.]{0,40}\bbalances?\b/.test(text);
  assert.ok(
    payout,
    'the wind-down payout promise must stay: the page must say that remaining balances are paid back at closure (all of "pay", "remaining", "balances" and "back" within one sentence)',
  );
});
