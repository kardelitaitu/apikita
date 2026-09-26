// The browser's half of the Midtrans environment cross-check.
//
// Two platforms carry this setting and nothing keeps them in sync: the server's
// MIDTRANS_ENV decides which Snap endpoint a session is really created against,
// and Cloudflare Pages' PUBLIC_MIDTRANS_ENV decides which Snap.js this page
// loads. The server is the side that actually makes the decision, so it reports
// its own value on the create-topup response; this compares that fact against
// the browser's build-time value.
//
// It replaces a key-prefix guess — `SB-Mid-client-` is sandbox, a bare
// `Mid-client-` is production. Midtrans documents that rule for no key, this
// repo holds no client-key sample at all, and the client key is not the value
// that picks the host anyway. On a money path a guess must not be load-bearing.
//
// Pure and DOM-free so the suite can load it directly (tests/midtrans-env.test.ts),
// following src/lib/login-error.ts: logic that used to live inside an `.astro`
// script block, which neither `tsc` nor `node --test` can see, lives here.

/** The only two values the server's `environment` field can carry. */
const KNOWN_ENVIRONMENTS = ['sandbox', 'production'];

/**
 * Compare this build's Midtrans environment against the one the server says it
 * created the session against.
 *
 * Returns `'mismatch'` only when both sides are known and disagree. Anything the
 * server could not tell us — `undefined` (Cloudflare Pages deployed ahead of the
 * API, or an older server that predates the field), or a value outside the
 * documented pair — returns `null` and the top-up proceeds unchanged. A missing
 * or unreadable value is "cannot check", never evidence of a mismatch: failing
 * closed there would turn a config lag into a total payment outage.
 *
 * A real disagreement is a confirmed fact, not a guess — a Snap token minted
 * against one environment cannot be paid by the other — so blocking it saves the
 * customer a payment that would certainly fail.
 */
export function midtransEnvMismatch(
  pagesEnv: string,
  serverEnv: string | undefined,
): 'mismatch' | null {
  if (serverEnv === undefined || !KNOWN_ENVIRONMENTS.includes(serverEnv)) return null;
  return pagesEnv === serverEnv ? null : 'mismatch';
}
