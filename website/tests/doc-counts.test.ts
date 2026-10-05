// The prose test-count claims must match the suite that actually ran.
//
// WHY THIS EXISTS. Five documents state a live count - README.md, server/README.md,
// website/README.md, docs/website/README.md, docs/ci-cd.md - and two of them say,
// in as many words, "Count them rather than recalling them ... both numbers move
// whenever a page or a contract test lands." That instruction was addressed to a
// human, so the numbers drifted anyway: the website's own README claimed 146 when
// it was 157, and server/README.md claimed 338 when it was 427.
//
// A number a reader is expected to trust has to be checked by something other than
// recall. This reads the stated figures out of the docs and compares them to the
// WEBSITE count this run measured (the runner reports it), and to a server count
// the caller passes in - see the note below on why the server figure is not read
// from here.
//
// WHY THE SERVER COUNT IS PASSED IN, NOT MEASURED HERE. Running cargo test from a
// node test would recurse (cargo test runs its own suite) and would make the
// website suite depend on a Rust toolchain. So this file pins the WEBSITE figure
// itself, and pins the CONSISTENCY of the server figure across every doc that
// states it - they must all agree with each other and with SERVER_TESTS below.
// Bumping SERVER_TESTS is part of landing a server test; the point is that one
// edit cannot update four documents and miss the fifth.
//
// WHAT THIS FIGURE DOES *NOT* COUNT, measured rather than assumed, because all four
// claim sites name `cargo test --lib` and a reader could easily take 667 for the
// whole suite. It is not: `cargo test` builds SEVEN test targets and the library is
// only the first. The five `src/bin` targets hold 31 further tests - `hold-sweep`
// 17, `migrate` 9, `usage-purge` 3, `ip-purge` 2, `benchmark` 0 - so the bare
// command reports 698 across its eight `test result:` lines while this constant
// stays 667. The scoping is deliberate: every claim is phrased "`cargo test --lib`",
// so 667 is the right number FOR WHAT IS CLAIMED.
//
// The asymmetry that leaves: `cargo test --lib -- --list` cannot see the bin
// targets, so DELETING one of those 31 tests lowers nothing any guard reads.
// MEASURED: commenting out an `ip-purge` test leaves this check passing at
// "667 vs 667". A bin-test FAILURE is still caught - CI runs the bare `cargo test`
// and it exits non-zero - so the uncovered direction is only a silent deletion, and
// the practical exposure is limited because those binaries are exercised
// independently: CI's maintenance smoke step drives `hold-sweep` against a seeded
// stranded hold and requires it to NAME the ref, and `migrate` appears in 17 tool
// scripts. This paragraph exists so a reader knows the boundary of the number
// instead of inferring coverage the constant does not provide.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', '..');
const read = (p: string) => readFileSync(join(root, p), 'utf8');

/**
 * An exported function in `src/lib/` that NOTHING calls is dead weight with a doc comment claiming a
 * job it does not have.
 *
 * WHY THIS EXISTS. `admin.ts` exported `actionLabel(action)`, documented as "the verb to put on the
 * button for the action a given status allows". It was never called: the admin island hardcodes
 * "Suspend account" and "Resume account" in its own markup, MEASURED by three independent methods -
 * an exhaustive grep (its only occurrence was its own definition), `tsc --noEmit` (which does not
 * flag unused EXPORTS, only unused locals), and the built output, where the bundler had tree-shaken
 * the name away entirely.
 *
 * A mutation of that function SURVIVED the whole suite, and the right reading was not "add a test":
 * there is no behaviour to test, because nothing runs it. The copy an operator reads comes from the
 * island's markup, and a second source of that copy - one that is never rendered - is a place where
 * the two can silently disagree.
 *
 * WHAT IT DOES NOT DO. It counts CALLS, not references, so a name appearing in a doc comment or a
 * string does not save it, and a function called only from a TEST counts as called. That last part is
 * deliberate: a helper the suite exercises is not dead, and this check must not argue with the tests
 * about it.
 *
 * COMMENTS ARE STRIPPED BEFORE COUNTING, and that is not tidiness - the first version of this check
 * MISSED ITS OWN SUBJECT because of them. `actionLabel`'s two occurrences were its definition and a
 * MENTION IN THIS PARAGRAPH; the count saw one non-definition hit and called it live, while
 * `errorRateSeverity` and `resendVerification` were flagged correctly. A doc comment that discusses a
 * function must not be able to keep it alive, or the check is defeated by whoever documents it.
 */
