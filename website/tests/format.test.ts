// Executable contract for the two display formatters.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { formatIdr, formatCount, formatTopupTime } from '../src/lib/format.ts';

const here = dirname(fileURLToPath(import.meta.url));

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

test('formatTopupTime never renders the string "Invalid Date"', () => {
  // THE ROW THIS EXISTS FOR is the top-up history, which is a deposit record: a customer reads it to
  // confirm money arrived. `new Date(x).toLocaleString()` returns the literal `"Invalid Date"` for a
  // value it cannot parse, and that word printed into a deposit row looks like a date while being a
  // failure message.
  //
  // NOT A LIVE BUG, and the test says so rather than implying otherwise: `created_at` is a
  // `chrono::DateTime<Utc>` on the server, which always serialises to RFC3339. This is a boundary
  // guard, and the reason it is worth having is that the alternative is silently plausible.
  //
  // MEASURED BEFORE THIS EXISTED: replacing the row's timestamp cell with the literal `'Invalid
  // Date'`, and deleting the cell outright, both left the suite at 226 pass / 0 fail.
  assert.equal(
    formatTopupTime('not a date'),
    'not a date',
    'an unparseable value must come back AS GIVEN, so a reader sees the malformed input rather ' +
      'than a word that looks like a timestamp',
  );
  assert.ok(
    !formatTopupTime('not a date').includes('Invalid'),
    'the fallback must not be the string "Invalid Date"',
  );
  assert.ok(
    !formatTopupTime('').includes('Invalid'),
    'an EMPTY string must not become "Invalid Date" either - `new Date("")` is also unparseable',
  );

  // And it still formats a real value. Without this the function could return its input always and
  // satisfy both assertions above.
  const formatted = formatTopupTime('2026-03-04T05:06:07Z');
  assert.ok(
    formatted !== '2026-03-04T05:06:07Z' && !formatted.includes('Invalid'),
    `a valid RFC3339 instant must be FORMATTED, not echoed: got ${formatted}`,
  );
});

test('the top-up history row formats its timestamp through the guarded formatter', () => {
  // The unit test above proves the FUNCTION is safe. This proves the ROW calls it - the two are
  // separate failures, and only the second one was live in the code this replaces.
  //
  // `TopUpForm.astro` used `new Date(topup.created_at).toLocaleString()` inline. That expression is
  // what the guard exists to replace, so a regression to it must fail here rather than pass by
  // leaving the function above still correct and still uncalled.
  const island = readFileSync(join(here, '..', 'src', 'islands', 'wallet', 'TopUpForm.astro'), 'utf8');

  assert.ok(
    /formatTopupTime\s*\(\s*topup\.created_at\s*\)/.test(island),
    "TopUpForm.astro must render the history row's timestamp with `formatTopupTime(topup.created_at)`. " +
      'The inline `new Date(...).toLocaleString()` it replaces renders "Invalid Date" for a value it ' +
      'cannot parse, and nothing else in this suite would notice.',
  );
  assert.ok(
    !/new Date\(topup\.created_at\)\.toLocaleString\(\)/.test(island),
    'TopUpForm.astro must not go back to the unguarded inline expression for the history row',
  );
});
