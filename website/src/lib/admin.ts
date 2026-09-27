// Pure logic for the operator surface (/admin).
//
// It lives in lib/ rather than in the .astro frontmatter for the same reason
// lib/dashboard-form.ts does: the test suite loads this module directly with
// Node (explicit .ts extensions, no bundler), and these are exactly the rules
// that must not silently invert.
//
// Sources: docs/admin-surface.md (the operator capabilities and the safety
// rules) and docs/server/api-spec.md (the four admin routes). Nothing here is
// guessed.
//
// The single most important fact about this module: NOTHING IT COMPUTES IS
// AUTHORIZATION. The server decides (`admin.rs::require_operator`) and answers
// 403 regardless of what this file believes. Everything here exists only to
// avoid showing an operator a control that cannot work, and to keep the copy
// honest about what an action does.

/**
 * The account states the admin surface understands.
 *
 * `active` and `suspended` are the two an operator can move between;
 * `closed` is terminal and is neither suspendable nor resumable. Mirrors the
 * `status` CHECK in the accounts table.
 */
export type AccountStatus = 'active' | 'suspended' | 'closed';

/**
 * Whether an operator may suspend this account.
 *
 * docs/server/api-spec.md: only an `active` account can be suspended; anything
 * else is a 409 and writes no audit row. Returning true here for a
 * suspended/closed account would offer a button whose only outcome is that 409.
 */
export function canSuspend(status: AccountStatus): boolean {
  return status === 'active';
}

/**
 * Whether an operator may resume this account.
 *
 * Only a `suspended` account can be resumed — the server refuses the rest.
 * Note that resuming does NOT restore the keys and sessions suspend revoked;
 * the caller's copy must say so (see `describeAction`).
 */
export function canResume(status: AccountStatus): boolean {
  return status === 'suspended';
}

/** The verb to put on the button for the action a given status allows. */
export function actionLabel(action: 'suspend' | 'resume'): string {
  return action === 'suspend' ? 'Suspend account' : 'Resume account';
}

/**
 * The exact, honest consequence text for an action, shown in the confirmation.
 *
 * These strings are load-bearing. docs/admin-surface.md is explicit that a
 * suspend revokes every live session AND every live key atomically, and that a
 * resume deliberately does NOT bring them back — an operator who assumes
 * otherwise will "resume" an account and believe the customer can use it again
 * when they cannot. The copy states the effect, not a euphemism.
 */
export function describeAction(action: 'suspend' | 'resume'): string {
  return action === 'suspend'
    ? 'This sets the account to suspended and revokes every live session and every live API key immediately. The customer is logged out everywhere and cannot mint new credentials. It does not move money.'
    : 'This sets the account back to active. It does NOT restore the sessions or API keys that suspension revoked — the customer must sign in again and create a new key.';
}

/**
 * The endpoint for an action, per docs/server/api-spec.md.
 *
 * `/restore` is a documented alias of `/resume` hitting the same handler; this
 * surface uses the canonical `/resume` so there is one name in one place.
 */
export function actionPath(accountId: string, action: 'suspend' | 'resume'): string {
  return `/api/admin/accounts/${encodeURIComponent(accountId)}/${action}`;
}

/** The read-only lookup endpoint for one account. */
export function accountPath(accountId: string): string {
  return `/api/admin/accounts/${encodeURIComponent(accountId)}`;
}

/** The account view the server returns from GET /api/admin/accounts/:id. */
export interface AdminAccount {
  account_id: string;
  status: AccountStatus;
  is_operator: boolean;
  created_at: string;
  balance_idr: number;
  /** Sessions usable right now: unrevoked AND unexpired. */
  live_sessions: number;
  /** Keys usable right now: unrevoked. */
  live_keys: number;
}

/** The outcome of a suspend/resume, including what it revoked. */
export interface AdminActionResult {
  account_id: string;
  status: AccountStatus;
  sessions_revoked: number;
  keys_revoked: number;
}

/**
 * Whether a string is a plausible account id, before spending a round trip on it.
 *
 * The server takes a UUID in the path; a non-UUID would make axum reject the
 * request with a non-JSON 400, which docs/error-model.md forbids and the UI
 * would render as a mystery. Checking the shape first keeps the failure local
 * and legible. This is a WELL-FORMEDNESS check, not an existence check — the
 * server still decides whether the account exists.
 */
export function isAccountId(candidate: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(
    candidate.trim(),
  );
}

/**
 * What to tell the operator when a lookup or action failed, given the status.
 *
 * The admin surface has one failure mode a customer page does not: **403**,
 * which means the signed-in user is not an operator. Telling them "forbidden"
 * with no more is unhelpful; the useful sentence is that operator access is
 * required. A 404 is genuinely "no such account". Everything else is delegated
 * to the shared error mapper.
 */
export function adminErrorMessage(status: number, fallback: string): string {
  if (status === 403) return 'Operator access is required for this action.';
  if (status === 401) return 'Your session has ended. Sign in again.';
  if (status === 404) return 'No account with that id.';
  if (status === 409) return 'That account is not in a state this action allows.';
  return fallback;
}

/**
 * The self-action rule, surfaced in the UI.
 *
 * docs/admin-surface.md: an operator may not act on their own account — not
 * even read it (their own account is at GET /api/me). The server enforces this
 * (refuse_self_action), so the UI checks it only to avoid a pointless request
 * and to explain why the control is absent.
 */
