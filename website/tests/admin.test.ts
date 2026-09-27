// Executable contract for the operator surface's pure logic (src/lib/admin.ts).
//
// The rules that matter most here are the state gates and the consequences:
// docs/server/api-spec.md says only an 'active' account can be suspended and
// only a 'suspended' one resumed, and docs/admin-surface.md says a resume does
// NOT restore what a suspend revoked. Copy that implied otherwise would have
// operators believing a resumed account can spend again when it cannot.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  accountListPath,
  accountPath,
  actionableAccountIds,
  actionPath,
  adminErrorMessage,
  canResume,
  canSuspend,
  describeAction,
  hasNextPage,
  isAccountId,
  isSelfAction,
  listSummary,
  type AccountStatus,
  type AdminAccountList,
  type AdminAccountSummary,
} from '../src/lib/admin.ts';

const ALL_STATUSES: AccountStatus[] = ['active', 'suspended', 'closed'];

test('only an active account can be suspended, only a suspended one resumed', () => {
  assert.equal(canSuspend('active'), true);
  assert.equal(canSuspend('suspended'), false);
  assert.equal(canSuspend('closed'), false);

  assert.equal(canResume('suspended'), true);
  assert.equal(canResume('active'), false);
  assert.equal(canResume('closed'), false);

  // No status may offer BOTH actions at once — two contradictory buttons.
  // 'closed' deliberately offers neither (it is terminal), so the invariant is
  // "never both", not "exactly one".
  for (const status of ALL_STATUSES) {
    assert.ok(!(canSuspend(status) && canResume(status)), `status ${status} offers both`);
  }
  assert.equal(canSuspend('closed') || canResume('closed'), false);
});

test('a resume does NOT claim to restore sessions or keys', () => {
  const suspend = describeAction('suspend').toLowerCase();
  const resume = describeAction('resume').toLowerCase();

  // Suspend states both revocations.
  assert.match(suspend, /session/);
  assert.match(suspend, /key/);

  // Resume must say it does not restore them. The word "not" carrying the
  // meaning is what stops an operator assuming the account just works again.
  assert.match(resume, /not restore|does not restore|does not bring/);
});

test('a suspend says it does not move money', () => {
  // docs/admin-surface.md: suspend is a security action, not a financial one.
  // An operator must not fear it debits the customer.
  assert.match(describeAction('suspend').toLowerCase(), /does not move money|not move money/);
});

test('the action paths target the canonical routes', () => {
  const id = '3f7c1a2e-9b64-4d0a-8f21-5c6e0b9d4a10';
  assert.equal(accountPath(id), `/api/admin/accounts/${id}`);
  assert.equal(actionPath(id, 'suspend'), `/api/admin/accounts/${id}/suspend`);
  assert.equal(actionPath(id, 'resume'), `/api/admin/accounts/${id}/resume`);
});

test('account ids are checked for UUID shape before a request is sent', () => {
  assert.equal(isAccountId('3f7c1a2e-9b64-4d0a-8f21-5c6e0b9d4a10'), true);
  // Case-insensitive, and surrounding whitespace is tolerated.
  assert.equal(isAccountId('  3F7C1A2E-9B64-4D0A-8F21-5C6E0B9D4A10  '), true);
  // Not a uuid.
  assert.equal(isAccountId(''), false);
  assert.equal(isAccountId('abc'), false);
  assert.equal(isAccountId('3f7c1a2e9b644d0a8f215c6e0b9d4a10'), false);
  assert.equal(isAccountId('3f7c1a2e-9b64-4d0a-8f21-5c6e0b9d4a1'), false);
});

test('a 403 explains that operator access is required, not "forbidden"', () => {
  const message = adminErrorMessage(403, 'Forbidden');
  assert.match(message.toLowerCase(), /operator access/);
  assert.notEqual(message, 'Forbidden');
});

