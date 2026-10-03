// Executable contract for the usage-trend geometry (src/lib/usage-trend.ts).
//
// The rules that matter most: an all-zero series must not divide by zero (the
// common case for a new account), and a series must be ordered OLDEST-FIRST (a
// trend read right-to-left is silently wrong). Both are pinned here.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  barHeight,
  buildTrend,
  metricLabel,
  shortDayLabel,
  trendHasData,
  type TrendMetric,
} from '../src/lib/usage-trend.ts';
import type { UsageBucket } from '../src/lib/usage.ts';

function bucket(day: string, overrides: Partial<UsageBucket> = {}): UsageBucket {
  return { day, input_tokens: 0, cache_read_tokens: 0, output_tokens: 0, cost_idr: 0, ...overrides };
}

test('barHeight maps a value onto 0..1 and never divides by zero', () => {
  assert.equal(barHeight(50, 100), 0.5);
  assert.equal(barHeight(100, 100), 1);
  assert.equal(barHeight(0, 100), 0);
  // The all-zero series: max 0 must give 0, never NaN or Infinity.
  assert.equal(barHeight(0, 0), 0);
  assert.equal(barHeight(5, 0), 0);
  // A negative or non-finite input cannot produce a negative or NaN height.
  assert.equal(barHeight(-10, 100), 0);
  assert.equal(barHeight(Number.NaN, 100), 0);
  // A value above the max is clamped to 1, never overflows the plot.
  assert.equal(barHeight(999, 100), 1);
});

test('buildTrend orders oldest-first even though the API sends newest-first', () => {
  // The server returns day DESC. The chart must read left-to-right as time.
  const bars = buildTrend([
    bucket('2026-03-03', { input_tokens: 30 }),
    bucket('2026-03-01', { input_tokens: 10 }),
    bucket('2026-03-02', { input_tokens: 20 }),
  ]);
  assert.deepEqual(bars.map((b) => b.day), ['2026-03-01', '2026-03-02', '2026-03-03']);
});

test('the tallest day is the full-height bar and the scale is per-series', () => {
  const bars = buildTrend([
    bucket('2026-03-01', { input_tokens: 10 }),
    bucket('2026-03-02', { input_tokens: 40 }),
  ]);
  assert.equal(bars[0].height, 0.25);
  assert.equal(bars[1].height, 1);
});

test('all-zero series yields zero heights, and trendHasData is false', () => {
  const bars = buildTrend([bucket('2026-03-01'), bucket('2026-03-02')]);
  assert.deepEqual(bars.map((b) => b.height), [0, 0]);
  assert.equal(trendHasData(bars), false);
  assert.equal(trendHasData(buildTrend([bucket('2026-03-01', { input_tokens: 1 })])), true);
});

// A MIXED series, which is the only shape that distinguishes `some` from `every`.
//
// THE TWO CALLS ABOVE CANNOT TELL THEM APART. `some(h > 0)` and `every(h > 0)` agree when all
// heights are zero and when all are non-zero; they differ only when the series has BOTH. Both
// fixtures above are on the agreeing side - `[0, 0]` and a single non-zero bucket - so MEASURED:
// rewriting `trendHasData` as `bars.every((b) => b.height > 0)` left this whole suite passing.
//
// That is not a hypothetical edit. `trendHasData` decides whether the usage chart renders at all
// (`UsageAnalytics.astro`: `trendWrap.toggleAttribute('hidden', !hasData)`), so under `every` a
// customer whose window contains ANY zero-usage day would be shown "no data yet" instead of their
// real chart - the common case, not the edge one.
test('trendHasData is true for a mixed series, which is what some() means', () => {
  const bars = buildTrend([
    bucket('2026-03-01'),
    bucket('2026-03-02', { input_tokens: 20 }),
  ]);
  assert.deepEqual(
    bars.map((b) => b.height),
    [0, 1],
    'the fixture must MIX zero and non-zero, or it cannot distinguish some() from every()',
  );
  assert.equal(
    trendHasData(bars),
    true,
    'one day with usage is enough to have data: a chart that hides itself because some other ' +
      'day was quiet is the defect this pins',
  );
});

test('a bucket with no day label is dropped', () => {
  const bars = buildTrend([bucket(''), bucket('2026-03-01', { input_tokens: 5 })]);
  assert.equal(bars.length, 1);
  assert.equal(bars[0].day, '2026-03-01');
});

test('the metric selects which counter is plotted', () => {
  // Two days, so the scale is a real comparison rather than a single-bar max.
  // input: 100 vs 25  -> 1.0 and 0.25
  // output: 10 vs 20  -> 0.5 and 1.0  (a DIFFERENT shape, proving the metric is read)
  const src = [
    bucket('2026-03-01', { input_tokens: 100, output_tokens: 10 }),
    bucket('2026-03-02', { input_tokens: 25, output_tokens: 20 }),
  ];
  assert.deepEqual(buildTrend(src, 'input_tokens').map((b) => b.height), [1, 0.25]);
  assert.deepEqual(buildTrend(src, 'output_tokens').map((b) => b.height), [0.5, 1]);
});

test('every metric has a label', () => {
  for (const m of ['input_tokens', 'output_tokens', 'cost_idr'] as TrendMetric[]) {
    assert.ok(metricLabel(m).length > 0, m);
  }
});

test('shortDayLabel renders a compact day and passes through a bad value', () => {
  assert.match(shortDayLabel('2026-09-27'), /27/);
  assert.equal(shortDayLabel('nonsense'), 'nonsense');
});

test('a non-numeric counter reads as zero, never NaN', () => {
  // Simulate a malformed payload field.
  const bars = buildTrend([{ day: '2026-03-01', input_tokens: 'x' as unknown as number, cache_read_tokens: 0, output_tokens: 0, cost_idr: 0 }]);
  assert.equal(bars[0].height, 0);
  assert.equal(bars[0].input_tokens, 0);
});
