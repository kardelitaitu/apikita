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
import { readFileSync } from 'node:fs';

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

// ---------------------------------------------------------------------------
// THE CASE SET, which the table above cannot see.
//
// Every test above drives a code I TYPED, through the real mapper, and asserts
// what comes out. That is the right test for the mapping of a code. It is not a
// test that the mapper HAS a case for every code the server can send: a code
// added to `AppError` and to docs/error-model.md, with no `case` in
// `describeError`, falls into `default` and renders the generic "Something was
// wrong with that request." to a user who just hit, say, a spend limit. Every
// assertion above stays green, because none of them asks about the code that is
// missing.
//
// docs/error-model.md's own reasoning is the argument: "a dashboard that only
// handles five of them shows a raw error or a blank screen for the rest." Five
// against fifteen, or fourteen against fifteen, is the same failure with a
// smaller number. And it is the direction a hand-kept list fails silently in,
// which is the lesson docs/error-model.md records about the retention tables and
// this file records about its own DOCUMENTED copy.
//
// So the case set is read out of the real source and compared, as a SET, to the
// real document. Both sides are parsed rather than transcribed - the point is to
// compare the two artifacts, not a third copy of them.

/** The `case 'x':` labels in `describeError`, from the real source. */
function mappedCodes(): string[] {
  const src = readFileSync(new URL('../src/lib/errors.ts', import.meta.url), 'utf8');
  const start = src.indexOf('export function describeError');
  assert.ok(start > -1, 'describeError must exist, or this reads a file that is not the mapper');
  // To the end of the function: the next top-level `\n}` after the switch. The
  // switch is the last statement, so the closing brace at column 0 ends it.
  const end = src.indexOf('\n}', start);
  const body = src.slice(start, end > -1 ? end : src.length);
  return [...body.matchAll(/^\s*case '([a-z_]+)':/gm)].map((m) => m[1]);
}

/** The codes in the published status table, which is the contract. */
function publishedCodes(): string[] {
  const doc = readFileSync(new URL('../../docs/error-model.md', import.meta.url), 'utf8');
  const start = doc.indexOf('## Status codes and their meanings');
  assert.ok(start > -1, 'the Status codes section is the contract and must be findable');
  const section = doc.slice(start);
  const next = section.indexOf('\n## ', 2);
  const table = next > -1 ? section.slice(0, next) : section;

  const codes: string[] = [];
  for (const line of table.split('\n')) {
    const cells = line.split('|').map((c) => c.trim());
    // cells[0] is the empty run before the leading pipe.
    if (cells.length < 4) continue;
    if (!/^\d{3}$/.test(cells[1])) continue;
    const code = cells[2].replaceAll('`', '').trim();
    if (code) codes.push(code);
  }
  return codes;
}

test('every code the contract publishes has a case in the mapper', () => {
  const published = publishedCodes();
  const mapped = mappedCodes();

  // Vacuity, both directions: an unparsable document or a renamed function would
  // otherwise compare two empty sets and pass.
  assert.ok(
    published.length >= 10,
    `only ${published.length} code(s) parsed out of the published status table, so this is not a comparison`,
  );
  assert.ok(
    mapped.length >= 10,
    `only ${mapped.length} case label(s) parsed out of describeError, so this is not a comparison`,
  );

  const missing = published.filter((c) => !mapped.includes(c));
  assert.deepEqual(
    missing,
    [],
    `describeError has no case for ${missing.join(', ')}. A code the server can send with no case here falls into \`default\` and renders "Something was wrong with that request." - a generic message for a specific, actionable failure. Every case in this file's table is driven by a code typed into the test, so none of them notices a code that is missing.`,
  );

  // And the reverse, which is the other half of "as a SET": a case for a code the
  // contract does not publish means the mapper knows something the contract does
  // not, and a client coding against the document has no row for what they see.
  const extra = mapped.filter((c) => !published.includes(c));
  assert.deepEqual(
    extra,
    [],
    `describeError maps ${extra.join(', ')}, which docs/error-model.md does not publish as a status code. Either the code needs a row - a client coding against the document has nothing to match on - or it is reserved and should say so, as wrong_credential_type does.`,
  );
});

test('the reserved code is mapped, and the document says it is reserved', () => {
  // This is the one row where "publish it" and "the server emits it" genuinely
  // differ, and the difference is deliberate: docs/error-model.md marks
  // `wrong_credential_type` "Reserved, never emitted - a cookie on /v1/*, or a key
  // on a cookie endpoint, returns 401 unauthenticated". The mapper keeps a case
  // for it anyway. Both halves of that decision are asserted, because either one
  // changing alone is the drift this file exists to catch: dropping the case makes
  // a reserved code render generically if it ever IS emitted, and dropping the
  // "Reserved" note turns a deliberate non-emission into a bug report.
  const doc = readFileSync(new URL('../../docs/error-model.md', import.meta.url), 'utf8');
  const row = doc.split('\n').find((l) => l.includes('`wrong_credential_type`'));
  assert.ok(row, 'the reserved code must have a row in the published table');
  assert.match(
    row,
    /Reserved, never emitted/,
    'the row for wrong_credential_type must still say it is reserved and never emitted; without it a reader finds a documented code the server never sends and files it as a bug',
  );
  assert.ok(
    mappedCodes().includes('wrong_credential_type'),
    'the mapper must keep a case for the reserved code, so that IF it is ever emitted it renders as a deliberate unexpected-credential-type message rather than the generic one',
  );
});
