// Guard for the reduced-motion contract in the global stylesheet.
//
// The failure this exists to catch: an animation added later (the loading
// skeletons' `animate-pulse` were) that the `prefers-reduced-motion` block does
// not stop, leaving a perpetual fade for the users who asked for less motion
// (WCAG 2.3.3). The stylesheet is read as TEXT because the guard is about what
// the file declares, not a value it computes.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(join(here, '..', 'src', 'styles', 'global.css'), 'utf8');

test('a prefers-reduced-motion block exists', () => {
  assert.ok(css.includes('prefers-reduced-motion: reduce'));
});

test('the reduced-motion block stops animations generally, not just the ticker', () => {
  // The ticker has its own handling; the general rule is what covers everything
  // added later (the skeletons pulse). Both must be present.
  const block = css.slice(css.indexOf('prefers-reduced-motion: reduce'));
  assert.ok(block.includes('.ticker__track'), 'the ticker handling must stay');
  assert.ok(
    /animation-duration\s*:\s*0\.001ms/.test(block),
    'the block must neutralise animation-duration globally, so a new animation is covered',
  );
  assert.ok(
    /animation-iteration-count\s*:\s*1/.test(block),
    'a one-shot count is what makes the neutralised duration actually stop a loop',
  );
});

test('transitions are neutralised too, not only animations', () => {
  const block = css.slice(css.indexOf('prefers-reduced-motion: reduce'));
  assert.ok(/transition-duration\s*:\s*0\.001ms/.test(block));
});