test('every exported function in src/lib is called somewhere', () => {
  /** Line comments, block comments and doc comments removed, so only CODE is counted. */
  const stripComments = (text: string): string =>
    text
      .replace(/\/\*[\s\S]*?\*\//g, '')
      .replace(/^\s*\/\/.*$/gm, '')
      .replace(/([^:])\/\/.*$/gm, '$1');

  const libDir = join(root, 'website', 'src', 'lib');
  const sources: Array<[string, string]> = [];
  const collect = (dir: string): void => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) collect(full);
      else if (/\.(ts|astro)$/.test(entry.name)) {
        sources.push([full, stripComments(readFileSync(full, 'utf8'))]);
      }
    }
  };
  collect(join(root, 'website', 'src'));
  sources.push(
    ...[join(root, 'website', 'tests')].flatMap((d) =>
      readdirSync(d)
        .filter((f) => f.endsWith('.ts'))
        .map((f): [string, string] => [join(d, f), stripComments(readFileSync(join(d, f), 'utf8'))]),
    ),
  );

  const exported: Array<[string, string]> = [];
  for (const [file, text] of sources) {
    if (!file.includes(`${'lib'}`)) continue;
    for (const m of text.matchAll(/^export (?:async )?function ([A-Za-z0-9_]+)/gm)) {
      exported.push([file, m[1]]);
    }
  }

  const uncalled: string[] = [];
  for (const [file, name] of exported) {
    // Count every mention across src AND tests, then subtract the DEFINITION itself. One mention per
    // definition line, so a name with no other occurrence is uncalled. Counting this way rather than
    // "is it imported" is deliberate: an import of a module that never names the function would
    // otherwise mark the whole module live.
    let mentions = 0;
    for (const [, text] of sources) {
      mentions += [...text.matchAll(new RegExp(`\\b${name}\\b`, 'g'))].length;
    }
    const definitions = sources.reduce(
      (n, [, text]) =>
        n + [...text.matchAll(new RegExp(`export (?:async )?function ${name}\\b`, 'g'))].length,
      0,
    );
    if (mentions - definitions === 0) {
      uncalled.push(`${file.replace(root, '').replace(/\\/g, '/')}: ${name}()`);
    }
  }

  assert.deepEqual(
    uncalled,
    [],
    'these src/lib exports are never called anywhere. An exported function with no caller still ' +
      'carries a doc comment describing behaviour, so a reader trusts copy that nothing renders. ' +
      'TWO DIFFERENT THINGS LAND HERE and the remedy differs: (a) DEAD WEIGHT - a redundant wrapper ' +
      'whose work is already done elsewhere, which should be deleted; (b) MISSING WIRING - a ' +
      'function whose SERVER ROUTE exists but which no page calls, which is a product gap to land, ' +
      'not code to remove. Check which one you have before deleting. Sites: ' +
      `${uncalled.join(', ')}`,
  );
});

/**
 * The website suite's count. A LITERAL, despite what this comment used to say.
 *
 * It said "as the runner just reported it", and it never did: the runner reports the count
 * to the shell, and a test cannot read another run's output without running it, which is the
 * recursion the note above is about. So it is a hand-kept number wearing the clothes of a
 * measurement, and that matters more than being wrong, because the comment tells the next
 * person there is nothing to update.
 *
 * WHY THIS ONE STILL CANNOT BE MEASURED, while the server count below now is. The server count comes
 * from `cargo test --lib -- --list`, which ENUMERATES without executing. The website suite has no
 * equivalent, and a STATIC COUNT OF THE SOURCE IS NOT ONE: MEASURED, a scan for `test(` calls across
 * these twenty-nine files finds 187 where the runner reports 204.
 *
 * Seventeen tests exist only at RUNTIME, and `website/tests/error-model.test.ts` is the reason to
 * stop looking for a scanner that would find them:
 *
 *     for (const c of CASES) {
 *       test(`${c.name}: ${c.view.message}`, async () => { ... });
 *     }
 *
 * One call site, one test per case in a list that grows whenever a status code is pinned. No
 * line-based count can produce 204 from that, and a count derived from the list length would be a
 * second hand-kept number rather than a measurement - the same defect wearing different clothes.
 *
 * So this figure is updated WITH the run that changes it. That is a smaller guarantee than the server
 * count now carries, and it is stated rather than implied.
 */
const WEBSITE_TESTS = 221;

/**
 * The server count, and the same kind of literal for the same reason.
 *
 * Update WITH the run that changes it. Both numbers were bumped together on 2026-09-30,
 * when the identity port finally made them stale in the same direction: the server went
 * 509 -> 556 because Phase 6 added the native auth routes and their tests, and the website
 * went 168 -> 171 with the auth-page migration. THE PREVIOUS NOTE SAID THEY WERE "CURRENTLY
 * BEHIND" and left it there, which is the one state this guard cannot detect: it pins the
 * documents to a number, and if the number itself is wrong then every document agrees on
 * something false and the suite stays green.
 *
 * That is the design working as intended rather than a broken guard: this file exists so
 * that ONE edit cannot update four documents and miss the fifth, and the cost of that is
 * that a stale number here is propagated consistently instead of being corrected
 * independently in five places. Bumping these is five documents and two constants, which is
 * why it has not been done opportunistically and why it should be done deliberately.
 */
const SERVER_TESTS = 673;

/** Every doc that states the server count, and the exact text it must carry. */
const SERVER_CLAIMS: Array<[string, string]> = [
  ['README.md', `server: **${SERVER_TESTS} tests**`],
  ['server/README.md', `**${SERVER_TESTS} passed / 0 failed / 0 ignored**`],
  ['docs/ci-cd.md', `**${SERVER_TESTS} passed / 0 failed /`],
  // THE WORKFLOW COUNTS TOO, and it was the one file this list missed. It carried
  // "623 passed (measured 2026-09-30)" through several bumps, because a CI file reads like
  // configuration rather than like a document - but it is a document, it states the figure, and
  // it is the file a contributor opens to find out how to run the suite. A number nobody checks
  // is the defect this whole test exists for; leaving out the one place CI describes itself is
  // how it kept a figure three bumps behind.
  ['.github/workflows/ci.yml', `${SERVER_TESTS} passed / 0 failed / 0 ignored`],
];

