// Closes the remaining branches of lib/api.ts that the error-model and
// rate-limit suites do not exercise: a 204 No Content body, a non-JSON error
// body, and the 401 -> redirectToLogin path (which touches `window.location`).
//
// THE 401 RULE IS THREE BEHAVIOURS AND EACH HAS ITS OWN TEST, which it did not always. One test
// used to cover "the 401 path" by calling apiFetch, asserting only that it REJECTED, then calling
// redirectToLogin ITSELF and asserting on that - so the redirect it verified was one the test
// performed. MEASURED: disabling the redirect inside apiFetch left all four suites green at
// 40/40. The three tests below are split so that deleting the call from apiFetch, ignoring the
// opt-out, or dropping the destination each fail a NAMED assertion rather than none.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { apiFetch, redirectToLogin } from '../src/lib/api.ts';

async function withMockFetch(
  mock: () => Response,
  fn: () => Promise<void>,
): Promise<void> {
  const original = globalThis.fetch;
  globalThis.fetch = async () => mock();
  try {
    await fn();
  } finally {
    globalThis.fetch = original;
  }
}

test('apiFetch returns undefined on a 204 No Content response', async () => {
  await withMockFetch(
    () => new Response(null, { status: 204 }),
    async () => {
      const result = await apiFetch('/x');
      assert.equal(result, undefined);
    },
  );
});

