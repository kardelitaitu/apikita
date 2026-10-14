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

/** Asserts `condition`, and returns the block body or fails. */
function reducedMotionBlock(): string {
  const start = css.indexOf('@media (prefers-reduced-motion: reduce)');
  assert.ok(start >= 0, 'global.css no longer has a prefers-reduced-motion: reduce block');
  const open = css.indexOf('{', start);
  assert.ok(open >= 0, 'the reduced-motion block has no opening brace');
  let depth = 0;
  for (let i = open; i < css.length; i += 1) {
    if (css[i] === '{') depth += 1;
    else if (css[i] === '}') {
      depth -= 1;
      if (depth === 0) {
        return css.slice(open + 1, i);
      }
    }
  }
  assert.fail('the reduced-motion block is never closed, so the sheet is not valid CSS');
}

test('a prefers-reduced-motion block exists', () => {
  assert.ok(css.includes('prefers-reduced-motion: reduce'));
});

test('the reduced-motion block stops animations generally, not just the ticker', () => {
  // The ticker has its own handling; the general rule is what covers everything
  // added later (the skeletons pulse). Both must be present.
  const block = reducedMotionBlock();
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
  const block = reducedMotionBlock();
  assert.ok(/transition-duration\s*:\s*0\.001ms/.test(block));
});

// THE READER IS WHAT MAKES THE THREE ABOVE MEAN WHAT THEY SAY. Without this test, a future edit that
// "simplifies" `reducedMotionBlock()` back to `css.slice(css.indexOf(...))` restores the hole
// silently: all three value tests keep passing, because the shipping sheet has the neutraliser inside
// the block and an unbounded slice finds it either way.
test('the reduced-motion reader stops at the block, and does not read to the end of the sheet', () => {
  const block = reducedMotionBlock();

  // THE BOUNDARY. `:focus-visible` is the rule immediately AFTER the block in global.css. A reader
  // that runs to EOF swallows it, and one that stops early would not have reached the block's own
  // content - so this pins both directions.
  assert.ok(
    !block.includes(':focus-visible'),
    'the reader ran PAST the closing brace and swallowed the :focus-visible rule that follows the block'
  );
  assert.ok(
    block.includes('.ticker__track'),
    "the reader stopped BEFORE the block's own content"
  );
  assert.ok(
    block.trimStart().startsWith('.ticker'),
    'the block body does not begin with its first rule, so the offset is wrong'
  );
});
