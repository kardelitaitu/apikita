// The pure half of the public auth pages: /signup, /verify, /reset, /reset/confirm.
//
// Identity lives in PocketBase (docs/architecture/identity.md lines 35-38), so
// these pages are SDK calls from the browser — not new server routes. The Rust
// API has exactly /auth/exchange, /auth/logout and /auth/logout-all.
//
// Everything here is deliberately argument-free where the spec demands
// neutrality. docs/website/03-functional-spec.md line 90: "Always respond with
// the same neutral message", and "### Duplicate email": "do not reveal that it
// exists". A function that is never told whether the account exists is a
// function that cannot leak it — that is why these take no such argument.
//
// Node loads this module directly (the test suite), which is why the import
// carries an explicit .ts extension.

import { describeError } from './errors.ts';

/** PocketBase's default minimum. Shown up front, per spec line 30. */
export const MIN_PASSWORD_LENGTH = 8;

/**
 * docs/website/03-functional-spec.md line 41, verbatim: "Show: 'If this email is
 * registered, we've sent sign-in instructions.'" Used for the signup reply, not
 * only for the duplicate case — see `signupReply`.
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
 * Covers all three landing states the spec asks for at /verify. PocketBase
 * clears a verification token when it is used, so an already-verified address
 * arrives here as an unusable token rather than as a distinct code.
 */
export const VERIFICATION_FAILED_MESSAGE =
  'This verification link is invalid, has expired, or has already been used. Sign in to continue.';

/** Spec line 92: "new password -> saved -> all sessions invalidated". */
export const PASSWORD_RESET_DONE_MESSAGE =
  'Password updated. Every session on this account has been signed out \u2014 sign in again with the new password.';

/** PocketBase errors carry a numeric `status`; 0 means the request never left. */
interface PocketBaseError {
  status?: unknown;
  data?: { data?: Record<string, { message?: unknown }> } | null;
}

function statusOf(err: unknown): number | null {
  if (err === null || typeof err !== 'object') return null;
  const status = (err as PocketBaseError).status;
  return typeof status === 'number' ? status : null;
}

/**
 * A request that never reached the server. Every other answer is a server answer,
 * and on a neutral page the two must be told apart: a transport failure is the
 * only thing a page may admit to.
 */
export function isUnreachable(err: unknown): boolean {
  return statusOf(err) === 0;
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
 * The token PocketBase put in the link. Read from the query string only — there
 * is one place a token is read, so there is one place to audit.
 */
export function readAuthToken(search: string): string | null {
  const token = new URLSearchParams(search).get('token');
  return token !== null && token.length > 0 ? token : null;
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

/** Field names PocketBase reports a 400 against. */
const EMAIL_FIELD = 'email';
const PASSWORD_FIELDS = ['password', 'passwordConfirm'];

/**
 * What a PocketBase failure shows the user.
 *
 * Spec "### Error codes -> UI" rule 5: "Never render a raw error body. Server
 * messages are for support, not users." PocketBase's own messages are raw error
 * bodies, so none of them pass through here — every branch returns our copy.
 *
 * `emailConflictMessage` is passed in because the neutral wording differs per
 * page (signup vs reset); it is never chosen by inspecting the error.
 */
export function describeAuthError(err: unknown, emailConflictMessage: string): string {
  const status = statusOf(err);
  if (status === null) return describeError(err).message;
  if (status === 0) return UNREACHABLE_MESSAGE;
  if (status === 401) return 'Email or password is incorrect.';
  if (status === 404) return 'That link is no longer valid. Request a new one.';
  if (status === 429) return 'Too many attempts. Try again later.';

  const fields = (err as PocketBaseError).data?.data;
  if (status === 400 && fields !== null && typeof fields === 'object') {
    if (EMAIL_FIELD in fields) return emailConflictMessage;
    if (PASSWORD_FIELDS.some((field) => field in fields)) {
      return `Password must be at least ${MIN_PASSWORD_LENGTH} characters, and both entries must match.`;
    }
  }

  return status >= 500
    ? 'Something went wrong on our side.'
    : 'That did not work. Check the details and try again.';
}
