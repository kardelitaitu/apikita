// A page under website/src that shows a config figure must READ it, not restate it.
//
// WHY THIS EXISTS
//
// website/src/pages/dashboard/wallet.astro carried this, directly under a comment
// explaining the problem it creates:
//
//   L13: import { minFirstDepositIdr, minTopupIdr, topupPerHour, idr } from '../../lib/models.ts';
//   L21: const topupPerHour = 5;   // shadows the import
//
// Line 21 shadows line 13. The page published the local literal, so `prices.test.ts`
// - which compares the imported constant against `config/apikita.toml` - passed while
// checking a value the page never rendered. Changing line 21 to `7` changed the built
// HTML to "rate-limited to 7 per hour" with the whole suite still green.
//
// And the comment above it said so. It reads: "consolidating the landing page's two
// copies into one constant did not touch this file, and no guard could see it, because
// tests/prices.test.ts checks the CONSTANT against config/apikita.toml and the constant
// was right. The two copies here were invisible to it by construction... That is the
// failure this file is now evidence of: a fix that consolidates SOME copies of a value,
// and a check that pins the consolidated one."
//
// So the edit that wrote that paragraph reintroduced the defect for the rate cap. The
// lesson is not that someone was careless - it is that a guard which pins a CONSTANT
// cannot see a SURFACE that publishes its own copy, and no amount of care fixes that.
// The guard has to read the surfaces.
//
// WHAT IT CHECKS
//
//   For every `.astro` file under website/src that imports an identifier from a local
//   module, that file must not declare the SAME identifier itself with a literal.
//
// That is the whole rule, and it is deliberately narrow: it does not try to find every
// hardcoded number on every page (a prose sentence legitimately says "2 years"), only
// the one shape that is unambiguously a bug - a name that is both imported and declared,
// where the declaration wins silently.
//
// WHY THE SHADOW IS SILENT
//
// A duplicate `const` in one module is normally a compile error, so the question is why
// this built at all. In an `.astro` frontmatter block the import and the declaration end
// up in the same module scope, and Vite's transform does not surface the redeclaration.
// The page builds, the local wins, and nothing anywhere says so. That silence is the
// reason this file exists rather than a lint rule.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.
// Tests run with cwd = website/, so the source tree is `src`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const SRC = 'src';

function astroFiles(dir: string, found: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) astroFiles(full, found);
    else if (entry.endsWith('.astro')) found.push(full.split('\\').join('/'));
  }
  return found;
}

/** The frontmatter block, which is everything between the opening and closing `---`. */
function frontmatter(text: string): string | null {
  const m = text.match(/^---\r?\n([\s\S]*?)\r?\n---/);
  return m === null ? null : m[1];
}

/** Identifiers a file imports from a LOCAL module (a `./` or `../` specifier). */
function localImports(front: string): Set<string> {
  const names = new Set<string>();
  for (const m of front.matchAll(/import\s*\{([^}]+)\}\s*from\s*['"](\.[^'"]*)['"]/g)) {
    for (const raw of m[1].split(',')) {
      const name = raw.trim().split(/\s+as\s+/).pop()?.trim();
      if (name) names.add(name);
    }
  }
  return names;
}

/**
 * Identifiers the file declares itself with a LITERAL value - `const x = 5`,
 * `let x = 'a'`, `const x = 30_000`. A declaration whose value is an expression
 * (`= usageWindowQuery(on)`) is not a restatement of a constant, so it is allowed:
 * only a literal copies a value that has a single source elsewhere.
 */
function literalDeclarations(front: string): Map<string, string> {
  const out = new Map<string, string>();
  for (const m of front.matchAll(/^\s*(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*=\s*([^;\n]+)/gm)) {
    const name = m[1];
    const value = m[2].trim();
    const isLiteral = /^-?[\d_]+(?:\.\d+)?$/.test(value) || /^'[^']*'$/.test(value) || /^"[^"]*"$/.test(value);
    if (isLiteral) out.set(name, value);
  }
  return out;
}

test('no page declares its own copy of a value it imported', () => {
  const files = astroFiles(SRC);
  assert.ok(files.length >= 15, `only ${files.length} .astro files found under src; the walk is not reading the source tree`);

  const problems: string[] = [];
  let filesWithLocalImports = 0;

  for (const file of files) {
    const front = frontmatter(readFileSync(file, 'utf8'));
    if (front === null) continue;

    const imported = localImports(front);
    if (imported.size > 0) filesWithLocalImports++;

    for (const [name, value] of literalDeclarations(front)) {
      if (imported.has(name)) {
        problems.push(
          `${file} imports \`${name}\` from a local module and also declares \`const ${name} = ${value};\`. The declaration shadows the import, so the page publishes the local literal and every test that checks the imported constant - including one in tests/prices.test.ts that compares it against config/apikita.toml - passes without ever seeing what renders. Delete the declaration and use the import.`,
        );
      }
    }
  }

  // Vacuity guard for the import parsing: if localImports() stopped matching, this
  // test would pass on an empty problem list while checking nothing at all.
  assert.ok(
    filesWithLocalImports >= 3,
    `only ${filesWithLocalImports} files were found importing from a local module; parsing is broken, so this guard is vacuous`,
  );

  assert.deepEqual(problems, [], `a page restates a value it could import:\n  ${problems.join('\n  ')}`);
});

test('the shadowing rule is reading the file it was written for', () => {
  // The positive control. wallet.astro imports four identifiers; if the parser
  // cannot see them, the guard above cannot see a shadow of them either.
  const wallet = astroFiles(SRC).find((f) => f.endsWith('dashboard/wallet.astro'));
  assert.ok(wallet, 'dashboard/wallet.astro is missing, so the control cannot run');

  const front = frontmatter(readFileSync(wallet, 'utf8'));
  assert.ok(front, 'wallet.astro has no frontmatter block');
  const imported = localImports(front);
  assert.ok(imported.has('topupPerHour'), `the import parser did not find topupPerHour among ${[...imported].join(', ')}; it is not reading the imports it must guard`);
  assert.ok(
    !literalDeclarations(front).has('topupPerHour'),
    'wallet.astro declares topupPerHour again; the shadow that this guard was written for is back',
  );
});
