// Every line citation under website/src must point where it claims, and the
// privacy page's "last updated" date must be the date that policy really changed.
//
// WHY THIS EXISTS
//
// `server/src/doc_claims.rs` enforces the opposite rule for docs/: a document
// under docs/ may NOT cite code by line, because a line number re-points the
// moment somebody inserts a line above it. It says so in the test name -
// `operational_docs_cite_code_by_name_and_never_by_line` - and it holds the list
// of offenders to empty.
//
// website/src was the surface where the "you may not cite by line" rule was
// neither stated nor checked, and it cites by line in nineteen places. When this
// file was written, ELEVEN of those citations were re-quoted and checked by hand
// and TEN had drifted: they pointed at a login example, a closing brace, a blank
// line, session-lookup code, a table header. None of them failed anything. A
// citation that points nowhere still reads exactly like a citation that points at
// the right thing, which is why nothing caught it for as long as it was wrong.
//
// TWO RULES, NOT ONE
//
//   1. A citation may point at any file in the repository, but the path must
//      EXIST and the line number must be IN RANGE for that file. That catches a
//      citation that survived a file move, a rename, or a deletion.
//   2. If the citation is to a RUST file, the line must hold something a reader
//      could check it against: a declaration (`fn`, `struct`, `enum`, `const`,
//      `static`, `impl`, `trait`, `type`), a mounted `.route(` line, a field of a
//      request struct, or a `SQL` clause. That catches the specific failure this
//      file was written for - a citation that drifted into the body of an
//      unrelated function, onto a bare `};`, or onto comment prose.
//
// Rule 2 is deliberately a SHAPE check and not a semantic one. Nothing here can
// know that `account.rs:248` is the right parse function; only a reader can. What
// it can know is that `account.rs:210` was a `};`, and that no citation to a Rust
// file should ever land on one. That is the difference between a guard that can
// be written and a guard that would have to reimplement the type checker.
//
// The `SQL` shape is not a loophole for "any line". It is there because a real
// citation points at a clause rather than a declaration - `account.rs:296` is
// `ORDER BY day DESC`, cited because the ORDER is the fact the client depends on.
// A guard that rejected it would push the source into citing the enclosing
// function instead, which is a coarser and less checkable claim.
//
// SOURCE OF TRUTH
//
// The citation sites are DISCOVERED, not listed. A hand-kept list of nineteen
// citations would be the same defect this file is about, one level up: the
// twentieth citation would simply not be in it. The test walks website/src and
// parses the citations out of the source text.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.
// Tests run with cwd = website/, so the repository root is `..`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync, existsSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { join, dirname, normalize } from 'node:path';

const REPO = '..';
const SRC = 'src';

// ---------------------------------------------------------------------------
// Discovering the citations
// ---------------------------------------------------------------------------

/**
 * A citation is `some/path.ext:123` or `some/path.ext:123-456` in the text of a
 * file under website/src. The leading-character guard keeps this from matching
 * inside a longer token: `https://host/page.md:12` is not a citation, and neither
 * is `foo.bar.rs:1` where `bar.rs` is not a file in this repository.
 */
const CITATION = /(?<![\w/.-])([A-Za-z0-9_][A-Za-z0-9_/.@-]*\.(?:md|rs|ts|toml|sql|py|sh|astro|yml|json)):(\d+)(?:-(\d+))?(?![\w-])/g;

interface Citation {
  /** File under website/src that writes the citation. */
  readonly from: string;
  /** 1-based line within `from`. */
  readonly fromLine: number;
  /** The path as written, e.g. `docs/server/api-spec.md`. */
  readonly path: string;
  /** First cited line within the target file. */
  readonly start: number;
  /** Last cited line, or `start` for a single-line citation. */
  readonly end: number;
}

function sourceFiles(dir: string, found: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      sourceFiles(full, found);
    } else if (/\.(ts|astro)$/.test(entry)) {
      found.push(full);
    }
  }
  return found;
}

function collectCitations(): Citation[] {
  const out: Citation[] = [];
  for (const file of sourceFiles(SRC)) {
    const text = readFileSync(file, 'utf8');
    text.split('\n').forEach((line, i) => {
      for (const m of line.matchAll(CITATION)) {
        const start = Number(m[2]);
        out.push({
          from: file.split('\\').join('/'),
          fromLine: i + 1,
          path: m[1],
          start,
          end: m[3] === undefined ? start : Number(m[3]),
        });
      }
    });
  }
  return out;
}

