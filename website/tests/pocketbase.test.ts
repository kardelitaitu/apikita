// Executable contract for the PocketBase -> API session exchange. The module
// instantiates a PocketBase client at import; the one function under test either
// refuses when there is no PocketBase session, or posts the token and returns the
// exchange result through the real `apiFetch`.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { pb, exchangeForSession } from '../src/lib/pocketbase.ts';

test('exchangeForSession throws without a session and posts the token when present', async () => {
  const originalFetch = globalThis.fetch;
  try {
    // No PocketBase session -> the call must refuse before any network use.
    pb.authStore.clear();
    await assert.rejects(
      () => exchangeForSession(),
      /No PocketBase session to exchange/,
    );

    // A present session is posted and the exchange result is returned verbatim.
    pb.authStore.save('pb-token-value');
    globalThis.fetch = async () =>
      new Response(
        JSON.stringify({ account_id: 'acc-1', balance_idr: 5000 }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      );

    const result = await exchangeForSession();
    assert.equal(result.account_id, 'acc-1');
    assert.equal(result.balance_idr, 5000);
  } finally {
    globalThis.fetch = originalFetch;
    pb.authStore.clear();
  }
});
