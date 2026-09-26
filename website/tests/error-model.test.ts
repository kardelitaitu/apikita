// Executable contract for the API error-code -> UI mapping.
//
// docs/error-model.md defines the codes the server can return;
// docs/website/03-functional-spec.md defines what a user must see for each one,
// and that every code has a behaviour at all ("a dashboard that only handles
// five of them shows a raw error or a blank screen for the rest").
//
// Each case below is driven through the real `apiFetch` (so the code, the
// Retry-After and the request_id are parsed by the same production path the
// islands use) and asserted against the real `describeError` — not a private
// helper — plus the real renderers. The assertions pin the four facts an island
// gets to show: the message, whether the correlation id applies, the field to
// highlight, and whether a retry is offered.
//
// Three rules that are easy to lose and are pinned here:
//   1. request_id reaches the screen on every 5xx (spec rule 1), and NOT on a
//      plain 4xx — an unknown code must not invent one.
//   2. A raw server message is never shown (spec rule 5), including for a code
//      this table does not know.
//   3. 402/403 offer no retry; 429 and 5xx do (spec rule 2).
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ApiError, apiFetch } from '../src/lib/api.ts';
import { describeError, fieldInput, inlineNotice, renderFieldError, renderNotice } from '../src/lib/errors.ts';
import type { ErrorView, NoticeTarget } from '../src/lib/errors.ts';
import { renderLoginError } from '../src/lib/login-error.ts';

const REQUEST_ID = 'req_5ff7d8a4beed40978e295ee50c71daac';

/** The server's own words; no mapping may ever surface this. */
const RAW = 'raw server message that must never be shown';

/**
 * Runs the real `apiFetch` against a canned error response and returns the
 * `ApiError` it throws — the same object an island catches.
 */
async function apiError(
  status: number,
  code: string,
  extra: Record<string, unknown> = {},
  headers: Record<string, string> = {},
): Promise<ApiError> {
  const original = globalThis.fetch;
  globalThis.fetch = (async () =>
    new Response(JSON.stringify({ error: { code, message: RAW, ...extra } }), {
      status,
      headers: { 'content-type': 'application/json', ...headers },
    })) as typeof fetch;

  try {
    await apiFetch('/api/me', { redirectOn401: false });
    throw new Error('the request was expected to fail');
  } catch (err) {
    assert.ok(err instanceof ApiError, `expected an ApiError, got ${err}`);
    return err;
  } finally {
    globalThis.fetch = original;
  }
}

interface Case {
  name: string;
  status: number;
  code: string;
  extra?: Record<string, unknown>;
  headers?: Record<string, string>;
  view: ErrorView;
}

// Every HTTP code in docs/error-model.md, plus two unknown codes. Each carries a
// request_id in the body so the table proves which codes surface it.
const CASES: Case[] = [
  {
    name: '400 invalid_request',
    status: 400,
    code: 'invalid_request',
    view: { message: 'Something was wrong with that request.', requestId: null, field: null, retryable: false },
  },
  {
    name: '401 unauthenticated',
    status: 401,
    code: 'unauthenticated',
    view: { message: 'Your session has ended. Sign in again.', requestId: null, field: null, retryable: true },
  },
  {
    name: '401 key_revoked',
    status: 401,
    code: 'key_revoked',
    view: { message: 'This key was revoked. Create a new one.', requestId: null, field: null, retryable: false },
  },
  {
    name: '401 key_expired',
    status: 401,
    code: 'key_expired',
    view: { message: 'This key expired. Create a new one.', requestId: null, field: null, retryable: false },
  },
  {
    name: '402 insufficient_balance',
    status: 402,
    code: 'insufficient_balance',
    view: { message: 'Balance too low. Top up to continue.', requestId: null, field: null, retryable: false },
  },
  {
    name: '402 key_limit_exceeded',
    status: 402,
    code: 'key_limit_exceeded',
    view: { message: 'This key hit its limit. Raise it or create a new key.', requestId: null, field: null, retryable: false },
  },
  {
    name: '403 model_not_allowed',
    status: 403,
    code: 'model_not_allowed',
    view: { message: 'This key cannot use that model.', requestId: null, field: null, retryable: false },
  },
  {
    name: '403 forbidden',
    status: 403,
    code: 'forbidden',
    view: { message: 'You do not have permission to do that.', requestId: null, field: null, retryable: false },
  },
  {
    name: '403 wrong_credential_type (reserved)',
    status: 403,
    code: 'wrong_credential_type',
    view: { message: 'Unexpected credential type. This is a bug on our side.', requestId: null, field: null, retryable: false },
  },
  {
    name: '404 not_found',
    status: 404,
    code: 'not_found',
    view: { message: 'Not found.', requestId: null, field: null, retryable: false },
  },
  {
    name: '409 conflict',
    status: 409,
    code: 'conflict',
    view: { message: 'That already exists. Refresh to see the current state.', requestId: null, field: null, retryable: true },
  },
  {
    name: '422 validation_failed names the field',
    status: 422,
    code: 'validation_failed',
    extra: { details: { field: 'rating' } },
    view: { message: 'Invalid value for "rating".', requestId: null, field: 'rating', retryable: false },
  },
  {
    name: '422 validation_failed with no field',
    status: 422,
    code: 'validation_failed',
    view: { message: 'Invalid value.', requestId: null, field: null, retryable: false },
  },
  {
    name: '429 rate_limited states the wait and keeps the id',
    status: 429,
    code: 'rate_limited',
    headers: { 'retry-after': '60' },
    view: { message: 'Too many requests. Try again in about 1 minute.', requestId: REQUEST_ID, field: null, retryable: true },
  },
  {
    name: '500 internal_error keeps the id',
    status: 500,
    code: 'internal_error',
    view: { message: 'Something went wrong on our side.', requestId: REQUEST_ID, field: null, retryable: true },
  },
  {
    name: '503 no_upstream_available keeps the id and does not read as a login failure',
    status: 503,
    code: 'no_upstream_available',
    view: { message: 'Inference is degraded right now. Your dashboard still works.', requestId: REQUEST_ID, field: null, retryable: true },
  },
  {
    name: 'an unknown 4xx code shows no raw body and invents no id',
    status: 418,
    code: 'teapot',
    view: { message: 'Something was wrong with that request.', requestId: null, field: null, retryable: false },
  },
  {
    name: 'an unknown 5xx code keeps the id',
    status: 500,
    code: 'mystery_failure',
    view: { message: 'Something went wrong on our side.', requestId: REQUEST_ID, field: null, retryable: true },
  },
];

