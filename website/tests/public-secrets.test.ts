// Guard for the checklist claim: "PUBLIC_* variables contain nothing secret."
//
// WHY THIS EXISTS. `.env.example:96` states the stakes itself:
//
//   IMPORTANT: anything prefixed with PUBLIC_ or NEXT_PUBLIC_ is INLINED INTO
//   BROWSER JAVASCRIPT AND IS NOT SECRET.
//
// That is the whole mechanism, and it is unforgiving: a value reachable through
// `import.meta.env.PUBLIC_*` is substituted into the bundle at BUILD time and served
// to every visitor. Measured, not assumed - planting a sentinel in PUBLIC_API_BASE_URL
// (a variable the source genuinely reads) puts it verbatim in
// `dist/_astro/errors.*.js`, and the build still exits 0.
//
// The existing Secret scan in CI does NOT cover this, and that is not a flaw in it: it
// checks that `.env` is untracked and that no private key is COMMITTED. Both are about
// SOURCE files. This mistake is not in the source - it is in what the build produces
// FROM it, which a source scan cannot see.
//
// WHAT MAKES A NAME REACHABLE is not the prefix alone. Astro/Vite only substitute the
// variables the source actually references, so the reachable set is small and knowable.
// This file pins it: if a fourth PUBLIC_ variable ever becomes reachable, the list below
// fails and someone has to decide - deliberately - whether its value may be published.
//
//   cd website && node --test 'tests/**/*.test.ts'

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, '..');

/** Every source file that the build can substitute a PUBLIC_ variable into. */
function sourceFiles(dir = join(root, 'src'), out = []) {
  for (const entry of readdirSync(dir)) {
    const path = join(dir, entry);
    if (statSync(path).isDirectory()) sourceFiles(path, out);
    else if (/\.(ts|astro|js|mjs)$/.test(entry)) out.push(path);
  }
  return out;
}

/** The PUBLIC_ names the source actually reads, which is the reachable set. */
function referencedPublicNames() {
  const names = new Set();
  for (const path of sourceFiles()) {
    for (const m of readFileSync(path, 'utf8').matchAll(/PUBLIC_([A-Z0-9_]+)/g)) {
      names.add(`PUBLIC_${m[1]}`);
    }
  }
  return [...names].sort();
}

// The reachable set is PINNED. Not every PUBLIC_ name is substituted - only the ones the
// source references - so this list is the complete surface that can leak into a bundle.
// Adding one is a decision about whether its value may be public; it should be made here,
// in a diff, rather than discovered in a shipped asset.
const REACHABLE = [
  'PUBLIC_API_BASE_URL',
  // Midtrans CLIENT keys are published in the page by design - the Snap.js integration
  // cannot work otherwise. `.env.example:100` says so explicitly: "The CLIENT key, not
  // the server key above: it is inlined into the browser bundle and is public by
  // design." The SERVER key must never appear here, which the test below enforces.
  'PUBLIC_MIDTRANS_CLIENT_KEY',
  'PUBLIC_MIDTRANS_ENV',
  'PUBLIC_POCKETBASE_URL',
];

test('the set of PUBLIC_ variables the build can inline is exactly the reviewed one', () => {
  const found = referencedPublicNames();
  assert.deepEqual(
    found,
    REACHABLE,
    'the inlinable PUBLIC_ set changed. Each name here is substituted into the browser ' +
      'bundle at build time and served to every visitor, so a new one is a decision about ' +
      'whether its value may be published - make it deliberately, in this list, with the ' +
      'reason in the surrounding comment.',
  );
});

// The names that LOOK like secrets. A variable holding one of these may not be reachable
// through a PUBLIC_ prefix, however it is spelled - `PUBLIC_MIDTRANS_SERVER_KEY` is the
// obvious trap, because the CLIENT key beside it is legitimately public.
const SECRET_SHAPED = /(SERVER_KEY|SECRET|PRIVATE|TOKEN|PASSWORD|PASSWD|API_KEY|SERVICE_ROLE|ENCRYPTION|WEBHOOK_SECRET)/;

test('no reachable PUBLIC_ variable is named like a secret', () => {
  const offenders = referencedPublicNames().filter((n) => SECRET_SHAPED.test(n));
  assert.deepEqual(
    offenders,
    [],
    `these PUBLIC_ variables are named like secrets but are inlined into the browser bundle: ${offenders.join(', ')}. ` +
      'The prefix is what publishes it, so the value would be readable by every visitor the moment it shipped.',
  );
});

// Migrations of the same idea: a PUBLIC_ name must not be assigned from a private
// counterpart. This catches the copy-paste that turns a server key into a public one.
test('no PUBLIC_ variable is assigned from a private counterpart', () => {
  const example = readFileSync(join(root, '..', '.env.example'), 'utf8');
  const offenders = [];
  for (const line of example.split('\n')) {
    const m = line.match(/^(PUBLIC_[A-Z0-9_]+)\s*=\s*(\S+)/);
    if (!m) continue;
    const [, name, value] = m;
    // A value that interpolates or names a private variable would be a leak at build time.
    if (/\$\{?[A-Z_]/.test(value) || /SERVER|SECRET|TOKEN|PRIVATE/.test(value.toUpperCase())) {
      offenders.push(`${name}=${value}`);
    }
  }
  assert.deepEqual(
    offenders,
    [],
    `these PUBLIC_ variables take a value that looks private: ${offenders.join(', ')}. ` +
      'Anything reachable through the PUBLIC_ prefix is inlined into the bundle and served ' +
      'to visitors, so it must be sourced from non-secret configuration.',
  );
});

// Guard the guard: the scan must be reading real files. A glob that matches nothing would
// make every assertion above pass over an empty set (the W38/W41 failure).
test('the source scan actually read the source tree', () => {
  const files = sourceFiles();
  assert.ok(
    files.length > 5,
    `the scan found only ${files.length} source file(s), so the assertions above would pass vacuously`,
  );
  assert.ok(
    files.some((f) => f.endsWith(join('lib', 'api.ts'))),
    'lib/api.ts is not among the scanned files, so the scan is looking somewhere else',
  );
});
