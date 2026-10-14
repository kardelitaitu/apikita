// Geometry for the usage trend on /dashboard/usage: pure, DOM-free maths.
//
// Why this is a lib and not inline in the island: the .astro <script> block is
// neither type-checked nor loadable by `node --test`, and a chart's scale maths
// is exactly where an off-by-one or a divide-by-zero silently produces a wrong
// picture. The same rule that put usageWindowQuery in lib/ applies here.
//
// No charting dependency is added. The project pins its dependency surface
// (astro + tailwind + pocketbase only), and a daily bar chart is a handful of
// rectangles — pulling in a chart library for it would be the larger cost.
//
// Sources: docs/website/03-functional-spec.md line 19 ("/dashboard/usage — Token
// usage over time") and docs/server/api-spec.md GET /api/usage (daily buckets).

import type { UsageBucket } from './usage.ts';

/** One day's plotted value, ready to draw. */
export interface TrendBar {
  /** The bucket's day label, or '' when the bucket carried neither spelling. */
  day: string;
  /** 0..1 of the tallest bar IN THIS SERIES. 0 for a zero-value day. */
  height: number;
  /** The three raw counters, carried so a caller needs no second lookup. */
  input_tokens: number;
  cache_read_tokens: number;
  output_tokens: number;
  cost_idr: number;
}

/**
 * A value turned into a 0..1 height against the series maximum.
 *
 * Zero when the maximum is zero — a flat-zero series has NO tallest day, and
 * dividing by it would yield NaN and draw nothing (or, worse, everything). This
 * is the single most important line in the module: the all-zero case is the
 * common one for a new account.
 */
export function barHeight(value: number, max: number): number {
  if (!Number.isFinite(value) || !Number.isFinite(max) || max <= 0) return 0;
  const clamped = Math.max(0, value);
  return Math.min(1, clamped / max);
}

/**
 * Which counter the trend plots.
 *
 * The trend shows **one** series at a time rather than a stacked bar of all
 * three, because stacking three token classes on one axis implies they are
 * commensurable, and they are not: cache-read is priced ~200x below output. A
 * single, clearly-labelled series never makes that false suggestion, and the flat
 * table beside it still carries all three.
 */
export type TrendMetric = 'input_tokens' | 'output_tokens' | 'cost_idr';

/**
 * Every member of {@link TrendMetric}, as a value.
 *
 * THE SAME REASON `SERVICE_STATUSES` EXISTS in `service-status.ts`, and found the same way. The test
 * checking "every metric has a label" held its own literal array, annotated `as TrendMetric[]`. That
 * annotation LOOKS like a tie to the type and is not one - a `TrendMetric[]` may hold a SUBSET.
 * MEASURED: dropping `'cost_idr'` from the test's list left the suite green AND `tsc --noEmit` at
 * exit 0, so the check would have narrowed to two metrics while `metricLabel` kept a third branch
 * that nothing exercised.
 *
 * The compiler helps in the other direction only: removing a case from `metricLabel` makes it fall
 * off its end and `tsc` reports it (MEASURED: exit 2).
 *
 * THIS IS ALSO THE LIST `UsageAnalytics.astro` SPELLS OUT IN MARKUP. Its three `data-metric` buttons
 * are the only source of the values reaching `metricLabel`, through an unchecked `as TrendMetric`
 * cast in the click handler. A fourth metric would need a fourth button, and that cast would not
 * notice the omission - the island's `?? 'input_tokens'` default only fires on a MISSING attribute,
 * not on one holding an unknown value.
 */
export const TREND_METRICS: TrendMetric[] = ['input_tokens', 'output_tokens', 'cost_idr'];

/** The metric's human label, for the axis title and the toggle. */
export function metricLabel(metric: TrendMetric): string {
  switch (metric) {
    case 'input_tokens':
      return 'Input tokens';
    case 'output_tokens':
      return 'Output tokens';
    case 'cost_idr':
      return 'Cost (IDR)';
  }
}

/**
 * Build the bars for a series, oldest-first.
 *
 * The API returns buckets newest-first (`ORDER BY day DESC`). A trend reads
 * left-to-right as time advancing, so the array is REVERSED here rather than by
 * the caller — a chart drawn newest-first is a chart read backwards, which is the
 * kind of error nobody notices until a number looks wrong.
 *
 * Buckets with no day label are dropped: they cannot be placed on a time axis.
 */
export function buildTrend(
  buckets: readonly UsageBucket[],
  metric: TrendMetric = 'input_tokens',
): TrendBar[] {
  const placed = buckets
    .map((b) => ({
      day: typeof (b.day ?? b.date) === 'string' ? (b.day ?? b.date ?? '') : '',
      value: numberOrZero(b[metric]),
      input_tokens: numberOrZero(b.input_tokens),
      cache_read_tokens: numberOrZero(b.cache_read_tokens),
      output_tokens: numberOrZero(b.output_tokens),
      cost_idr: numberOrZero(b.cost_idr),
    }))
    .filter((b) => b.day !== '')
    .sort((a, b) => (a.day < b.day ? -1 : a.day > b.day ? 1 : 0));

  const max = placed.reduce((m, b) => (b.value > m ? b.value : m), 0);

  return placed.map((b) => ({
    day: b.day,
    height: barHeight(b.value, max),
    input_tokens: b.input_tokens,
    cache_read_tokens: b.cache_read_tokens,
    output_tokens: b.output_tokens,
    cost_idr: b.cost_idr,
  }));
}

/** A missing or non-numeric counter reads as 0, never NaN. */
function numberOrZero(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

/**
 * A compact axis label for a day: the day and month, e.g. "27 Sep".
 *
 * The full ISO date is unreadable repeated 30 times across a chart. An
 * unparseable day is returned as-is rather than as "Invalid Date".
 */
export function shortDayLabel(day: string): string {
  const parsed = new Date(`${day}T00:00:00.000Z`);
  if (Number.isNaN(parsed.getTime())) return day;
  return new Intl.DateTimeFormat('en-GB', {
    timeZone: 'UTC',
    day: '2-digit',
    month: 'short',
  }).format(parsed);
}

/**
 * Whether a series is worth drawing at all.
 *
 * An all-zero series draws thirty flat bars that say nothing. The caller shows
 * the empty state instead, which is the honest answer.
 */
export function trendHasData(bars: readonly TrendBar[]): boolean {
  return bars.some((b) => b.height > 0);
}
