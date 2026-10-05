// Executable contract for the privacy disclosure (src/lib/privacy.ts).
//
// The rule this suite exists to enforce, from docs/data-retention.md:42-43: the
// /privacy page "must be updated in the same change if any of it moves again".
// The failure mode is silent — a new stored category (or a new long-lived table)
// that the disclosure never mentions — so the guard is a coverage assertion, not
// a snapshot: it checks that the categories the schema actually holds are named,
// rather than pinning the exact list, which would break on every wording tweak.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { stored, notStored, retention, requests } from '../src/lib/privacy.ts';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');

/** Every table the migrations CREATE, read from the migrations themselves. */
function schemaTables(): string[] {
  const dir = join(root, 'server', 'migrations');
  const names = new Set<string>();
  for (const file of readdirSync(dir)) {
    if (!file.endsWith('.sql')) continue;
    const sql = readFileSync(join(dir, file), 'utf8');
    for (const m of sql.matchAll(/CREATE TABLE(?:\s+IF NOT EXISTS)?\s+([a-z_]+)/gi)) {
      names.add(m[1].toLowerCase());
    }
  }
  // A REBUILD STAGING TABLE is not a category of data: it is a transient copy the migration itself
  // creates and drops. Excluded by name rather than by a hand-kept allowlist so a real table cannot
  // hide behind the exclusion.
  for (const n of [...names]) {
    if (/_rebuild_staging$/.test(n)) names.delete(n);
  }
  return [...names].sort();
}

function mentions(rows: readonly { what: string }[], needle: string): boolean {
  return rows.some((r) => r.what.toLowerCase().includes(needle.toLowerCase()));
}

/**
 * EVERY table the schema creates must be disclosed, checked against the SCHEMA rather than a list.
 *
 * THIS TEST USED TO CHECK ELEVEN HAND-PICKED CATEGORIES while its own header claimed it checked "the
 * categories the schema actually holds". MEASURED: deleting the whole `auth_attempts` disclosure row
 * left this file green at 7 passed / 0 failed, because `auth_attempts` was not one of the eleven.
 * The disclosure itself was complete - all 21 tables were covered - so the defect was the CHECK, not
 * the subject, which is the harder of the two to notice.
 *
 * The coverage is now derived from `server/migrations/*.sql`, so a NEW table fails here until it is
 * disclosed. That is the property the header always described.
 */
test('every table the migrations create is disclosed as stored', () => {
  const tables = schemaTables();
  assert.ok(
    tables.length > 15,
    `only ${tables.length} tables were read from server/migrations, so this check is looking at the ` +
      'wrong place and would pass vacuously',
  );

  const undisclosed = tables.filter((t) => !stored.some((r) => (r.covers ?? []).includes(t)));
  assert.deepEqual(
    undisclosed,
    [],
    'these tables exist in the schema and NO stored-disclosure row claims them. docs/data-retention.md ' +
      'requires the /privacy page to change in the same commit as the data model, so a new table ' +
      `must be disclosed here: ${undisclosed.join(', ')}`,
  );
});

test('the disclosure names the human categories, not only table names', () => {
  // Coverage by table name is necessary and not sufficient: a row can claim a table and still fail to
  // say what the data IS. These are the phrases docs/data-retention.md names.
  for (const category of [
    'email',
    'password hash',
    'google',
    'telegram',
    'wallet',
    'top-up',
    'usage',
    'per-request usage',
    'api keys',
    'review',
    'session',
    'sign-in attempt',
  ]) {
    assert.ok(mentions(stored, category), `the disclosure does not name a stored category: ${category}`);
  }
});

test('the three token/behavioural tables are each named, not merged', () => {
  // "Token usage per day" and "Per-request usage" are distinct rows because they
  // have different retention and different sensitivity. A single "usage" row
  // would understate the per-request detail.
  assert.ok(mentions(stored, 'token usage per day'), 'daily usage row missing');
  assert.ok(mentions(stored, 'per-request usage'), 'per-request usage row missing');
});

test('the never-stored list keeps the promises that matter most', () => {
  // Prompts and completions are launch Gate 4 (docs/data-retention.md). Losing
  // either row would drop the single most important claim on the page.
  assert.ok(mentions(notStored, 'prompt'), 'prompts must stay on the never-stored list');
  assert.ok(mentions(notStored, 'completion'), 'completions must stay on the never-stored list');
  assert.ok(mentions(notStored, 'raw ip'), 'raw IPs must stay on the never-stored list');
  assert.ok(mentions(notStored, 'plaintext api keys'), 'plaintext keys must stay on the never-stored list');
});

test('every stored row states where the data lives', () => {
  for (const row of stored) {
    assert.ok(row.where.length > 0, `${row.what} has no location`);
    assert.ok(row.sensitivity.length > 0, `${row.what} has no sensitivity`);
  }
});

test('every retention row states a period and a reason', () => {
  for (const row of retention) {
    assert.ok(row.keep.length > 0, `${row.what} has no retention period`);
    assert.ok(row.why.length > 0, `${row.what} has no rationale`);
  }
});

test('per-request usage carries a retention period', () => {
  // The row added with the usage_events table. It must state a period, and the
  // period the doc settles is 90 days.
  const row = retention.find((r) => r.what.toLowerCase().includes('per-request usage'));
  assert.ok(row, 'per-request usage must have a retention row');
  assert.match(row.keep, /90 days/);
});

test('the access-request list covers see, export and delete', () => {
  assert.ok(requests.length >= 4, 'the access/deletion list is thin');
  for (const r of requests) {
    assert.ok(r.request.length > 0 && r.how.length > 0, 'an access row is incomplete');
  }
});