export function isSelfAction(operatorAccountId: string, targetAccountId: string): boolean {
  return operatorAccountId.toLowerCase() === targetAccountId.trim().toLowerCase();
}

/** One row of `GET /api/admin/accounts`. Same fields as the single view. */
export interface AdminAccountSummary {
  account_id: string;
  status: AccountStatus;
  is_operator: boolean;
  created_at: string;
  balance_idr: number;
  live_sessions: number;
  live_keys: number;
}

/** The listing page the server returns from `GET /api/admin/accounts`. */
export interface AdminAccountList {
  accounts: AdminAccountSummary[];
  limit: number;
  offset: number;
}

/** The page size the console requests; matches the server's default. */
export const ADMIN_LIST_LIMIT = 25;

/**
 * The listing URL for a filter, with the page parameters the API expects.
 *
 * Only non-empty values are sent, so the request the operator makes is the one
 * they can read in the address bar: a blank search box sends no `q` at all,
 * rather than `q=` which the server would have to treat as absent anyway.
 */
export function accountListPath(
  filter: { q?: string; status?: string; offset?: number; limit?: number } = {},
): string {
  const params = new URLSearchParams();
  const q = filter.q?.trim();
  if (q) params.set('q', q);
  const status = filter.status?.trim();
  if (status) params.set('status', status);
  params.set('limit', String(filter.limit ?? ADMIN_LIST_LIMIT));
  if (filter.offset && filter.offset > 0) params.set('offset', String(filter.offset));
  return `/api/admin/accounts?${params.toString()}`;
}

/**
 * The account ids in a listing that the operator may act on.
 *
 * This exists so the console never offers a control the server will refuse. Two
 * exclusions, both server-enforced and both explained in the UI:
 *
 * 1. **The operator's own account** — `refuse_self_action` refuses it, and the
 *    operator's own balance/keys are visible on their own dashboard anyway.
 * 2. Nothing else. Suspend/resume eligibility is a function of the row's
 *    `status` (see `canSuspend`/`canResume`), which the row itself carries.
 *
 * Returning a Set keeps the caller's lookup O(1) per row when rendering a page.
 */
export function actionableAccountIds(
  accounts: readonly AdminAccountSummary[],
  operatorAccountId: string,
): Set<string> {
  const mine = operatorAccountId.trim().toLowerCase();
  return new Set(
    accounts
      .filter((a) => a.account_id.toLowerCase() !== mine)
      .map((a) => a.account_id),
  );
}

/**
 * Whether there is a next page, given the page the server returned.
 *
 * A full page MAY have more after it, so this is deliberately optimistic: it
 * returns true only when the page came back full to its own limit. A short page
 * is provably the last one, so "Next" is hidden rather than shown and then
 * leading to an empty screen.
 */
export function hasNextPage(list: AdminAccountList): boolean {
  return list.accounts.length >= list.limit;
}

/** A human count for the listing heading. */
export function listSummary(list: AdminAccountList): string {
  const n = list.accounts.length;
  const noun = n === 1 ? 'account' : 'accounts';
  if (n === 0) return 'No accounts match';
  return `${n} ${noun}${hasNextPage(list) ? ' (more available)' : ''}`;
}

/** One row of `GET /api/admin/accounts/:id/audit`. */
export interface AdminAuditEntry {
  id: number;
  operator_id: string;
  action: string;
  target_type: string;
  target_id: string;
  detail: string | null;
  created_at: string;
}

/** The audit page the server returns. */
export interface AdminAuditList {
  entries: AdminAuditEntry[];
  limit: number;
}

/** The audit path for one account. */
export function accountAuditPath(accountId: string, limit = 50): string {
  return `/api/admin/accounts/${encodeURIComponent(accountId)}/audit?limit=${limit}`;
}

/**
 * A plain-language label for an audit action.
 *
 * The action is a machine value stored in the DB; this is the operator-facing
 * word for it. An unknown action is shown as-is rather than blanked — a new
 * action type must still be visible in the trail, not hidden because the UI did
 * not learn its label yet.
 */
export function auditActionLabel(action: string): string {
  switch (action) {
    case 'suspend':
      return 'Suspended';
    case 'resume':
      return 'Resumed';
    default:
      return action;
  }
}

/**
 * A human summary of the audit row's `detail` JSON, or null when there is none.
 *
 * `detail` is free-form JSON text the handlers wrote (counts of what was
 * revoked, the prior status). Parsed defensively: a value that is not JSON, or
 * JSON of an unexpected shape, returns null rather than being rendered raw — the
 * trail's readability must not depend on a field's format staying fixed.
 */
export function auditDetailSummary(detail: string | null): string | null {
  if (detail === null || detail.trim() === '') return null;
  let parsed: unknown;
  try {
    parsed = JSON.parse(detail);
  } catch {
    return null;
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) return null;

  const record = parsed as Record<string, unknown>;
  const parts: string[] = [];
  if (typeof record.status_from === 'string' && typeof record.status_to === 'string') {
    parts.push(`${record.status_from} → ${record.status_to}`);
  }
  if (typeof record.sessions_revoked === 'number') {
    parts.push(`${record.sessions_revoked} session(s) revoked`);
  }
  if (typeof record.keys_revoked === 'number') {
    parts.push(`${record.keys_revoked} key(s) revoked`);
  }
  return parts.length === 0 ? null : parts.join(' · ');
}

/** A short, unambiguous timestamp for a trail row, in WIB. */
export function formatAuditTime(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return iso;
  return new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Asia/Jakarta',
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(new Date(ms));
}
