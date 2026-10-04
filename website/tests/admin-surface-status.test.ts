// The operator-surface docs must not call a built route unbuilt, or vice versa.
//
// WHY THIS EXISTS. This class produced TWO real defects. The whitepaper published
// pricing figures the repo had already disowned, and docs/abuse-runbook.md told an
// operator responding to a live incident that account suspension was "endpoints not
// yet built" - long after it shipped, was tested, and had its own console buttons.
// Neither document was wrong when written; both went stale, and nothing connected
// them to the code that made them stale.
//
// So this reads the build-status claims out of docs/admin-surface.md and checks
// them against the routes actually registered in server/src/routes/mod.rs. A
// "planned" route that ships without the doc moving fails here, and so does a
// "built" route that is removed.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../..', import.meta.url));
const doc = readFileSync(root + '/docs/admin-surface.md', 'utf8');
const mod = readFileSync(root + '/server/src/routes/mod.rs', 'utf8');

/** The Non-money actions table, between its heading and the next section. */
function nonMoneyTable(): string {
  const start = doc.indexOf('## Non-money actions');
  const end = doc.indexOf('## Audit trail');
  assert.ok(start > -1 && end > start, 'the Non-money actions table is present');
  return doc.slice(start, end);
}

/**
 * Every data row of the table, as `{ path, status }`.
 *
 * THE STATUS PATTERN WAS ASYMMETRIC AND THAT WAS A HOLE, MEASURED. `claims('built')` required the
 * literal `**built**` with its bold markers, while `claims('planned')` accepted a plain `planned`.
 * So a row rewritten `| built |` - bold dropped by an editor, or a status copy-pasted from the
 * planned column - matched NEITHER set and fell out of both assertions. The guard passed while that
 * route's status was never checked at all. Removing the bold from one of the three `**built**` rows
 * left the suite at 2 pass / 0 fail, because the vacuity guard is satisfied by the other two.
 *
 * The asymmetry is what made it silent: a pattern that requires decoration on one value and not on
 * the other cannot notice when the decoration goes missing. Both are now matched the same way, and
 * an unrecognised status is REPORTED rather than skipped.
 */
function rows(): { path: string; status: string }[] {
  return nonMoneyTable()
    .split('\n')
    .filter((line) => /^\|/.test(line))
    // The `|---|---|` separator and the header row.
    .filter((line) => !/^\|\s*[-: |]+\|?\s*$/.test(line))
    .filter((line) => !/Endpoint\s*\|/.test(line))
    .map((line) => {
      const path = (line.match(/`([A-Z]+ \/api\/[^`]+)`/) || [])[1];
      // The status cell, with any bold markers stripped so `built` and `**built**` are the same word.
      const status = (line.match(/\|\s*\*{0,2}([A-Za-z]+)\*{0,2}\s*\|/) || [])[1];
      return { path: path ?? '', status: (status ?? '').toLowerCase() };
    })
    .filter((r) => r.path !== '');
}

/** Rows whose path claim a given status. */
function claims(status: 'built' | 'planned'): string[] {
  return rows().filter((r) => r.status === status).map((r) => r.path);
}

/**
 * THE STATUS VOCABULARY, checked so a third status cannot appear unnoticed.
 *
 * The point is not that these two words are correct; it is that a status the guard does not know is
 * currently INDISTINGUISHABLE from no status at all. A route marked `partial` would be excluded from
 * both assertions and checked by nothing.
 */
test('every row carries a status the guard understands', () => {
  const unknown = rows().filter((r) => r.status !== 'built' && r.status !== 'planned');
  assert.deepEqual(
    unknown,
    [],
    `a row's status is neither built nor planned, so NEITHER assertion below looks at it: ${JSON.stringify(unknown)}`,
  );
  assert.ok(rows().length > 0, 'the table has no data rows, so every assertion here is vacuous');
});

/** Every admin route registered in the router, with {id} normalised to :id. */
function registered(): string[] {
  return [...mod.matchAll(/"(\/api\/admin\/[^"]+)"/g)]
    .map((m) => m[1])
    // Test fixtures carry a literal zero uuid; they are not separate routes.
    .filter((p) => !p.includes('00000000-0000-0000-0000-000000000000'))
    .map((p) => p.replace('{id}', ':id'));
}

test('every route the doc calls built is actually registered', () => {
  const have = registered();
  const built = claims('built');
  assert.ok(built.length > 0, 'the table states at least one built route');
  for (const route of built) {
    // "METHOD /path" -> check the path is registered.
    const path = route.split(' ')[1];
    assert.ok(
      have.includes(path),
      `docs/admin-surface.md calls "${route}" built, but ${path} is not registered in mod.rs`,
    );
  }
});

test('no route the doc calls planned is actually registered', () => {
  const have = registered();
  const planned = claims('planned');
  assert.ok(planned.length > 0, 'the table states at least one planned route');
  for (const route of planned) {
    const path = route.split(' ')[1];
    assert.ok(
      !have.includes(path),
      `docs/admin-surface.md calls "${route}" planned, but ${path} IS registered - the doc is stale`,
    );
  }
});
