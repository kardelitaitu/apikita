// Error codes -> what the user sees.
//
// docs/website/03-functional-spec.md defines a UI behaviour for every one of the
// 14 machine-readable codes in docs/error-model.md. Rules that follow from it and
// are honoured here: show request_id on 5xx, never offer a retry for 402/403,
// never render a raw server message, and never treat a 503 as a login failure.

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
    case 'insufficient_balance':
      return { ...base, message: 'Balance too low. Top up to continue.', retryable: false };
    case 'key_limit_exceeded':
      return { ...base, message: 'This key hit its limit. Raise it or create a new key.', retryable: false };
    case 'model_not_allowed':
      return { ...base, message: 'This key cannot use that model.', retryable: false };
    case 'wrong_credential_type':
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
      // Browsing still works; inference is degraded.
      return { ...base, message: 'Inference is degraded right now. Your dashboard still works.', retryable: true };
    case 'internal_error':
      return { ...base, message: 'Something went wrong on our side.', requestId: err.requestId, retryable: true };
    default:
      return { ...base, message: err.status >= 500 ? 'Something went wrong on our side.' : err.message, requestId: err.status >= 500 ? err.requestId : null };
  }
}
