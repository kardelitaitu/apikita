// The pages promise credit expires two years after each deposit. NOTHING IMPLEMENTS IT.
//
// WHY THIS EXISTS
//
// website/src/pages/dashboard/wallet.astro says "Credit expires 2 years after each
// deposit" and website/src/pages/index.astro says the same in the FAQ and the pricing
// table. The policy is settled - docs/decisions.md:76 records "2 years (24 months) from
// each deposit's own date", settled by the owner - and the term is disclosed deliberately,
// because a term that extinguishes value must be disclosed before it can be relied on
// (docs/terms-of-service.md:21).
//
// The implementation does not exist, and the repository says so in three places:
//
//   docs/decisions.md:77        "Not built - the terms promise something the code does
//                                not do yet... no per-deposit expiry column, no sweep
//                                job, and no refusal of a spend against aged credit."
//   docs/terms-of-service.md:122 "**Not implemented.** Nothing in the system currently
//                                expires credit. There is no `expires_at` on `wallets`,
//                                no sweep job, and no code that would refuse a spend"
//   docs/launch-checklist.md:357 "the code is not written - no per-deposit expiry column,
//                                no sweep job, no refusal of a spend against aged credit"
//
// docs/decisions.md:69 gives the mechanism, and it is not a missing feature but a missing
// CAPABILITY: "The schema holds one **un-aged** `wallets.balance_idr` (no per-deposit date
// exists), so expired and live credit are **not distinguishable today**".
//
// So this is not "the page is wrong". It is worse and more interesting than that: the
// pages make a promise in the present tense that the system cannot keep even in
// principle, because the data needed to keep it is not recorded. Every existing test
// checks that the SENTENCE is present - landing-claims.test.ts:134 asserts the wallet
// page contains 'expires 2 years', :161 asserts the landing page contains '2 years' -
// and none of them can see the difference between a term that is stated and a term that
// is honoured. A page that stopped mentioning expiry would fail; a page whose term
// silently became true, or silently stopped being documented as false, fails nothing.
//
// WHAT THIS CHECKS
//
//   1. The absence is REAL: no `expires_at` / expiry column on `wallets`, and no
//      credit-expiry mechanism in server/src. If someone implements expiry, this test
//      fails and whoever implemented it is told to delete this file - which is the
//      point, because the file is a claim about the code and claims about code rot.
//   2. The pages do not OVERSTATE it. The text may state the term; it may not say it is
//      enforced, applied, automatic, or already in effect.
//   3. The three documents still SAY it is unimplemented. Deleting the warning while
//      the code is unbuilt is the silent version of the same lie.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.
// Tests run with cwd = website/, so the source tree is `src` and docs are `../docs`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join } from 'node:path';

const SRC = 'src';
const REPO = '..';

/** Every .rs file under server/src, recursively. */
function rustFiles(dir: string, found: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) rustFiles(full, found);
    else if (entry.endsWith('.rs')) found.push(full.split('\\').join('/'));
  }
  return found;
}

/** The concatenated CREATE TABLE statement for a table, from every migration. */
function tableDefinition(table: string): string {
  const dir = join(REPO, 'server', 'migrations');
  let all = '';
  for (const entry of readdirSync(dir)) {
    if (!entry.endsWith('.sql')) continue;
    const text = readFileSync(join(dir, entry), 'utf8');
    const m = text.match(new RegExp(`CREATE TABLE[^;]*\\b${table}\\b[^;]*;`, 'i'));
    if (m !== null) all += m[0] + '\n';
  }
  return all;
}

/**
 * Rust source with comments removed, so a check about what the code DOES is not
 * satisfied or defeated by prose about what the code does.
 *
 * Handles the three shapes that matter here: `//` line comments, block comments
 * (which NEST in Rust, unlike C - a naive scan ends the comment at the first
 * closing marker and would treat a nested comment's tail as code), and `///`
 * doc-comments, which are line comments and fall out of the same rule.
 *
 * It does NOT model string literals, so a `//` inside a string would truncate a
 * line. That is acceptable for what this is for - the SQL here is written as
 * multi-line string literals and the guards look for a statement, not for
 * punctuation - and the alternative is a Rust lexer in a test. What it must not
 * do is fail OPEN: the callers assert on the stripped text, and a stripper that
 * returned "" would make every absence assertion pass, so each caller checks the
 * stripped length is still substantial.
 */
