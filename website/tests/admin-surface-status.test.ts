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

/** Rows of the form: | `METHOD /path` | **built**|planned | ... */
function claims(status: 'built' | 'planned'): string[] {
  const needle = status === 'built' ? /\*\*built\*\*/ : /\|\s*planned/;
  return nonMoneyTable()
    .split('\n')
    .filter((line) => needle.test(line))
    .map((line) => (line.match(/`([A-Z]+ \/api\/[^`]+)`/) || [])[1])
    .filter((p): p is string => Boolean(p));
}

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
