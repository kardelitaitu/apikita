// The ALERT-COVERAGE counts stated in prose must match tools/alert/alerts.tsv.
//
// WHY THIS EXISTS. docs/launch-checklist.md said "9 of 10 entries covered" and it
// was RIGHT when written - the table has grown since, and nothing tied the sentence
// to the file. Measured when this guard was added: alerts.tsv holds 11 alerts and
// 10 are covered. The one exception is relay_5xx, which is needs-metrics because it
// cannot be derived from the database or /health alone.
//
// This is the doc-counts.test.ts problem in a second place: a number a reader is
// expected to trust, maintained by recall. The difference is that these counts are
// computed from a machine-readable file, so the guard derives them exactly instead
// of pinning a constant a human has to bump.
//
// It also pins the CROSS-REFERENCE. todo.md and docs/launch-checklist.md describe
// the same work and used to describe it differently - one said only that the sweep
// scripts exist, the other that the code half is done and only the channel and the
// schedule remain. Two documents about one task have to agree about its state.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (rel: string) => readFileSync(join(root, rel), 'utf8');

/// The rows of alerts.tsv, header dropped.
///
/// The header is identified by its FIRST cell being exactly "id" rather than by
/// position, so inserting a comment above it cannot silently shift every count.
function alertRows(): string[][] {
  const rows = read('tools/alert/alerts.tsv')
    .split('\n')
    .filter((line) => line.trim() !== '' && !line.startsWith('#'))
    .map((line) => line.split('\t'));
  return rows.filter((cells) => cells[0] !== 'id');
}

test('the alert table holds what the prose says it holds', () => {
  const rows = alertRows();

  // Guard the fixture BEFORE trusting the counts: a parser that read nothing
  // would make every number zero and pass vacuously.
  assert.ok(rows.length >= 10, 'alerts.tsv parsed to ' + rows.length + ' rows - it must have been read');
  for (const cells of rows) {
    assert.ok(cells.length >= 6, 'every row needs a coverage column; got ' + JSON.stringify(cells));
  }

  const covered = rows.filter((cells) => cells[5] === 'covered').length;
  const total = rows.length;

  // The counts must be internally meaningful: a fixture where everything is one
  // value cannot distinguish a correct guard from a broken one.
  assert.ok(covered > 0, 'no alert is covered - the fixture or the file is wrong');
  assert.ok(covered < total, 'every alert is covered - the negative case is gone');

  // Every document that names the file must state the counts the file holds.
  const docs = ['docs/launch-checklist.md', 'todo.md'];
  let checked = 0;
  for (const doc of docs) {
    const text = read(doc);
    if (!text.includes('alerts.tsv')) continue;
    const m = text.match(/(\d+) of (?:its )?(\d+)/);
    assert.ok(m, doc + ' names alerts.tsv but states no "N of M" claim to verify');
    assert.equal(Number(m![1]), covered, doc + ' claims ' + m![1] + ' covered; the file has ' + covered);
    assert.equal(Number(m![2]), total, doc + ' claims ' + m![2] + ' alerts; the file has ' + total);
    checked += 1;
  }
  assert.ok(checked >= 2, 'expected both documents to state the counts, checked ' + checked);
});
