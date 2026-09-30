// A rule the server enforces and a page states is ONE rule with two copies, and
// nothing compared them.
//
// WHY THIS EXISTS
//
// server/src/config.rs documents its password floor like this:
//
//   /// on both signup and reset, so the two paths cannot drift. The signup page
//   /// states this same number to the user, so the validator refuses a value
//   /// below 8 - a server floor under the published one would make the page
//   /// describe a rule we do not enforce.
//
// That is the whole argument for this file: the floor is a number the server
// ENFORCES and the page ANNOUNCES, and the document says in as many words that a
// server floor under the published one would leave the page describing a rule we
// do not enforce. Nothing tested it. Nine copies of `8` sat in three pages -
// `minlength="8"` in the markup and "At least 8 characters" in the prose - against
// `config/apikita.toml:156 password_min_length = 8` and the loader's own literal.
//
// Lowering the config value would leave the pages refusing a password the server
// accepts (annoying, and a support ticket). Raising it would leave the pages
// ACCEPTING a password the server rejects - the user fills a form the page has
// just told them is valid, and it fails on submit. Neither failed a test.
//
// This is the same shape as round 24's wallet.astro shadow: a value with one
// source, restated on a surface, with a check that pins the source and cannot see
// the surface. That guard catches an identifier that shadows an import; this one
// catches a literal that has no identifier at all.
//
// WHAT IT CHECKS
//
//   Every `minlength="N"` and every "at least N characters" / "minimum N
//   characters" under website/src equals the configured password floor.
//
// The `minlength` attribute is the one that matters most: it is what the BROWSER
// enforces, so it is what decides whether the user gets a native validation error
// or a server rejection.
//
// Style follows the other suites: node:test + node:assert/strict, explicit .ts
// extensions so Node loads the modules with no bundler and no new dependency.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const SRC = join(here, '..', 'src');
const CONFIG = join(here, '..', '..', 'config', 'apikita.toml');

const config = readFileSync(CONFIG, 'utf8');

/** The number a `key = value` line carries in the top-level config, else null. */
function configNumber(key: string): number | null {
  const m = config.match(new RegExp('^\\s*' + key + '\\s*=\\s*([0-9]+)', 'm'));
  return m === null ? null : Number(m[1]);
}

function sourceFiles(dir: string, found: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) sourceFiles(full, found);
    else if (/\.(astro|ts)$/.test(entry)) found.push(full);
  }
  return found;
}

test('every stated password minimum is the minimum the config sets', () => {
  const configured = configNumber('password_min_length');
  assert.ok(
    configured !== null,
    'config/apikita.toml has no `password_min_length`; this guard cannot check a floor that is not set',
  );

  const files = sourceFiles(SRC).filter((f) => !f.endsWith('.d.ts'));
  assert.ok(files.length >= 20, `only ${files.length} source files found; the walk is not reading the tree`);

  const stated: { file: string; line: number; text: string; value: number; what: string }[] = [];

  for (const file of files) {
    const rel = file.slice(SRC.length + 1).split('\\').join('/');
    const lines = readFileSync(file, 'utf8').split('\n');

    lines.forEach((line, index) => {
      // The markup attribute, which is the rule the BROWSER enforces.
      for (const m of line.matchAll(/minlength="(\d+)"/g)) {
        stated.push({ file: rel, line: index + 1, text: line.trim(), value: Number(m[1]), what: 'minlength' });
      }
      // The prose, which is the rule the CUSTOMER reads.
      for (const m of line.matchAll(/(?:at least|minimum|min\.?)\s+(\d+)\s+characters/gi)) {
        stated.push({ file: rel, line: index + 1, text: line.trim(), value: Number(m[1]), what: 'prose' });
      }
    });
  }

  // Vacuity: a regex that stopped matching would make every assertion below vanish.
  assert.ok(
    stated.length >= 8,
    `only ${stated.length} password-minimum statements found; the scan is not reading the pages`,
  );
  assert.ok(
    stated.some((s) => s.what === 'minlength') && stated.some((s) => s.what === 'prose'),
    'the scan found only one KIND of statement, so one of the two patterns has stopped matching',
  );

  for (const s of stated) {
    assert.equal(
      s.value,
      configured,
      `${s.file}:${s.line} states a password minimum of ${s.value} (${s.what}) but config/apikita.toml sets password_min_length = ${configured}. server/src/config.rs refuses a configured floor below the published one precisely because "a server floor under the published one would make the page describe a rule we do not enforce". A LOWER page value accepts a password the server rejects; a HIGHER one rejects a password the server accepts.`,
    );
  }
});