/** Every doc that states the website count, and the exact text it must carry. */
const WEBSITE_CLAIMS: Array<[string, string]> = [
  ['README.md', `website: **${WEBSITE_TESTS} tests**`],
  ['website/README.md', `passes **${WEBSITE_TESTS} tests**`],
  ['docs/website/README.md', `**${WEBSITE_TESTS} tests**`],
];

/**
 * The exact strings that are LIVE, so the retired-count guard can tell a figure that
 * has been superseded from one that is merely the OTHER suite's current count.
 *
 * Built from the two constants and the claim shapes above, so it cannot go stale on
 * its own: bumping a constant updates this set in the same edit.
 */
const LIVE_FORMS: ReadonlySet<string> = new Set([
  `${SERVER_TESTS} tests`,
  `${SERVER_TESTS} passed`,
  `${SERVER_TESTS} / 0 / 0`,
  `server: **${SERVER_TESTS} tests**`,
  `${WEBSITE_TESTS} tests`,
  `${WEBSITE_TESTS} passed`,
  `website: **${WEBSITE_TESTS} tests**`,
]);

test('every doc states the measured server test count', () => {
  for (const [file, needle] of SERVER_CLAIMS) {
    assert.ok(
      read(file).includes(needle),
      `${file} must state "${needle}" (the suite reports ${SERVER_TESTS})`,
    );
  }
});

/**
 * THE LINK THE CHAIN WAS MISSING: `SERVER_TESTS` itself, against the suite.
 *
 * Every other test in this file compares a DOCUMENT to the constant. None compared the constant to
 * reality, so a stale figure propagated consistently - which is the design working for the docs and
 * failing for the number. MEASURED twice in four rounds: the constant read 656 while the suite had
 * 658, then 658 while it had 659. Both times every doc agreed with the constant and the suite was
 * green, which is exactly the state the comment above calls "the one state this guard cannot detect".
 *
 * WHY A LITERAL WAS EVER RIGHT, AND WHY IT IS NOT ANY MORE. The header says the count is passed in
 * because running `cargo test` from a node test would recurse. That is true of running. It is NOT
 * true of LISTING: `cargo test --lib -- --list` enumerates the tests and executes none of them, so
 * nothing recurses and the website suite is not re-entered. MEASURED: 624 ms against 53 s for a real
 * run, and a node test that shells out to it returns the right figure with no recursion.
 *
 * So the number is now checked by a measurement rather than by recall - which is the standard this
 * file set for every document and had never applied to itself.
 *
 * WHAT IT DOES WHEN CARGO IS ABSENT. It SKIPS rather than passes. A contributor without a Rust
 * toolchain, or a `cargo` that fails to start, must not be told the count is correct: that is the
 * same fail-open this repository writes guards against. The skip is loud and says what it did not
 * check.
 */
test('SERVER_TESTS is the count the library suite actually has', (t) => {
  let listed = '';
  try {
    listed = execFileSync('cargo', ['test', '--lib', '--', '--list'], {
      cwd: join(root, 'server'),
      encoding: 'utf8',
      timeout: 600_000,
      stdio: ['ignore', 'pipe', 'pipe'],
    });
  } catch (err) {
    t.skip(`cargo test --lib -- --list could not run here (${(err as Error).message.split('\n')[0]}), so the count was NOT verified`);
    return;
  }

  const summary = listed.match(/(\d+) tests?, \d+ benchmarks?/);
  assert.ok(
    summary,
    `cargo test --lib -- --list printed no "N tests, M benchmarks" summary, so the count was not read. First 300 chars: ${listed.slice(0, 300)}`,
  );

  const listed_count = Number(summary[1]);
  // A LISTING CANNOT BE ZERO. A floor, because a build that failed, or a target that was renamed,
  // would print "0 tests" and this assertion would otherwise read that as agreement with a constant
  // that had also gone to zero.
  assert.ok(
    listed_count > 600,
    `the library suite listed only ${listed_count} tests, which is far below this repository's size - the listing did not read the real suite`,
  );

  assert.equal(
    SERVER_TESTS,
    listed_count,
    `SERVER_TESTS is ${SERVER_TESTS} and the library suite has ${listed_count} tests. The four documents below this constant now agree with a figure that is wrong, because every check in this file compares a document to the CONSTANT and none compared the constant to the SUITE. Bump SERVER_TESTS and the four SERVER_CLAIMS sites together`,
  );
});

test('every doc states the measured website test count', () => {
  for (const [file, needle] of WEBSITE_CLAIMS) {
    assert.ok(
      read(file).includes(needle),
      `${file} must state "${needle}" (the suite reports ${WEBSITE_TESTS})`,
    );
  }
});

