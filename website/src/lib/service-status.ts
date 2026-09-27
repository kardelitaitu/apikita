// The service-status indicator: pure logic for interpreting GET /health.
//
// It lives in lib/ rather than in a <script> block for the same reason
// lib/recent-usage.ts does: the test suite loads this module directly with Node,
// and these are exactly the rules that must not silently invert.
//
// Scope, deliberately narrow and stated plainly: `/health` reports the PROCESS
// and the DATABASE only. docs/observability.md:193 — "**/health must not check
// upstream providers** — an upstream outage would then look like a dead server
// and trigger a restart loop." So this indicator can honestly say "the platform
// is reachable" and must NOT claim to know whether any upstream model provider is
// healthy. The copy is written to that limit; overstating it would be a lie the
// screen cannot support.

/** The body `GET /health` returns, per docs/server/api-spec.md:25. */
export interface HealthBody {
  status: string;
  database: string;
}

/**
 * What the badge shows. Four states, not two:
 *
 * - `checking` — the first request is in flight; nothing is claimed yet.
 * - `operational` — the API answered 200 with `status: "healthy"`.
 * - `degraded` — the API answered, but not healthy (a 503 with `status:
 *   "degraded"` means the database probe failed). The platform is up; something
 *   it depends on is not.
 * - `unreachable` — no answer at all (network error, or a non-2xx/503 the
 *   caller could not parse). Distinct from `degraded`: "we could not ask" is a
 *   different fact from "the answer was bad", and collapsing them would hide
 *   which side of the connection to look at.
 */
export type ServiceStatus = 'checking' | 'operational' | 'degraded' | 'unreachable';

/** The label for one state — the words on the badge. */
export function statusLabel(status: ServiceStatus): string {
  switch (status) {
    case 'checking':
      return 'Checking…';
    case 'operational':
      return 'All systems operational';
    case 'degraded':
      return 'Degraded service';
    case 'unreachable':
      return 'Status unavailable';
  }
}

/**
 * The one-line explanation under the badge. Every line is careful to say what
 * the indicator actually knows: reachability of the platform, never the health
 * of an upstream provider.
 */
export function statusDetail(status: ServiceStatus): string {
  switch (status) {
    case 'checking':
      return 'Contacting the API…';
    case 'operational':
      return 'The API and its database are responding.';
    case 'degraded':
      return 'The API is responding but its database is not. Requests that need data may fail; try again shortly.';
    case 'unreachable':
      return 'The API did not respond. This may be a network problem on your side or a service outage.';
  }
}

/**
 * The status a fetch outcome maps to.
 *
 * `httpStatus` is the response code (0 when the request never completed), and
 * `body` is the parsed JSON when there was one. The rule the server documents:
 * 200 + `status: "healthy"` is operational; a 503 carrying `degraded` is a real
 * answer (the platform is up, the database is down); anything else — including a
 * malformed body — is `unreachable`, because an unparseable answer is
 * indistinguishable from no answer and must not be shown as healthy.
 */
export function statusFromResponse(httpStatus: number, body: HealthBody | null): ServiceStatus {
  if (httpStatus === 0) return 'unreachable';
  if (body === null || typeof body.status !== 'string') return 'unreachable';
  if (httpStatus === 200 && body.status === 'healthy') return 'operational';
  if (httpStatus === 503 && body.status === 'degraded') return 'degraded';
  // A 200 that does not say "healthy", or any other code with a body, is not a
  // success. Refusing to guess keeps the badge from ever overstating.
  return 'unreachable';
}

/** How often the indicator re-checks. Matches the dashboard's poll cadence. */
export const STATUS_POLL_MS = 60_000;

/** The health endpoint path. Unauthenticated, so it works before sign-in state is known. */
export function healthPath(): string {
  return '/health';
}
