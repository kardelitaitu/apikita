// Executable contract for the live-balance store (SSE first, polling fallback).
// The purely browser-coupled surface is exercised here against a minimal
// EventSource/window shim so the store's state machine — connect, event
// handlers, retry, stop, and the loadMe error branches — is covered without a
// real browser.

import { test } from 'node:test';
import assert from 'node:assert/strict';

// Minimal browser shims. Timers are no-ops so the store never actually spins a
// poll loop or a retry loop during the test; those paths are asserted
// structurally instead.
const noopTimer = () => 0;
class FakeEventSource {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  listeners: Record<string, Array<(ev: { data: unknown }) => void>> = {};
  readyState = FakeEventSource.OPEN;
  url: string;
  constructor(url: string) {
    this.url = url;
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, cb: (ev: { data: unknown }) => void): void {
    (this.listeners[type] ??= []).push(cb);
  }
  close(): void {
    this.readyState = FakeEventSource.CLOSED;
  }
  // `data` is optional because a real EventSource event does not always carry
  // one: 'open' and 'error' fire with no payload, and the tests dispatch exactly
  // those. The parameter was required in the first version, which made `tsc`
  // reject the two 1-argument calls below — the signature was wrong, not the call.
  dispatch(type: string, data?: unknown): void {
    for (const cb of this.listeners[type] ?? []) cb({ data });
  }
  static instances: FakeEventSource[] = [];
}

globalThis.EventSource = FakeEventSource as unknown as typeof EventSource;
// Run each scheduled callback exactly once (asynchronously, so a retry that
// recreates the stream cannot recurse synchronously) so the store's timer
// bodies — and the single recreate-on-closed path — are exercised.
const runOnce = (cb: () => void): number => {
  setImmediate(cb);
  return 1;
};
globalThis.window = {
  setInterval: runOnce,
  clearInterval: noopTimer,
  setTimeout: runOnce,
  clearTimeout: noopTimer,
  location: { pathname: '/', search: '', replace: () => {} },
} as unknown as Window & typeof globalThis;

const { createLiveStore, getLiveStore, nextRetryDelay } = await import(
  '../src/lib/live.ts'
);

const ME = {
  account_id: 'a',
  balance_idr: 100,
  usage_today: {
    input_tokens: 1,
    cache_read_tokens: 0,
    output_tokens: 2,
    cost_idr: 3,
  },
  telegram_linked: false,
  status: 'active',
};

test('nextRetryDelay doubles from 1s and caps at 8s', () => {
  assert.equal(nextRetryDelay(0), 1000);
  assert.equal(nextRetryDelay(1), 2000);
  assert.equal(nextRetryDelay(2), 4000);
  assert.equal(nextRetryDelay(3), 8000);
  assert.equal(nextRetryDelay(10), 8000);
  assert.equal(nextRetryDelay(-5), 1000);
});

test('getLiveStore anchors a single store on window', () => {
  const first = getLiveStore();
  const second = getLiveStore();
  assert.equal(first, second, 'the same store is returned on repeated calls');
});

test('the store keeps the last value and reacts to every stream event', async () => {
  FakeEventSource.instances = [];
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response(JSON.stringify(ME), {
      status: 200,
      headers: { 'content-type': 'application/json' },
    });
  try {
    const store = createLiveStore();
    const states: number[] = [];
    const unsub = store.subscribe((s) => states.push(s.balanceIdr as number));

    // Initial state is delivered synchronously on subscribe.
    assert.equal(store.getState().status, 'connecting');
    assert.equal(store.getState().balanceIdr, null);

    store.start();
    await new Promise((r) => setImmediate(r));

    const es = FakeEventSource.instances[FakeEventSource.instances.length - 1];
    es.dispatch('open');
    es.dispatch('balance', JSON.stringify({ balance_idr: 777 }));
    es.dispatch(
      'usage',
      JSON.stringify({
        input_tokens: 10,
        cache_read_tokens: 1,
        output_tokens: 5,
        cost_idr: 20,
      }),
    );
    es.dispatch(
      'key',
      JSON.stringify({ key_id: 'k1', revoked_at: '2020-01-01T00:00:00Z' }),
    );
    // revoke frame with no revoked_at must not change the list.
    es.dispatch('key', JSON.stringify({ key_id: 'k2', revoked_at: null }));

    // Error frame with parseable data marks the store stale and records id.
    es.dispatch(
      'error',
      JSON.stringify({ error: { message: 'boom', request_id: 'req-9' } }),
    );
    assert.equal(store.getState().status, 'stale');
    assert.equal(store.getState().requestId, 'req-9');

    // Error frame while OPEN does not schedule a retry.
    es.readyState = FakeEventSource.OPEN;
    es.dispatch('error', JSON.stringify({ error: { message: 'm', request_id: 'r' } }));
    assert.equal(store.getState().requestId, 'r');

    // Error frame while CLOSED schedules a retry (no-op timer here).
    es.readyState = FakeEventSource.CLOSED;
    es.dispatch('error', JSON.stringify({ error: { message: 'dead', request_id: 'req-10' } }));

    // Error frame that is not JSON leaves the parsed fields null.
    es.readyState = FakeEventSource.OPEN;
    es.dispatch('error', 'not json');
    assert.equal(store.getState().error, null);

    // Empty-string error data also leaves the parsed fields null.
    es.dispatch('error', '');

    const finalState = store.getState();
    assert.equal(finalState.balanceIdr, 777);
    assert.equal(finalState.usage?.cost_idr, 20);
    assert.deepEqual(finalState.revokedKeyIds, ['k1']);

    assert.ok(states.length > 0, 'subscribers were notified');
    unsub();
    store.stop();
    // stop() must keep the last known value, never blank it.
    assert.equal(store.getState().balanceIdr, 777);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('loadMe returns null on 401 without setting an error', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response(
      JSON.stringify({ error: { code: 'unauthorized', message: 'no' } }),
      { status: 401, headers: { 'content-type': 'application/json' } },
    );
  try {
    const store = createLiveStore();
    store.start();
    await new Promise((r) => setImmediate(r));
    assert.equal(store.getState().balanceIdr, null);
    assert.equal(store.getState().error, null);
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('loadMe records a non-ApiError failure as an error', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () => {
    throw new Error('network down');
  };
  try {
    const store = createLiveStore();
    store.start();
    await new Promise((r) => setImmediate(r));
    assert.equal(store.getState().error, 'network down');
  } finally {
    globalThis.fetch = originalFetch;
  }
});

test('loadMe records an ApiError failure with its request id', async () => {
  const originalFetch = globalThis.fetch;
  globalThis.fetch = async () =>
    new Response(
      JSON.stringify({
        error: { code: 'server_error', message: 'boom', request_id: 'rid-42' },
      }),
      { status: 500, headers: { 'content-type': 'application/json' } },
    );
  try {
    const store = createLiveStore();
    store.start();
    await new Promise((r) => setImmediate(r));
    await new Promise((r) => setImmediate(r));
    assert.equal(store.getState().error, 'boom');
    assert.equal(store.getState().requestId, 'rid-42');
  } finally {
    globalThis.fetch = originalFetch;
  }
});
