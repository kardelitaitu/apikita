// Executable contract for the two dashboard pages that have no island:
// /dashboard/keys/new and /dashboard/settings.
//
// The rule that matters most here is the model allowlist. docs/website/06-api-keys-and-limits.md
// line 41: "A key with an empty allowlist can call **nothing** — deny by default."
// server/src/routes/proxy.rs \`is_model_allowed\` was fixed to enforce exactly that.
// The UI mirror of it is \`allowsModel\` / \`describeModelAccess\` in
// src/lib/dashboard-form.ts, and this suite is what stops it inverting: an empty
// allowlist read as "all models" hands the dearest configured model to a key whose
// owner never granted it.
//
// Style follows tests/rate-limit.test.ts: node:test + node:assert/strict, explicit
// .ts extensions so Node loads the modules with no bundler and no new dependency.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ENABLED_MODELS,
  allowsModel,
  buildCreateKeyRequest,
  describeModelAccess,
  expiryToIso,
  limitError,
  passwordChangeError,
  plaintextKeyOf,
  type CreateKeyFields,
} from '../src/lib/dashboard-form.ts';

/** The create-key form as the island reads it, with one field overridden. */
function fields(overrides: Partial<CreateKeyFields> = {}): CreateKeyFields {
  return {
    label: 'prod',
    models: ['flash'],
    spend_limit_idr: '0',
    token_limit: '0',
    rate_limit_rpm: '0',
    expires_on: '',
    ...overrides,
  };
}

test('an empty allowlist permits nothing, and every listed model is permitted', () => {
  // Deny by default. This is the inversion the suite exists to catch.
  assert.equal(allowsModel([], 'flash'), false);
  assert.equal(allowsModel([], 'deepseek-v4-flash'), false);

  // An explicit list permits exactly what it names, and nothing else.
  assert.equal(allowsModel(['flash'], 'flash'), true);
  assert.equal(allowsModel(['flash'], 'deepseek-v4-flash'), false);

  // "All models" is the full list written out — not a wildcard.
  assert.equal(allowsModel([...ENABLED_MODELS], 'flash'), true);
  assert.equal(allowsModel([...ENABLED_MODELS], 'deepseek-v4-flash'), true);

  // A model enabled later is not granted by an allowlist written before it.
  assert.equal(allowsModel(['flash'], 'model-enabled-next-year'), false);
});

test('the model-access summary says "nothing" for empty and "all" only for the full list', () => {
  const none = describeModelAccess([]);
  assert.match(none, /can call nothing/);
  assert.match(none, /deny by default/);

  const all = describeModelAccess([...ENABLED_MODELS]);
  assert.match(all, /^All 6 enabled models/);

  const some = describeModelAccess(['flash']);
  assert.match(some, /^1 of 6 enabled models/);
  assert.match(some, /flash/);

  // A key created while another model was enabled still carries it; the summary
  // reports it rather than silently dropping it from the count.
  const retired = describeModelAccess(['flash', 'a-retired-model']);
  assert.match(retired, /not currently enabled/);
  assert.match(retired, /a-retired-model/);
  assert.match(retired, /^1 of 6 enabled models/);
});

test('a limit field accepts blank and zero, and refuses anything else', () => {
  // docs line 45: "A limit of 0 or null means no limit of this kind."
  for (const raw of ['', '  ', '0', '1', '50000']) {
    assert.equal(limitError(raw, 'Spend limit'), null, JSON.stringify(raw));
  }

  // server/src/routes/keys.rs refuses a negative spend_limit_idr outright, and
  // limit_reached treats a non-positive limit as no limit — so a negative here
  // would earn a 402 the customer should never see.
  assert.match(String(limitError('-1', 'Spend limit')), /whole number/);
  assert.match(String(limitError('-50000', 'Token limit')), /whole number/);

  // Number() would accept all of these; an integer field does not.
  for (const raw of ['1.5', '1e3', '0x10', '12abc', 'NaN', 'Infinity', '+5']) {
    assert.notEqual(limitError(raw, 'Rate limit'), null, JSON.stringify(raw));
  }

  // The message names the field, so a 422-style highlight can point at it.
  assert.match(String(limitError('-1', 'Rate limit (req/min)')), /Rate limit \(req\/min\)/);
});

