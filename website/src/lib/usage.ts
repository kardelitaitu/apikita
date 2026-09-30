// The `GET /api/usage` request window, and which bucket is "today".
//
// Both live here rather than in the island's `.astro` <script> block for the same
// reason lib/retry-wait.ts and lib/dashboard-form.ts do: those blocks are neither
// type-checked nor loadable by `node --test`, which is exactly how /dashboard/usage
// shipped calling `?from=-30d` — a bound the server has always rejected.
//
// Sources: docs/server/api-spec.md:296-307 (the endpoint) and
// server/src/routes/account.rs `parse_usage_day` / `get_usage`.

/** The daily breakdown window the dashboard asks for: the last 30 days, inclusive. */
export const USAGE_WINDOW_DAYS = 30;

/** `YYYY-MM-DD`, the only form `parse_usage_day` accepts. */
export function toIsoDay(date: Date): string {
  return date.toISOString().slice(0, 10);
}

/**
 * The `?from=&to=` query string for the daily breakdown, ending `on` (inclusive).
 *
 * docs/server/api-spec.md:302: "Both bounds are ISO `YYYY-MM-DD`; **anything else is
 * a `422`** naming the field". server/src/routes/account.rs:248-250 parses with
 * `%Y-%m-%d` and nothing else, so a relative shorthand like `-30d` — what this
 * island used to send — is a guaranteed 422 and the page never renders a row.
 *
 * Both ends are real dates, and both are sent. A window with two bounds is the
 * question the API actually answers; leaving `to` off would let a server-side
 * default decide how far forward the window reaches.
 *
 * Both bounds are INCLUSIVE (api-spec.md:304), so the span is 30 days *including*
 * today: `to - (USAGE_WINDOW_DAYS - 1)`.
 *
 * `Date.UTC` rolls impossible dates over (2025-02-30 becomes 2025-03-02), which
 * cannot arise from a real `Date`; the round trip is checked anyway so a
 * rolled-over bound is refused rather than sent as a different day than asked for
 * — the same discipline as `expiryToIso` in lib/dashboard-form.ts.
 */
export function usageWindowQuery(on: Date = new Date()): string {
  const to = new Date(Date.UTC(on.getUTCFullYear(), on.getUTCMonth(), on.getUTCDate()));
  if (Number.isNaN(to.getTime())) {
    throw new RangeError('usageWindowQuery needs a valid Date');
  }
  const from = new Date(to);
  from.setUTCDate(from.getUTCDate() - (USAGE_WINDOW_DAYS - 1));

  const toDay = toIsoDay(to);
  const fromDay = toIsoDay(from);
  const roundTrip = new Date(`${fromDay}T00:00:00.000Z`);
  if (roundTrip.toISOString().slice(0, 10) !== fromDay) {
    throw new RangeError(`usageWindowQuery produced an invalid from-day: ${fromDay}`);
  }

  return `?from=${fromDay}&to=${toDay}`;
}

/** One bucket of `GET /api/usage`, in either field spelling. */
export interface UsageBucket {
  /**
   * The day, as the server names it. server/src/routes/account.rs:324 emits
   * `"day"`; the island declared and read `date`, so every row rendered "—" under
   * a correct-looking table. Read through `bucketDay`, never directly.
   */
  day?: string;
  /** Legacy spelling, still tolerated. Not the server's field. */
  date?: string;
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  cost_idr: number;
}

/**
 * The day label for one bucket, or `null` when it carries neither spelling.
 *
 * The server's `day` wins when both are present: that is the field the server
 * actually sends, and a payload carrying both is one where `day` is authoritative.
 * Tolerating `date` costs one `??` and keeps a renamed field from blanking the
 * table again.
 */
export function bucketDay(bucket: Pick<UsageBucket, 'day' | 'date'>): string | null {
  const day = bucket.day ?? bucket.date;
  return typeof day === 'string' && day.length > 0 ? day : null;
}

/**
 * The newest bucket in a `GET /api/usage` response — the one the "Today" panel
 * shows.
 *
 * server/src/routes/account.rs:296 orders `day DESC`, so the NEWEST day is the
 * FIRST bucket, not the last. The island took `buckets[buckets.length - 1]` and
 * so labelled the oldest day of the window "Today".
 *
 * Order is read from `day` rather than from array position, so the picker does not
 * rest on the server's `ORDER BY` staying `DESC` — and so a response envelope that
 * happens to arrive ascending still yields the newest day. ISO `YYYY-MM-DD` sorts
 * correctly as a string.
 *
 * Precondition: `buckets` is the response array in the order it arrived.
 */
export function newestBucket<T extends Pick<UsageBucket, 'day' | 'date'>>(
  buckets: readonly T[],
): T | null {
  let newest: T | null = null;
  let newestDay: string | null = null;
  for (const bucket of buckets) {
    const day = bucketDay(bucket);
    if (day === null) continue;
    if (newestDay === null || day > newestDay) {
      newest = bucket;
      newestDay = day;
    }
  }
  return newest;
}
