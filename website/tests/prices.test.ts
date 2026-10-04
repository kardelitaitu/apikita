// Every figure the customer-facing site shows that config/apikita.toml sets: the token
// rates, the ticker cards for all six models, the deposit minimums, the per-hour top-up
// cap, and the key-metadata cache TTL. Derived, not transcribed.
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

/**
 * The body of a `[section]`, up to the NEXT section header - not to the end of the file.
 *
 * `config.slice(config.indexOf('[wallet]'))` looks equivalent and is not: it runs to EOF, so a key
 * that MOVED out of `[wallet]` into any later section is still found, and the guard passes while the
 * key is no longer in the section it names. MEASURED: `min_topup` moved into `[limits]` left this
 * file at 6 pass / 0 fail, and `[wallet]` is followed by some thirty more sections.
 *
 * That matters here more than it would elsewhere, because the section IS part of the claim. A
 * `min_topup` under `[limits]` is not the wallet's deposit minimum whatever its value, so a guard
 * that cannot tell the two apart is not checking that the page quotes "a limit the config has".
 */
function section(name: string): string {
  const header = new RegExp('^\\[' + name + '\\]\\s*$', 'm');
  const start = config.search(header);
  assert.ok(start >= 0, 'the config no longer has a [' + name + '] section');
  const rest = config.slice(start + 1);
  // The next top-level or array-of-table header, whichever comes first.
  const next = rest.search(/^\[/m);
  const body = next >= 0 ? rest.slice(0, next) : rest;
  assert.ok(
    !new RegExp('^\\[' + name + '\\]', 'm').test(body.slice(1)),
    'the [' + name + '] slice swallowed another section header'
  );
  return body;
}

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

  const wallet = section('wallet');
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
  const { KNOWN_MODELS } = await import('../src/lib/dashboard-form.ts');
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

// The per-hour top-up cap, the third figure the wallet page shows. It was a local
// `const` in that page naming this key, which is better than a bare literal and still a
// transcription - and a transcription is exactly what the two minimums were. It lives in
// lib/models.ts so this can DERIVE it: the wallet page is the only surface that shows it,
// so there was nothing to consolidate with, and moving it here is what makes it checkable
// rather than merely documented.
test('the wallet rate cap is the config topup_per_hour', async () => {
  const { topupPerHour } = await import('../src/lib/models.ts');

  // [limits] in the config, read as text for the reason the whole file is text - and read through
  // `section()`, which stops at the next header, so this cannot pick up a `topup_per_hour` that has
  // been moved somewhere else.
  const limits = section('limits');
  const match = limits.match(/^\s*topup_per_hour\s*=\s*([0-9]+)/m);
  assert.ok(match, 'the config no longer declares [limits] topup_per_hour');

  assert.equal(
    topupPerHour,
    Number(match[1]),
    'the wallet page quotes a top-up cap the config does not have, so a customer reads a limit that is not enforced'
  );
});

// The key-metadata cache TTL, which the quickstart quotes as the wait for a LOWERED key
// limit to apply. It was a bare number in prose, which no guard could see: a customer-facing
// figure with no derivation behind it, on the page a developer plans against.
test('the key-metadata cache TTL is the config key_metadata_cache_seconds', async () => {
  const { keyMetadataCacheSeconds } = await import('../src/lib/models.ts');

  // [limits], NOT [key_pool]: the proxy key-metadata TTL is a rate cap, and the
  // section it lives in is the one that decides it. I looked under [key_pool]
  // first, on the reasonable guess that a cache TTL belongs beside the pool, and
  // the guard failed - which is what it is for.
  //
  // THAT REASONING IS WHY THIS NOW USES `section()`. The point of the test is which SECTION the key
  // is in, and the old `slice(indexOf('[limits]'))` ran to EOF - so moving
  // `key_metadata_cache_seconds` under `[key_pool]`, the very mistake this comment describes, would
  // have passed. A guard that cannot tell the two sections apart cannot make this claim at all.
  const limits = section('limits');
  const match = limits.match(/^\s*key_metadata_cache_seconds\s*=\s*([0-9]+)/m);
  assert.ok(match, 'the config no longer declares key_metadata_cache_seconds');

  assert.equal(
    keyMetadataCacheSeconds,
    Number(match[1]),
    'the quickstart quotes a wait the config does not have, so a developer plans around a limit change that lands sooner or later than it will'
  );
});

// `section()` IS THE GUARD THAT MAKES THE THREE ABOVE MEAN WHAT THEY SAY, so it gets its own test.
// Every other assertion in this file reads a value; this one reads the READER. Without it, a future
// edit that "simplifies" `section()` back to `slice(indexOf(...))` would restore the hole silently -
// all three tests would keep passing, because the shipping config has every key in the right place.
test('section() stops at the next header, so it cannot read a key from a later section', () => {
  const wallet = section('wallet');
  // The wallet body must NOT contain the headers that follow it, nor their keys.
  assert.ok(!/^\[/m.test(wallet), 'section() swallowed a following header: ' + wallet.slice(-80));
  assert.ok(
    !/^\s*topup_per_hour\s*=/m.test(wallet),
    'section([wallet]) contains a [limits] key, so it is not bounded at the next header'
  );
  assert.ok(
    /^\s*min_topup\s*=/m.test(wallet),
    'section([wallet]) lost its own key, so the boundary is wrong in the other direction'
  );

  // And the boundary is real: [wallet] is followed by [sessions], which is a different body.
  const sessions = section('sessions');
  assert.notEqual(sessions, wallet);
  assert.ok(!/^\s*min_topup\s*=/m.test(sessions), 'section([sessions]) contains a [wallet] key');

  // A section that does not exist must fail loudly rather than return the rest of the file, which is
  // the fail-open shape the helper replaced: `indexOf` returns -1 and `slice(-1)` is the LAST
  // character, so the old form returned a one-character string and every regex missed silently.
  assert.throws(() => section('no_such_section_exists'), /no longer has a \[no_such_section_exists\]/);
});
