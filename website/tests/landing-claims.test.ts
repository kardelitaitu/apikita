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
  const text = read('signup.astro').toLowerCase();

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
test('the promises that must stay are still present', () => {
  const text = read('index.astro').toLowerCase();
  // Non-refundable, the 2-year expiry, and paying balances back at closure.
  assert.ok(text.includes('non-refundable'), 'the non-refundable promise must stay');
  assert.ok(text.includes('2 years'), 'the credit-expiry term must stay stated');
  assert.ok(text.includes('pay remaining balances back'), 'the wind-down payout promise must stay');
});
