// The pure half of the public auth pages: /signup, /verify, /reset, /reset/confirm.
//
// Identity is served natively by the Rust API (lib/auth-api.ts), so these pages
// call our own routes with a session cookie. There is no PocketBase SDK in this
// path and no token exchange: the handlers set the cookie themselves.
//
// Everything here is deliberately argument-free where the spec demands
// neutrality. docs/website/03-functional-spec.md: "Always respond with the same
// neutral message", and "### Duplicate email": "do not reveal that it exists". A
// function that is never told whether the account exists is a function that cannot
// leak it — that is why these take no such argument.
//
// Node loads this module directly (the test suite), which is why the import
// carries an explicit .ts extension.

import { ApiError } from './api.ts';
import { describeError } from './errors.ts';

/** Shown up front, per spec. Mirrors `[auth] password_min_length` in config. */
export const MIN_PASSWORD_LENGTH = 8;

/**
 * docs/website/03-functional-spec.md, verbatim: "Show: 'If this email is
 * registered, we've sent sign-in instructions.'" Used for the signup reply, not
 * only for the duplicate case — see `signupReply`.
 *
 * The server sends its own neutral wording in the 202 body and it is deliberately
 * not used here: this string is the page's copy, fixed before any request is made,
 * so no response can vary it.
 */
export const DUPLICATE_SIGNUP_MESSAGE =
  'If this email is registered, we\'ve sent sign-in instructions.';

/**
 * The reset page's single answer (spec line 90). Wording follows the duplicate-email
 * line above so the two neutral replies read as one voice.
 */
export const NEUTRAL_RESET_MESSAGE =
  'If this email is registered, we\'ve sent password reset instructions.';

/**
 * The one failure a neutral page is allowed to admit: the request never reached
 * the server, so there is no answer to be neutral about.
 */
export const UNREACHABLE_MESSAGE =
  'Could not reach the identity service. Check your connection and try again.';

/**
 * Covers all three landing states the spec asks for at /verify. A verification
 * token is single-use and consumed by redemption, so an already-verified address
 * arrives here as an unusable token rather than as a distinct code.
 */
export const VERIFICATION_FAILED_MESSAGE =
  'This verification link is invalid, has expired, or has already been used. Sign in to continue.';

/** Spec line 92: "new password -> saved -> all sessions invalidated". */
export const PASSWORD_RESET_DONE_MESSAGE =
  'Password updated. Every session on this account has been signed out \u2014 sign in again with the new password.';

/**
 * A request that never reached the server.
 *
 * There is no numeric status to read here. `ApiError` is constructed only from a
 * RESPONSE, so its absence is the signal: a transport failure throws the
 * platform's own `TypeError` from `fetch` and never becomes an `ApiError`. The
 * retired PocketBase SDK instead reported status 0 for the same case, which is
 * what the old shape checked for; checking for `0` against `ApiError` would have
 * matched nothing and quietly turned every network failure into a server answer.
 *
 * Every other answer IS a server answer, and on a neutral page the two must be
 * told apart: a transport failure is the only thing a page may admit to.
 */
export function isUnreachable(err: unknown): boolean {
  return !(err instanceof ApiError);
}

/**
 * The signup reply, for both outcomes of the email + password path.
 *
 * One string for created, for already-registered and for rejected, because a
 * reply that varies with the outcome is an enumeration oracle — the exact thing
 * spec lines 38-43 forbid. Only a request that never reached the server says so.
 */
export function signupReply(err: unknown): string {
  return isUnreachable(err) ? UNREACHABLE_MESSAGE : DUPLICATE_SIGNUP_MESSAGE;
}

/**
 * The reset reply: the neutral message for every server answer, including one
 * that names the address as unknown. Only a transport failure is admitted.
 */
export function resetRequestReply(err: unknown): string {
  return isUnreachable(err) ? UNREACHABLE_MESSAGE : NEUTRAL_RESET_MESSAGE;
}

/** Password rules are shown up front (spec line 30), never revealed on error. */
export function passwordRuleError(password: string): string | null {
  if (password.length < MIN_PASSWORD_LENGTH) {
    return `Password must be at least ${MIN_PASSWORD_LENGTH} characters.`;
  }
  return null;
}

/** A typo in the confirmation must be caught before anything is sent. */
export function confirmationError(password: string, confirmation: string): string | null {
  return password === confirmation ? null : 'The two passwords do not match.';
}

/**
 * The token the link carried. Read from the query string only — there is one place
 * a token is read, so there is one place to audit.
 */
export function readAuthToken(search: string): string | null {
  const token = new URLSearchParams(search).get('token');
  return token !== null && token.length > 0 ? token : null;
}

/**
 * The address the link was sent to, when the mail template carries it.
 *
 * The reset-confirm endpoint requires the address alongside the token, so the page
 * needs one from somewhere. `?email=` is a convenience, not a credential: the token
 * is what authorises the reset, and the server refuses a mismatched pair. A link
 * without it means the page has to ask (`reset/confirm.astro` does).
 */
export function readAuthEmail(search: string): string | null {
  const email = new URLSearchParams(search).get('email');
  return email !== null && email.length > 0 ? email : null;
}

/**
 * The same URL with the token removed (spec line 95: "never echoed back in a URL
 * that gets logged"). Called immediately after the read, so the token does not
 * survive in the address bar, in a bookmark, or in the referrer of the next
 * navigation. Other parameters and the fragment are preserved.
 */
export function withoutAuthToken(href: string): string {
  const hashAt = href.indexOf('#');
  const hash = hashAt === -1 ? '' : href.slice(hashAt);
  const beforeHash = hashAt === -1 ? href : href.slice(0, hashAt);
  const queryAt = beforeHash.indexOf('?');
  if (queryAt === -1) return href;

  const params = new URLSearchParams(beforeHash.slice(queryAt));
  params.delete('token');
  const rest = params.toString();
  return beforeHash.slice(0, queryAt) + (rest ? `?${rest}` : '') + hash;
}

/**
 * What an auth failure shows the user.
 *
 * Spec "### Error codes -> UI" rule 5: "Never render a raw error body. Server
 * messages are for support, not users." The server's own messages never pass
 * through here — every branch returns our copy, and the fallback goes through
 * `describeError`, which maps by stable error CODE rather than by prose.
 *
 * `emailConflictMessage` is passed in because the neutral wording differs per page
 * (signup vs reset); it is never chosen by inspecting the error.
 */
export function describeAuthError(err: unknown, emailConflictMessage: string): string {
  if (isUnreachable(err)) return UNREACHABLE_MESSAGE;

  const status = (err as ApiError).status;
  if (status === 401) return 'Email or password is incorrect.';
  if (status === 429) return 'Too many attempts. Try again later.';

  // A rejected field is named structurally, `details.field`, not as a map of
  // per-field prose: the old PocketBase SDK reported `data.data.<field>.message`
  // and this branch read that shape, which no longer exists.
  const field = (err as ApiError).details?.field;
  if (typeof field === 'string') {
    if (field === 'email') return emailConflictMessage;
    if (field === 'password' || field === 'passwordConfirm') {
      return `Password must be at least ${MIN_PASSWORD_LENGTH} characters, and both entries must match.`;
    }
  }

  if (status >= 500) return 'Something went wrong on our side.';
  return describeError(err).message;
}