test('every stated seconds figure is the figure its own mechanism uses', () => {
  // There are THREE different intervals in the site and they are one sentence apart in
  // places, so a regex that matched "N seconds" alone conflated them - which is how
  // this test was first written, and its first run accused pages/index.astro:68 of
  // stating the poll interval when it states the metadata cache TTL. All three are real
  // facts with real sources; none may be guessed from the phrase "N seconds".
  //
  //   the POLL interval      live.ts POLL_INTERVAL_MS, used by the client's poller
  //   the CACHE TTL          config key_metadata_cache_seconds, how long a lowered key
  //                          limit takes to apply at the proxy
  //   the COOLDOWN CEILING   config cooldown_max_seconds, the longest a tripped upstream
  //                          can stay out (the ladder is 30 -> 60 -> 120 -> 240 -> 480 -> 900)
  const live = readFileSync(join(SRC, 'lib', 'live.ts'), 'utf8');
  const pollMatch = live.match(/POLL_INTERVAL_MS\s*=\s*([\d_]+)/);
  assert.ok(pollMatch, 'website/src/lib/live.ts no longer declares POLL_INTERVAL_MS, so this guard is checking nothing');
  const pollMs = Number(pollMatch[1].replace(/_/g, ''));
  assert.ok(pollMs > 0 && Number.isFinite(pollMs), `POLL_INTERVAL_MS is not a positive number: ${pollMatch[1]}`);
  const pollSeconds = pollMs / 1000;
  assert.ok(
    Number.isInteger(pollSeconds),
    `POLL_INTERVAL_MS is ${pollMs}ms, which is not a whole number of seconds, so no page can state it without rounding`,
  );

  const ttl = configNumber('key_metadata_cache_seconds');
  assert.ok(ttl !== null, 'config/apikita.toml has no key_metadata_cache_seconds, so the cache-TTL half of this test checks nothing');

  // The cooldown ceiling lives in the [circuit_breaker] table, so it is read from that
  // block rather than the top level. The block is found first and its absence is a
  // failure, not a null that quietly matches nothing.
  const cbBlock = config.split(/^\[/m).find((b) => /^circuit_breaker\]/.test(b));
  assert.ok(cbBlock, 'config/apikita.toml has no [circuit_breaker] block, so the cooldown-ceiling half of this test checks nothing');
  const ceilMatch = cbBlock.match(/^\s*cooldown_max_seconds\s*=\s*(\d+)/m);
  assert.ok(ceilMatch, '[circuit_breaker] no longer sets cooldown_max_seconds, so the 503 Retry-After ceiling is unstated');
  const cooldownCeiling = Number(ceilMatch[1]);

  // The lib constant the pages import. If it drifts from config, that is its own bug
  // and it gets its own message rather than being folded into a page's.
  const models = readFileSync(join(SRC, 'lib', 'models.ts'), 'utf8');
  const ttlConst = models.match(/keyMetadataCacheSeconds\s*=\s*(\d+)/);
  assert.ok(ttlConst, 'website/src/lib/models.ts no longer declares keyMetadataCacheSeconds');
  assert.equal(
    Number(ttlConst[1]),
    ttl,
    `models.ts declares keyMetadataCacheSeconds = ${ttlConst[1]} but config/apikita.toml sets key_metadata_cache_seconds = ${ttl}. The pages import the constant, so the constant must carry the configured value.`,
  );

  // Each statement is classified by the sentence it sits in, not by its number: the
  // mechanisms have different words, and those words are the only thing that
  // distinguishes them.
  const marked = [
    { what: 'the poll interval', pattern: /poll|\/api\/me/gi, expected: pollSeconds, source: `POLL_INTERVAL_MS (${pollMs}ms)` },
    { what: 'the metadata cache TTL', pattern: /metadata cache|lowering|lowered|cache ttl/gi, expected: ttl, source: `key_metadata_cache_seconds (${ttl})` },
    { what: 'the cooldown ceiling', pattern: /cooldown/gi, expected: cooldownCeiling, source: `[circuit_breaker] cooldown_max_seconds (${cooldownCeiling})` },
  ] as const;

  const stale: string[] = [];
  const counts: Record<string, number> = {};
  const unclassified: string[] = [];

  for (const file of sourceFiles(SRC)) {
    const rel = file.slice(SRC.length + 1).split('\\').join('/');
    const lines = readFileSync(file, 'utf8').split('\n');
    lines.forEach((line, index) => {
      for (const m of line.matchAll(/(?:every|within|up to|as long as)\s+(\d+)\s+seconds/gi)) {
        // Which mechanism does THIS sentence describe?
        const owner = marked.find((k) => {
          k.pattern.lastIndex = 0;
          return k.pattern.test(line);
        });
        if (owner === undefined) {
          // An unclassified "N seconds" is not a pass and not a failure to assert on -
          // it is a statement this guard cannot check, and saying so is the point.
          unclassified.push(
            `${rel}:${index + 1} states "${m[0]}" but the sentence names no mechanism this test knows. Either classify it here (with its config key or lib constant) or remove the unverifiable figure. Line: ${line.trim()}`,
          );
          continue;
        }
        counts[owner.what] = (counts[owner.what] ?? 0) + 1;
        if (Number(m[1]) !== owner.expected) {
          stale.push(
            `${rel}:${index + 1} says "${m[0]}" about ${owner.what}, but ${owner.source}. A page stating a stale figure is read by a customer who then measures the difference. Line: ${line.trim()}`,
          );
        }
      }
    });
  }

  assert.deepEqual(
    unclassified,
    [],
    `a stated seconds figure has no mechanism to check it against:\n  ${unclassified.join('\n  ')}`,
  );

  assert.deepEqual(
    stale,
    [],
    `a stated seconds figure does not match its mechanism:\n  ${stale.join('\n  ')}`,
  );

  // The inventory, pinned so that a NEW statement is reviewed rather than silently
  // trusted. This is deliberately an equality: adding a fourth "N seconds" to a page is
  // exactly the moment a human should say which mechanism it describes.
  assert.deepEqual(
    counts,
    { 'the poll interval': 1, 'the metadata cache TTL': 2, 'the cooldown ceiling': 1 },
    `the set of stated seconds figures changed: ${JSON.stringify(counts)}. This is the guard telling you the inventory moved, so that a new statement is classified rather than silently trusted. Update this expectation in the same change that adds or removes one.`,
  );
});

