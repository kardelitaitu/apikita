// Error codes -> what the user sees.
//
// docs/website/03-functional-spec.md defines a UI behaviour for every one of the
// 14 machine-readable codes in docs/error-model.md. Rules that follow from it and
// are honoured here: show request_id on 5xx, never offer a retry for 402/403,
// never render a raw server message, and never treat a 503 as a login failure.

import { ApiError } from './api';

export interface ErrorView {
  message: string;
  /** Support's starting point. Only present for 5xx. */
  requestId: string | null;
  /** Field to highlight, from details.field (422). */
  field: string | null;
  /** 402 and 403 are permanent until the user acts — no retry offered. */
  retryable: boolean;
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
      return { ...base, message: 'Too many requests. Wait a moment and try again.', retryable: true };
    case 'no_upstream_available':
      // Browsing still works; inference is degraded.
      return { ...base, message: 'Inference is degraded right now. Your dashboard still works.', retryable: true };
    case 'internal_error':
      return { ...base, message: 'Something went wrong on our side.', requestId: err.requestId, retryable: true };
    default:
      return { ...base, message: err.status >= 500 ? 'Something went wrong on our side.' : err.message, requestId: err.status >= 500 ? err.requestId : null };
  }
}
