// Error codes -> what the user sees.
//
// docs/website/03-functional-spec.md defines a UI behaviour for every HTTP code
// in docs/error-model.md. Rules that follow from it and are honoured here: show
// request_id on 5xx, never offer a retry for 402/403, never render a raw server
// message, and never treat a 503 as a login failure. An unknown code still falls
// through to safe copy rather than the server's own words.

import { ApiError } from './api.ts';
import { rateLimitedMessage } from './retry-wait.ts';

export interface ErrorView {
  message: string;
  /** Support's starting point. Present for 5xx, and for a 429. */
  requestId: string | null;
  /** Field to highlight, from details.field (422). */
  field: string | null;
  /** 402 and 403 are permanent until the user acts — no retry offered. */
  retryable: boolean;
}

/** What every notice shows: the mapped message and the correlation id. */
export interface Noticeable {
  message: string;
  requestId: string | null;
}

/** The two elements the wallet's two-line notice writes to. */
export interface NoticeTarget {
  message: { textContent: string | null };
  correlation: { textContent: string | null };
}

/** The correlation line under a notice: empty when there is no id, so the element
 * is blanked rather than left showing a dangling label. */
function correlationLine(requestId: string | null): string {
  return requestId === null ? '' : `request_id: ${requestId}`;
}

/**
 * The wallet notice: the message, then the correlation line beneath it.
 *
 * The id is read from `notice` rather than passed beside the message, so a caller
 * has no second argument to drop — an error path hands over the whole view.
 */
export function renderNotice(target: NoticeTarget, notice: Noticeable): void {
  target.message.textContent = notice.message;
  target.correlation.textContent = correlationLine(notice.requestId);
}

/**
 * The keys modal's notice: the same two facts on one line, because it has one
 * element. Same single-argument rule as `renderNotice`.
 */
export function inlineNotice(notice: Noticeable): string {
  return notice.requestId === null ? notice.message : `${notice.message} (${notice.requestId})`;
}

export function describeError(err: unknown): ErrorView {
  if (!(err instanceof ApiError)) {
    return {
      message: err instanceof Error ? err.message : 'Something went wrong.',
      requestId: null,
      field: null,
      retryable: true,
    };
  }

  const field = typeof err.details?.field === 'string' ? err.details.field : null;
  const base: ErrorView = { message: err.message, requestId: null, field, retryable: false };

  switch (err.code) {
    case 'invalid_request':
      return { ...base, message: 'Something was wrong with that request.', retryable: false };
    case 'unauthenticated':
      return { ...base, message: 'Your session has ended. Sign in again.', retryable: true };
    case 'key_revoked':
      // docs/website/03-functional-spec.md: "This key was revoked." The remedy is
      // part of the sentence, the same way key_limit_exceeded states its own.
      return { ...base, message: 'This key was revoked. Create a new one.', retryable: false };
    case 'key_expired':
      // The spec asks for the expiry date, but the error body carries no date, so
      // the message stays honest about what it knows rather than inventing one.
      return { ...base, message: 'This key expired. Create a new one.', retryable: false };
    case 'insufficient_balance':
      return { ...base, message: 'Balance too low. Top up to continue.', retryable: false };
    case 'key_limit_exceeded':
      return { ...base, message: 'This key hit its limit. Raise it or create a new key.', retryable: false };
    case 'model_not_allowed':
      return { ...base, message: 'This key cannot use that model.', retryable: false };
    case 'forbidden':
      // docs/error-model.md: authenticated, but not permitted — permanent until
      // the user's access changes, so no retry is offered.
      return { ...base, message: 'You do not have permission to do that.', retryable: false };
    case 'wrong_credential_type':
      // Reserved, never emitted (docs/error-model.md): a wrong-type credential
      // arrives as 401 unauthenticated. Mapped defensively if it ever appears.
      return { ...base, message: 'Unexpected credential type. This is a bug on our side.', retryable: false };
    case 'not_found':
      return { ...base, message: 'Not found.', retryable: false };
    case 'conflict':
      return { ...base, message: 'That already exists. Refresh to see the current state.', retryable: true };
    case 'validation_failed':
      return { ...base, message: field ? `Invalid value for "${field}".` : 'Invalid value.', retryable: false };
    case 'rate_limited':
      // The server's Retry-After IS the wait (docs/error-model.md): an hourly cap
      // is not "a moment". A 429 is a 4xx, so the 5xx-only rule above must not
      // swallow its request id either.
      return { ...base, message: rateLimitedMessage(err.retryAfterSeconds), requestId: err.requestId, retryable: true };
    case 'no_upstream_available':
      // A 503 is a 5xx, so request_id applies (docs/website/03-functional-spec.md
      // rule 1). Browsing still works; only inference is degraded.
      return { ...base, message: 'Inference is degraded right now. Your dashboard still works.', requestId: err.requestId, retryable: true };
    case 'internal_error':
      return { ...base, message: 'Something went wrong on our side.', requestId: err.requestId, retryable: true };
    default:
      // An unknown code is still server output: never render its raw message
      // (rule 5). A 5xx keeps its correlation id, a 4xx reads like a malformed
      // request rather than a raw body.
      return {
        ...base,
        message: err.status >= 500
          ? 'Something went wrong on our side.'
          : 'Something was wrong with that request.',
        requestId: err.status >= 500 ? err.requestId : null,
        retryable: err.status >= 500,
      };
  }
}