// ---------------------------------------------------------------------------
// The figures whose only source is a Rust literal.
//
// These four are different in kind from the password floor and the seconds
// figures above, and the difference is why they survived longer.
//
// The password floor has a CONFIG key, so the page, the loader and the config
// file could be compared as three transcriptions of one setting. These four have
// no config key at all: the number exists ONLY as a literal inside a Rust
// function, and the website restates it in a TypeScript constant. Nothing
// compared them, and every test that touched the constants was CIRCULAR:
//
//   tests/admin.test.ts:279        assert.equal(ERROR_RATE_THRESHOLD, 0.05)
//   tests/recent-usage.test.ts:23  assert.equal(recentUsagePath(),
//                                    `/api/usage/recent?limit=${RECENT_USAGE_LIMIT}`)
//
// The first restates the number beside the constant that defines it, so changing
// both in one edit passes. The second interpolates the constant into its own
// expected value, so it is true for any value the constant takes. Neither reads
// the server. A dashboard that asked for 50 rows from an endpoint that defaults
// to 20 would render a short list with a "load more" that never fires, and both
// of those tests would stay green.
//
// So each of these is checked against the Rust that actually enforces it, by
// reading the file and extracting the literal. The extraction is deliberately
// narrow - an `unwrap_or(N)` or a `const NAME: T = N` on a named line - because a
// pattern broad enough to survive any refactor is also broad enough to match the
// wrong number, and silently matching the wrong number is the failure mode this
// file exists to prevent.

