// Executable contract for the login page's failed-sign-in notice.
//
// Login is the one auth page whose failures come from the Rust API (the
// PocketBase token is exchanged for a session there), so its 429 carries the API
// error shape: a Retry-After wait and a request_id. It used to build that line by
// hand inside an `.astro` script block, which neither `tsc` nor this suite can
// see — the format could drift with every 429 still green, and it had drifted
// from the shared one-line renderer. `renderLoginError` (src/lib/login-error.ts)
// is the exact function the page calls, and it delegates to the shared
// `describeError` + `inlineNotice`, so one definition now serves the login box,
// the keys modal and the usage panel.
//
// Real surface, not a re-implementation: the error is produced by the real
// `apiFetch` (the production HTTP path, `parseRetryAfter` and ApiError included)
// from a 429 response carrying the server's documented bytes, and it is rendered
// through the real function on a DOM-shaped element. The assertions are the
// literal strings the user reads, so reverting login to its hand-built format
// fails here.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ApiError, apiFetch } from '../src/lib/api.ts';
import { renderLoginError } from '../src/lib/login-error.ts';

const REQUEST_ID = 'req_5ff7d8a4beed40978e295ee50c71daac';

/** The 429 body the API sends, verbatim from docs/error-model.md's shape. */
function rateLimitedBody(): unknown {
  return { error: { code: 'rate_limited', message: 'Rate limited', request_id: REQUEST_ID } };
}

/** Runs the real `apiFetch` against a canned 429 and returns the thrown error. */
async function loginFailure(
  body: unknown,
  headers: Record<string, string> = {},
): Promise<ApiError> {
  const original = globalThis.fetch;
  globalThis.fetch = (async () =>
    new Response(JSON.stringify(body), {
      status: 429,
      headers: { 'content-type': 'application/json', ...headers },
    })) as typeof fetch;

  try {
    await apiFetch('/auth/exchange', {
      method: 'POST',
      body: '{}',
      redirectOn401: false,
    });
    throw new Error('the request was expected to fail');
  } catch (err) {
    assert.ok(err instanceof ApiError, `expected an ApiError, got ${err}`);
    return err;
  } finally {
    globalThis.fetch = original;
  }
}

/** The one element the login notice writes to. */
function noticeBox(): { textContent: string | null } {
  return { textContent: null };
}

test('the login 429 shows the server wait and the correlation id', async () => {
  const box = noticeBox();
  renderLoginError(box, await loginFailure(rateLimitedBody(), { 'retry-after': '3600' }));

  // One line, the shared shape: message, then the bare id in parentheses. The
  // wait is the header's 3600s (an hour), not a fixed "a moment".
  assert.equal(box.textContent, `Too many requests. Try again in about 1 hour. (${REQUEST_ID})`);
});

test('a 429 with no Retry-After promises only "later", id still shown', async () => {
  const box = noticeBox();
  renderLoginError(box, await loginFailure(rateLimitedBody()));

  assert.equal(box.textContent, `Too many requests. Try again later. (${REQUEST_ID})`);
});

test('a failure with no id shows no dangling correlation label', async () => {
  const box = noticeBox();
  renderLoginError(box, new Error('Failed to fetch'));

  // The bug being guarded: a stale or invented `(request_id: …)` label must not
  // survive when support has no id to look up.
  assert.equal(box.textContent, 'Failed to fetch');
});
