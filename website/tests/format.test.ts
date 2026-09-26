// Executable contract for the two display formatters.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { formatIdr, formatCount } from '../src/lib/format.ts';

test('formatIdr renders Indonesian Rupiah with no fraction digits', () => {
  assert.ok(formatIdr(1000).includes('1.000'), `got ${formatIdr(1000)}`);
  assert.ok(formatIdr(1000).startsWith('Rp'), `got ${formatIdr(1000)}`);
  assert.ok(formatIdr(1_250_000).includes('1.250.000'));
  assert.ok(formatIdr(0).startsWith('Rp'));
});

test('formatCount renders a US-grouped integer', () => {
  assert.equal(formatCount(0), '0');
  assert.equal(formatCount(1234), '1,234');
  assert.equal(formatCount(1_000_000), '1,000,000');
});
