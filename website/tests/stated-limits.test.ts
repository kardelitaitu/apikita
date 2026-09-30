// A rule the server enforces and a page states is ONE rule with two copies, and
// nothing compared them.
//
// WHY THIS EXISTS
//
// server/src/config.rs documents its password floor like this:
//
//   /// on both signup and reset, so the two paths cannot drift. The signup page
//   /// states this same number to the user, so the validator refuses a value
//   /// below 8 - a server floor under the published one would make the page
//   /// describe a rule we do not enforce.
//
// That is the whole argument for this file: the floor is a number the server
// ENFORCES and the page ANNOUNCES, and the document says in as many words that a
// server floor under the published one would leave the page describing a rule we
// do not enforce. Nothing tested it. Nine copies of `8` sat in three pages -
// `minlength="8"` in the markup and "At least 8 characters" in the prose - against
// `config/apikita.toml:156 password_min_length = 8` and the loader's own literal.
//
// Lowering the config value would leave the pages refusing a password the server
// accepts (annoying, and a support ticket). Raising it would leave the pages
// ACCEPTING a password the server rejects - the user fills a form the page has
// just told them is valid, and it fails on submit. Neither failed a test.
//
// This is the same shape as round 24's wallet.astro shadow: a value with one
// source, restated on a surface, with a check that pins the source and cannot see
// the surface. That guard catches an identifier that shadows an import; this one
// catches a literal that has no identifier at all.
//
// WHAT IT CHECKS
//
//   Every `minlength="N"` and every "at least N characters" / "minimum N
//   characters" under website/src equals the configured password floor.
//
// The `minlength` attribute is the one that matters most: it is what the BROWSER
// enforces, so it is what decides whether the user gets a native validation error
// or a server rejection.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const SRC = join(here, '..', 'src');
const CONFIG = join(here, '..', '..', 'config', 'apikita.toml');

const config = readFileSync(CONFIG, 'utf8');

/** The number a `key = value` line carries in the top-level config, else null. */
function configNumber(key: string): number | null {
  const m = config.match(new RegExp('^\\s*' + key + '\\s*=\\s*([0-9]+)', 'm'));
  return m === null ? null : Number(m[1]);
}

function sourceFiles(dir: string, found: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) sourceFiles(full, found);
    else if (/\.(astro|ts)$/.test(entry)) found.push(full);
  }
  return found;
}

test('every stated password minimum is the minimum the config sets', () => {
  const configured = configNumber('password_min_length');
  assert.ok(
    configured !== null,
    'config/apikita.toml has no `password_min_length`; this guard cannot check a floor that is not set',
  );

  const files = sourceFiles(SRC).filter((f) => !f.endsWith('.d.ts'));
  assert.ok(files.length >= 20, `only ${files.length} source files found; the walk is not reading the tree`);

  const stated: { file: string; line: number; text: string; value: number; what: string }[] = [];

  for (const file of files) {
    const rel = file.slice(SRC.length + 1).split('\\').join('/');
    const lines = readFileSync(file, 'utf8').split('\n');

    lines.forEach((line, index) => {
      // The markup attribute, which is the rule the BROWSER enforces.
      for (const m of line.matchAll(/minlength="(\d+)"/g)) {
        stated.push({ file: rel, line: index + 1, text: line.trim(), value: Number(m[1]), what: 'minlength' });
      }
      // The prose, which is the rule the CUSTOMER reads.
      for (const m of line.matchAll(/(?:at least|minimum|min\.?)\s+(\d+)\s+characters/gi)) {
        stated.push({ file: rel, line: index + 1, text: line.trim(), value: Number(m[1]), what: 'prose' });
      }
    });
  }

  // Vacuity: a regex that stopped matching would make every assertion below vanish.
  assert.ok(
    stated.length >= 8,
    `only ${stated.length} password-minimum statements found; the scan is not reading the pages`,
  );
  assert.ok(
    stated.some((s) => s.what === 'minlength') && stated.some((s) => s.what === 'prose'),
    'the scan found only one KIND of statement, so one of the two patterns has stopped matching',
  );

  for (const s of stated) {
    assert.equal(
      s.value,
      configured,
      `${s.file}:${s.line} states a password minimum of ${s.value} (${s.what}) but config/apikita.toml sets password_min_length = ${configured}. server/src/config.rs refuses a configured floor below the published one precisely because "a server floor under the published one would make the page describe a rule we do not enforce". A LOWER page value accepts a password the server rejects; a HIGHER one rejects a password the server accepts.`,
    );
  }
});