/** Read a file under the repository root. */
function repoFile(...parts: string[]): string {
  return readFileSync(join(here, '..', '..', ...parts), 'utf8');
}

/**
 * The integer a Rust line carries, matched by a narrow, named anchor.
 *
 * `anchor` is a regular expression with ONE capture group, applied to the whole
 * file. If it does not match, that is a FAILURE rather than a skipped check: the
 * server may have refactored, and a guard that quietly stops measuring is worse
 * than no guard.
 */
function rustNumber(file: string, anchor: RegExp, describe: string): number {
  const text = repoFile('server', 'src', file);
  assert.ok(text.length > 500, `server/src/${file} was not read, so this check is vacuous`);
  const m = text.match(anchor);
  assert.ok(
    m !== null,
    `server/src/${file} no longer matches the anchor for ${describe}. The server may have refactored - find the literal by hand and update the anchor, rather than deleting the check.`,
  );
  const value = Number(m[1]);
  assert.ok(Number.isFinite(value), `the ${describe} anchor matched "${m[1]}", which is not a number`);
  return value;
}

test('the figures the website states are the figures the server enforces', async () => {
  // --- the account list page size -------------------------------------------
  // server/src/routes/admin.rs:368  let limit = query.limit.unwrap_or(25).clamp(1, 100);
  const listLimit = rustNumber(
    'routes/admin.rs',
    /fn list_accounts[\s\S]*?query\.limit\.unwrap_or\((\d+)\)/,
    'the admin accounts list default page size',
  );
  const { ADMIN_LIST_LIMIT } = await import('../src/lib/admin.ts');
  assert.equal(
    ADMIN_LIST_LIMIT,
    listLimit,
    `website/src/lib/admin.ts asks for ${ADMIN_LIST_LIMIT} accounts per page but server/src/routes/admin.rs:368 defaults to ${listLimit}. The admin list would page at a size the server does not use.`,
  );

  // --- the dashboard's "last N metered calls" -------------------------------
  // server/src/routes/account.rs:384  const RECENT_USAGE_DEFAULT_LIMIT: i64 = 20;
  const recentLimit = rustNumber(
    'routes/account.rs',
    /const RECENT_USAGE_DEFAULT_LIMIT:\s*i64\s*=\s*(\d+)/,
    'the recent-usage default row count',
  );
  const recent = await import('../src/lib/recent-usage.ts');
  assert.equal(
    recent.RECENT_USAGE_LIMIT,
    recentLimit,
    `website/src/lib/recent-usage.ts requests ${recent.RECENT_USAGE_LIMIT} recent calls but server/src/routes/account.rs:384 defaults to ${recentLimit}. RECENT_USAGE_LIMIT is also the threshold \`mayHaveMore\` uses to decide whether to offer "load more", so a mismatch shows a short list with a button that never fires.`,
  );

  // --- the usage window -----------------------------------------------------
  // server/src/routes/account.rs:259  const USAGE_DEFAULT_LIMIT: i64 = 30;
  const usageLimit = rustNumber(
    'routes/account.rs',
    /const USAGE_DEFAULT_LIMIT:\s*i64\s*=\s*(\d+)/,
    'the usage-chart default bucket count',
  );
  const usage = await import('../src/lib/usage.ts');
  assert.equal(
    usage.USAGE_WINDOW_DAYS,
    usageLimit,
    `website/src/lib/usage.ts charts ${usage.USAGE_WINDOW_DAYS} days but server/src/routes/account.rs:259 returns ${usageLimit} buckets when no window is asked for. The chart would draw a period the server does not send.`,
  );

  // --- the error-rate threshold --------------------------------------------
  // tools/alert/probe.sh:419  BREACH=$(awk -v r="$RATE" 'BEGIN { print (r > 0.05) ? "yes" : "no" }')
  // This one lives in the alerting probe rather than the server, which is exactly
  // why it drifted out of view: it is the number that decides whether an operator
  // is paged, and the dashboard paints the same number red.
  const probe = repoFile('tools', 'alert', 'probe.sh');
  assert.ok(probe.length > 1000, 'tools/alert/probe.sh was not read, so this check is vacuous');
  const probeAnchor = probe.match(/r > ([0-9]*\.?[0-9]+)\s*\)\s*\?\s*"yes"/);
  assert.ok(
    probeAnchor !== null,
    'tools/alert/probe.sh no longer contains the error_rate comparison, so the dashboard threshold cannot be checked against the one that fires the alert',
  );
  const probeThreshold = Number(probeAnchor[1]);
  const admin = await import('../src/lib/admin.ts');
  assert.equal(
    admin.ERROR_RATE_THRESHOLD,
    probeThreshold,
    `website/src/lib/admin.ts marks a series red above ${admin.ERROR_RATE_THRESHOLD} but tools/alert/probe.sh:419 fires the error_rate alert above ${probeThreshold}. The dashboard would colour a rate the pager ignores, or leave grey a rate that has just paged someone.`,
  );

  // Vacuity: the four values must be distinguishable from each other and from
  // zero. If a regex matched a comment or a stale copy, several of these would
  // collapse onto one number and every assertion above would still pass.
  const seen = [listLimit, recentLimit, usageLimit, probeThreshold];
  assert.ok(
    seen.every((n) => n > 0),
    `a server figure was read as ${JSON.stringify(seen)}; a zero or negative value means an anchor matched the wrong text`,
  );
  assert.ok(
    new Set([listLimit, recentLimit, usageLimit]).size >= 2,
    `the three row-count anchors returned ${listLimit}, ${recentLimit}, ${usageLimit} - too alike to tell whether each matched its own line. Check the anchors before trusting the comparisons above.`,
  );
  assert.ok(
    probeThreshold > 0 && probeThreshold < 1,
    `the probe threshold was read as ${probeThreshold}, which is not a plausible error RATE. The comparison in tools/alert/probe.sh is a percentage expressed as a fraction`,
  );
});

