// The whitepaper publishes a rate card to outsiders, and it drifted: it carried
// ~1,100 / ~4,400 / ~22 IDR per 1M - a stale single-vintage number uniformly ~18%
// below the provider's real price list - while business/02-pricing.md had already
// corrected its own card and explicitly called those figures wrong. Nothing tied
// the two documents together, so the whitepaper kept asserting a cost the rest of
// the repo had disowned.
//
// This pins the whitepaper's three wholesale figures and its consumer prices to
// the verified values in config/apikita.toml (flash, billing_basis="peak"). It
// reads the real files rather than a transcription, so a config change that is
// not reflected in the whitepaper fails here.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../..', import.meta.url));
const whitepaper = readFileSync(root + '/docs/whitepaper.md', 'utf8');
const config = readFileSync(root + '/config/apikita.toml', 'utf8');

/** Reads `key = value` from the flash model block of the real config. */
function flashRate(key: string): number {
  // The flash block is the first [[models]] with name = "flash"; its rate lines
  // sit a little below it, before the next [[models]].
  const flashStart = config.indexOf('name = "flash"');
  assert.ok(flashStart > -1, 'the flash model block exists in config/apikita.toml');
  const block = config.slice(flashStart, config.indexOf('[[models]]', flashStart + 1));
  const line = block.split('\n').find((l) => l.trimStart().startsWith(key + ' '));
  assert.ok(line, `${key} is present in the flash block`);
  const m = line!.match(/=\s*([0-9.]+)/);
  assert.ok(m, `${key} has a numeric value`);
  return Number(m![1]);
}

/** Renders a number the way the whitepaper groups it: 2676.78 -> "2,676.78". */
function grouped(n: number, decimals: number): string {
  return n.toLocaleString('en-US', {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  });
}

test('the whitepaper rate card matches the verified config, at peak', () => {
  const input = flashRate('input_peak');
  const output = flashRate('output_peak');
  const cache = flashRate('cache_read_peak');

  // The exact figures the whitepaper must carry (from 02-pricing.md's verified
  // card). Asserting against the CONFIG values, so a config move breaks this.
  for (const [label, value] of [['input', input], ['output', output], ['cache read', cache]] as const) {
    assert.ok(
      whitepaper.includes(grouped(value, 2)),
      `the whitepaper must state the peak ${label} rate ${grouped(value, 2)}`,
    );
  }
});

test('the whitepaper states a consumer price of peak x 1.5, not the stale card', () => {
  const input = flashRate('input_peak');
  const output = flashRate('output_peak');
  const cache = flashRate('cache_read_peak');
  const round = (n: number) => grouped(Math.round(n * 1.5), 0);
  for (const [label, peak] of [['input', input], ['output', output], ['cache', cache]] as const) {
    assert.ok(
      whitepaper.includes(round(peak)),
      `the whitepaper must state the +50% ${label} price ${round(peak)}`,
    );
  }
});

test('the whitepaper no longer publishes the disowned wholesale figures', () => {
  // These are the values business/02-pricing.md labels WRONG. They may appear in
  // the "Corrected." note (which quotes them to explain the change), but never as
  // the live rate card, so the check is that each is confined to that note.
  for (const stale of ['1,100', '4,400']) {
    const at = whitepaper.indexOf(stale);
    if (at === -1) continue; // fully removed is also fine
    const nearby = whitepaper.slice(Math.max(0, at - 200), at);
    assert.ok(
      nearby.includes('Corrected'),
      `${stale} may only appear in the Corrected note, not as a live rate`,
    );
  }
});