test('every stated seconds figure is the figure its own mechanism uses', () => {
  // There are THREE different intervals in the site and they are one sentence apart in
  // places, so a regex that matched "N seconds" alone conflated them - which is how
  // this test was first written, and its first run accused pages/index.astro:68 of
  // stating the poll interval when it states the metadata cache TTL. All three are real
  // facts with real sources; none may be guessed from the phrase "N seconds".
  //
  //   the POLL interval      live.ts POLL_INTERVAL_MS, used by the client's poller
  //   the CACHE TTL          config key_metadata_cache_seconds, how long a lowered key
  //                          limit takes to apply at the proxy
  //   the COOLDOWN CEILING   config cooldown_max_seconds, the longest a tripped upstream
  //                          can stay out (the ladder is 30 -> 60 -> 120 -> 240 -> 480 -> 900)
  const live = readFileSync(join(SRC, 'lib', 'live.ts'), 'utf8');
  const pollMatch = live.match(/POLL_INTERVAL_MS\s*=\s*([\d_]+)/);
  assert.ok(pollMatch, 'website/src/lib/live.ts no longer declares POLL_INTERVAL_MS, so this guard is checking nothing');
  const pollMs = Number(pollMatch[1].replace(/_/g, ''));
  assert.ok(pollMs > 0 && Number.isFinite(pollMs), `POLL_INTERVAL_MS is not a positive number: ${pollMatch[1]}`);
  const pollSeconds = pollMs / 1000;
  assert.ok(
    Number.isInteger(pollSeconds),
    `POLL_INTERVAL_MS is ${pollMs}ms, which is not a whole number of seconds, so no page can state it without rounding`,
  );

  const ttl = configNumber('key_metadata_cache_seconds');
  assert.ok(ttl !== null, 'config/apikita.toml has no key_metadata_cache_seconds, so the cache-TTL half of this test checks nothing');

  // The cooldown ceiling lives in the [circuit_breaker] table, so it is read from that
  // block rather than the top level. The block is found first and its absence is a
  // failure, not a null that quietly matches nothing.
  const cbBlock = config.split(/^\[/m).find((b) => /^circuit_breaker\]/.test(b));
  assert.ok(cbBlock, 'config/apikita.toml has no [circuit_breaker] block, so the cooldown-ceiling half of this test checks nothing');
  const ceilMatch = cbBlock.match(/^\s*cooldown_max_seconds\s*=\s*(\d+)/m);
  assert.ok(ceilMatch, '[circuit_breaker] no longer sets cooldown_max_seconds, so the 503 Retry-After ceiling is unstated');
  const cooldownCeiling = Number(ceilMatch[1]);

  // The lib constant the pages import. If it drifts from config, that is its own bug
  // and it gets its own message rather than being folded into a page's.
  const models = readFileSync(join(SRC, 'lib', 'models.ts'), 'utf8');
  const ttlConst = models.match(/keyMetadataCacheSeconds\s*=\s*(\d+)/);
  assert.ok(ttlConst, 'website/src/lib/models.ts no longer declares keyMetadataCacheSeconds');
  assert.equal(
    Number(ttlConst[1]),
    ttl,
    `models.ts declares keyMetadataCacheSeconds = ${ttlConst[1]} but config/apikita.toml sets key_metadata_cache_seconds = ${ttl}. The pages import the constant, so the constant must carry the configured value.`,
  );

  // Each statement is classified by the sentence it sits in, not by its number: the
  // mechanisms have different words, and those words are the only thing that
  // distinguishes them.
  const marked = [
    { what: 'the poll interval', pattern: /poll|\/api\/me/gi, expected: pollSeconds, source: `POLL_INTERVAL_MS (${pollMs}ms)` },
    { what: 'the metadata cache TTL', pattern: /metadata cache|lowering|lowered|cache ttl/gi, expected: ttl, source: `key_metadata_cache_seconds (${ttl})` },
    { what: 'the cooldown ceiling', pattern: /cooldown/gi, expected: cooldownCeiling, source: `[circuit_breaker] cooldown_max_seconds (${cooldownCeiling})` },
  ] as const;

  const stale: string[] = [];
  const counts: Record<string, number> = {};
  const unclassified: string[] = [];

  for (const file of sourceFiles(SRC)) {
    const rel = file.slice(SRC.length + 1).split('\\').join('/');
    const lines = readFileSync(file, 'utf8').split('\n');
    lines.forEach((line, index) => {
      for (const m of line.matchAll(/(?:every|within|up to|as long as)\s+(\d+)\s+seconds/gi)) {
        // Which mechanism does THIS sentence describe?
        const owner = marked.find((k) => {
          k.pattern.lastIndex = 0;
          return k.pattern.test(line);
        });
        if (owner === undefined) {
          // An unclassified "N seconds" is not a pass and not a failure to assert on -
          // it is a statement this guard cannot check, and saying so is the point.
          unclassified.push(
            `${rel}:${index + 1} states "${m[0]}" but the sentence names no mechanism this test knows. Either classify it here (with its config key or lib constant) or remove the unverifiable figure. Line: ${line.trim()}`,
          );
          continue;
        }
        counts[owner.what] = (counts[owner.what] ?? 0) + 1;
        if (Number(m[1]) !== owner.expected) {
          stale.push(
            `${rel}:${index + 1} says "${m[0]}" about ${owner.what}, but ${owner.source}. A page stating a stale figure is read by a customer who then measures the difference. Line: ${line.trim()}`,
          );
        }
      }
    });
  }

  assert.deepEqual(
    unclassified,
    [],
    `a stated seconds figure has no mechanism to check it against:\n  ${unclassified.join('\n  ')}`,
  );

  assert.deepEqual(
    stale,
    [],
    `a stated seconds figure does not match its mechanism:\n  ${stale.join('\n  ')}`,
  );

  // The inventory, pinned so that a NEW statement is reviewed rather than silently
  // trusted. This is deliberately an equality: adding a fourth "N seconds" to a page is
  // exactly the moment a human should say which mechanism it describes.
  assert.deepEqual(
    counts,
    { 'the poll interval': 1, 'the metadata cache TTL': 2, 'the cooldown ceiling': 1 },
    `the set of stated seconds figures changed: ${JSON.stringify(counts)}. This is the guard telling you the inventory moved, so that a new statement is classified rather than silently trusted. Update this expectation in the same change that adds or removes one.`,
  );
});

