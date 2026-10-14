// The native auth transport, lib/auth-api.ts.
//
// WHY THIS FILE EXISTS, and it is a MEASURED gap rather than a suspicion. `postAuth` is the shared
// transport for every native auth verb - signup, login, Google sign-in, email verification, password
// reset and change - and NO TEST reached it. Deleting `credentials: 'include'`, the 204 short-circuit
// and the parsed error envelope all at once left the whole website suite GREEN at 211 passed /
// 0 failed, because the only suites that named this module read it as TEXT (`doc-counts`,
// `public-secrets`, `session-body-claim`) and never called it.
//
// The three properties below are the ones whose failure is silent:
//
//   - `credentials: 'include'` is what carries the session cookie. Narrowing it drops the cookie the
//     response just set, and the symptom is not an error - it is a user who is signed out on the
//     next page load, which no assertion about THIS call would notice.
//   - the 204 short-circuit. `res.json()` on an empty body throws, and the verbs that answer 204
//     (verify, reset, change) would surface a parse error instead of success.
//   - the parsed error envelope. Discarding it turns a specific server refusal into a status line,
//     so the UI can no longer pick copy for the code it was given.
//
// Ported from `api-extra.test.ts`'s shape - the same `withMockFetch` helper, the same split into
// named tests - because that file's header records the identical lesson: a single assertion that
// only proved something REJECTED was satisfied by a redirect the TEST performed.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  signup,
  login,
  verifyEmail,
  requestPasswordReset,
  confirmPasswordReset,
  changePassword,
  listProviders,
} from '../src/lib/auth-api.ts';
import { ApiError } from '../src/lib/api.ts';

/** Runs `fn` with `fetch` replaced, restoring it afterwards even on a throw. */
async function withMockFetch(
  mock: (url: string, init: RequestInit | undefined) => Response,
  fn: () => Promise<void>,
): Promise<void> {
  const original = globalThis.fetch;
  globalThis.fetch = (async (input: RequestInfo | URL, init?: RequestInit) =>
    mock(String(input), init)) as typeof fetch;
  try {
    await fn();
  } finally {
    globalThis.fetch = original;
  }
}

test('the auth transport sends the session cookie', async () => {
  let seen: RequestInit | undefined;
  await withMockFetch(
    (_url, init) => {
      seen = init;
      return new Response(JSON.stringify({ message: 'ok' }), { status: 200 });
    },
    async () => {
      await signup('a@example.test', 'a-long-enough-password');
    },
  );

  assert.ok(seen, 'fetch was never called, so this test proves nothing');
  assert.equal(
    seen!.method,
    'POST',
    'every native auth verb is a POST; a GET here means the helper is calling the wrong thing',
  );
  // The property whose absence is SILENT: the response to sign-in sets the HttpOnly session cookie,
  // and a request that does not opt into credentials drops it - the next page load is signed out
  // with no error anywhere.
  assert.equal(
    seen!.credentials,
    'include',
    "the auth transport must send `credentials: 'include'`. Without it the browser drops the " +
      'session cookie the response just set, and the user is signed out on the next page load ' +
      'with nothing failing here.',
  );
  assert.equal(
    (seen!.headers as Record<string, string>)['Content-Type'],
    'application/json',
    'the server parses these bodies as JSON; a missing content type is a 415, not a field error',
  );
});

test('the auth transport returns undefined for a 204 rather than parsing an empty body', async () => {
  // The verbs that answer 204 with no body: verify, reset confirm, change.
  await withMockFetch(
    () => new Response(null, { status: 204 }),
    async () => {
      await verifyEmail('a-token');
      await confirmPasswordReset('a-token', 'a@example.test', 'a-long-enough-password');
      await changePassword('an-old-password', 'a-long-enough-password');
    },
  );
  // Reaching here IS the assertion: `res.json()` on a 204 throws, so a missing short-circuit fails
  // these three calls rather than returning.
});

test('the auth transport keeps the parsed error envelope on a failure', async () => {
  const envelope = {
    error: {
      code: 'insufficient_balance',
      message: 'Balance too low.',
      request_id: 'req_auth_1',
    },
  };
  await withMockFetch(
    () => new Response(JSON.stringify(envelope), { status: 402, statusText: 'Payment Required' }),
    async () => {
      await assert.rejects(
        () => login('a@example.test', 'a-long-enough-password'),
        (err: unknown) => {
          assert.ok(err instanceof ApiError, `expected an ApiError, got ${String(err)}`);
          // The CODE is the property. Dropping the parsed envelope leaves `code` undefined and the
          // UI falls back to a status line, so a specific refusal reads as a generic one.
          assert.equal(
            (err as ApiError).code,
            'insufficient_balance',
            'the parsed error envelope was discarded, so the client can no longer pick copy for ' +
              'the code the server sent. This is the field `describeError` switches on.',
          );
          assert.equal((err as ApiError).status, 402);
          return true;
        },
      );
    },
  );
});

test('a non-JSON error body falls through to the status line rather than throwing a parse error', async () => {
  await withMockFetch(
    () => new Response('<html>gateway</html>', { status: 502, statusText: 'Bad Gateway' }),
    async () => {
      await assert.rejects(
        () => requestPasswordReset('a@example.test'),
        (err: unknown) => {
          assert.ok(
            err instanceof ApiError,
            'a non-JSON error body must still raise the ApiError the UI knows how to render, not a ' +
              `SyntaxError from res.json(): got ${String(err)}`,
          );
          assert.equal((err as ApiError).status, 502);
          // `ApiError` spells the no-envelope case `internal_error` rather than leaving `code`
          // undefined (api.ts: `body?.code ?? 'internal_error'`), and the MESSAGE falls back to the
          // status line this helper passes. Both are asserted rather than assumed - an earlier
          // version of this test expected `undefined` and was wrong about its own subject.
          assert.equal(
            (err as ApiError).code,
            'internal_error',
            'an unparseable error body must still carry a code the UI can switch on',
          );
          assert.equal(
            (err as ApiError).message,
            'Bad Gateway',
            'the status line is what the fallback message must be, so the user sees a reason ' +
              'rather than an empty string',
          );
          return true;
        },
      );
    },
  );
});

test('a successful JSON body is returned to the caller', async () => {
  await withMockFetch(
    () => new Response(JSON.stringify({ providers: ['password', 'google'] }), { status: 200 }),
    async () => {
      const result = await listProviders();
      assert.deepEqual(result, { providers: ['password', 'google'] });
    },
  );
});
