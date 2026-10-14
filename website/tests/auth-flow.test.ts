// Executable contract for the public auth pages.
//
// Three rules here are security rules, not copy preferences, and each one is
// asserted on the function the page actually calls:
//
//   1. The reset reply is neutral (docs/website/03-functional-spec.md) and so is
//      the signup reply for a duplicate address. The reply functions take no
//      "does this account exist" argument at all, so a call site cannot leak one;
//      what the suite pins is that a server error naming the address unknown still
//      produces the neutral message.
//   2. The reset token is read from the query string and then removed from the
//      URL ("never echoed back in a URL that gets logged").
//   3. A mismatched password confirmation never reaches the server.
//
// The failure shape is `ApiError`, which the polled API calls also use: a status,
// a stable `code`, and optional `details`. The transport-failure case is its
// ABSENCE — `fetch` throws a plain TypeError that never becomes an ApiError — so
// these tests pass a bare `TypeError` for it rather than inventing a status 0.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ApiError } from '../src/lib/api.ts';
import {
  confirmationError,
  describeAuthError,
  DUPLICATE_SIGNUP_MESSAGE,
  NEUTRAL_RESET_MESSAGE,
  passwordRuleError,
  PASSWORD_RESET_DONE_MESSAGE,
  readAuthEmail,
  readAuthToken,
  resetRequestReply,
  signupReply,
  UNREACHABLE_MESSAGE,
  withoutAuthToken,
} from '../src/lib/auth-flow.ts';

const RAW = 'raw server message that must never be shown';

/**
 * A server failure in the shape the API actually returns.
 *
 * `details` goes on the BODY, where `ApiError` reads it from — the old form of
 * these fixtures attached it after construction, which is why `describeAuthError`'s
 * field branch would have matched nothing.
 */
function apiError(
  status: number,
  code: string,
  details: Record<string, unknown> | null = null,
): ApiError {
  return new ApiError(
    status,
    details === null ? { code, message: RAW } : { code, message: RAW, details },
    RAW,
    null,
  );
}

/** What a `fetch` that never reached the server throws. NOT an ApiError. */
function transportFailure(): TypeError {
  return new TypeError('Failed to fetch');
}

test('the reset reply is identical whether or not the address is registered', () => {
  // The endpoint answers 202 with the neutral message for a known address and for
  // an unknown one alike. Both land on the same sentence; only "the request never
  // left" differs.
  //
  // There is deliberately no `resetRequestReply(null)` case. These functions are
  // called from a `catch`, so "success" never reaches them, and with transport
  // failure defined as "not an ApiError" a `null` is indistinguishable from a
  // failure — it would take the unreachable branch. The success path is the page
  // showing the neutral copy without calling these at all.
  const unknown = resetRequestReply(apiError(202, 'ok'));
  const rejected = resetRequestReply(apiError(400, 'invalid_request', { field: 'email' }));

  assert.equal(unknown, NEUTRAL_RESET_MESSAGE);
  assert.equal(rejected, NEUTRAL_RESET_MESSAGE);
});

test('a reset request that never reached the server says so, and only that', () => {
  assert.equal(resetRequestReply(transportFailure()), UNREACHABLE_MESSAGE);
  assert.equal(resetRequestReply(apiError(500, 'internal_error')), NEUTRAL_RESET_MESSAGE);
});

test('the signup reply is identical for created and for duplicate', () => {
  const duplicate = signupReply(apiError(400, 'invalid_request', { field: 'email' }));
  const rejected = signupReply(apiError(400, 'validation_failed', { field: 'password' }));

  // Spec, verbatim.
  assert.equal(DUPLICATE_SIGNUP_MESSAGE, "If this email is registered, we've sent sign-in instructions.");
  assert.equal(duplicate, DUPLICATE_SIGNUP_MESSAGE);
  assert.equal(rejected, DUPLICATE_SIGNUP_MESSAGE);
  assert.equal(signupReply(transportFailure()), UNREACHABLE_MESSAGE);
});

