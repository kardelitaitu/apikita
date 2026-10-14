// Executable contract for the recent-requests panel logic (src/lib/recent-usage.ts).
//
// The rule that matters most: totalTokens is a DISPLAY sum, never a billing one.
// The three token classes have three different prices, so no total may feed a
// cost — that is why the panel shows the server's cost_idr beside the split.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  formatRequestTime,
  mayHaveMore,
  recentUsageHeading,
  recentUsagePath,
  totalTokens,
  RECENT_USAGE_LIMIT,
} from '../src/lib/recent-usage.ts';

test('the path carries the limit and matches the server default', () => {
  assert.equal(recentUsagePath(), `/api/usage/recent?limit=${RECENT_USAGE_LIMIT}`);
  assert.equal(recentUsagePath(5), '/api/usage/recent?limit=5');
});

test('totalTokens sums all three classes for display', () => {
  assert.equal(totalTokens({ input_tokens: 100, cache_read_tokens: 10, output_tokens: 50 }), 160);
  assert.equal(totalTokens({ input_tokens: 0, cache_read_tokens: 0, output_tokens: 0 }), 0);
});

test('a request time is shown in WIB, not the visitor clock', () => {
  // 2026-01-01T00:00:00Z is 07:00 in Jakarta (UTC+7).
  assert.match(formatRequestTime('2026-01-01T00:00:00Z'), /07:00/);
  // An unparseable value is returned as-is, never "Invalid Date".
  assert.equal(formatRequestTime('nonsense'), 'nonsense');
});

test('the heading reflects the count and pluralises', () => {
  assert.equal(recentUsageHeading(0), 'No requests yet');
  assert.equal(recentUsageHeading(1), 'Last 1 request');
  assert.equal(recentUsageHeading(4), 'Last 4 requests');
});

test('more pages are claimed only when the page came back full', () => {
  assert.equal(mayHaveMore(RECENT_USAGE_LIMIT), true);
  assert.equal(mayHaveMore(RECENT_USAGE_LIMIT - 1), false);
  assert.equal(mayHaveMore(0), false);
  // A custom limit is respected.
  assert.equal(mayHaveMore(5, 5), true);
  assert.equal(mayHaveMore(4, 5), false);
});