test('a 404 is "no such account" and a 409 is a state conflict', () => {
  assert.match(adminErrorMessage(404, 'x').toLowerCase(), /no account/);
  assert.match(adminErrorMessage(409, 'x').toLowerCase(), /not in a state/);
  // Anything unmapped falls back to the caller's own message.
  assert.equal(adminErrorMessage(500, 'Server error'), 'Server error');
});

test('the self-action check is case-insensitive and trims the target', () => {
  const mine = '3f7c1a2e-9b64-4d0a-8f21-5c6e0b9d4a10';
  assert.equal(isSelfAction(mine, mine), true);
  assert.equal(isSelfAction(mine, mine.toUpperCase()), true);
  assert.equal(isSelfAction(mine, ` ${mine} `), true);
  assert.equal(isSelfAction(mine, '00000000-0000-0000-0000-000000000000'), false);
});

// --- The listing ------------------------------------------------------------

const OP = '3f7c1a2e-9b64-4d0a-8f21-5c6e0b9d4a10';

function summary(overrides: Partial<AdminAccountSummary> = {}): AdminAccountSummary {
  return {
    account_id: OP,
    status: 'active',
    is_operator: false,
    created_at: '2026-01-01T00:00:00Z',
    balance_idr: 0,
    live_sessions: 0,
    live_keys: 0,
    ...overrides,
  };
}

test('the listing path sends only the parameters that are set', () => {
  // A blank search sends no q at all, not q=.
  assert.equal(accountListPath(), `/api/admin/accounts?limit=25`);
  assert.equal(accountListPath({ q: '   ' }), `/api/admin/accounts?limit=25`);
  // A real filter is carried, trimmed.
  assert.match(accountListPath({ q: '  abc  ' }), /q=abc/);
  assert.match(accountListPath({ status: 'suspended' }), /status=suspended/);
  // offset 0 is omitted; a positive offset is carried.
  assert.doesNotMatch(accountListPath({ offset: 0 }), /offset/);
  assert.match(accountListPath({ offset: 25 }), /offset=25/);
  // A custom limit is honoured.
  assert.match(accountListPath({ limit: 5 }), /limit=5/);
});

test('a filter value is URL-encoded, not spliced raw', () => {
  // A percent sign must not reach the API as a bare wildcard-looking char in the
  // query string; URLSearchParams encodes it.
  const path = accountListPath({ q: '%' });
  assert.match(path, /q=%25/);
});

test('the actionable ids exclude the operator but keep everyone else', () => {
  const other = '00000000-0000-0000-0000-000000000001';
  const ids = actionableAccountIds(
    [summary({ account_id: OP }), summary({ account_id: other, is_operator: true })],
    OP,
  );
  assert.equal(ids.has(OP), false, 'the operator must not be offered an action on themselves');
  // Another operator IS listed; whether an action is possible is the status rule,
  // which the caller applies separately. The id set only removes self.
  assert.equal(ids.has(other), true);
});

test('the actionable-ids match is case-insensitive', () => {
  const ids = actionableAccountIds([summary({ account_id: OP })], OP.toUpperCase());
  assert.equal(ids.size, 0);
});

test('a full page reports a possible next page; a short one does not', () => {
  const full: AdminAccountList = { accounts: Array.from({ length: 25 }, () => summary()), limit: 25, offset: 0 };
  const short: AdminAccountList = { accounts: [summary()], limit: 25, offset: 0 };
  const empty: AdminAccountList = { accounts: [], limit: 25, offset: 0 };
  assert.equal(hasNextPage(full), true);
  assert.equal(hasNextPage(short), false);
  assert.equal(hasNextPage(empty), false);
});

test('the listing summary counts and flags more pages', () => {
  assert.equal(listSummary({ accounts: [], limit: 25, offset: 0 }), 'No accounts match');
  assert.equal(listSummary({ accounts: [summary()], limit: 25, offset: 0 }), '1 account');
  const full: AdminAccountList = { accounts: Array.from({ length: 25 }, () => summary()), limit: 25, offset: 0 };
  assert.match(listSummary(full), /25 accounts/);
  assert.match(listSummary(full), /more available/);
});