function stripComments(source: string): string {
  let out = '';
  let i = 0;
  let depth = 0;

  while (i < source.length) {
    if (depth > 0) {
      if (source.startsWith('/*', i)) {
        depth++;
        i += 2;
      } else if (source.startsWith('*/', i)) {
        depth--;
        i += 2;
      } else {
        i++;
      }
      continue;
    }
    if (source.startsWith('//', i)) {
      // Drop to end of line, keeping the newline so line counts survive.
      const nl = source.indexOf('\n', i);
      i = nl === -1 ? source.length : nl;
      continue;
    }
    if (source.startsWith('/*', i)) {
      depth = 1;
      i += 2;
      continue;
    }
    out += source[i];
    i++;
  }
  return out;
}

test('credit expiry is still unimplemented, so the pages must not say it works', () => {
  // --- 1. The absence is real. -------------------------------------------------
  const wallets = tableDefinition('wallets');
  assert.ok(
    wallets.length > 0,
    'the `wallets` table definition was not found, so this guard is checking nothing about the schema',
  );
  assert.ok(
    !/expires_at|expiry|expires_on/i.test(wallets),
    'wallets now HAS an expiry column, so credit expiry may be implemented - and this file is a claim that it is not. Verify the feature, update docs/decisions.md:77, docs/terms-of-service.md:122 and docs/launch-checklist.md:357, and delete this test.',
  );

  const rust = rustFiles(join(REPO, 'server', 'src'))
    .map((f) => readFileSync(f, 'utf8'))
    .join('\n');
  assert.ok(rust.length > 1000, 'server/src was not read, so this guard is vacuous');
  assert.ok(
    !/credit[_ ]?expir|expire[d]?[_ ]credit|aged[_ ]credit|CREDIT_EXPIRY/i.test(rust),
    'a credit-expiry mechanism now exists in server/src, so the pages may be telling the truth and this file is out of date. Verify it, update the three documents, and delete this test.',
  );

  // Vacuity guard: the SCHEMA property that makes the feature impossible, not merely
  // absent. docs/decisions.md:69 rests on this exact fact.
  assert.ok(
    /balance_idr/.test(wallets) && !/credited_at|deposit_date/i.test(wallets),
    'wallets no longer holds a single un-aged balance, so the reasoning in docs/decisions.md:69 ("expired and live credit are not distinguishable today") no longer applies and must be revisited',
  );

  // --- 2. The pages do not overstate it. ---------------------------------------
  const pages = ['pages/dashboard/wallet.astro', 'pages/index.astro'];
  const overstated = [
    'expiry is applied',
    'expiry is enforced',
    'expires automatically',
    'automatically expires',
    'credit is expired',
    'expired credit is removed',
    'we remove expired',
  ];

  let pagesMentioningExpiry = 0;
  for (const page of pages) {
    const text = readFileSync(join(SRC, page), 'utf8');
    if (/expir/i.test(text)) pagesMentioningExpiry++;
    for (const phrase of overstated) {
      assert.ok(
        !text.toLowerCase().includes(phrase),
        `${page} says "${phrase}", which claims the expiry is ENFORCED. Nothing implements it, and the schema cannot distinguish expired credit from live credit - see docs/decisions.md:69. A page may state the term; it may not describe it as working.`,
      );
    }
  }
  assert.ok(
    pagesMentioningExpiry >= 2,
    `only ${pagesMentioningExpiry} of the two pages still mention expiry; the term is deliberately disclosed, so a page silently dropping it is a different - and also unchecked - change`,
  );

  // --- 3. The documents still record the gap. ----------------------------------
  const recorded = [
    ['decisions.md', /Not built|not built/],
    ['terms-of-service.md', /Not implemented|not implemented/],
    ['launch-checklist.md', /the code is not written/],
  ] as const;

  for (const [doc, pattern] of recorded) {
    const text = readFileSync(join(REPO, 'docs', doc), 'utf8');
    assert.ok(
      pattern.test(text),
      `docs/${doc} no longer records that credit expiry is unimplemented. The code still does not implement it, so removing the warning is the silent version of the same false promise - see docs/decisions.md:77, which flags it precisely because "a policy the code does not keep is worse than no policy".`,
    );
  }
});