/**
 * Where a cited path lives. A citation written as `account.rs:190` is
 * crate-root-relative in intent but is not a path from the repository root, so a
 * bare filename is resolved by search. `routes/mod.rs` and `keys.rs` appear once
 * each in this repository, which is what makes the search unambiguous.
 */
function resolveTarget(cited: string): string | null {
  const direct = normalize(join(REPO, cited));
  if (existsSync(direct)) return direct;

  if (!cited.includes('/')) {
    const hits: string[] = [];
    const walk = (dir: string): void => {
      for (const entry of readdirSync(dir)) {
        if (entry === 'node_modules' || entry === '.git' || entry === 'target' || entry === 'dist') continue;
        const full = join(dir, entry);
        if (statSync(full).isDirectory()) walk(full);
        else if (entry === cited) hits.push(full);
      }
    };
    walk(REPO);
    if (hits.length === 1) return hits[0];
  }
  return null;
}

// ---------------------------------------------------------------------------
// Rule 1 — the target exists and the line is in range
// ---------------------------------------------------------------------------

test('every line citation under website/src points at a line that exists', () => {
  const citations = collectCitations();

  // Vacuity guard: a regex that silently stopped matching would make every
  // assertion below disappear without failing anything.
  assert.ok(
    citations.length >= 15,
    `the citation scan found ${citations.length} citations; it found nineteen when this guard was written, so the pattern is no longer reading what this test thinks it is`,
  );

  const problems: string[] = [];
  for (const c of citations) {
    const target = resolveTarget(c.path);
    if (target === null) {
      problems.push(`${c.from}:${c.fromLine} cites ${c.path}, which is not a file in this repository`);
      continue;
    }
    const lines = readFileSync(target, 'utf8').split('\n').length;
    if (c.start < 1 || c.end > lines) {
      problems.push(
        `${c.from}:${c.fromLine} cites ${c.path}:${c.start}${c.end === c.start ? '' : `-${c.end}`} but that file has ${lines} lines`,
      );
    }
  }

  assert.deepEqual(problems, [], `a citation points at nothing:\n  ${problems.join('\n  ')}`);
});

test('the citation scan is reading the files it claims to read', () => {
  // The positive control for the discovery step. If sourceFiles() returned an
  // empty list, the test above would pass on an empty problem list and mean
  // nothing at all.
  const citations = collectCitations();
  const files = new Set(citations.map((c) => c.from));
  assert.ok(files.size >= 5, `citations were found in only ${files.size} files, which is too few to be the real source tree`);
  assert.ok(
    citations.some((c) => c.from.endsWith('website/src/lib/usage.ts') || c.from.endsWith('src/lib/usage.ts')),
    'the scan did not reach src/lib/usage.ts, which held five citations when this guard was written',
  );
});

// ---------------------------------------------------------------------------
// Rule 2 — a Rust citation lands on a declaration, not inside a body
// ---------------------------------------------------------------------------

/**
 * Shapes that can be what a citation is pointing at, in the order a reader would
 * check them. Request-struct fields are included because citations to a PATCH
 * body legitimately point at `pub struct UpdateKeyRequest`, and the struct
 * marker would catch the struct heading rather than the field - so the field
 * form is accepted explicitly rather than by relaxing to "any line".
 */
