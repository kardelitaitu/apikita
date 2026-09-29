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


