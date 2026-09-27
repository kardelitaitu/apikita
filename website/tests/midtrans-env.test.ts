// Executable contract for the browser's Midtrans environment cross-check.
//
// The page used to decide this by guessing from the client key's prefix
// (`SB-Mid-client-` = sandbox, `Mid-client-` = production) — a rule Midtrans
// does not document, with no client-key sample anywhere in this repo. The server
// now reports the environment it actually used (`CreateTopupResponse.environment`,
// server/src/routes/account.rs), so the page compares two facts instead of
// trusting a prefix on a money path. `midtransEnvMismatch` (src/lib/midtrans-env.ts)
// is the exact function the island calls; it lives outside the `.astro` block so
// this suite can load it.
//
// The load-bearing case is the LAST one: an absent server value must fail OPEN.
// Cloudflare Pages is deployed independently of the API, so a Pages-first deploy
// — or a Pages build against an older server — sends no `environment`. Treating
// that as a mismatch would hard-block every top-up, turning a cosmetic config lag
// into a total payment outage.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { midtransEnvMismatch } from '../src/lib/midtrans-env.ts';

test('a sandbox build against a production server is a mismatch', () => {
  assert.equal(midtransEnvMismatch('sandbox', 'production'), 'mismatch');
});

test('a production build against a sandbox server is a mismatch', () => {
  assert.equal(midtransEnvMismatch('production', 'sandbox'), 'mismatch');
});

test('agreement on either environment is not a mismatch', () => {
  assert.equal(midtransEnvMismatch('sandbox', 'sandbox'), null);
  assert.equal(midtransEnvMismatch('production', 'production'), null);
});

test('an absent server value fails OPEN — undefined must never block a top-up', () => {
  // R1. The browser cannot check what the server did not report, so it allows
  // the top-up exactly as before instead of blocking every payment.
  assert.equal(midtransEnvMismatch('sandbox', undefined), null);
  assert.equal(midtransEnvMismatch('production', undefined), null);
});

test('a server value outside the documented pair is unreadable, not a disagreement', () => {
  // The contract is exactly "sandbox" or "production"; anything else is a value
  // this build cannot interpret, so it is "cannot check" rather than a mismatch.
  assert.equal(midtransEnvMismatch('sandbox', ''), null);
  assert.equal(midtransEnvMismatch('sandbox', 'staging'), null);
});