// ---------------------------------------------------------------------------
// THE RETENTION PERIODS THE PRIVACY PAGE PUBLISHES
// ---------------------------------------------------------------------------
//
// `website/src/lib/privacy.ts` is the disclosure a customer reads, and its
// `retention` array states a period for every kind of data we keep. Fifteen rows
// today. Every one of those periods is a promise, and eight of them are numbers
// that also exist somewhere else in the repository as the thing that ACTUALLY
// expires the data.
//
// Nothing compared the two halves. The Rust constants the sweep reads are pinned
// by `db.rs`'s own tests (`an_ip_hash_table_past_its_window_is_reported_as_behind`
// asserts the lag report names them), and the page's `keep:` strings are pinned by
// `privacy.test.ts` - which checks that each is NON-EMPTY and, for one row, that
// it matches `/90 days/`. Neither reads the other.
//
// THE FAILURE THIS PREVENTS is quiet and legal rather than loud and technical.
// Raising `usage_events` to 180 days in `db.rs` keeps every Rust test green: the
// sweep still deletes what the constant says, the lag report still names the
// window, and the page still says "90 days" to a customer who is relying on it.
// The disclosure would be wrong with no red anywhere. The mirror case is worse in
// the other direction - a page promising "7 days" for a table the sweep holds for
// 90 is a promise kept by the document and broken by the code, which is exactly
// the shape of two earlier rounds: `identity_tokens` had a purge with no caller,
// and `link_codes` had a published 24-hour window and no delete path at all.
//
// WHAT THIS CHECKS, and why it is a TABLE rather than a scan:
//
//   Each numeric period the page publishes is compared to the one source of
//   truth that owns it - a `pub const` in the Rust, a config key, or a constant
//   in another website lib module.
//
// The mapping is written out by hand, and that is deliberate. A scan that looked
// for any number on the page and any number in the server would be guessing at
// which rows the two describe; `7 days` appears four times in `privacy.ts` and
// four times in the Rust, and matching them by value would connect the wrong
// pairs and pass. Naming the pairs is work a person does once and the guard then
// holds; a guess would be a check whose green means nothing.

