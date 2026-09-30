// The one HTTP entry point for dashboard islands.
//
// Endpoints, the error shape and the 401 behaviour come from
// docs/server/api-spec.md, docs/error-model.md and
// docs/website/03-functional-spec.md. Nothing here is guessed.

import { parseRetryAfter } from './retry-wait.ts';

/**
 * Server origin. No production domain is settled yet, so this is a build-time
 * PUBLIC_* variable (see .env.example) defaulting to the documented local API.
 * Optional-chained: this module is also loaded outside a bundler (the test
 * suite), where `import.meta.env` does not exist at all.
 */
export const API_BASE: string =
  import.meta.env?.PUBLIC_API_BASE_URL ?? 'http://localhost:8080';

/** docs/error-model.md: every error returns this shape. */
export interface ApiErrorBody {
  code: string;
  message: string;
  request_id?: string;
  details?: Record<string, unknown>;
}

export class ApiError extends Error {
  readonly status: number;
  /** Stable, machine-readable — switch on this, never on the message. */
  readonly code: string;
  readonly requestId: string | null;
  readonly details: Record<string, unknown> | null;
  /**
   * `Retry-After` in seconds, or null when the response carried none.
   *
   * docs/error-model.md: on a 429 this is the exact seconds until the caller's
   * window frees. A fixed "wait a moment" understates an hourly cap by 3600x, so
   * the UI reads the wait from here instead of inventing one. Null is a real
   * case — a response that carries no header must fall back to a wait the copy
   * can honestly state, never to a guess.
   */
  readonly retryAfterSeconds: number | null;

  constructor(
    status: number,
    body: ApiErrorBody | null,
    fallback: string,
    retryAfterSeconds: number | null = null,
  ) {
    super(body?.message ?? fallback);
    this.name = 'ApiError';
    this.status = status;
    this.code = body?.code ?? 'internal_error';
    this.requestId = body?.request_id ?? null;
    this.details = body?.details ?? null;
    this.retryAfterSeconds = retryAfterSeconds;
  }
}

/** A 401 means the session is gone: send the user to login, keeping the destination. */
export function redirectToLogin(): void {
  const next = window.location.pathname + window.location.search;
  window.location.replace(`/login?next=${encodeURIComponent(next)}`);
}

export interface ApiFetchInit extends RequestInit {
  /**
   * 401 -> redirect to /login. Off for the calls made *from* an auth page — the
   * signed-in probe on /login and the POSTs on /login, /signup, /reset and
   * /verify — which legitimately answer 401 and must own that answer themselves
   * rather than bounce the visitor back to the page they are already on.
   */
  redirectOn401?: boolean;
}

export async function apiFetch<T>(path: string, init: ApiFetchInit = {}): Promise<T> {
  const { redirectOn401 = true, headers, ...rest } = init;

  const res = await fetch(API_BASE + path, {
    credentials: 'include',
    ...rest,
    headers: {
      Accept: 'application/json',
      ...(rest.body ? { 'Content-Type': 'application/json' } : {}),
      ...headers,
    },
  });

  if (!res.ok) {
    let body: ApiErrorBody | null = null;
    try {
      body = ((await res.json()) as { error?: ApiErrorBody }).error ?? null;
    } catch {
      // Non-JSON error body: fall back to the status line.
    }
    if (res.status === 401 && redirectOn401) redirectToLogin();
    throw new ApiError(
      res.status,
      body,
      res.statusText,
      parseRetryAfter(res.headers.get('retry-after')),
    );
  }

  if (res.status === 204) return undefined as T;
  return (await res.json()) as T;
}

/** GET /api/me — everything the dashboard needs on load, and the poll fallback. */
export interface UsageToday {
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  cost_idr: number;
}

export interface Me {
  account_id: string;
  balance_idr: number;
  usage_today: UsageToday;
  telegram_linked: boolean;
  status: string;
}
