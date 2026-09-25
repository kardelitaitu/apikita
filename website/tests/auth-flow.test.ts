// Executable contract for the public auth pages.
//
// Three rules here are security rules, not copy preferences, and each one is
// asserted on the function the page actually calls:
//
//   1. The reset reply is neutral (docs/website/03-functional-spec.md line 90)
//      and so is the signup reply for a duplicate address (lines 38-43). The
//      reply functions take no "does this account exist" argument at all, so a
//      call site cannot leak one; what the suite pins is that a server error
//      naming the address unknown still produces the neutral message.
//   2. The reset token is read from the query string and then removed from the
//      URL (line 95: "never echoed back in a URL that gets logged").
//   3. A mismatched password confirmation never reaches the server.
//
// PocketBase is never contacted: every input here is a plain object.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  confirmationError,
  describeAuthError,
  DUPLICATE_SIGNUP_MESSAGE,
  NEUTRAL_RESET_MESSAGE,
  passwordRuleError,
  PASSWORD_RESET_DONE_MESSAGE,
  readAuthToken,
  resetRequestReply,
  signupReply,
  UNREACHABLE_MESSAGE,
  withoutAuthToken,
} from '../src/lib/auth-flow.ts';

/** A PocketBase failure: an HTTP status plus its per-field error body. */
function pbError(status: number, fields: Record<string, { message: string }> = {}) {
  return { status, message: 'raw server message that must never be shown', data: { data: fields } };
}

test('the reset reply is identical whether or not the address is registered', () => {
  // PocketBase answers 404 for an unknown address and 204 for a known one. Both
  // land on the same sentence; only "the request never left" differs.
  const known = resetRequestReply(null);
  const unknown = resetRequestReply(pbError(404, { email: { message: 'not found' } }));
  const rejected = resetRequestReply(pbError(400, { email: { message: 'invalid' } }));

  assert.equal(known, NEUTRAL_RESET_MESSAGE);
  assert.equal(unknown, NEUTRAL_RESET_MESSAGE);
  assert.equal(rejected, NEUTRAL_RESET_MESSAGE);
  assert.equal(unknown, known);
});

test('a reset request that never reached the server says so, and only that', () => {
  assert.equal(resetRequestReply(pbError(0)), UNREACHABLE_MESSAGE);
  assert.equal(resetRequestReply(pbError(500)), NEUTRAL_RESET_MESSAGE);
});

test('the signup reply is identical for created and for duplicate', () => {
  const created = signupReply(null);
  const duplicate = signupReply(pbError(400, { email: { message: 'already in use' } }));
  const rejected = signupReply(pbError(400, { password: { message: 'too short' } }));

  // Spec line 41, verbatim.
  assert.equal(created, "If this email is registered, we've sent sign-in instructions.");
  assert.equal(duplicate, created);
  assert.equal(rejected, created);
  assert.equal(signupReply(pbError(0)), UNREACHABLE_MESSAGE);
});

test('a duplicate address is not reported as a field error', () => {
  const message = describeAuthError(pbError(400, { email: { message: 'already in use' } }), DUPLICATE_SIGNUP_MESSAGE);
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

test('no PocketBase message reaches the user, and a 5xx keeps its own words', () => {
  const raw = 'raw server message that must never be shown';
  const cases: [unknown, string][] = [
    [pbError(400, { password: { message: 'too short' } }), 'Password must be at least 8 characters, and both entries must match.'],
    [pbError(401), 'Email or password is incorrect.'],
    [pbError(404), 'That link is no longer valid. Request a new one.'],
    [pbError(429), 'Too many attempts. Try again later.'],
    [pbError(500), 'Something went wrong on our side.'],
    [pbError(0), UNREACHABLE_MESSAGE],
  ];

  for (const [err, message] of cases) {
    const shown = describeAuthError(err, DUPLICATE_SIGNUP_MESSAGE);
    assert.equal(shown, message, JSON.stringify(err));
    assert.ok(!shown.includes(raw), 'a raw server message was shown');
  }
});

test('the success copy names the invalidation the spec requires', () => {
  // Spec line 92: new password -> saved -> all sessions invalidated.
  assert.ok(PASSWORD_RESET_DONE_MESSAGE.includes('Every session'));
  assert.ok(PASSWORD_RESET_DONE_MESSAGE.includes('sign in again'));
});
