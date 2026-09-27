// Executable contract for what a rate-limited user is told.
//
// The server sends `Retry-After` on a 429 (docs/error-model.md); the UI must
// state that wait, not a fixed "wait a moment" that understates an hourly cap by
// ~3600x, and it must keep the request id so support can trace the refusal.
//
// The message is asserted on the renderers the islands actually call
// (lib/errors.ts renderNotice / inlineNotice), not on an intermediate view, so
// dropping the correlation id there turns this suite red. The islands' own
// element wiring cannot be seen by `tsc` — `.astro` script blocks are not
// type-checked without @astrojs/check — which is why the id is read from the
// notice object rather than passed as a second, droppable argument.
//
// The imports carry explicit `.ts` extensions, and `API_BASE` reads
// `import.meta.env` optionally: that is what lets Node load these modules
// directly, with no bundler and no new dependency.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { parseRetryAfter, rateLimitedMessage } from '../src/lib/retry-wait.ts';
import { API_BASE, ApiError, apiFetch } from '../src/lib/api.ts';
import { describeError, inlineNotice, renderNotice } from '../src/lib/errors.ts';
import type { NoticeTarget } from '../src/lib/errors.ts';

test('parseRetryAfter reads seconds, and reads nothing else', () => {
  const cases: [header: string | null, seconds: number | null][] = [
    ['3600', 3600],
    [' 86400 ', 86400],
    ['1', 1],
    ['45.2', null], // a fraction is not a second count
    ['1e3', null], // nor is exponent notation
    ['0x10', null], // nor hex
    ['120, 60', null], // nor the HTTP-date form
    ['0', null],
    ['-30', null],
    ['soon', null],
    ['', null],
    [null, null],
  ];

  for (const [header, seconds] of cases) {
    assert.equal(parseRetryAfter(header), seconds, `header ${JSON.stringify(header)}`);
  }
});

test('rateLimitedMessage states the wait, with the unit changing at each boundary', () => {
  const cases: [seconds: number | null, message: string][] = [
    [1, 'Too many requests. Try again in 1 second.'],
    [45, 'Too many requests. Try again in 45 seconds.'],
    [60, 'Too many requests. Try again in about 1 minute.'],
    [3599, 'Too many requests. Try again in about 60 minutes.'],
    [3600, 'Too many requests. Try again in about 1 hour.'],
    [86_399, 'Too many requests. Try again in about 24 hours.'],
    [86_400, 'Too many requests. Try again in about 1 day.'],
    [172_800, 'Too many requests. Try again in about 2 days.'],
    [null, 'Too many requests. Try again later.'],
  ];

  for (const [seconds, message] of cases) {
    assert.equal(rateLimitedMessage(seconds), message, `seconds ${seconds}`);
  }
});

test('the API base is readable outside a bundler', () => {
  assert.equal(API_BASE, 'http://localhost:8080');
});

const REQUEST_ID = 'req_5ff7d8a4beed40978e295ee50c71daac';

/** Runs the real `apiFetch` against a canned response and views the failure. */
async function viewOf(status: number, body: unknown, headers: Record<string, string> = {}) {
  const original = globalThis.fetch;
  globalThis.fetch = (async () =>
    new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json', ...headers },
    })) as typeof fetch;

  try {
    await apiFetch('/api/keys', { method: 'POST', body: '{}' });
    throw new Error('the request was expected to fail');
  } catch (err) {
    assert.ok(err instanceof ApiError, `expected an ApiError, got ${err}`);
    return describeError(err);
  } finally {
    globalThis.fetch = original;
  }
}

test('the 429 the server sends reaches the user, correlation id included', async () => {
  const view = await viewOf(
    429,
    { error: { code: 'rate_limited', message: 'Rate limited', request_id: REQUEST_ID } },
    { 'retry-after': '3600' },
  );

  assert.deepEqual(view, {
    message: 'Too many requests. Try again in about 1 hour.',
    requestId: REQUEST_ID,
    field: null,
    retryable: true,
  });
});

/** The two text nodes the wallet island's notice writes to. */
function walletTarget(): NoticeTarget {
  return { message: { textContent: '' }, correlation: { textContent: '' } };
}

test('a 429 renders its wait and its id, through the real renderers', async () => {
  const view = await viewOf(
    429,
    { error: { code: 'rate_limited', message: 'Rate limited', request_id: REQUEST_ID } },
    { 'retry-after': '3600' },
  );
  const message = 'Too many requests. Try again in about 1 hour.';

  // The wallet notice: message on one line, correlation line beneath it. These
  // are the calls the island makes; both renderers take the whole notice, so an
  // error path has no request-id argument it can forget to pass.
  const wallet = walletTarget();
  renderNotice(wallet, view);
  assert.equal(wallet.message.textContent, message);
  assert.equal(wallet.correlation.textContent, `request_id: ${REQUEST_ID}`);

  // The keys modal has one element, so the same two facts share a line.
  assert.equal(inlineNotice(view), `${message} (${REQUEST_ID})`);
});

test('a notice with no id shows no id — and blanks a stale one', () => {
  const message = 'Payment could not be opened in this browser.';
  const noId = { message, requestId: null };

  const wallet = walletTarget();
  wallet.correlation.textContent = `request_id: ${REQUEST_ID}`; // left over from a previous notice
  renderNotice(wallet, noId);
  assert.equal(wallet.message.textContent, message);
  assert.equal(wallet.correlation.textContent, '');

  assert.equal(inlineNotice(noId), message);
});

test('a 429 with no Retry-After promises only "later", id still shown', async () => {
  const view = await viewOf(429, {
    error: { code: 'rate_limited', message: 'Rate limited', request_id: REQUEST_ID },
  });

  assert.equal(view.message, 'Too many requests. Try again later.');
  assert.equal(view.requestId, REQUEST_ID);

  const wallet = walletTarget();
  renderNotice(wallet, view);
  assert.equal(wallet.message.textContent, 'Too many requests. Try again later.');
  assert.equal(wallet.correlation.textContent, `request_id: ${REQUEST_ID}`);
});

test('other codes keep the 5xx-only request-id rule', async () => {
  const view = await viewOf(422, {
    error: {
      code: 'validation_failed',
      message: 'Validation failed: bad',
      request_id: REQUEST_ID,
    },
  });

  assert.equal(view.message, 'Invalid value.');
  assert.equal(view.requestId, null);
});
