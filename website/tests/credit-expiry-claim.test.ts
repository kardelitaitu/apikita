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
