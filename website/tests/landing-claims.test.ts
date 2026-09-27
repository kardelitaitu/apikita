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

test('the promises that must stay are still present', () => {
  const text = read('index.astro').toLowerCase();
  // Non-refundable, the 2-year expiry, and paying balances back at closure.
  assert.ok(text.includes('non-refundable'), 'the non-refundable promise must stay');
  assert.ok(text.includes('2 years'), 'the credit-expiry term must stay stated');
  assert.ok(text.includes('pay remaining balances back'), 'the wind-down payout promise must stay');
});