const RUST_SHAPES: readonly { what: string; test: (line: string) => boolean }[] = [
  { what: 'a function', test: (l) => /^\s*(pub\s+)?(async\s+)?(unsafe\s+)?fn\s+\w/.test(l) },
  { what: 'a struct or enum', test: (l) => /^\s*(pub\s+)?struct\s+\w|^\s*(pub\s+)?enum\s+\w/.test(l) },
  { what: 'a constant or static', test: (l) => /^\s*(pub\s+)?(const|static)\s+\w/.test(l) },
  { what: 'an impl, trait or type alias', test: (l) => /^\s*(pub\s+)?(impl|trait|type)\s/.test(l) },
  { what: 'a mounted route', test: (l) => /\.route\(/.test(l) },
  { what: 'a request-struct field', test: (l) => /^\s*(pub\s+)?#\[serde|^\s*pub\s+\w+\s*:\s*(Option<|String|i64|i32|u64|u32|bool|Vec<|DateTime)/.test(l) },
  // A clause, not a declaration. Cited when the clause IS the claim - the day
  // column's type, the ORDER the client's "today" depends on, the field a value
  // is read into.
  { what: 'a SQL clause', test: (l) => /\b(ORDER BY|GROUP BY|SELECT|FROM|WHERE|LIMIT|HAVING)\b/.test(l) && !/^\s*\/\//.test(l) },
  { what: 'a field assignment', test: (l) => /^\s*"\w+"\s*:\s*\w/.test(l) },
  // A call that IS the cited fact - `chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")`
  // is cited for its format string, and no declaration line carries it.
  { what: 'a standard-library call', test: (l) => /^\s*[a-z_][a-z0-9_]*::[A-Z]\w*::\w+\(|^\s*[A-Z]\w*::\w+::\w+\(/.test(l) },
];

function looksLikeRustAnchor(line: string): string | null {
  for (const shape of RUST_SHAPES) {
    if (shape.test(line)) return shape.what;
  }
  return null;
}

test('every citation into a Rust file lands on a declaration it could be citing', () => {
  // The failure this catches, from the repair that created this file: five
  // citations in src/lib/usage.ts and src/lib/service-status.ts had drifted onto
  // `};`, session-lookup statements and comment prose. Every one of those is a
  // line a Rust anchor can never be, and the check is exact rather than
  // heuristic on that point.
  const citations = collectCitations().filter((c) => c.path.endsWith('.rs'));
  assert.ok(citations.length >= 4, `only ${citations.length} Rust citations found; there were at least five when this guard was written`);

  const problems: string[] = [];
  for (const c of citations) {
    const target = resolveTarget(c.path);
    if (target === null) continue; // rule 1 already reports this
    const lines = readFileSync(target, 'utf8').split('\n');
    if (c.start > lines.length) continue; // rule 1 already reports this

    // A span is accepted if ANY line inside it is an anchor. `account.rs:296`
    // single-line citations and multi-line spans both work.
    const span = lines.slice(c.start - 1, Math.min(c.end, lines.length));
    if (!span.some((line) => looksLikeRustAnchor(line) !== null)) {
      problems.push(
        `${c.from}:${c.fromLine} cites ${c.path}:${c.start}${c.end === c.start ? '' : `-${c.end}`}, which holds no function, struct, const, impl, route or request-struct field - so it cannot be what is being cited. The lines there are: ${span.map((l) => JSON.stringify(l)).join(', ')}`,
      );
    }
  }

  assert.deepEqual(problems, [], `a citation into Rust points at a body, not a declaration:\n  ${problems.join('\n  ')}`);
});

// ---------------------------------------------------------------------------
// The privacy page's date, checked against the commit that really set it
// ---------------------------------------------------------------------------

/**
 * website/src/pages/privacy.astro states its own provenance: the date is "the
 * date it last changed in git (git log -1 -- docs/data-retention.md)". That is a
 * checkable claim, and it was wrong when this test was written - the page said
 * 27 September while the document had last changed on 30 September, in the very
 * commit that added two rows to the retention policy the page restates.
 *
 * `%d %B %Y` renders `30 September 2026`, which is byte-for-byte the format the
 * page uses, so this comparison is exact and needs no date parsing.
 */
test('the privacy page states the date its source document last changed', () => {
  const page = readFileSync('src/pages/privacy.astro', 'utf8');

  const stated = page.match(/const\s+lastUpdated\s*=\s*'([^']+)'/);
  assert.ok(stated, 'privacy.astro no longer declares `const lastUpdated`, so this guard is checking nothing');

  const actual = execFileSync(
    'git',
    ['log', '-1', '--format=%ad', '--date=format:%d %B %Y', '--', 'docs/data-retention.md'],
    { cwd: REPO, encoding: 'utf8' },
  ).trim();

  assert.ok(actual.length > 0, 'git returned no date for docs/data-retention.md; this guard cannot check anything');

  assert.equal(
    stated[1],
    actual,
    `the privacy page says it was last updated "${stated[1]}", but docs/data-retention.md last changed "${actual}". The page states its own rule at website/src/pages/privacy.astro:11-12 - the date is the git date of the document it restates - so the page is the thing that is out of date. Change the page, not this test, unless the page has stopped being a restatement of that document.`,
  );
});
