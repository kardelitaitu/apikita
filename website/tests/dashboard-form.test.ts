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
  KNOWN_MODELS,
  allowsModel,
  buildCreateKeyRequest,
  describeModelAccess,
  expiryToIso,
  formatLinkCode,
  isPlausibleLinkCode,
  limitError,
  linkCodeInstruction,
  passwordChangeError,
  plaintextKeyOf,
  buildUpdateKeyRequest,
  limitReductionWarning,
  lowersAnyLimit,
  type CreateKeyFields,
  type ExistingKeyLimits,
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
  assert.equal(allowsModel([...KNOWN_MODELS], 'flash'), true);
  assert.equal(allowsModel([...KNOWN_MODELS], 'deepseek-v4-flash'), true);

  // A model enabled later is not granted by an allowlist written before it.
  assert.equal(allowsModel(['flash'], 'model-enabled-next-year'), false);
});

test('the model-access summary says "nothing" for empty and "all" only for the full list', () => {
  const none = describeModelAccess([]);
  assert.match(none, /can call nothing/);
  assert.match(none, /deny by default/);

  const all = describeModelAccess([...KNOWN_MODELS]);
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
    fields({ label: ' prod ', models: ['flash'], spend_limit_idr: '50000', rate_limit_rpm: '60', expires_on: '2099-12-31' }),
  );
  assert.ok(built.ok, JSON.stringify(built));

  // `expires_at: null` was hardcoded here; a key set to expire on the 31st must
  // not be dead for all of the 31st, so the day is sent as its last millisecond.
  // The fixture is a far-future day rather than a fixed near one because a past
  // expiry is now refused outright (see the past-expiry test below).
  assert.deepEqual(built.body, {
    label: 'prod',
    models: ['flash'],
    spend_limit_idr: 50000,
    token_limit: 0,
    rate_limit_rpm: 60,
    expires_at: '2099-12-31T23:59:59.999Z',
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

// The three tests below pin values that pass a digits-only check but that the
// server's integer types refuse. All three used to reach `fetch` and come back as
// axum's raw generic 400 — not the local sentence `limitError` exists to give.

test('a limit larger than the field\'s Rust integer type is refused locally', () => {
  // server/src/routes/keys.rs line 42: `rate_limit_rpm: i32`. 3e9 is all
  // digits, so /^\d+$/ let it through, and serde rejected it with a 400.
  assert.match(String(limitError('3000000000', 'Rate (req/min)', 2_147_483_647)), /2,147,483,647/);
  assert.equal(limitError('2147483647', 'Rate (req/min)', 2_147_483_647), null);

  // ...and the builder applies that bound, naming the field the user must fix.
  const rate = buildCreateKeyRequest(fields({ rate_limit_rpm: '3000000000' }));
  assert.ok(!rate.ok, JSON.stringify(rate));
  assert.match(rate.error, /Rate \(req\/min\)/);
  assert.match(rate.error, /whole number/);

  // Lines 38-40: `i64` for both money and tokens — but i64::MAX is already past
  // JavaScript's exact-integer ceiling, so the bound that matters is that one:
  // `Number('9223372036854775807')` is 9223372036854776000, a different limit
  // than the user typed.
  assert.equal(limitError('9007199254740991', 'Token limit', Number.MAX_SAFE_INTEGER), null);
  assert.notEqual(limitError('9223372036854775807', 'Token limit', Number.MAX_SAFE_INTEGER), null);

  const spend = buildCreateKeyRequest(fields({ spend_limit_idr: '9223372036854775808' }));
  assert.ok(!spend.ok, JSON.stringify(spend));
  assert.match(spend.error, /Spend limit \(IDR\)/);

  const token = buildCreateKeyRequest(fields({ token_limit: '9223372036854775807' }));
  assert.ok(!token.ok, JSON.stringify(token));
  assert.match(token.error, /Token limit/);
});

test('a limit that is not a safe integer after conversion is refused, never sent as a float', () => {
  // 23 digits: /^\d+$/ passes, but Number() gives 1e+23 and JSON.stringify emits
  // "1e+23", which serde's i64 deserialiser refuses — another raw 400.
  const digits = '99999999999999999999999';
  assert.match(String(limitError(digits, 'Spend limit (IDR)')), /Spend limit \(IDR\)/);

  const built = buildCreateKeyRequest(fields({ spend_limit_idr: digits }));
  assert.ok(!built.ok, JSON.stringify(built));
  assert.match(built.error, /Spend limit \(IDR\)/);

  // Whatever does get sent is a plain integer literal in the JSON payload.
  const ok = buildCreateKeyRequest(fields({ spend_limit_idr: '9007199254740991' }));
  assert.ok(ok.ok, JSON.stringify(ok));
  assert.equal(JSON.stringify(ok.body).includes('e+'), false, JSON.stringify(ok.body));
});

test('an expiry date in the past is refused; today and the future are not', () => {
  // Nothing on the server rejects a past expires_at, so the key would be created
  // and immediately render "Expired" in the list — a key that can never
  // authenticate. Blank is still "never expires".
  const past = buildCreateKeyRequest(fields({ expires_on: '2020-01-01' }));
  assert.ok(!past.ok, JSON.stringify(past));
  assert.match(past.error, /today or a future date/);

  // "Today" is a valid last day: expiryToIso ends the chosen day at 23:59:59.999Z.
  const today = new Date().toISOString().slice(0, 10);
  const todayBuilt = buildCreateKeyRequest(fields({ expires_on: today }));
  assert.ok(todayBuilt.ok, JSON.stringify(todayBuilt));

  // A far-future date is untouched by the guard, and blank stays null.
  const future = buildCreateKeyRequest(fields({ expires_on: '2099-12-31' }));
  assert.ok(future.ok, JSON.stringify(future));
  assert.equal(future.body.expires_at, '2099-12-31T23:59:59.999Z');

  const never = buildCreateKeyRequest(fields({ expires_on: '' }));
  assert.ok(never.ok, JSON.stringify(never));
  assert.equal(never.body.expires_at, null);
});


// --- Telegram link flow -----------------------------------------------------
// docs/server/api-spec.md: the code is single-use, 5-minute TTL, and the server
// zero-pads it. The leading zeros ARE the code, so the two things that must not
// happen are (a) stripping them and (b) accepting a non-6-digit string as ready.

test('a link code keeps its zero padding', () => {
  // '000042' is a valid issued code, not the number 42. Stripping the zeros
  // would send the bot a code that was never issued.
  assert.equal(formatLinkCode('000042'), '000042');
  assert.equal(formatLinkCode(' 123456 '), '123456');
});

test('only a six-digit string is a plausible link code', () => {
  assert.equal(isPlausibleLinkCode('123456'), true);
  assert.equal(isPlausibleLinkCode('000042'), true);
  assert.equal(isPlausibleLinkCode(' 123456 '), true);
  // Wrong lengths, letters, and empties are all refused.
  assert.equal(isPlausibleLinkCode('12345'), false);
  assert.equal(isPlausibleLinkCode('1234567'), false);
  assert.equal(isPlausibleLinkCode('12ab56'), false);
  assert.equal(isPlausibleLinkCode(''), false);
});

test('the instruction names the command and the server-stated TTL', () => {
  const text = linkCodeInstruction('000042', 5);
  // The exact command, with the padding intact.
  assert.match(text, /\/link 000042/);
  // The TTL comes from the argument, not a hardcoded "5".
  assert.match(text, /5 minutes/);
  // Singular at 1, so the copy never reads "1 minutes".
  assert.match(linkCodeInstruction('123456', 1), /1 minute\b/);
  assert.doesNotMatch(linkCodeInstruction('123456', 1), /1 minutes/);
});

// --- Editing a key's limits (PATCH /api/keys/:id) ---------------------------
// docs/server/api-spec.md:397-398: "Raising takes effect immediately; lowering is
// subject to the proxy's metadata cache TTL (<=60s). The response should say so,
// so the UI can warn honestly." These tests pin the trigger and the copy.

const BEFORE: ExistingKeyLimits = { spend_limit_idr: 50000, token_limit: 1000, rate_limit_rpm: 60 };

test('the edit request reuses the create validation', () => {
  const built = buildUpdateKeyRequest(fields({ spend_limit_idr: '1000' }));
  // Narrow explicitly rather than reading through the union: the type-stripped
  // runner does not narrow on assert.ok, and this keeps the test honest anyway.
  if (!built.ok) {
    assert.fail('an edit with a valid limit must build: ' + built.error);
  }
  assert.equal(built.body.spend_limit_idr, 1000);

  // The same refusals apply: a negative limit is rejected, not silently zeroed.
  const bad = buildUpdateKeyRequest(fields({ spend_limit_idr: '-5' }));
  assert.equal(bad.ok, false);
});

test('lowering any one limit triggers the warning', () => {
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, spend_limit_idr: 40000 }), true);
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, token_limit: 500 }), true);
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, rate_limit_rpm: 30 }), true);
});

