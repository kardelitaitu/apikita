// The customer-facing prices, derived from the config that sets them.
//
// THIS IS THE CHECK THE "no TOML dependency" DECISION WAS WRONGLY BELIEVED TO FORBID.
// That decision governs the BUILD: nothing read here is ever bundled, because a test
// runs on the repository and not on the page. The technique is the one
// landing-claims.test.ts already uses deliberately - read the file as TEXT - so no
// parser and no dependency is needed, and the constraint on the shipped site holds.
//
// WHY IT EXISTS. config/apikita.toml warns at its own top that we collect IDR and pay
// CNY, so an FX move changes our cost with no change in the upstream price list. That is
// precisely when the six figures in src/lib/models.ts go stale, and they are what a
// customer reads before buying. Until this existed, the only defence was the
// multiplication written out in a comment, which is a claim a reader must choose to
// check. This makes it an assertion the suite makes on every run.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const config = readFileSync(join(here, '..', '..', 'config', 'apikita.toml'), 'utf8');
const { rates, rateClasses, tickerCards } = await import('../src/lib/models.ts');

/** The [[models]] block for `name = "flash"`, which is the one the site quotes. */
function flashBlock(): string {
  const blocks = config.split('[[models]]');
  const block = blocks.find((b) => /^\s*name = "flash"/m.test(b));
  assert.ok(block, 'the config no longer has a [[models]] entry named flash');
  return block;
}

/** Reads `key = <number>` from a block, ignoring any trailing comment. */
function numberIn(block: string, key: string): number {
  const match = block.match(new RegExp('^\\s*' + key + '\\s*=\\s*([0-9.]+)', 'm'));
  assert.ok(match, 'the flash model no longer declares ' + key);
  return Number(match[1]);
}

/** What the site displays, as a number: '4,015' is 4015. */
const asNumber = (shown: string): number => Number(shown.replace(/,/g, ''));

test('the quoted peak prices are the config peak rates times the model multiplier', () => {
  const block = flashBlock();
  const multiplier = numberIn(block, 'price');
  const keys = ['input_peak', 'cache_read_peak', 'output_peak'];

  assert.equal(rates.length, keys.length, 'the rate array and the config keys must align');
  assert.equal(rateClasses.length, keys.length);

  rates.forEach((rate, i) => {
    const expected = numberIn(block, keys[i]) * multiplier;
    assert.equal(
      asNumber(rate.price),
      Math.round(expected),
      `${rate.label}: the site shows ${rate.price} but ${keys[i]} x ${multiplier} = ${expected}. The config header warns that rates drift on an FX move, and this is the check that says so.`
    );
  });
});

// EVERY figure on the ticker, not just the three the pricing table shows. The comment
// above `tickerModels` claims each number below is the configured rate times the
// configured multiplier for EVERY model at BOTH bases, and that is 6 x 2 x 3 = 36
// numbers. Comparing the EXPORTED array rather than the source text matters: two models
// share the `peakPrices` constant and four carry literals, so a source-reading check
// silently runs past the first two and reports the third model's figures for them -
// which is exactly the false alarm this was written to catch, arriving by the other
// route.
test('every ticker figure is its own model config rate times its own multiplier', async () => {
  const { tickerModels } = await import('../src/lib/models.ts');

  const blocks = config
    .split('[[models]]')
    .slice(1)
    .map((b) => ({
      name: (b.match(/^\s*name = "([^"]+)"/m) ?? [])[1],
      block: b,
    }))
    .filter((m) => Boolean(m.name));

  assert.equal(
    tickerModels.length,
    blocks.length,
    'the ticker and the config have a different number of models, so a figure cannot be checked against the model it belongs to'
  );

  const bases: [string, string[]][] = [
    ['peak', ['input_peak', 'cache_read_peak', 'output_peak']],
    ['offPeak', ['input_offpeak', 'cache_read_offpeak', 'output_offpeak']],
  ];

  for (const model of tickerModels) {
    const mine = blocks.find((b) => b.name === model.name);
    assert.ok(mine, `the ticker shows ${model.name} and the config does not declare it`);
    const multiplier = numberIn(mine.block, 'price');

    for (const [basis, keys] of bases) {
      const shown = model[basis as keyof typeof model] as string[];
      assert.equal(shown.length, keys.length, `${model.name} ${basis} is not three figures`);
      keys.forEach((key, i) => {
        const expected = Math.round(numberIn(mine.block, key) * multiplier);
        assert.equal(
          asNumber(shown[i]),
          expected,
          `${model.name} ${basis} ${key}: the site shows ${shown[i]} but ${key} x ${multiplier} = ${expected}`
        );
      });
    }
  }
});

// The OTHER transcription in this module: the deposit minimums. They were written into
// one place two rounds ago because the landing page stated them twice, and a figure that
// lives in one place is still a COPY - one place is not one source. `min_first_deposit`
// and `min_topup` are read here for the same reason the rates are: a customer who
// deposits 10,000 having read 10,001 is rejected by a server the page contradicts, and
// the page is what they read.
test('the deposit minimums are the config wallet minimums', async () => {
  const { minFirstDepositIdr, minTopupIdr } = await import('../src/lib/models.ts');

  const wallet = config.slice(config.indexOf('[wallet]'));
  const read = (key: string): number => {
    const m = wallet.match(new RegExp('^\\s*' + key + '\\s*=\\s*([0-9]+)', 'm'));
    assert.ok(m, 'the config no longer declares wallet.' + key);
    return Number(m[1]);
  };

  assert.equal(minFirstDepositIdr, read('min_first_deposit'));
  assert.equal(minTopupIdr, read('min_topup'));
  // A first deposit below the later minimum would make the second figure unreachable,
  // which is a coherent-looking config that no longer means what the page says.
  assert.ok(
    minFirstDepositIdr > minTopupIdr,
    'min_first_deposit is not above min_topup, so the page would quote two minimums where one is unreachable'
  );
});

// The third transcription: the model NAMES. `KNOWN_MODELS` says it is the config's
// full inventory - config/apikita.toml, [[models]] name = ..., IN FILE ORDER - and the
// access summary built on it tells a customer how many of their key's models are not
// currently enabled. If a model is added to the config and not here, the summary
// undercounts; if one is removed from the config and left here, the summary names a
// model that cannot exist. Both are silent, because the number is a count.
test('KNOWN_MODELS is the config model list, in file order', async () => {
  const { KNOWN_MODELS, models } = await import('../src/lib/dashboard-form.ts');
  const lib = await import('../src/lib/models.ts');

  const declared = config
    .split('[[models]]')
    .slice(1)
    .map((b) => (b.match(/^\s*name = "([^"]+)"/m) ?? [])[1])
    .filter(Boolean);

  assert.ok(declared.length >= 4, `only ${declared.length} models were read from the config`);
  assert.deepEqual(
    [...KNOWN_MODELS],
    declared,
    'KNOWN_MODELS is the config inventory, so it must BE that list in that order. The access summary counts against it, and a count that is quietly wrong tells a customer the wrong thing about their own key.'
  );

  // The routable subset must be a subset of the inventory: the picker offers what the
  // proxy can serve, and a name outside the inventory could not be routed at all.
  for (const m of lib.models) {
    assert.ok(
      KNOWN_MODELS.includes(m),
      `models lists ${m}, which is not in the config inventory`
    );
  }
});