for (const c of CASES) {
  test(`${c.name}: ${c.view.message}`, async () => {
    const err = await apiError(c.status, c.code, { request_id: REQUEST_ID, ...c.extra }, c.headers);
    const view = describeError(err);

    assert.deepEqual(view, c.view, `${c.name} mapping`);
    assert.ok(!view.message.includes(RAW), `${c.name} rendered the raw server message`);
  });
}

/** The two text nodes the wallet / dashboard banner notice writes to. */
function noticeTarget(): NoticeTarget {
  return { message: { textContent: '' }, correlation: { textContent: '' } };
}

test('a 5xx puts its correlation id on screen; a 4xx without one does not', async () => {
  // The dashboard banner: message on one line, correlation line beneath it.
  const outage = await apiError(503, 'no_upstream_available', { request_id: REQUEST_ID });
  const banner = noticeTarget();
  renderNotice(banner, describeError(outage));
  assert.equal(banner.message.textContent, 'Inference is degraded right now. Your dashboard still works.');
  assert.equal(banner.correlation.textContent, `request_id: ${REQUEST_ID}`);
  assert.equal(
    inlineNotice(describeError(outage)),
    `Inference is degraded right now. Your dashboard still works. (${REQUEST_ID})`,
  );

  // A forbidden key failure is a 4xx with no correlation contract: the line is
  // blanked, not left showing a stale label.
  const forbidden = noticeTarget();
  forbidden.correlation.textContent = `request_id: ${REQUEST_ID}`;
  renderNotice(forbidden, describeError(await apiError(403, 'forbidden', { request_id: REQUEST_ID })));
  assert.equal(forbidden.message.textContent, 'You do not have permission to do that.');
  assert.equal(forbidden.correlation.textContent, '');
});

/** A minimal input stand-in that records whether it was focused. */
function fakeInput(): { focusCount: number; focus(): void } {
  return {
    focusCount: 0,
    focus() {
      this.focusCount += 1;
    },
  };
}

test('a named field resolves to the page input; an unknown or absent one does not', () => {
  const email = fakeInput();
  const password = fakeInput();
  const inputs = { email, password };

  assert.equal(fieldInput(null, inputs), null, 'no field must not focus anything');
  assert.equal(fieldInput('email', inputs), email);
  assert.equal(fieldInput('password', inputs), password);
  // A field this page does not own is not guessed at: the notice still renders.
  assert.equal(fieldInput('nickname', inputs), null);
});

test('renderFieldError writes the notice and moves focus to the named input', async () => {
  const err = await apiError(422, 'validation_failed', { details: { field: 'spend_limit_idr' } });
  const target = { textContent: null as string | null };
  const spend = fakeInput();
  const label = fakeInput();

  const focused = renderFieldError(target, describeError(err), { spend_limit_idr: spend, label });

  assert.equal(target.textContent, 'Invalid value for "spend_limit_idr".');
  assert.equal(focused, spend);
  assert.equal(spend.focusCount, 1);
  assert.equal(label.focusCount, 0, 'the wrong input must not be focused');
});

test('renderFieldError with no field renders the notice and focuses nothing', async () => {
  const err = await apiError(422, 'validation_failed');
  const target = { textContent: null as string | null };
  const label = fakeInput();

  const focused = renderFieldError(target, describeError(err), { label });

  assert.equal(target.textContent, 'Invalid value.');
  assert.equal(focused, null);
  assert.equal(label.focusCount, 0);
});

test('the login renderer focuses the input a server-named field points at', async () => {
  const err = await apiError(422, 'validation_failed', { details: { field: 'email' } });
  const box = { textContent: null as string | null };
  const email = fakeInput();
  const password = fakeInput();

  renderLoginError(box, err, { email, password });

  assert.equal(box.textContent, 'Invalid value for "email".');
  assert.equal(email.focusCount, 1);
  assert.equal(password.focusCount, 0);
});