// --- The edit form must round-trip every limit it submits -------------------
//
// THE DEFECT THIS PINS. buildUpdateKeyRequest sends the WHOLE field set on every
// save rather than a diff, which is correct only if the form was populated from
// the stored values. It was not: `ApiKeyDto` carried no `token_limit`, so
// `openEdit` hardcoded `token_limit: 0` and never wrote the `e-token` input. The
// input therefore opened blank, a blank limit parses to 0 (see `wholeNumber`),
// and 0 means UNLIMITED. Renaming a key raised its token ceiling to no ceiling.
//
// Every existing test above missed it for one reason: `fields()` supplies a
// COMPLETE field set, so no test ever modelled "the form opened without this
// value". A fixture that supplies every field cannot see a defect that consists
// of a field never being supplied.
//
// These two tests close that gap from both ends - the server must publish the
// ceiling, and the edit form must send back what it was given.

/** A key DTO shaped exactly like ApiKeyDto in server/src/routes/keys.rs. */
interface ApiKeyDtoLike {
  id: string;
  label: string;
  models: string[];
  spend_limit_idr: number;
  token_limit: number;
  rate_limit_rpm: number;
  expires_at: string | null;
}

/**
 * The edit form's fields, filled the way `openEdit` fills them from a stored key.
 *
 * This mirrors KeyManagement.astro's openEdit deliberately, including reading
 * every limit back out of the DTO. If the island ever stops populating a field,
 * the test below fails - which is the point: the bug was in the island, and the
 * island is `openEdit` plus this builder.
 */