test('the expiry guard is reading the files it names', () => {
  // Positive control. Each read above could silently return a stub, and every
  // assertion in this file is an ABSENCE assertion, which an empty string satisfies.
  const wallets = tableDefinition('wallets');
  assert.ok(/balance_idr/.test(wallets), 'the wallets definition did not contain balance_idr');
  assert.ok(/accounts/.test(wallets), 'the wallets definition did not contain its foreign key');

  const walletPage = readFileSync(join(SRC, 'pages/dashboard/wallet.astro'), 'utf8');
  assert.ok(
    /expires 2 years/i.test(walletPage),
    'dashboard/wallet.astro no longer states the expiry term at all, so the overstatement check above is vacuous',
  );

  const rust = rustFiles(join(REPO, 'server', 'src'));
  assert.ok(rust.length >= 10, `only ${rust.length} .rs files found under server/src; the walk is not reading the tree`);
});

// A SECOND claim of the same shape, found while fixing the first.
//
// "Reviews: Until deleted by user" was published in three places -
// website/src/lib/privacy.ts, docs/data-retention.md:75 and server/src/db.rs:551 -
// and all three described a deletion that DOES NOT EXIST. There is no
// `DELETE FROM reviews` anywhere in server/src. The only user-facing act is
// `POST /api/reviews/withdraw`, which sets `withdrawn_at` and deliberately does
// not delete: the row has to keep occupying the account's one slot, which is what
// "one review per account" means.
//
// So the window is not "until deleted by user", it is "forever". The phrase reads
// as a bound and is not one, which is the same failure as the credit-expiry claim
// above: a policy sentence that is grammatically a commitment and factually a
// placeholder.
//
// The consequence is not only editorial. `reviews.account_id` is
// `ON DELETE SET NULL`, so a review OUTLIVES the account that wrote it and stays
// in the public aggregate attached to nobody. `account_id IS NULL` is also the
// partial-index predicate for Telegram-authored reviews, so a row whose author
// left becomes indistinguishable from one written through the bot.
//
// This test pins the ABSENCE, so implementing deletion deletes this test - which
// is the design of the file it sits in. What it cannot check is the account
// closure path, and the honest reason is that the path does not exist either:
// the schema allows `accounts.status = 'closed'` and NO code in the crate sets
// it. When closure is implemented, the retention rows must be revisited with it.
test('nothing deletes a review, so the retention rows must not promise that it does', () => {
  // COMMENTS ARE STRIPPED FIRST, and the first run of this test is why. The scan
  // below looks for a `DELETE FROM reviews`, and the doc-comment recording the
  // absence NAMES the statement it is denying ("There is no `DELETE FROM reviews`
  // anywhere in server/src"). A raw text scan cannot tell a promise from a
  // sentence that quotes one in order to say it does not exist, so the check
  // fired on its own explanatory comment. The fix is to look only at executable
  // text, which is what the claim is about.
  const raw = rustFiles(join(REPO, 'server', 'src'))
    .map((f) => readFileSync(f, 'utf8'))
    .join('\n');
  const executable = stripComments(raw);
  assert.ok(executable.length > 1000, 'server/src was not read, so this guard is vacuous');
  assert.ok(
    executable.length < raw.length,
    'stripping comments did not remove anything, so this check is not reading executable text and the guards below are unproven',
  );

  // The absence itself, in every spelling a deletion could take.
  const deletes = executable.match(/DELETE\s+FROM\s+reviews?\b/gi) ?? [];
  assert.equal(
    deletes.length,
    0,
    `server/src now deletes from \`reviews\` (${deletes.join(', ')}). That is a real retention path, so website/src/lib/privacy.ts, docs/data-retention.md:75 and server/src/db.rs:551 must be rewritten to state the window it produces and this test deleted. docs/data-retention.md promises a body is "cleared if requested" when an account closes, which is the closest thing to a documented path - check whether that is what landed.`,
  );
  assert.ok(
    !/DELETE\s+FROM\s+review_history\b/i.test(executable),
    'server/src now deletes from `review_history` directly. The table was documented as bounded only by its `review_id ... ON DELETE CASCADE`, so a direct delete is a second, undocumented retention path.',
  );

  // The withdrawal path is a FLAG. If this ever becomes a delete, "withdraw" and
  // "delete" have been conflated and the account's one slot is freed by an act
  // that was supposed to keep it occupied.
  const withdraw = executable.match(/UPDATE reviews SET withdrawn_at = \?[^"]*/);
  assert.ok(
    withdraw !== null,
    'the withdrawal UPDATE was not found in server/src, so the check that it is a flag rather than a delete is not reading the code it was written for',
  );

  // The three documents, checked as text because they are three transcriptions of
  // one policy and the whole defect was that they agreed with each other and with
  // nothing else.
  const documents: Array<[string, string]> = [
    [join(SRC, 'lib', 'privacy.ts'), readFileSync(join(SRC, 'lib', 'privacy.ts'), 'utf8')],
    [join(REPO, 'docs', 'data-retention.md'), readFileSync(join(REPO, 'docs', 'data-retention.md'), 'utf8')],
    [join(REPO, 'server', 'src', 'db.rs'), readFileSync(join(REPO, 'server', 'src', 'db.rs'), 'utf8')],
  ];

  let corrected = 0;
  for (const [name, text] of documents) {
    // Look for the phrase as a PUBLISHED VALUE - `keep: 'Until deleted by user'`,
    // a table cell, or a doc-comment line that asserts it - and not in prose that
    // quotes it in order to say it was wrong. The correction comments all quote the
    // retired phrase, exactly as they should, and a plain substring search fires on
    // that. A sentence saying a claim is false is not itself the claim.
    //
    // The second pattern is the one that matters: it is the shape the claim had in
    // `db.rs`, a bullet stating the window as fact. Matching that, and not merely
    // mentioning the words, is the difference between checking the policy and
    // checking the vocabulary.
    assert.ok(
      !/keep:\s*'[^']*Until deleted by user/i.test(text) &&
        !/\|\s*Until deleted by user\s*\|/i.test(text) &&
        !/`review_history`\s*[—-]\s*kept until the user deletes/i.test(text),
      `${name} still publishes "Until deleted by user" for reviews, which describes a deletion nothing implements. Nothing in server/src issues a DELETE against \`reviews\`.`,
    );
    // Each must state that the row is kept without a user-triggered delete. The
    // wordings differ deliberately - a privacy page, a policy table and a
    // doc-comment should not read identically - so the patterns are per-meaning
    // rather than one string, and the count below is what holds all three to it.
    if (/kept indefinitely|forever, unless the account is closed|kept \*\*indefinitely\*\*/i.test(text)) {
      corrected++;
    } else {
      assert.fail(
        `${name} no longer states that a review is kept indefinitely. The correction has to land in ` +
          `website/src/lib/privacy.ts, docs/data-retention.md and server/src/db.rs together, or the policy ` +
          `is published differently depending on where a reader looks - which is how the original claim ` +
          `survived three copies agreeing with each other.`,
      );
    }
  }
  assert.equal(
    corrected,
    3,
    `only ${corrected} of the three transcriptions state that a review is kept indefinitely. The correction has to land in all three or the policy is published differently depending on where a reader looks - which is how the original claim survived three copies agreeing with each other.`,
  );
});
