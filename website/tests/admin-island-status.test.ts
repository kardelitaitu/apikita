// Executable contract for the admin island's two status switches.
//
// `AccountAdmin.astro` exports nothing and is not exercised by any test - MEASURED: it was named in
// NO test file at all, while being the largest island in the repository at 797 lines. The two
// switches below are the part of it that can be checked from source without a DOM.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const ISLAND = join(here, '..', 'src', 'islands', 'admin', 'AccountAdmin.astro');

/**
 * The body of one top-level function in the island, read from `function name(` to the closing brace
 * that sits at the function's own indentation.
 *
 * Read from SOURCE rather than imported, because an `.astro` island has no module surface a test can
 * import - which is also why these two switches went unchecked while their inputs were covered.
 */
function fnBody(source: string, name: string): string {
  const start = source.indexOf(`function ${name}(`);
  assert.ok(start >= 0, `AccountAdmin.astro must still define \`${name}\`; this test reads its body.`);
  const lines = source.slice(start).split('\n');
  const out: string[] = [];
  for (const line of lines) {
    out.push(line);
    if (out.length > 1 && /^ {2}\}/.test(line)) break;
  }
  return out.join('\n');
}

/** The class strings a switch returns, in source order. */
function returnedClasses(body: string): string[] {
  return [...body.matchAll(/return '([^']+)'/g)].map((m) => m[1]);
}

test('both admin status switches answer for a status they do not enumerate', () => {
  // WHY THIS EXISTS, and it is a measured gap rather than a style preference.
  //
  // `statusBadge` and `statusClasses` are two switches over the same `AccountStatus` union
  // (`lib/admin.ts`: `'active' | 'suspended' | 'closed'`, mirroring the accounts CHECK constraint),
  // and they return the SAME three class strings. `statusBadge` ended in `default:` and
  // `statusClasses` enumerated the three members and stopped.
  //
  // That is safe only while the union stays exactly three values, and MEASURED the compiler does not
  // hold that line: adding a fourth member to `AccountStatus` left `tsc --noEmit` at exit 0. A
  // status the new member can hold would then fall off the end of `statusClasses`, which returns
  // `undefined`, and the call site interpolates the result into a className - where `${undefined}`
  // is the string "undefined". The rendered row would carry
  // "rounded border px-2 py-0.5 text-xs font-medium undefined" and no status colour at all.
  //
  // The island is named in no test file, so nothing else can catch this.
  const island = readFileSync(ISLAND, 'utf8');
  const badge = fnBody(island, 'statusBadge');
  const classes = fnBody(island, 'statusClasses');

  for (const [name, body] of [['statusBadge', badge], ['statusClasses', classes]] as const) {
    assert.match(
      body,
      /default:/,
      `\`${name}\` must keep a \`default:\` arm. Without one it returns \`undefined\` for any status ` +
        'the union does not enumerate, and the call site interpolates that into a className as the ' +
        'literal string "undefined". MEASURED: tsc does not catch a missing arm - widening ' +
        '`AccountStatus` leaves `tsc --noEmit` at exit 0.',
    );
  }

  // AND THE TWO MUST AGREE. The point of the fallback is that a status with no explicit styling is
  // rendered the SAME way by the badge and by the row, so the fix is not "any default" but "the
  // same default". Two different fallbacks would mean a badge that contradicts its own row.
  const badgeFallback = [...badge.matchAll(/default:\s*\n\s*return '([^']+)'/g)].map((m) => m[1]);
  const classesFallback = [...classes.matchAll(/default:\s*\n\s*return '([^']+)'/g)].map((m) => m[1]);
  assert.equal(badgeFallback.length, 1, 'statusBadge must have exactly one default arm');
  assert.equal(classesFallback.length, 1, 'statusClasses must have exactly one default arm');
  assert.equal(
    classesFallback[0],
    badgeFallback[0],
    `the two status switches must fall back to the SAME classes: the badge uses ` +
      `"${badgeFallback[0]}" and the row uses "${classesFallback[0]}". A status with no explicit ` +
      'styling would then be rendered two different ways in one row.',
  );

  // And the ENUMERATED cases map identically, so the two are the same rule written twice rather than
  // two rules that happen to share a fallback.
  //
  // Only the cases each switch NAMES are compared, and the comparison is by status: `statusBadge`
  // spells three (`active`, `suspended`, then `default`) while `statusClasses` spells four (`active`,
  // `suspended`, `closed`, then `default`). A positional slice would compare the wrong pair - the
  // badge's fallback against the row's `closed` - which is what the first version of this assertion
  // did, and it failed against a correct tree.
  const byStatus = (body: string) => {
    const out = new Map<string, string>();
    for (const m of body.matchAll(/case '([^']+)':\s*\n\s*return '([^']+)'/g)) out.set(m[1], m[2]);
    return out;
  };
  const badgeByStatus = byStatus(badge);
  const classesByStatus = byStatus(classes);

  assert.deepEqual(
    [...badgeByStatus.keys()].sort(),
    ['active', 'suspended'],
    'statusBadge must name `active` and `suspended` explicitly, with the rest falling to `default:`',
  );
  for (const status of ['active', 'suspended']) {
    assert.equal(
      classesByStatus.get(status),
      badgeByStatus.get(status),
      `the two status switches disagree about \`${status}\`: statusBadge says ` +
        `"${badgeByStatus.get(status)}" and statusClasses says "${classesByStatus.get(status)}". ` +
        'They style the same badge in the same row, so a divergence shows the customer two ' +
        'different colours for one status.',
    );
  }
});