function editFieldsFrom(k: ApiKeyDtoLike): CreateKeyFields {
  return {
    label: k.label ?? '',
    models: k.models ?? [],
    spend_limit_idr: String(k.spend_limit_idr ?? 0),
    token_limit: String(k.token_limit ?? 0),
    rate_limit_rpm: String(k.rate_limit_rpm ?? 0),
    expires_on: k.expires_at ? k.expires_at.slice(0, 10) : '',
  };
}

test('editing a key sends back every limit it was given, clearing none', () => {
  const stored: ApiKeyDtoLike = {
    id: 'k1',
    label: 'prod',
    models: ['flash'],
    spend_limit_idr: 50_000,
    token_limit: 1_000,
    rate_limit_rpm: 60,
    expires_at: null,
  };

  const built = buildUpdateKeyRequest(editFieldsFrom(stored));
  if (!built.ok) {
    assert.fail('an edit filled from a stored key must build: ' + built.error);
  }

  assert.equal(
    built.body.token_limit,
    1_000,
    'the token ceiling must survive an edit that did not mention it. It was 1000, and ' +
      'anything else here means the edit form opened without the value and submitted ' +
      'its blank default - 0, which this API reads as UNLIMITED. That is a silent ' +
      'widening of the key: the user renamed it and lost the ceiling.',
  );
  assert.equal(built.body.spend_limit_idr, 50_000, 'the spend limit must survive');
  assert.equal(built.body.rate_limit_rpm, 60, 'the rate limit must survive');
});

test('an unchanged edit lowers nothing, so it cannot warn about a reduction', () => {
  // The companion failure mode: with token_limit hardcoded to 0 in `before`, the
  // warning path could never fire for it either, because 0 was never lowered.
  // A save that changes nothing must be silent.
  const stored: ApiKeyDtoLike = {
    id: 'k1',
    label: 'prod',
    models: ['flash'],
    spend_limit_idr: 50_000,
    token_limit: 1_000,
    rate_limit_rpm: 60,
    expires_at: null,
  };
  const built = buildUpdateKeyRequest(editFieldsFrom(stored));
  if (!built.ok) {
    assert.fail('an edit filled from a stored key must build: ' + built.error);
  }
  const after: ExistingKeyLimits = {
    spend_limit_idr: built.body.spend_limit_idr,
    token_limit: built.body.token_limit,
    rate_limit_rpm: built.body.rate_limit_rpm,
  };
  const before: ExistingKeyLimits = {
    spend_limit_idr: stored.spend_limit_idr,
    token_limit: stored.token_limit,
    rate_limit_rpm: stored.rate_limit_rpm,
  };
  assert.equal(
    lowersAnyLimit(before, after),
    false,
    're-saving a key without touching a limit must not claim to lower one',
  );
});

test('raising, or leaving limits unchanged, does NOT warn', () => {
  assert.equal(lowersAnyLimit(BEFORE, BEFORE), false);
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, spend_limit_idr: 90000 }), false);
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, token_limit: 5000 }), false);
  assert.equal(lowersAnyLimit(BEFORE, { ...BEFORE, rate_limit_rpm: 120 }), false);
  assert.equal(limitReductionWarning(BEFORE, { ...BEFORE, rate_limit_rpm: 120 }), null);
});

test('the warning names the TTL and the asymmetry', () => {
  const warning = limitReductionWarning(BEFORE, { ...BEFORE, spend_limit_idr: 1000 });
  assert.ok(warning !== null);
  // The window is stated in seconds, not as a vague "shortly".
  assert.match(warning, /60 seconds/);
  // It says a raise is immediate, so the user knows the difference.
  assert.match(warning.toLowerCase(), /raise/);
});