test('apiFetch parses and returns a successful JSON body', async () => {
  await withMockFetch(
    () =>
      new Response(JSON.stringify({ ok: true, value: 42 }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    async () => {
      const result = await apiFetch<{ ok: boolean; value: number }>('/x');
      assert.deepEqual(result, { ok: true, value: 42 });
    },
  );
});

test('apiFetch tolerates a non-JSON error body and still surfaces the status', async () => {
  await withMockFetch(
    () =>
      new Response('plain text error', {
        status: 500,
        headers: { 'content-type': 'text/plain' },
      }),
    async () => {
      await assert.rejects(
        () => apiFetch('/x'),
        (err: unknown) => (err as { status: number }).status === 500,
      );
    },
  );
});

// A 401 must send the visitor to /login, and this asserts that apiFetch ITSELF does it.
//
// THIS TEST USED TO NOT PIN THAT. It called apiFetch('/x'), asserted only that the call
// REJECTED with 401, and then called redirectToLogin() ITSELF and asserted on that. So the
// redirect it checked was one the test performed, not one apiFetch performed - MEASURED: with
// the redirect inside apiFetch disabled entirely (`if (false && res.status === 401 ...)`), the
// whole file still passed 40/40. The name and the file header both claimed to cover "the
// 401 -> redirectToLogin path" while the assertion held for another reason.
//
// The fix is to assert the redirect happened WITHOUT calling it, which is the only shape that
// can fail when apiFetch stops doing it.
function withWindowStub(fn: () => Promise<void>): Promise<void> {
  const original = globalThis.fetch;
  const stub = {
    location: {
      pathname: '/dash',
      search: '?a=1',
      replace(target: string) {
        (stub as { redirectedTo?: string }).redirectedTo = target;
      },
    },
    redirectedTo: null as string | null,
  };
  globalThis.window = stub as unknown as Window & typeof globalThis;
  return fn().finally(() => {
    globalThis.fetch = original;
    delete (globalThis as { window?: unknown }).window;
  });
}

function stub401(): void {
  globalThis.fetch = async () =>
    new Response(
      JSON.stringify({ error: { code: 'unauthorized', message: 'nope' } }),
      { status: 401, headers: { 'content-type': 'application/json' } },
    );
}

test('apiFetch redirects to /login on a 401, preserving the destination', async () => {
  await withWindowStub(async () => {
    stub401();
    await assert.rejects(
      () => apiFetch('/x'),
      (err: unknown) => (err as { status: number }).status === 401,
    );
    // The assertion that matters: apiFetch did this. Nothing below calls redirectToLogin, so
    // deleting the call from apiFetch makes this fail.
    assert.equal(
      (globalThis.window as unknown as { redirectedTo: string | null }).redirectedTo,
      `/login?next=${encodeURIComponent('/dash?a=1')}`,
      'apiFetch must redirect a 401 itself, keeping where the visitor was going',
    );
  });
});

test('a 401 with redirectOn401 false does NOT redirect', async () => {
  await withWindowStub(async () => {
    stub401();
    // The auth pages make calls that legitimately answer 401 and must own that answer: the
    // signed-in probe on /login would otherwise bounce the visitor to the page they are on.
    await assert.rejects(
      () => apiFetch('/x', { redirectOn401: false }),
      (err: unknown) => (err as { status: number }).status === 401,
    );
    assert.equal(
      (globalThis.window as unknown as { redirectedTo: string | null }).redirectedTo,
      null,
      'redirectOn401: false must suppress the redirect, or /login loops on itself',
    );
  });
});

test('redirectToLogin preserves the destination when called directly', async () => {
  await withWindowStub(async () => {
    redirectToLogin();
    assert.equal(
      (globalThis.window as unknown as { redirectedTo: string | null }).redirectedTo,
      `/login?next=${encodeURIComponent('/dash?a=1')}`,
    );
  });
});

// THE REQUEST SIDE. Every test above checks what apiFetch does with a RESPONSE; none checked what it
// SENDS, and MEASURED, two properties could be deleted with the whole website suite green at
// 216 passed / 0 failed:
//
//   credentials: 'include'              -> the browser drops the session cookie
//   the body-conditional Content-Type   -> a POST without it is a 415
//
// Neither failure is loud. Dropping the cookie signs the user out on the NEXT request rather than
// failing this one, and a missing content type is refused by the server, not by anything here. The
// same shape the file header describes - an assertion that holds for a reason other than its subject.

/**
 * Captures the `init` apiFetch passes to `fetch`, and answers with `response`.
 *
 * The captured value is handed out through a GETTER rather than as an argument: `fetch` runs
 * asynchronously, so a value passed into `fn` would still be the initial `undefined` when the
 * assertions ran - which is exactly how the first version of these tests failed.
 */
async function withCapturedRequest(
  response: () => Response,
  fn: (seen: () => RequestInit | undefined) => Promise<void>,
): Promise<void> {
  const original = globalThis.fetch;
  let seen: RequestInit | undefined;
  globalThis.fetch = (async (_input: RequestInfo | URL, init?: RequestInit) => {
    seen = init;
    return response();
  }) as typeof fetch;
  try {
    await fn(() => seen);
  } finally {
    globalThis.fetch = original;
  }
}

test('apiFetch sends the session cookie', async () => {
  await withCapturedRequest(
    () => new Response(JSON.stringify({ ok: true }), { status: 200 }),
    async (seen) => {
      await apiFetch('/x');
      assert.ok(seen(), 'fetch was never called, so this test proves nothing');
      assert.equal(
        seen()!.credentials,
        'include',
        "apiFetch must send `credentials: 'include'`. Narrowing it drops the HttpOnly session " +
          'cookie, and the symptom is a user signed out on the NEXT request - not a failure here.',
      );
    },
  );
});

test('apiFetch sends Content-Type only when it has a body', async () => {
  const headersOf = (seen: RequestInit | undefined): Record<string, string> =>
    (seen?.headers ?? {}) as Record<string, string>;

  await withCapturedRequest(
    () => new Response(JSON.stringify({ ok: true }), { status: 200 }),
    async (seen) => {
      await apiFetch('/x');
      assert.equal(
        headersOf(seen())['Content-Type'],
        undefined,
        'a GET with no body must not carry a Content-Type. The header describes content that is ' +
          'not there, and it is what makes a request look like it has one.',
      );
    },
  );

  await withCapturedRequest(
    () => new Response(JSON.stringify({ ok: true }), { status: 200 }),
    async (seen) => {
      await apiFetch('/x', { method: 'POST', body: JSON.stringify({ a: 1 }) });
      assert.equal(
        headersOf(seen())['Content-Type'],
        'application/json',
        'a POST WITH a body must carry Content-Type: the server parses these as JSON, and a ' +
          'missing header is a 415 rather than a field error, so nothing here would fail.',
      );
    },
  );
});

test('a caller header wins over the default Accept', async () => {
  await withCapturedRequest(
    () => new Response(JSON.stringify({ ok: true }), { status: 200 }),
    async (seen) => {
      await apiFetch('/x', { headers: { Accept: 'text/event-stream' } });
      assert.equal(
        ((seen()?.headers ?? {}) as Record<string, string>).Accept,
        'text/event-stream',
        'a caller-supplied header must win. Spreading the defaults after the caller would silently ' +
          'clobber the Accept the SSE island depends on.',
      );
    },
  );
});
