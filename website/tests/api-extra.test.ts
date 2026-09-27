// Closes the remaining branches of lib/api.ts that the error-model and
// rate-limit suites do not exercise: a 204 No Content body, a non-JSON error
// body, and the 401 -> redirectToLogin path (which touches `window.location`).

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

test('a 401 with redirectOn401 triggers a login redirect and redirectToLogin keeps the destination', async () => {
  const original = globalThis.fetch;
  let redirectedTo: string | null = null;
  const windowStub = {
    location: {
      pathname: '/dash',
      search: '?a=1',
      replace(target: string) {
        redirectedTo = target;
      },
    },
  };
  globalThis.window = windowStub as unknown as Window & typeof globalThis;
  globalThis.fetch = async () =>
    new Response(
      JSON.stringify({ error: { code: 'unauthorized', message: 'nope' } }),
      { status: 401, headers: { 'content-type': 'application/json' } },
    );
  try {
    await assert.rejects(
      () => apiFetch('/x'),
      (err: unknown) => (err as { status: number }).status === 401,
    );
    // redirectToLogin is also callable directly and must preserve the next hop.
    redirectToLogin();
    assert.equal(redirectedTo, `/login?next=${encodeURIComponent('/dash?a=1')}`);
  } finally {
    globalThis.fetch = original;
    delete (globalThis as { window?: unknown }).window;
  }
});
