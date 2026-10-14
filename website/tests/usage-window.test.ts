// Executable contract for /dashboard/usage's request window and "Today" panel.
//
// The page was broken on EVERY load and the suite could not see it: the three
// defects below all lived in islands/usage/UsageAnalytics.astro's <script> block,
// which astro does not type-check and `node --test` cannot load. They now live in
// src/lib/usage.ts and are pinned here.
//
// Each test names the OLD behaviour it fails on, so a regression is legible.
//
// Style follows tests/rate-limit.test.ts and tests/dashboard-form.test.ts:
// node:test + node:assert/strict, explicit .ts extensions, no new dependency.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { USAGE_WINDOW_DAYS, bucketDay, newestBucket, usageWindowQuery } from '../src/lib/usage.ts';

/**
 * server/src/routes/account.rs `parse_usage_day` accepts one form and rejects
 * everything else: `NaiveDate::parse_from_str(raw, "%Y-%m-%d")`, otherwise a 422
 * naming the field. Mirrored here so the test can say WHY the old query failed
 * rather than just asserting a string.
 */
const ACCEPTED_BY_SERVER = /^\d{4}-\d{2}-\d{2}$/;

/** `YYYY-MM-DD` -> a UTC Date at that midnight, so the clock in the test is fixed. */
function day(iso: string): Date {
  return new Date(`${iso}T00:00:00.000Z`);
}

test('the usage window is two real YYYY-MM-DD bounds, not the -30d shorthand the server 422s', () => {
  const query = usageWindowQuery(day('2026-03-15'));

  // (a) The old value was `?from=-30d`. parse_usage_day cannot parse that, so
  // GET /api/usage answered 422 on every single load and the page showed only its
  // error box. Both bounds must be dates the server's parser accepts.
  assert.doesNotMatch(query, /-30d/);
  assert.match(query, /^\?from=\d{4}-\d{2}-\d{2}&to=\d{4}-\d{2}-\d{2}$/);

  const params = new URLSearchParams(query);
  const from = params.get('from');
  const to = params.get('to');
  assert.ok(from !== null && to !== null, query);
  assert.match(from, ACCEPTED_BY_SERVER);
  assert.match(to, ACCEPTED_BY_SERVER);

  // A relative shorthand is exactly what the server refuses.
  assert.equal(ACCEPTED_BY_SERVER.test('-30d'), false);
  assert.equal(ACCEPTED_BY_SERVER.test('2026-3-5'), false);

  // `to` is the day asked for, and the span is the whole window. Both bounds are
  // INCLUSIVE (api-spec:105), so 30 days back INCLUDING today is to - 29.
  assert.equal(to, '2026-03-15');
  assert.equal(from, '2026-02-14');
  const span = (day(to).getTime() - day(from).getTime()) / 86_400_000 + 1;
  assert.equal(span, USAGE_WINDOW_DAYS);

  // The month boundary is a real date rollover, not arithmetic on the string.
  assert.equal(usageWindowQuery(day('2026-03-01')), '?from=2026-01-31&to=2026-03-01');
  // And the leap day.
  assert.equal(usageWindowQuery(day('2028-03-01')), '?from=2028-02-01&to=2028-03-01');

  // An unusable clock is refused loudly rather than sent as "Invalid Date".
  assert.throws(() => usageWindowQuery(new Date(Number.NaN)), RangeError);

  // AND THE REFUSAL MUST BE THE ONE THIS FUNCTION WROTE, not any RangeError that happens along.
  //
  // MEASURED: deleting the `Number.isNaN(to.getTime())` guard left the whole website suite GREEN at
  // 211 passed / 0 failed. An invalid date still throws - `toIsoDay` calls `toISOString()`, which
  // raises `RangeError: Invalid time value` - so the assertion above is satisfied by a DIFFERENT
  // line's error and cannot see the guard go.
  //
  // The guard's whole contribution is therefore the MESSAGE: it names the function and the cause,
  // where the fallback names neither. A diagnostic whose value is its message is not tested by
  // asserting its error type, so the message is asserted here.
  assert.throws(
    () => usageWindowQuery(new Date(Number.NaN)),
    (err: unknown) =>
      err instanceof RangeError && /usageWindowQuery needs a valid Date/.test(err.message),
    'an invalid Date must be refused by usageWindowQuery\'s OWN check, with its own message. If \
     this fails while the type assertion above passes, the NaN guard is gone and the reported \
     error comes from toIsoDay instead - which names neither the function nor the cause.'
  );
});

test('a bucket is labelled from the server\'s "day" field, and "date" is still tolerated', () => {
  // (b) server/src/routes/account.rs:324 emits `"day"`. The island declared
  // `date` and rendered `b.date`, so every row's Date cell read "—" even once the
  // window was fixed.
  assert.equal(bucketDay({ day: '2026-03-15' }), '2026-03-15');

  // Tolerating the legacy spelling, not renaming to it.
  assert.equal(bucketDay({ date: '2026-03-14' }), '2026-03-14');
  assert.equal(bucketDay({ day: '2026-03-15', date: '2026-03-14' }), '2026-03-15');

  // A bucket with neither spelling has no label; it is not invented.
  assert.equal(bucketDay({}), null);
  assert.equal(bucketDay({ day: '' }), null);

  // The panel's rows come from a real server payload, which carries `day`.
  const payload = [
    { day: '2026-03-15', input_tokens: 10, cache_read_tokens: 0, output_tokens: 1, cost_idr: 5 },
  ];
  assert.deepEqual(
    payload.map(bucketDay),
    ['2026-03-15'],
  );
});

test('the newest bucket is the one "day DESC" puts first, not the last row', () => {
  // (c) account.rs:296 orders `day DESC`. The island took
  // `buckets[buckets.length - 1]` — the OLDEST day of the window — and labelled it
  // "Today".
  const descending = [
    { day: '2026-03-15', input_tokens: 300, cache_read_tokens: 0, output_tokens: 30, cost_idr: 9 },
    { day: '2026-03-14', input_tokens: 200, cache_read_tokens: 0, output_tokens: 20, cost_idr: 6 },
    { day: '2026-03-13', input_tokens: 100, cache_read_tokens: 0, output_tokens: 10, cost_idr: 3 },
  ];

  const today = newestBucket(descending);
  assert.equal(today, descending[0]);
  assert.equal(bucketDay(today!), '2026-03-15');
  assert.equal(today!.input_tokens, 300);

  // The old pick, stated so the regression is unambiguous.
  assert.notEqual(today, descending[descending.length - 1]);

  // The pick is read from the day, not from array position, so an ascending
  // payload still yields the newest day.
  const ascending = [...descending].reverse();
  assert.equal(bucketDay(newestBucket(ascending)!), '2026-03-15');

  // A response that carries the legacy spelling is still ordered correctly.
  const legacy = [{ date: '2026-03-14' }, { date: '2026-03-15' }];
  assert.equal(bucketDay(newestBucket(legacy)!), '2026-03-15');

  // An empty window (api-spec:107 — a bounded range matching nothing returns [])
  // has no "today" at all, and the panel shows em dashes.
  assert.equal(newestBucket([]), null);
  assert.equal(newestBucket([{}]), null);
});