test('no doc still claims a superseded count', () => {
  /**
   * A retired figure is a count IN THE SPACE OF ONE SUITE, and the two suites share
   * that numeric space.
   *
   * This was a flat string list. It worked while the two counts were far apart, so a
   * single integer identified which suite it had been retired from. It stopped working
   * when the round-23 case-set guard retired website 172 and, in the same commit, the
   * server count rose to 582: README.md:102 states BOTH counts on one line
   * (`server: **582 tests** — website: **174 tests**`), so a bare `'582 tests'` matches
   * a line that is required to be there, and the guard demands a document not state the
   * number SERVER_CLAIMS above requires it to state. The first fix skipped any entry
   * matching a live count - which worked, and was wrong: it also skipped `'174 tests'`,
   * a genuinely retired website figure that README.md:102 is exactly the line most
   * likely to regress to.
   *
   * So the association is now explicit. Each entry names the suite it was retired from
   * and carries that suite's QUALIFIED form - `server: **N tests**` or
   * `website: **N tests**` - which are the two shapes SERVER_CLAIMS and WEBSITE_CLAIMS
   * already write and which cannot collide, whatever the integers do. The bare forms are
   * kept as well, but a BARE FORM IS ONLY CHECKED AGAINST THE DOCUMENTS WHOSE SUBJECT IT
   * IS: a bare server figure cannot be asserted against website/README.md, because that
   * file has no business stating a server count, and asserting it there is how the
   * collision happened in the first place.
   */
  type Retired = { suite: 'server' | 'website'; forms: string[] };
  const superseded: Retired[] = [
    {
      suite: 'server',
      forms: [
        '391 tests', '163 tests', '161 tests', '160 tests', '157 tests', '146 tests',
        '338 passed', '134 passed',
        // The pair bumped on 2026-09-30. Without these two the guard only stops a
        // number that was stale when it was WRITTEN from coming back; a number it
        // has itself retired could reappear in one document unnoticed, because
        // every check above asserts the CURRENT constant is PRESENT somewhere and
        // none of them asserts a retired one is ABSENT.
        '509 tests', '168 tests',
        // Retired on 2026-09-30 when the password-change and provider-list tests
        // landed. 556 is the figure the Phase 6 register entry still names as the
        // count AT that phase, which is why it is retired here and annotated rather
        // than rewritten in docs/decisions.md.
        '556 tests', '556 passed', '556 / 0 / 0',
        // Retired within the same session as the bump above, when the test for a
        // password change clearing outstanding reset links landed.
        '565 tests', '565 passed',
        // Retired when the account-lifecycle tests landed: signup answered a known
        // address differently from an unknown one, the reset pair and resend were
        // undriven, and google_sign_in had no test at all. THIRTEEN tests over the
        // five handlers that carry a new customer from an address to a working
        // sign-in, and back in again when the password is lost.
        '566 tests', '566 passed', '566 / 0 / 0',
        // Retired when the route-list guard landed: it compares the `routes!` macro
        // invocation inside `create_router` against the `ROUTES` inventory, which the
        // sibling check reads. It is the one test that catches a route added to the
        // router and to MOUNTED while `ROUTES` was left alone - a 404 for a path the
        // inventory does not admit exists.
        '579 tests', '579 passed', '579 / 0 / 0',
        // Retired when the retention-sweep gaps were closed. `identity::tokens::purge_expired`
        // had a unit test and NO caller of any kind, so expired links were never deleted; the
        // maintenance entrypoint - the thing that actually runs, since `bin/usage-purge.rs` is
        // not shipped - never mentioned `identity_tokens` OR `link_code_issues` at all, while
        // the Rust sweep deleted from both. One test now drives each end of that pair, and a
        // shell guard compares the two table lists so neither can drift alone again.
        '580 tests', '580 passed', '580 / 0 / 0',
        // Retired when the quickstart's error-code table was pinned to the code. The guard
        // found a real drift on its FIRST run: the page published 14 rows against the
        // contract's 15, so `403 forbidden` - which admin.rs::forbidden_response emits - had
        // no row for a developer to look it up in. The page is a third transcription of the
        // same table, and it was the only one nothing read.
        '581 tests', '581 passed', '581 / 0 / 0',
        // Retired when the Telegram link-code purge landed. `docs/data-retention.md:77` has
        // always published "Until used or expired + 24h" for a link code, and the Rust sweep
        // listed `link_codes` among the tables it "deliberately does NOT touch" - an exception
        // written when it was true and left standing after a later round brought an
        // expires-then-delete table into the same sweep. The only delete was the ONE code
        // `issue_link_code` supersedes, so a code requested and never redeemed stayed on disk
        // forever. The new test seeds FOUR rows so it pins the window from both sides: a
        // terminal row 25 hours back (must go), a redeemed one whose `expires_at` is still in
        // the future (must go, and only `used_at` finds it), one terminal two hours ago (must
        // STAY, and this is the row that makes a zero-day grace fail), and one still live.
        '582 tests', '582 passed', '582 / 0 / 0',
        // Retired when `GET /api/reviews/mine` started returning a withdrawn review. The query
        // filtered `withdrawn_at IS NULL`, so an account that had withdrawn was told
        // `has_review: false` - that it had never written anything - while its row sat in the
        // table still occupying its one slot, and `withdrawn` was hard-coded `false` on every
        // reachable path, making the field incapable of being true. Two documents stated the
        // real contract and neither was checked: docs/telegram/README.md:305 ("remain visible
        // to" the author) and the response shape at docs/server/api-spec.md:540. The new test
        // asserts all four facts at once - has_review true, withdrawn true, body returned,
        // rating returned - and then that the public aggregate still excludes it.
        '602 tests', '602 passed', '602 / 0 / 0',
        // Retired when the review-retention windows were corrected. Three files published the
        // same row - website/src/lib/privacy.ts, docs/data-retention.md and server/src/db.rs -
        // and none was true: there is no `DELETE FROM reviews` anywhere in server/src, and
        // `reviews.account_id` is `ON DELETE SET NULL`, so a review outlives its author and
        // stays in the public aggregate attached to nobody. The correction also narrowed
        // `doc_claims.rs`, which had been forbidding the PHRASE "kept indefinitely" across the
        // whole privacy file - a proxy for one bad row that rejected a different, honest one.
        '603 tests', '603 passed', '603 / 0 / 0',
        // Retired when `send_link_mail` started addressing the mail it sends. It built every
        // verification and reset message with `to: String::new()` - an empty recipient, with
        // the address in scope and unused - so `EmailSender::send` failed to parse the mailbox
        // and returned `EmailError::Build` before dialling anything. The caller logs that
        // against the account id and never the address, so it read like a relay problem. No
        // verification mail and no reset mail had ever been sent, and password reset is the
        // only recovery path. Every test of the mailer built its own `Email` with a written-out
        // address, so the module was covered and the route was not; the guard for it was itself
        // vacuous at first, and the mutations said so.
        '605 tests', '605 passed', '605 / 0 / 0',
        // Retired when `list_keys` started publishing `token_limit`. `ApiKeyDto` omitted the
        // field and the SELECT did not read the column, while `CreateKeyRequest` and
        // `UpdateKeyRequest` both accept it. The dashboard's edit form submits the WHOLE field
        // set on every save rather than a diff - deliberately, so that clearing a limit is
        // expressible - so a field the response omits is submitted as its blank default, and a
        // blank limit parses to 0, which means UNLIMITED. The form also never wrote the
        // `e-token` input, and hardcoded `token_limit: 0` into `before`, so it could not even
        // warn: renaming a key silently raised its token ceiling to no ceiling. The account
        // export read `token_limit` from the start, which is part of why the omission looked
        // deliberate. Both halves are pinned now.
        '606 tests', '606 passed', '606 / 0 / 0',
        // Retired when `logout_all` was made to judge its caller by the whole session
        // rule. Its lookup filtered on `revoked_at IS NULL AND expires_at > ?` only, so an
        // IDLE-but-unexpired session was accepted there and refused everywhere else - and
        // could then revoke every other device on the account. That is a denial of service
        // against a customer, available to anyone holding a token they can no longer use.
        // No existing test could see it: `add_live_session` always stamps
        // `last_seen_at = now`, so every session any test built was freshly touched, and
        // the fixture made the defect unreachable. The auth tests' own copy of the resolver
        // had the same omission while its comment claimed production parity.
        '607 tests', '607 passed', '607 / 0 / 0',
        // Retired when the doc-comment method-citation guard landed. `server/src/config.rs`
        // named a `from_config` constructor on `EmailSender` at four sites and
        // `EmailSender::send` at three more; the constructor is `new`, `send` takes an
        // already-built message and reads no config, and nothing called `from_config` ever
        // existed. A reader following one of those names finds nothing and concludes the
        // field is UNUSED, which is the opposite of what the comment was for. The first
        // version of the guard then reproduced the same failure inside itself: it rejected
        // any type preceded by `::`, so it extracted nothing from a QUALIFIED citation -
        // `identity::email::EmailSender::new`, the form this codebase actually writes - and
        // reported success over an empty set. The mutation sweep caught it by seeding a
        // citation of a method that does not exist and finding the suite still green; a
        // surviving mutant is evidence about the check before it is evidence about the anchor.
        '608 tests', '608 passed', '608 / 0 / 0',
        // Retired with credit expiry. Three separate rounds moved this number and only one
        // of them was the feature: 609 -> 610 was `account_for_email` gaining its ORDER BY
        // (`identities_provider_email_uniq` is UNIQUE (provider, email), so one address
        // really can resolve to two accounts, and the pre-fix query answered that state in
        // rowid order); 610 -> 611 was the SIGNUP GATE, the half the ORDER BY did not fix -
        // signup read "an account exists" as "already registered", so a Google-only address
        // became unregisterable for a password user, and the gate had to ask
        // `password_identity` instead; and the rest was the credit-expiry sweep and its
        // guards. Two lessons recorded here because they cost real time: the first mutation
        // of the signup gate did not COMPILE (`password_identity` returns a struct,
        // `account_for_email` an Option<Uuid>), and a mutation that does not compile is not
        // evidence; and a parallel-run failure that passes in isolation is about shared
        // state, not about the test - here the fixture accumulated three deposits through
        // the one-argument `fund_through_topup`, whose whole job is asserting the balance
        // equals that single deposit.
        '609 tests', '609 passed', '609 / 0 / 0',
        // Retired with the credit-expiry migration's own conformance check. The two new
        // columns were added as bare `TEXT`, and `tools/sqlite-probes/
        // validate-migration-schema.py` refused them: every timestamp in the schema carries
        // a GLOB format CHECK, matched by name suffix, and these were the only two dates
        // that would have accepted any string. SQLite cannot add a TABLE-level constraint
        // by ALTER, but it can add a column-level one - verified before writing it, since
        // the alternative is a migration that fails at run time. 622 was the same commit
        // that inverted the credit-expiry claim test: it had pinned the ABSENCE of the
        // mechanism and carried its own exit instructions, so implementing the feature
        // meant rewriting it to pin the presence and the four caveats the ToS section now
        // states rather than deleting it.
        '621 tests', '621 passed', '621 / 0 / 0',
        // Retired with the sign-in body guard, which is also where three payload fields
        // nothing read were removed, and the sharpest lesson of the three is in how the
        // guard was FIRST written. It failed on two assertions and both were the test's
        // fault, not the code's: the route slice ran to END OF FILE on the reasoning that
        // "the assertions are about what does NOT appear, so a wider window can only make
        // them stronger" - which is backwards. It swept in the `#[cfg(test)]` module,
        // whose ledger-drift helper legitimately selects `w.balance_idr`, and reported
        // the route for reading a column only a test reads; and the spec assertion
        // matched a `\n` regex against a file stored with CRLF, so it never matched and
        // blamed a spec that already said the right thing. A wider window is not a
        // stronger claim, it is a wider one. The third wrong trial was the mutation
        // rather than the test: it prepended the regression to the handler's DECLARATION,
        // which the slice begins at, so the injected code landed outside the region under
        // test and survived - and a surviving mutant is evidence about the check only
        // after the mutation has been shown to be a real regression.
        '622 tests', '622 passed', '622 / 0 / 0',
        // Retired on 2026-10-01, and this one is worth the note because it was the exact
        // state the constant's own comment warns about: 623 was never bumped as three
        // server tests landed (`cced211` added one for the sweep against an in-flight
        // reservation, `b52bccb` and the expiry-ordering test added the others), so the
        // constant, README.md, server/README.md and docs/ci-cd.md all agreed on a number
        // that was three behind - and the suite stayed GREEN, because every assertion
        // here compares a document to THIS constant and none compares it to the suite.
        // Bumped to 626 alongside the new expiry-attribution test.
        '623 tests', '623 passed', '623 / 0 / 0',
        // Retired the round after, when `credit_expiry_instant` finally got a test. It had
        // none at all, while its doc-comment carried two worked examples - and both were
        // misleading (the clamp illustrated with a one-month span when the shipped setting
        // is 24, and the calendar-month argument demonstrated on 15 March, a date where
        // months and days AGREE). The behaviour was correct; nothing checked it.
        '626 tests', '626 passed', '626 / 0 / 0',
        // Retired the round after, when the two weak reconciliation helpers were brought
        // back into agreement with the shipped gate. `ledger_drift_rows` exists in three
        // files; `db.rs` had been corrected to a FULL OUTER JOIN and `auth.rs` and
        // `account.rs` had not, so those two reported zero drift for an account whose
        // ledger holds money and whose wallets row is missing - the case the schema
        // permits and the gate catches. The new test pins the helper directly.
        '627 tests', '627 passed', '627 / 0 / 0',
        // Retired the round after, when the SAME weakness was found in four more copies of
        // the same rule - `drift_rows` in admin.rs, keys.rs, proxy.rs and webhooks.rs. The
        // previous round fixed the copies it found; this one searched for the class
        // (duplicated helpers whose doc cites a document) and found the rest. Three of the
        // four are still unpinned by any test, which is recorded in those files.
        '628 tests', '628 passed', '628 / 0 / 0',
        // Retired together, three rounds of tests landing without a bump. The gap is the
        // point: the constant was 629 while the suite was 632, and every document agreed
        // with the constant, so the guard stayed green on a number nothing measured.
        '629 tests', '629 passed', '629 / 0 / 0',
        '630 tests', '630 passed', '630 / 0 / 0',
        '631 tests', '631 passed', '631 / 0 / 0',
        '632 tests', '632 passed', '632 / 0 / 0',
        '633 tests', '633 passed', '633 / 0 / 0',
        '636 tests', '636 passed', '636 / 0 / 0',
        '637 tests', '637 passed', '637 / 0 / 0',
        '638 tests', '638 passed', '638 / 0 / 0',
        '639 tests', '639 passed', '639 / 0 / 0',
        '641 tests', '641 passed', '641 / 0 / 0',
        '643 tests', '643 passed', '643 / 0 / 0',
        '645 tests', '645 passed', '645 / 0 / 0',
        '646 tests', '646 passed', '646 / 0 / 0',
        '647 tests', '647 passed', '647 / 0 / 0',
        '644 tests', '644 passed', '644 / 0 / 0',
        // Retired when the citation guard landed, then when the reservation guard's two call
        // sites were pinned. 648 was never published on its own - it is recorded here so the
        // guard covers the count the suite passed through rather than only the ones a doc
        // happened to name, and so a document carrying it cannot reappear unnoticed.
        '648 tests', '648 passed', '648 / 0 / 0',
        '649 tests', '649 passed', '649 / 0 / 0',
        '650 tests', '650 passed', '650 / 0 / 0',
        '651 tests', '651 passed', '651 / 0 / 0',
        '652 tests', '652 passed', '652 / 0 / 0',
        '653 tests', '653 passed', '653 / 0 / 0',
        '654 tests', '654 passed', '654 / 0 / 0',
        '655 tests', '655 passed', '655 / 0 / 0',
        '656 tests', '656 passed', '656 / 0 / 0',
        // 657 was passed through between the two constants and never published on its own;
        // recorded for the same reason 648 was, so a document carrying it cannot reappear.
        '657 tests', '657 passed', '657 / 0 / 0',
        '658 tests', '658 passed', '658 / 0 / 0',
        '659 tests', '659 passed', '659 / 0 / 0',
        '660 tests', '660 passed', '660 / 0 / 0',
        // 662 and 663 were passed through in one commit, which added three tests at once, and
        // neither was published on its own. Recorded for the same reason 648 and 657 were: a
        // document carrying a count the suite passed through must not be able to reappear.
        '662 tests', '662 passed', '662 / 0 / 0',
        '663 tests', '663 passed', '663 / 0 / 0',
        '664 tests', '664 passed', '664 / 0 / 0',
        '665 tests', '665 passed', '665 / 0 / 0',
        '666 tests', '666 passed', '666 / 0 / 0',
        '661 tests', '661 passed', '661 / 0 / 0',
      ],
    },
    {
      suite: 'website',
      forms: [
        // Retired when the doc-denial guard landed below: it is one test, and it is the
        // one that would otherwise let a document deny a gate CI runs.
        'website: **171 tests**', 'passes **171 tests**', '**171 tests**',
        // Retired when the error-code CASE SET was pinned to the published contract. Every
        // test in error-model.test.ts drives a code typed into the test, so none of them
        // noticed a code with no `case` in describeError: it falls into `default` and renders
        // "Something was wrong with that request." for a specific, actionable failure. Two
        // tests now compare the case set to the document as a SET, in both directions.
        'website: **172 tests**', 'passes **172 tests**', '**172 tests**',
        // Retired when the citation guard and the shadow guard landed. The citation
        // guard was written because `website/src` cites code by line in nineteen
        // places and doc_claims.rs's citation triage walks `docs/` only - so nothing
        // verified any of them. Ten of the eleven that were checked by hand had
        // drifted onto a login example, a closing brace, a blank line. The shadow
        // guard was written because wallet.astro imported `topupPerHour` and then
        // declared `const topupPerHour = 5;` underneath it, so prices.test.ts
        // compared the CONSTANT against config while the page published the literal.
        'website: **174 tests**', 'passes **174 tests**', '**174 tests**',
        // Retired when the claim guards landed: credit expiry is promised in the present
        // tense on two pages and NOTHING implements it - the schema holds one un-aged
        // `wallets.balance_idr`, so expired and live credit are not even distinguishable
        // (docs/decisions.md:69). The guard pins that absence and makes the three documents
        // that record it fail if their warning is dropped. Its sibling pins the numbers the
        // pages state against the config and lib constants that own them: nine copies of
        // the password floor across three pages, and three different "N seconds" figures
        // belonging to three different mechanisms.
        'website: **180 tests**', 'passes **180 tests**', '**180 tests**',
        // Retired when the server-only figures were pinned. These four numbers have no
        // config key - they exist only as a literal inside a Rust or shell function, and
        // the website restated each one in a TypeScript constant that nothing compared
        // to its source. Every test that touched those constants was circular: admin.test.ts
        // asserted `ERROR_RATE_THRESHOLD === 0.05` beside the line defining it, and
        // recent-usage.test.ts interpolated `RECENT_USAGE_LIMIT` into its own expected
        // path, so both would hold for any value. A dashboard asking for 50 rows from an
        // endpoint defaulting to 20 renders a short list and a "load more" that never
        // fires, with the suite green.
        'website: **184 tests**', 'passes **184 tests**', '**184 tests**',
        // Retired when the retention periods the privacy page publishes were pinned. Every
        // row of `website/src/lib/privacy.ts`'s `retention` array states a period, and eight
        // of those periods are numbers that also exist elsewhere as the thing that actually
        // expires the data - a `pub const` in the sweep, or a config TTL. The Rust constants
        // were pinned by db.rs's own tests and the page's strings were pinned by
        // privacy.test.ts as NON-EMPTY, and nothing read both. Raising `usage_events` to 180
        // days would have kept every existing test green while the page still told a
        // customer 90. Its first run settled a unit question too: the page says "24 months"
        // for `usage_daily` where the Rust says 730 days, and the constant's own doc records
        // 730 as 24 months at a 365-day year - so the guard converts months by 365/12, not by
        // 30, or it would have reported a disagreement the code had deliberately avoided.
        'website: **185 tests**', 'passes **185 tests**', '**185 tests**',
        // Retired when the review-retention claim was corrected. Three files published
        // "Reviews: Until deleted by user" - website/src/lib/privacy.ts, docs/data-retention.md
        // and server/src/db.rs - and NONE of them was true: there is no `DELETE FROM reviews`
        // anywhere in server/src. The only act is `POST /api/reviews/withdraw`, which sets
        // `withdrawn_at` on purpose and does not delete, because the row must keep occupying
        // the account's one slot. `reviews.account_id` is `ON DELETE SET NULL`, so a review
        // outlives its author and stays in the public aggregate attached to nobody. The new
        // test pins the absence, so implementing deletion fails it and names all three files.
        'website: **186 tests**', 'passes **186 tests**', '**186 tests**',
        // Retired when the edit form was made to round-trip the limits it submits. Two tests
        // were added: one fills the edit form from a stored key DTO that carries a non-zero
        // ceiling and asserts the PATCH body keeps it, and one asserts that re-saving without
        // touching a limit does not claim to lower one. The pair exists because the original
        // bug was invisible to every test that came before it: `fields()` supplies a COMPLETE
        // field set, so no test ever modelled "the form opened without this value". A fixture
        // that supplies every field cannot see a defect that consists of a field never being
        // supplied.
        'website: **187 tests**', 'passes **187 tests**', '**187 tests**',
        // Retired when the two READMEs' counts were regenerated after a test-file sweep.
        'website: **188 tests**', 'passes **188 tests**', '**188 tests**',
        // Retired when the sign-in body was emptied. `POST /auth/login` and
        // `POST /auth/google` used to answer `{ account_id, balance_idr }`, mirrored by
        // `SessionResult` in website/src/lib/auth-api.ts, and NOTHING read either field on
        // either side: website/src/pages/login.astro is the only caller and both handlers
        // end `.then(() => window.location.assign(destination))`, discarding the value. The
        // payload was always the HttpOnly `Set-Cookie: session=…` beside it. `balance_idr`
        // was the costly half - it made the login route run a wallet SELECT no client
        // consumed, while the balance has a delivery path clients DO read (the SSE `balance`
        // event, consumed in lib/live.ts). The struct, the client type and the API spec were
        // all emptied, and a new guard (all four assertions mutation-proved) now fails if any
        // of the three grows a field back.
        'website: **189 tests**', 'passes **189 tests**', '**189 tests**',
        // Retired when the website suite reached 203. The count had been stale since before
        // this bump: 196 was the figure the docs carried and the suite had already grown past it,
        // which is exactly the drift this constant exists to make visible.
        'website: **196 tests**', 'passes **196 tests**', '**196 tests**',
        'website: **204 tests**', 'passes **204 tests**', '**204 tests**',
        'website: **206 tests**', 'passes **206 tests**', '**206 tests**',
        'website: **207 tests**', 'passes **207 tests**', '**207 tests**',
        'website: **208 tests**', 'passes **208 tests**', '**208 tests**',
        'website: **209 tests**', 'passes **209 tests**', '**209 tests**',
        'website: **210 tests**', 'passes **210 tests**', '**210 tests**',
      ],
    },
  ];

  // The documents each suite's count actually lives in. A bare figure is checked
  // against these and nothing else, so a server number is never asserted against a
  // file that has no reason to state one.
  const subject: Record<Retired['suite'], string[]> = {
    server: ['README.md', 'server/README.md', 'docs/ci-cd.md'],
    website: ['README.md', 'website/README.md', 'docs/website/README.md'],
  };

  for (const { suite, forms } of superseded) {
    // The qualified form of every bare figure in this group, built from the suite
    // rather than transcribed a second time.
    const qualified = suite === 'server'
      ? forms.map((f) => `server: **${f}**`)
      : forms.map((f) => `website: **${f}**`);

    for (const stale of [...forms, ...qualified]) {
      // A figure that IS a live count of either suite is not superseded, and the
      // qualified form is what makes that testable: `server: **582 tests**` is
      // present-when-582-is-live and must not be asserted absent, while a bare
      // `'582 tests'` is also what a website line legitimately contains.
      if (LIVE_FORMS.has(stale)) continue;
      for (const file of subject[suite]) {
        assert.ok(
          !read(file).includes(stale),
          `${file} still states the superseded ${suite} count "${stale}". Every check above asserts the CURRENT count is present; nothing else asserts a retired one is absent.`,
        );
      }
    }
  }

  // Vacuity, both directions. A list that skipped to nothing, or a subject map that
  // named no file, would make the loop above check nothing at all.
  const applicable = superseded.flatMap(({ suite, forms }) =>
    [...forms, ...forms.map((f) => (suite === 'server' ? `server: **${f}**` : `website: **${f}**`))]
      .filter((s) => !LIVE_FORMS.has(s)),
  );
  assert.ok(
    applicable.length > 10,
    `only ${applicable.length} retired figure(s) are still being checked, so the live-count skip has swallowed the list`,
  );
  for (const [suite, files] of Object.entries(subject)) {
    assert.ok(
      files.length > 0,
      `no documents are checked for the ${suite} count, so its retired figures are asserted against nothing`,
    );
  }

  // The qualified forms must not be vacuous themselves: each must be FOUND in a
  // document while that count is live, or the prefix is a string nothing writes and
  // the collision this whole block exists to prevent is back with a quieter symptom.
  assert.ok(
    read('README.md').includes(`server: **${SERVER_TESTS} tests**`),
    'README.md must state the live server count in the qualified form, or the qualified forms above are not the shape documents actually use',
  );
  assert.ok(
    read('README.md').includes(`website: **${WEBSITE_TESTS} tests**`),
    'README.md must state the live website count in the qualified form, or the qualified forms above are not the shape documents actually use',
  );
});
/**
 * The format gate is described, not imagined.
 *
 * docs/testing.md claimed for as long as anyone could check that "The crate has no
 * `cargo fmt --check` in CI". It has had one since the workflow was added, and the
 * sentence was load-bearing: it was the stated reason the route inventory's
 * `);`-terminated lines were considered safe from a formatter. A doc that says a check
 * does not exist is a doc that tells the next reader not to run it.
 *
 * So the claim is now pinned against the workflow rather than against memory. This is
 * deliberately a ONE-WAY check: it fails when the doc denies a gate the workflow has.
 * It does not fail when the workflow gains a gate the doc never mentioned, because
 * that is the ordinary case and pinning it would mean editing this test every time a
 * stage is added.
 */
test('no doc denies a check that CI actually runs', () => {
  const workflow = read('.github/workflows/ci.yml');
  const fmtIsGated = workflow.includes('cargo fmt --check');
  const doc = read('docs/testing.md');

  if (fmtIsGated) {
    for (const denial of [
      'has no `cargo fmt --check` in CI',
      'no `cargo fmt --check`',
    ]) {
      assert.ok(
        !doc.includes(denial),
        `docs/testing.md denies a gate ci.yml runs: it still says "${denial}"`,
      );
    }
  }
});
