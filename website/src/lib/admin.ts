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