test('every numeric retention period the privacy page publishes is the one that expires it', async () => {
  const privacy = await import('../src/lib/privacy.ts');

  /** The `keep:` string of the one row matching, found FIRST so a rename fails. */
  function published(fragment: string): string {
    const row = privacy.retention.find((r: { what: string }) => r.what.includes(fragment));
    assert.ok(
      row !== undefined,
      `website/src/lib/privacy.ts has no retention row mentioning "${fragment}". Either the row was renamed or removed - and a row that cannot be found is a silent pass, which is why this fails instead of skipping.`,
    );
    return row.keep;
  }

  /** The number of days a `keep:` string states, in days. */
  //
  // MONTHS ARE CONVERTED, and the conversion is the point rather than a
  // convenience. `website/src/lib/privacy.ts` publishes "24 months" for
  // `usage_daily`, while `db::USAGE_DAILY_RETENTION_DAYS` is 730 DAYS. Those
  // describe the same window in two different units, and a comparison that
  // could not read one of them would have had to skip the row - which is how
  // the divergence survived every check that existed before this one.
  //
  // A MONTH IS 365/12 DAYS, NOT 30. That is not pedantry: the Rust constant's own
  // doc says "730 days is 24 months to the day at the common 365-day year", so
  // the two sides agree by construction and a 30-day month would report a
  // mismatch that does not exist. 730 / 30 = 24.33, and a guard that failed on
  // that would be wrong about a pair the code went out of its way to align.
  const MONTH_DAYS = 365 / 12;

  function daysIn(keep: string): number {
    const days = keep.match(/(\d+)\s*days?/);
    if (days !== null) return Number(days[1]);

    const months = keep.match(/(\d+)\s*months?/);
    if (months !== null) return Math.round(Number(months[1]) * MONTH_DAYS);

    const years = keep.match(/(\d+)\s*years?/);
    if (years !== null) return Math.round(Number(years[1]) * 365);

    assert.fail(
      `the period "${keep}" states no number of days, months or years, so it cannot be compared to a retention window. If this row genuinely has no period, it does not belong in this table.`,
    );
  }

  // --- the sweep's own windows, read from the Rust ---------------------------
  // `db::*` and `ip_tracking::*` hold one constant per swept table, and the
  // sweep tests already prove the sweep reads them. This proves the PAGE does.
  const pairs: Array<[string, string, RegExp, string]> = [
    [
      'Per-request usage',
      'db.rs',
      /const USAGE_EVENTS_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the usage_events retention window',
    ],
    [
      'Usage daily',
      'db.rs',
      /const USAGE_DAILY_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the usage_daily retention window',
    ],
    [
      'Sessions',
      'db.rs',
      /const SESSION_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the sessions retention window',
    ],
    [
      'Link-redemption attempts',
      'db.rs',
      /const LINK_ATTEMPT_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the link_redemption_attempts retention window',
    ],
    [
      'per address seen',
      'ip_tracking.rs',
      /const SEEN_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the key_ip_seen retention window',
    ],
    [
      'per day',
      'ip_tracking.rs',
      /const DAILY_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the key_ip_daily retention window',
    ],
    [
      'Sign-in attempt counters',
      'ip_tracking.rs',
      /const AUTH_ATTEMPT_RETENTION_DAYS:\s*i64\s*=\s*(\d+)/,
      'the auth_attempts retention window',
    ],
  ];

  for (const [fragment, file, anchor, describe] of pairs) {
    const keep = published(fragment);
    const stated = daysIn(keep);
    const enforced = rustNumber(file, anchor, describe);
    assert.equal(
      stated,
      enforced,
      `website/src/lib/privacy.ts publishes "${keep}" for the row mentioning "${fragment}", but ${describe} in server/src/${file} is ${enforced} days. The page a customer relies on and the sweep that runs would disagree, and only this comparison can see both.`,
    );
  }

  // --- the identity links, whose window is a CONFIG TTL, not a sweep --------
  // These are the two figures the file header has been promising to check since
  // the `minlength="8"` round, and they are the reason this test exists at all.
  // A verification or reset link is not deleted N days after anything: it is
  // inert the moment `expires_at` passes, and `identity::tokens::consume` refuses
  // a row past it. So the number on the page has to equal the number the config
  // hands `tokens::issue`.
  //
  // NOTE the two units. The config is in MINUTES; the page is in `h` and `m`.
  // A guard that compared the bare integers would call 1440 and 24 equal only by
  // accident of neither being read - so both sides are converted to minutes and
  // the conversion is asserted, not assumed.
  const linksKeep = published('Email verification and password-reset links');
  const verificationMinutes = configNumber('verification_ttl_minutes');
  const resetMinutes = configNumber('reset_ttl_minutes');
  assert.ok(
    verificationMinutes !== null && resetMinutes !== null,
    'config/apikita.toml no longer states verification_ttl_minutes and reset_ttl_minutes, so the two link lifetimes on the privacy page have no source to be checked against',
  );

  const statedVerification = linksKeep.match(/(\d+)\s*h\b/);
  const statedReset = linksKeep.match(/(\d+)\s*m\b/);
  assert.ok(
    statedVerification !== null,
    `the link row publishes "${linksKeep}", which states no HOURS for the verification link, so it cannot be compared to verification_ttl_minutes`,
  );
  assert.ok(
    statedReset !== null,
    `the link row publishes "${linksKeep}", which states no MINUTES for the reset link, so it cannot be compared to reset_ttl_minutes`,
  );

  // The config is minutes; the page is hours for one and minutes for the other.
  // Comparing the page's "24" to the config's "1440" would fail, and comparing
  // nothing at all is what this test was written to stop.
  assert.equal(
    Number(statedVerification[1]) * 60,
    verificationMinutes,
    `website/src/lib/privacy.ts publishes "${linksKeep}" for email links, so the verification window reads as ${Number(statedVerification[1])} hours, but config/apikita.toml sets verification_ttl_minutes = ${verificationMinutes} (= ${verificationMinutes / 60} hours). A verification link that really lived for ${Number(statedVerification[1])} hours would expire before the config says it does, and the page would be telling the customer the wrong lifetime.`,
  );
  assert.equal(
    Number(statedReset[1]),
    resetMinutes,
    `website/src/lib/privacy.ts publishes "${linksKeep}" for email links, so the reset window reads as ${Number(statedReset[1])} minutes, but config/apikita.toml sets reset_ttl_minutes = ${resetMinutes}.`,
  );

  // --- the four rows that state NO period, checked for the right reason -----
  // "Forever", "Until deleted by user", "Same as review" and "Keep record, drop
  // personal data" have no number by design. They are listed here so that a
  // period APPEARING on one of them is a failure rather than an unnoticed edit:
  // a row that gains a number is a row the sweep may have to agree with, and
  // nothing would say so.
  const unnumbered = ['Ledger', 'Top-ups', 'Reviews', 'Review history', 'Accounts (closed)'];
  for (const fragment of unnumbered) {
    const keep = published(fragment);
    assert.doesNotMatch(
      keep,
      /\d/,
      `website/src/lib/privacy.ts publishes "${keep}" for the row mentioning "${fragment}", which used to state no period at all. A number here is a retention window the sweep does not necessarily have - add it to the table above once something backs it.`,
    );
  }

  // --- vacuity --------------------------------------------------------------
  // The pairs above are found by FRAGMENT, so a rename that left two fragments
  // pointing at the SAME row would compare one row seven times and pass. The
  // seven must be seven distinct rows.
  const matched = pairs.map(([fragment]) => published(fragment));
  assert.equal(
    new Set(pairs.map(([fragment]) => privacy.retention.find((r: { what: string }) => r.what.includes(fragment))?.what)).size,
    pairs.length,
    'two of the fragments in the table above matched the same retention row, so this test is comparing one row against several constants. Fix the fragments before trusting any of it.',
  );
  assert.ok(
    privacy.retention.length >= 15,
    `privacy.ts publishes only ${privacy.retention.length} retention rows; this test's table was written against fifteen, so it is no longer reading the page it was written for`,
  );
  assert.equal(
    matched.length,
    pairs.length,
    'the retention table did not yield one period per row it names',
  );
});



