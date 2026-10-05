// A test that pins the island's MODELS_ALL copy to the lib's authority, and covers the four
// untested formatters' RULES.
//
// WHY. MEASURED: KeyManagement.astro:210 declares its own `MODELS_ALL = ['flash','deepseek-v4-flash']`
// while lib/models.ts:76 exports the same list. Removing an entry from the ISLAND's copy fails no test
// (verified with the writer's unrelated in-flight failure filtered out), and the consequence is
// customer-visible: `modelsText` returns 'All models' when `k.models.length >= MODELS_ALL.length`, so
// a SHORTER island copy makes a restricted key read as unrestricted - the opposite of the
// deny-by-default allowlist that string is meant to convey.
//
// This is docs/testing.md's opening rule - "A duplicate rule drifts, and it drifts toward the weaker
// reading" - and the four formatters around it (statusLabel, spendText, modelsText, lastUsedText)
// have no test asserting any of their output strings.
//
// HOW IT READS THE ISLAND. The formatters are inside an Astro `<script>` block, which is plain
// TypeScript in the built page but is not importable here. So the file is read as TEXT and the
// functions are pinned by their RULES, evaluated by extracting the function bodies and running them -
// rather than by a regex on the source, which would pass on a function whose body changed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const ISLAND = join(root, 'src', 'islands', 'keys', 'KeyManagement.astro');
const islandSrc = readFileSync(ISLAND, 'utf8');

test('the key island lists exactly the models the lib publishes', () => {
  // The authority.
  const libSrc = readFileSync(join(root, 'src', 'lib', 'models.ts'), 'utf8');
  const libMatch = libSrc.match(/export const models = (\[[^\]]*\])/);
  assert.ok(libMatch, 'lib/models.ts must still export `models`, or this comparison has no authority');

  const islandMatch = islandSrc.match(/const MODELS_ALL = (\[[^\]]*\])/);
  assert.ok(
    islandMatch,
    'KeyManagement.astro must still declare MODELS_ALL. If it was renamed, update this test - but do not delete it: the island compares `k.models.length >= MODELS_ALL.length`, so the length is load-bearing.',
  );

  const parse = (s: string): string[] => s.replace(/[\[\]'"]/g, '').split(',').map((x: string) => x.trim()).filter(Boolean);
  const lib = parse(libMatch[1]);
  const island = parse(islandMatch[1]);

  // Worded so a reader learns WHICH direction is dangerous.
  assert.deepEqual(
    island.slice().sort(),
    lib.slice().sort(),
    `the island's MODELS_ALL is ${JSON.stringify(island)} and lib/models.ts publishes ${JSON.stringify(lib)}. ` +
      'modelsText returns "All models" when k.models.length >= MODELS_ALL.length, so a SHORTER island copy ' +
      'makes a key restricted to a subset read as unrestricted. Import the lib list instead of keeping a ' +
      'second copy, or fix this array - but the two must not drift.',
  );

  // The positive control: both sides are non-empty, or the deepEqual above passes over empty arrays.
  assert.ok(lib.length > 0 && island.length > 0, 'both lists must be non-empty or the comparison is vacuous');
});

/**
 * Evaluate one of the island's formatters by extracting its source and running it.
 *
 * A regex over the island text would pass on a function whose body had changed; running it does not.
 *
 * THE TYPE ANNOTATIONS ARE STRIPPED. The island is TypeScript inside an Astro `<script>`, and
 * `new Function` parses JavaScript - so `function statusLabel(k: ApiKey): string {` throws
 * "Unexpected token ':'". Only the ANNOTATIONS are removed, never the logic: the parameter list's
 * `: Type` and the return type after `)`, which are the two forms these formatters use. A test that
 * silently fell back to a regex here would be the defect this whole file is about.
 */
function islandFormatter(name: string, stubs: Record<string, unknown> = {}): (k: Record<string, unknown>) => string {
  const at = islandSrc.indexOf(`function ${name}(`);
  assert.ok(at > -1, `KeyManagement.astro must still define ${name}`);
  let depth = 0;
  let started = false;
  let end = at;
  for (let i = at; i < islandSrc.length; i += 1) {
    const ch = islandSrc[i];
    if (ch === '{') { depth += 1; started = true; } else if (ch === '}') { depth -= 1; }
    if (started && depth === 0) { end = i + 1; break; }
  }
  const raw = islandSrc.slice(at, end);

  // Strip ONLY annotations: `param: Type` -> `param`, and `): Type {` -> `) {`.
  const body = raw
    .replace(/(\w+)\s*:\s*[A-Za-z_][\w<>\[\]|.]*\s*(?=[,)])/g, '$1')
    .replace(/\)\s*:\s*[A-Za-z_][\w<>\[\]|.]*\s*\{/, ') {');

  assert.ok(
    !/:\s*[A-Z]/.test(body),
    `an annotation survived the strip for ${name}, so this helper would throw rather than test: ${body.slice(0, 120)}`,
  );

  const names = Object.keys(stubs);
  // eslint-disable-next-line no-new-func
  const fn = new Function(...names, `${body}\nreturn ${name};`)(...names.map((n: string) => stubs[n]));
  return fn as (k: Record<string, unknown>) => string;
}