test('expiry is inclusive of the chosen day, and impossible dates are refused', () => {
  // A date input is a calendar day. Reading it as midnight UTC would expire the
  // key at the start of that day, so "expires 31 Dec" would already be dead.
  assert.equal(expiryToIso('2025-12-31'), '2025-12-31T23:59:59.999Z');
  assert.equal(expiryToIso('2026-01-01'), '2026-01-01T23:59:59.999Z');

  // Date.UTC rolls these over rather than failing, so the round trip is checked:
  // 30 Feb must not be stored as 2 Mar, which is a day the user never chose.
  assert.equal(expiryToIso('2025-02-30'), null);
  assert.equal(expiryToIso('2025-13-01'), null);
  assert.equal(expiryToIso('2025-04-31'), null);

  // Blank means "never", which the API models as null.
  assert.equal(expiryToIso(''), null);
  assert.equal(expiryToIso('31/12/2025'), null);

  // A real leap day is accepted.
  assert.equal(expiryToIso('2028-02-29'), '2028-02-29T23:59:59.999Z');
});

test('a password change needs the current password and a matching, long-enough new one', () => {
  // Spec line 151: "Change password (requires current password)".
  assert.equal(passwordChangeError('', 'newpassword', 'newpassword'), 'Enter your current password.');
  assert.match(String(passwordChangeError('old', 'short', 'short')), /at least 8 characters/);
  assert.match(String(passwordChangeError('old', 'newpassword', 'different1')), /do not match/);

  assert.equal(passwordChangeError('old', 'newpassword', 'newpassword'), null);
});

test('only a create response can reveal a plaintext key, and a missing one is never invented', () => {
  // docs lines 122-124: shown exactly once, at creation, never stored.
  assert.equal(plaintextKeyOf({ key: 'apk_live_abc123' }), 'apk_live_abc123');

  // A response with no plaintext (or a wrong-typed one) reveals nothing — the
  // page must surface the missing field rather than fabricate a key.
  assert.equal(plaintextKeyOf({}), null);
  assert.equal(plaintextKeyOf({ key: '' }), null);
  assert.equal(plaintextKeyOf({ key: 42 }), null);
  assert.equal(plaintextKeyOf(null), null);
  assert.equal(plaintextKeyOf(undefined), null);
});

// The three tests below are the create-key request itself. The island used to
// hardcode `expires_at: null` while collecting nothing that could fill it, and
// coerced every limit with a local `num()` that turned `-5` into `0` silently —
// so a negative spend limit reached the server and earned a 402 from
// `check_spend_limit`, and the expiry helpers were dead code. All three fail
// against that behaviour: the first two because no builder existed, the third
// because `-5` was never refused.

test('the create-key request carries the chosen expiry day, inclusive', () => {
  const built = buildCreateKeyRequest(
    fields({ label: ' prod ', models: ['flash'], spend_limit_idr: '50000', rate_limit_rpm: '60', expires_on: '2025-12-31' }),
  );
  assert.ok(built.ok, JSON.stringify(built));

  // `expires_at: null` was hardcoded here; a key set to expire on the 31st must
  // not be dead for all of the 31st, so the day is sent as its last millisecond.
  assert.deepEqual(built.body, {
    label: 'prod',
    models: ['flash'],
    spend_limit_idr: 50000,
    token_limit: 0,
    rate_limit_rpm: 60,
    expires_at: '2025-12-31T23:59:59.999Z',
  });
});

test('a blank expiry is "never", and an impossible date is refused rather than downgraded', () => {
  const never = buildCreateKeyRequest(fields({ expires_on: '  ' }));
  assert.ok(never.ok, JSON.stringify(never));
  assert.equal(never.body.expires_at, null);

  // expiryToIso returns null for 2025-02-30; a field the user filled in must not
  // quietly become a key that never expires.
  const impossible = buildCreateKeyRequest(fields({ expires_on: '2025-02-30' }));
  assert.ok(!impossible.ok, JSON.stringify(impossible));
  assert.match(impossible.error, /real calendar date/);
});

test('a limit the API would reject is refused locally, with the field named', () => {
  const negative: [keyof CreateKeyFields, string][] = [
    ['spend_limit_idr', 'Spend limit (IDR)'],
    ['token_limit', 'Token limit'],
    ['rate_limit_rpm', 'Rate (req/min)'],
  ];
  for (const [field, label] of negative) {
    // The old island's num() sent 0 here instead, so nothing was refused and a
    // negative spend limit earned a 402 the customer should never read.
    const built = buildCreateKeyRequest(fields({ [field]: '-5' }));
    assert.ok(!built.ok, `${field}: ${JSON.stringify(built)}`);
    assert.match(built.error, /whole number/);
    assert.ok(built.error.includes(label), `${field}: ${built.error}`);
  }

  // Blank stays the API's own "no limit of this kind" (0), not an error.
  const blank = buildCreateKeyRequest(fields({ spend_limit_idr: '', token_limit: ' ', rate_limit_rpm: '' }));
  assert.ok(blank.ok, JSON.stringify(blank));
  assert.equal(blank.body.spend_limit_idr, 0);
  assert.equal(blank.body.token_limit, 0);
  assert.equal(blank.body.rate_limit_rpm, 0);
});

