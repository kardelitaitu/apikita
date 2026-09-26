// Executable contract for the transcribed pricing/availability data that both
// the landing page and the models page import. Importing the module runs the
// `tickerCards` derivation, so asserting on it pins the figures against drift.

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  rateClasses,
  rates,
  models,
  tickerModels,
  tickerCards,
} from '../src/lib/models.ts';

test('models exposes the transcribed pricing and availability tables', () => {
  assert.deepEqual(rateClasses, ['Input', 'Cache hit', 'Output']);
  assert.equal(rates.length, 3);
  assert.deepEqual(models, ['flash', 'deepseek-v4-flash']);
  assert.equal(tickerModels.length, 6);
  // Twelve distinct cards: six models x two bases.
  assert.equal(tickerCards.length, 12);
});

test('purchasable models are billed; placeholders and coming-soon are not', () => {
  for (const card of tickerCards) {
    if (card.name === 'flash' || card.name === 'deepseek-v4-flash') {
      if (card.basis === 'Peak') {
        assert.equal(card.billed, true, `${card.name} peak is purchasable`);
        assert.equal(card.mark, 'Billed');
      }
    } else {
      assert.equal(card.billed, false, `${card.name} is not billed`);
    }
  }
});

test('a mock placeholder card declares itself not for sale on its face', () => {
  const placeholder = tickerCards.find(
    (c) => c.name === 'dummy-glm-5.3-flash' && c.basis === 'Peak',
  );
  assert.ok(placeholder, 'placeholder card exists');
  assert.equal(placeholder.mark, 'Placeholder');
  assert.equal(placeholder.billed, false);
});