test('only a transport failure is ever admitted, on any neutral reply', () => {
  // The invariant both reply functions share: the inputs producing
  // UNREACHABLE_MESSAGE must be exactly those that never reached the server.
  // Anything the API ANSWERED — including a 5xx, which is an answer — is neutral.
  const serverAnswers: unknown[] = [
    apiError(202, 'ok'),
    apiError(400, 'invalid_request', { field: 'email' }),
    apiError(401, 'unauthenticated'),
    apiError(429, 'rate_limited'),
    apiError(500, 'internal_error'),
  ];
  for (const err of serverAnswers) {
    assert.notEqual(signupReply(err), UNREACHABLE_MESSAGE, `signupReply admitted ${JSON.stringify(err)}`);
    assert.notEqual(resetRequestReply(err), UNREACHABLE_MESSAGE, `resetRequestReply admitted ${JSON.stringify(err)}`);
  }

  const neverArrived: unknown[] = [new TypeError('Failed to fetch'), new Error('offline')];
  for (const err of neverArrived) {
    assert.equal(signupReply(err), UNREACHABLE_MESSAGE);
    assert.equal(resetRequestReply(err), UNREACHABLE_MESSAGE);
  }
});

test('a duplicate address is not reported as a field error', () => {
  const message = describeAuthError(
    apiError(400, 'invalid_request', { field: 'email' }),
    DUPLICATE_SIGNUP_MESSAGE,
  );
  assert.equal(message, DUPLICATE_SIGNUP_MESSAGE);
});

test('the confirmation must match the password, before anything is sent', () => {
  assert.equal(confirmationError('correct-horse', 'correct-horse'), null);
  assert.equal(confirmationError('correct-horse', 'correct-hors'), 'The two passwords do not match.');
  assert.equal(confirmationError('', ''), null); // empty is the length rule's business
});

test('password rules are stated up front, and the same rule for both pages', () => {
  assert.equal(passwordRuleError('1234567'), 'Password must be at least 8 characters.');
  assert.equal(passwordRuleError('12345678'), null);
});

test('a reset token is read from the query string, and only from there', () => {
  assert.equal(readAuthToken('?token=abc123'), 'abc123');
  assert.equal(readAuthToken('?next=/dashboard&token=abc123'), 'abc123');
  assert.equal(readAuthToken('?token='), null);
  assert.equal(readAuthToken('?token'), null);
  assert.equal(readAuthToken(''), null);
  // The fragment is not the query string: a token there is not accepted.
  assert.equal(readAuthToken('#token=abc123'), null);
});

test('the address a reset link carries is read, and is not a credential', () => {
  // The reset-confirm endpoint requires the address with the token. It comes from
  // the query string when the mail template supplies it, and the page asks when it
  // does not — so this reader returning null must be a normal outcome, not an error.
  assert.equal(readAuthEmail('?token=t&email=ada%40example.com'), 'ada@example.com');
  assert.equal(readAuthEmail('?token=t'), null);
  assert.equal(readAuthEmail('?token=t&email='), null);
  assert.equal(readAuthEmail(''), null);
});

test('the token is removed from the URL and the rest of it survives', () => {
  assert.equal(withoutAuthToken('https://x.test/reset/confirm?token=abc123'), 'https://x.test/reset/confirm');
  assert.equal(
    withoutAuthToken('https://x.test/reset/confirm?token=abc123&next=/dashboard'),
    'https://x.test/reset/confirm?next=%2Fdashboard',
  );
  assert.equal(withoutAuthToken('https://x.test/reset/confirm?next=/dashboard'), 'https://x.test/reset/confirm?next=%2Fdashboard');
  assert.equal(withoutAuthToken('https://x.test/reset/confirm#top'), 'https://x.test/reset/confirm#top');
  assert.equal(withoutAuthToken('https://x.test/reset/confirm?token=abc#top'), 'https://x.test/reset/confirm#top');
  assert.equal(withoutAuthToken('https://x.test/verify'), 'https://x.test/verify');
});

test('no server message reaches the user, and a 5xx keeps its own words', () => {
  const cases: [unknown, string][] = [
    [apiError(400, 'validation_failed', { field: 'password' }), 'Password must be at least 8 characters, and both entries must match.'],
    [apiError(401, 'unauthenticated'), 'Email or password is incorrect.'],
    [apiError(429, 'rate_limited'), 'Too many attempts. Try again later.'],
    [apiError(500, 'internal_error'), 'Something went wrong on our side.'],
    [transportFailure(), UNREACHABLE_MESSAGE],
  ];

  for (const [err, message] of cases) {
    const shown = describeAuthError(err, DUPLICATE_SIGNUP_MESSAGE);
    assert.equal(shown, message, JSON.stringify(err));
    assert.ok(!shown.includes(RAW), 'a raw server message was shown');
  }
});

test('the success copy names the invalidation the spec requires', () => {
  // Spec: new password -> saved -> all sessions invalidated.
  assert.ok(PASSWORD_RESET_DONE_MESSAGE.includes('Every session'));
  assert.ok(PASSWORD_RESET_DONE_MESSAGE.includes('sign in again'));
});

