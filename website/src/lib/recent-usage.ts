// The recent-requests panel on /dashboard: pure logic for GET /api/usage/recent.
//
// It lives in lib/ rather than in the page's <script> block for the same reason
// lib/usage.ts does: the test suite loads this module directly with Node, and
// these are exactly the rules that must not silently invert.
//
// Sources: docs/server/api-spec.md (GET /api/usage/recent) and
// docs/website/03-functional-spec.md, the Dashboard table's "Recent requests —
// last N metered calls". Nothing here is guessed.

/** One metered request, as `GET /api/usage/recent` returns it. */
export interface RecentUsage {
  id: string;
  api_key_id: string | null;
  model: string;
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  cost_idr: number;
  created_at: string;
}

/** How many requests the dashboard asks for. Matches the server's default. */
export const RECENT_USAGE_LIMIT = 20;

/** The path for the recent-requests list. */
export function recentUsagePath(limit: number = RECENT_USAGE_LIMIT): string {
  return `/api/usage/recent?limit=${limit}`;
}

/**
 * The total tokens one request moved, across all three classes.
 *
 * Summed ONLY for display, never for billing. The three classes are priced
 * differently (cache-read ~200x below output), so the API returns them separately
 * and no total is ever used to compute money — this is a row-summary convenience,
 * and the cost shown beside it comes from the server, not from this.
 */
export function totalTokens(request: Pick<RecentUsage, 'input_tokens' | 'cache_read_tokens' | 'output_tokens'>): number {
  return request.input_tokens + request.cache_read_tokens + request.output_tokens;
}

/**
 * A short request time, in the customer's timezone (WIB, UTC+7).
 *
 * The API sends RFC3339 UTC. Rendering in WIB rather than the visitor's local
 * clock means a request log read across two timezones agrees about when a request
 * happened — the same rule the top-up history and docs/decisions.md state for the
 * Telegram feed. An unparseable value returns the raw string, never "Invalid Date".
 */
export function formatRequestTime(iso: string): string {
  const ms = Date.parse(iso);
  if (Number.isNaN(ms)) return iso;
  return new Intl.DateTimeFormat('en-GB', {
    timeZone: 'Asia/Jakarta',
    dateStyle: 'medium',
    timeStyle: 'short',
  }).format(new Date(ms));
}

/**
 * The heading for the recent-requests panel.
 *
 * The count is what loaded, and the panel requests a fixed limit, so the copy
 * does not claim "all" — it says "last N" to match what was asked for.
 */
export function recentUsageHeading(count: number): string {
  if (count === 0) return 'No requests yet';
  return `Last ${count} request${count === 1 ? '' : 's'}`;
}

/**
 * Whether the list is long enough that more almost certainly exist.
 *
 * True only when a full page came back: a short page is provably the whole
 * history, so the "showing the most recent N" note is only added when it is true.
 */
export function mayHaveMore(count: number, limit: number = RECENT_USAGE_LIMIT): boolean {
  return count >= limit;
}