test('a key with no spend limit reads Unlimited, and one with a limit reads the figure', () => {
  const spendText = islandFormatter('spendText', { formatIdr: (n: number) => Number(n).toLocaleString('en-US') });

  // No limit at all.
  assert.equal(spendText({ spend_limit_idr: 0, spend_used_idr: 0 }), 'Unlimited');
  assert.equal(spendText({ spend_limit_idr: null, spend_used_idr: 0 }), 'Unlimited');
  // A NEGATIVE limit is not a limit either - the comparison is `<= 0`.
  assert.equal(
    spendText({ spend_limit_idr: -1, spend_used_idr: 0 }),
    'Unlimited',
    'a negative limit cannot be a cap, so it must read as unlimited rather than as a negative figure',
  );

  // A real limit shows both figures and the window.
  const withLimit = spendText({ spend_limit_idr: 50_000, spend_used_idr: 1_000 });
  assert.match(
    withLimit,
    /50,000/,
    'a key WITH a limit must never read Unlimited - that tells the customer a capped key cannot overspend',
  );
  assert.match(withLimit, /1,000/, 'the used amount must be shown beside the limit');
  assert.match(withLimit, /\(30d\)/, 'the window the limit applies to must be stated, or the figure is ambiguous');

  // THE BOUNDARY, which is the line that decides the reading.
  assert.notEqual(
    spendText({ spend_limit_idr: 1, spend_used_idr: 0 }),
    'Unlimited',
    'a limit of 1 IDR is a limit: the branch is `<= 0`, so the smallest positive value must NOT read Unlimited',
  );
});

test('a key restricted to a subset never reads All models', () => {
  const libSrc = readFileSync(join(root, 'src', 'lib', 'models.ts'), 'utf8');
  const libMatch = libSrc.match(/export const models = (\[[^\]]*\])/);
  assert.ok(libMatch, 'lib/models.ts must still export `models`, or this test has no authority to compare against');
  const all = libMatch[1].replace(/[\[\]'"]/g, '').split(',').map((x: string) => x.trim()).filter(Boolean);

  // `modelsText` closes over the island's module-level MODELS_ALL, so it is supplied here from the
  // ISLAND's own declaration - the same value test 1 checks against the lib. Reading it from the lib
  // instead would hide the very drift test 1 exists to catch.
  const islandMatch2 = islandSrc.match(/const MODELS_ALL = (\[[^\]]*\])/);
  assert.ok(islandMatch2, 'KeyManagement.astro must still declare MODELS_ALL for this test to supply it');
  const islandList = islandMatch2[1]
    .replace(/[\[\]'"]/g, '').split(',').map((x: string) => x.trim()).filter(Boolean);
  const modelsText = islandFormatter('modelsText', { MODELS_ALL: islandList });

  assert.equal(modelsText({ models: [] }), 'None (deny by default)', 'an empty allowlist permits nothing and must say so');
  assert.equal(modelsText({ models: all }), 'All models', 'the full list is the only input that may read All models');

  // ONE SHORT OF THE FULL LIST must NOT read "All models" - this is the assertion the drift breaks.
  assert.notEqual(
    modelsText({ models: all.slice(0, -1) }),
    'All models',
    'a key missing even one model is restricted, and reading it as All models tells the customer the opposite',
  );

  const one = modelsText({ models: [all[0]] });
  assert.match(one, /^1 model$/, 'a single model must read as singular, not "1 models"');

  // THE PLURAL ARM IS REACHED ONLY WHEN THE LISTS ARE LONG ENOUGH FOR A SUBSET TO EXIST. With
  // MODELS_ALL at two entries, a two-model list is the FULL list and correctly reads "All models" -
  // so the plural branch is only reachable when MODELS_ALL is longer than two. Supplying a longer
  // island list here is what reaches it; the live value is checked against the lib by test 1.
  const longIsland = [...islandList, 'third-model'];
  const modelsTextLong = islandFormatter('modelsText', { MODELS_ALL: longIsland });
  assert.equal(
    modelsTextLong({ models: [all[0], 'third-model'] }),
    '2 models',
    'two models out of three is a restricted key, and it must read as a count rather than as All models',
  );
  assert.equal(modelsTextLong({ models: [all[0]] }), '1 model', 'the singular form must not gain an s');
});

test('a revoked key reads Revoked even when it has also expired', () => {
  const statusLabel = islandFormatter('statusLabel', {});
  const past = new Date(Date.now() - 86_400_000).toISOString();
  const future = new Date(Date.now() + 86_400_000).toISOString();

  assert.equal(statusLabel({ revoked_at: null, expires_at: null }), 'Active');
  assert.equal(statusLabel({ revoked_at: null, expires_at: future }), 'Active');
  assert.equal(statusLabel({ revoked_at: null, expires_at: past }), 'Expired');

  // THE PRECEDENCE. A revoked key that has ALSO expired must read Revoked: the revoke is the
  // operator's action and the security fact, and reporting it as a natural expiry hides that
  // somebody chose to disable it.
  assert.match(
    statusLabel({ revoked_at: past, expires_at: past }),
    /^Revoked/,
    'revocation is checked first; a key that is both revoked and expired must not read as merely Expired',
  );
  assert.match(statusLabel({ revoked_at: past, expires_at: future }), /^Revoked/);

  // An unparseable timestamp must not blank the label - the key is still revoked.
  assert.match(statusLabel({ revoked_at: 'not-a-date', expires_at: null }), /^Revoked/);
});

test('a never-used key reads Never rather than a date', () => {
  const lastUsedText = islandFormatter('lastUsedText', {});
  assert.equal(lastUsedText({ last_used_at: null }), 'Never');
  assert.equal(lastUsedText({ last_used_at: '' }), 'Never');
  // A real timestamp renders, and an unparseable one shows a placeholder rather than "Invalid Date".
  assert.match(lastUsedText({ last_used_at: new Date().toISOString() }), /\d/);
  assert.equal(lastUsedText({ last_used_at: 'not-a-date' }), '—', 'an unreadable timestamp must not print "Invalid Date" to a customer');
});
