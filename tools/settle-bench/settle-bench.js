#!/usr/bin/env node
//
// settle-bench — drives the SETTLEMENT path, the one `docs/benchmark.md` Scenario 3 is about.
//
// ===========================================================================
// WHY THIS EXISTS, AND WHY IT IS NOT PART OF tools/loadtest
// ===========================================================================
//
// Scenario 3 ("Database & Ledger Contention") publishes three pass criteria:
//
//   * zero SQLITE_BUSY failures and zero write-transaction deadlocks
//   * zero balance discrepancies (post-test reconciliation returns 0)
//   * > 100 settlements/sec with connection pool size = 8
//
// MEASURED before this tool existed: two of the three appeared in NO file except that
// document line, and `server/src/doc_claims.rs` pins the scenario BY NAME as the one the
// harness cannot run - "its criteria are unowned". That is an honest record, not a fix.
// This closes the third criterion, which is the one with no other owner.
//
// WHY A SEPARATE TOOL rather than a `--path` on tools/loadtest. That harness sends ONE FIXED
// BODY from several worker processes, which is exactly right for a latency distribution and
// impossible here: every webhook must carry a DIFFERENT `order_id` and a signature computed
// over that specific body, because the unique `order_id` is the idempotency key the credit
// is keyed on. A replay of one body would settle once and return `already_settled` forever
// after - a throughput figure for a database doing nothing.
//
// WHAT IT MEASURES: how many DISTINCT settlements per second the money path completes,
// each one a real `topups` claim plus a wallet credit plus a ledger row, in one
// BEGIN IMMEDIATE transaction. That is the single-writer serialisation Scenario 3 names.
//
// WHAT IT DELIBERATELY DOES NOT DO: it does not report a latency distribution (that is
// tools/loadtest's job, and its calibration warnings apply there rather than here), and it
// does not decide whether 100/sec is enough - it reports the figure against the
// DOCUMENT'S number and says which one it compared.
//
// Usage:
//   node tools/settle-bench/settle-bench.js --base-url http://127.0.0.1:8080 \
//     --server-key <midtrans server key> --accounts 50 --per-account 4 --concurrency 16
//
// Exit 0 every criterion met, 1 one not met, 3 a prerequisite is missing.
'use strict';

const http = require('node:http');
const crypto = require('node:crypto');
const { execFileSync } = require('node:child_process');
const fs = require('node:fs');

/** The published bar, from docs/benchmark.md Scenario 3. */
const PUBLISHED_SETTLEMENTS_PER_SEC = 100;

process.on('unhandledRejection', (err) => {
  process.stderr.write(`settle-bench: unhandled rejection: ${err && err.message}\n`);
  process.exit(3);
});

function parseArgs(argv) {
  const known = {
    '--base-url': 'string', '--server-key': 'string', '--db': 'string',
    '--accounts': 'number', '--per-account': 'number', '--concurrency': 'number',
    '--amount-idr': 'number', '--json': 'string', '--help': 'flag',
  };
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const raw = argv[i];
    const eq = raw.indexOf('=');
    const flag = eq === -1 ? raw : raw.slice(0, eq);
    const inline = eq === -1 ? null : raw.slice(eq + 1);
    const type = known[flag];
    if (type === undefined) {
      const err = new Error(`unrecognised flag ${flag}. This parser refuses unknown flags on purpose: a flag that is silently ignored produces a run that SUCCEEDS while measuring something else.`);
      err.exitCode = 3;
      throw err;
    }
    if (type === 'flag') { out[flag] = true; continue; }
    const value = inline !== null ? inline : argv[++i];
    if (value === undefined) { const e = new Error(`flag ${flag} needs a value`); e.exitCode = 3; throw e; }
    out[flag] = type === 'number' ? Number(value) : value;
  }
  return out;
}

const USAGE = `settle-bench — settlements/second on the webhook money path.

  node tools/settle-bench/settle-bench.js --base-url <url> --server-key <key> [options]

  --base-url <url>        The running server. REQUIRED.
  --server-key <key>      The Midtrans server key the SIGNATURE is computed with, so it must
                          match the running server's MIDTRANS_SERVER_KEY. REQUIRED.
  --db <path>             SQLite file, used to seed pending top-ups and to run the
                          reconciliation check afterwards. REQUIRED: without it nothing can
                          seed an order and NO settlement would be possible.
  --accounts <n>          Distinct accounts to spread settlements across. Default 50, the
                          figure Scenario 3 names. Distinct accounts matter: one account
                          serialises on a single wallet row, which measures row contention
                          rather than the single-writer property the scenario is about.
  --per-account <n>       Pending top-ups to seed per account. Default 4.
  --concurrency <n>       Webhooks in flight. Default 16.
  --amount-idr <n>        Each top-up's amount. Default 10000, the configured minimum.
  --json <path>           Also write the result object as JSON.
`;
/**
 * Runs SQL through the sqlite3 CLI, with the SCRIPT ON STDIN rather than in argv.
 *
 * MEASURED: passing it as an argument fails with `spawnSync sqlite3 ENAMETOOLONG` at 200 seeded
 * rows, because a whole seed script is one argv entry and Windows caps a command line at ~32k.
 * The tool then reported "could not seed" and exited 3 - correct behaviour, but for the wrong
 * reason, and it would have capped the tool at whatever row count fits in a command line rather
 * than at what the server can settle. Stdin has no such limit.
 */
function sqlite(dbPath, sql) {
  return execFileSync('sqlite3', [dbPath], { encoding: 'utf8', input: sql }).trim();
}

/**
 * One signed settlement notification.
 *
 * SIGNATURE FORMULA, verbatim from server/src/money.rs and tools/fake-midtrans: `SHA512(order_id
 * + status_code + gross_amount + server_key)`, over the VALUES in that order -- not over the JSON
 * key order, so the body's field order is free. Reproduced here rather than imported because
 * `tools/fake-midtrans` is a single-shot script with no module surface, and a shared helper would
 * be a new file every gate has to know about. `tools/fake-check` already pins the fake's signature
 * against the server's own vector, so if the formula moves, that gate fails and this file is the
 * second place to update -- which is exactly what the comment is for.
 */
function signedNotification(orderId, amountIdr, serverKey) {
  const statusCode = '200';
  const grossAmount = `${amountIdr}.00`;
  const signature = crypto
    .createHash('sha512')
    .update(`${orderId}${statusCode}${grossAmount}${serverKey}`)
    .digest('hex');
  return JSON.stringify({
    order_id: orderId,
    status_code: statusCode,
    gross_amount: grossAmount,
    transaction_status: 'settlement',
    signature_key: signature,
  });
}

function post(baseUrl, body) {
  return new Promise((resolve) => {
    const url = new URL('/webhooks/midtrans', baseUrl);
    const req = http.request(
      { host: url.hostname, port: url.port, path: url.pathname, method: 'POST',
        headers: { 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(body) },
        timeout: 10000 },
      (res) => {
        let out = '';
        res.setEncoding('utf8');
        res.on('data', (c) => { out += c; });
        res.on('end', () => resolve({ status: res.statusCode, body: out }));
      },
    );
    req.on('timeout', () => { req.destroy(); resolve({ status: 0, body: 'timeout' }); });
    req.on('error', (err) => resolve({ status: 0, body: err.message }));
    req.end(body);
  });
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args['--help']) { process.stdout.write(USAGE); process.exit(0); }

  const baseUrl = args['--base-url'];
  const serverKey = args['--server-key'];
  const dbPath = args['--db'];
  const accounts = args['--accounts'] ?? 50;
  const perAccount = args['--per-account'] ?? 4;
  const concurrency = args['--concurrency'] ?? 16;
  const amountIdr = args['--amount-idr'] ?? 10000;

  const missing = [];
  if (!baseUrl) missing.push('--base-url');
  if (!serverKey) missing.push('--server-key');
  if (!dbPath) missing.push('--db');
  if (missing.length) {
    process.stderr.write(`settle-bench: missing ${missing.join(', ')}\n\n${USAGE}`);
    process.exit(3);
  }
  if (!Number.isInteger(accounts) || accounts < 1 || !Number.isInteger(perAccount) || perAccount < 1) {
    process.stderr.write('settle-bench: --accounts and --per-account must be positive integers\n');
    process.exit(3);
  }
  if (!Number.isInteger(concurrency) || concurrency < 1) {
    process.stderr.write('settle-bench: --concurrency must be a positive integer\n');
    process.exit(3);
  }

  const progress = (s) => process.stderr.write(`settle-bench: ${s}\n`);

  // --- seed: accounts, wallets, and PENDING top-ups ------------------------------
  //
  // A settlement for an order that does not exist is refused (`unknown_order`), so the
  // seeding is not decoration - without it every request would be a 404 and the tool would
  // report a throughput of zero against a server doing nothing. Printed with the count so a
  // reader can see the denominator.
  //
  // The seed is DELETED on the way out (see the finally block) so a repeated run does not
  // accumulate accounts in the operator's listing.
  const runTag = `sb${Date.now().toString(36)}`;
  // Published for the cleanup in the caller - see the `finally` at the bottom of this file.
  process.env.SETTLE_BENCH_TAG = runTag;
  process.env.SETTLE_BENCH_DB = dbPath;
  const now = new Date().toISOString().replace(/\.\d+Z$/, '+00:00');
  const orderIds = [];
  const accountIds = [];
  const stmts = [];
  for (let a = 0; a < accounts; a += 1) {
    const accountId = `00000000-0000-4000-8000-${String(a).padStart(12, '0')}`;
    accountIds.push(accountId);
    stmts.push(`INSERT OR REPLACE INTO accounts (id,status,is_operator,created_at,updated_at) VALUES ('${accountId}','active',0,'${now}','${now}');`);
    stmts.push(`INSERT OR REPLACE INTO wallets (account_id,balance_idr,updated_at) VALUES ('${accountId}',0,'${now}');`);
    for (let k = 0; k < perAccount; k += 1) {
      const orderId = `${runTag}-${a}-${k}`;
      orderIds.push(orderId);
      // A REAL UUID FOR `topups.id`, and this was a defect in the first version of this tool
      // rather than a detail: `credit_topup_transaction` decodes that column as a UUID, so a
      // readable tag like `sb1-t-0-0` makes every settlement fail with a 500. The run then
      // reports 0 settlements/sec and the cause is the SEEDER, not the server - which is the
      // worst kind of benchmark result, because the number looks like a measurement.
      //
      // DETERMINISTIC rather than random, so a run is reproducible and a leftover row is
      // traceable to the tag in `order_id`, which is free-form TEXT.
      const topupId = `00000000-0000-4000-9000-${String(a).padStart(8, '0')}${String(k).padStart(4, '0')}`;
      stmts.push(`INSERT OR REPLACE INTO topups (id,account_id,amount_idr,order_id,status,rail,created_at) VALUES ('${topupId}','${accountId}',${amountIdr},'${orderId}','pending','midtrans','${now}');`);
    }
  }
  try {
    sqlite(dbPath, stmts.join('\n'));
  } catch (err) {
    process.stderr.write(`settle-bench: could not seed ${orderIds.length} pending top-ups via sqlite3: ${err.message}\n`);
    process.stderr.write('settle-bench:   sqlite3 must be on PATH and --db must be the server own database file.\n');
    process.exit(3);
  }
  progress(`seeded ${orderIds.length} pending top-ups across ${accounts} account(s)`);
  // --- reconciliation BEFORE, so 'no drift' is a delta and not a coincidence -------
  //
  // Read through the SHIPPED gate rather than a query written here: `tools/reconcile` is
  // the launch Gate 2 money check, and a second copy of the invariant in this file would be
  // the duplicate-rule that drifts. Its DSN comes from DATABASE_URL, so it is set for the
  // child process.
  function reconcileDrift() {
    try {
      const out = execFileSync('sh', ['tools/reconcile/reconcile.sh'], {
        encoding: 'utf8', cwd: process.cwd(),
        env: { ...process.env, DATABASE_URL: `sqlite://${dbPath}` },
      });
      return out.includes('0 drifting accounts');
    } catch (err) {
      const text = `${err.stdout || ''}${err.stderr || ''}`;
      if (text.includes('drifting account')) return false;
      return null;
    }
  }
  const driftBefore = reconcileDrift();
  progress(`reconciliation before: ${driftBefore === null ? 'could not be read' : driftBefore ? 'clean' : 'DRIFT PRESENT'}`);

  // --- drive the settlements ----------------------------------------------------
  const bodies = orderIds.map((id) => signedNotification(id, amountIdr, serverKey));
  const statuses = new Map();
  let settled = 0;
  let next = 0;
  const startedNs = process.hrtime.bigint();
  const startedAt = Date.now();

  async function worker() {
    for (;;) {
      const i = next;
      next += 1;
      if (i >= bodies.length) return;
      const res = await post(baseUrl, bodies[i]);
      const key = `${res.status} ${res.body.slice(0, 60)}`;
      statuses.set(key, (statuses.get(key) || 0) + 1);
      // COUNTED ONLY ON A CREDIT. A 200 `already_settled` or a 4xx would otherwise be
      // counted as a settlement, and the figure would then measure requests made rather
      // than credits completed - which is the difference this whole tool is about.
      if (res.status === 200 && res.body.includes('"status":"settled"')) settled += 1;
    }
  }
  await Promise.all(Array.from({ length: concurrency }, () => worker()));

  const wallMs = Date.now() - startedAt;
  const elapsedNs = Number(process.hrtime.bigint() - startedNs);
  const perSec = wallMs > 0 ? (settled / wallMs) * 1000 : 0;

  // --- reconciliation AFTER -----------------------------------------------------
  const driftAfter = reconcileDrift();

  // --- the verdicts -------------------------------------------------------------
  const busyless = [...statuses.keys()].every((k) => !/busy|locked|500/i.test(k));
  const throughputOk = perSec > PUBLISHED_SETTLEMENTS_PER_SEC;
  const everySettled = settled === orderIds.length;
  const allOk = busyless && driftAfter !== false && driftAfter !== null && throughputOk && everySettled;
  // --- report -------------------------------------------------------------------
  const out = [];
  const line = (s = '') => out.push(s);
  line('='.repeat(78));
  line('  SETTLEMENT THROUGHPUT — docs/benchmark.md Scenario 3, third criterion');
  line('='.repeat(78));
  line('');
  line(`    Settlements        : ${settled} of ${orderIds.length} offered`);
  line(`    Wall time          : ${(wallMs / 1000).toFixed(2)} s at concurrency ${concurrency}`);
  line(`    Settlements/sec    : ${perSec.toFixed(1)}`);
  line(`    Accounts           : ${accounts} distinct (${perAccount} top-up(s) each)`);
  line(`    Published bar      : > ${PUBLISHED_SETTLEMENTS_PER_SEC} settlements/sec, pool size = 8`);
  line('');
  line('  Response codes:');
  for (const [k, v] of [...statuses.entries()].sort((a, b) => b[1] - a[1])) {
    line(`    ${String(v).padStart(6)}  ${k}`);
  }
  line('');
  line('  VERDICTS');
  line(`    [${throughputOk ? 'PASS' : 'BELOW TARGET'}] ${perSec.toFixed(1)} settlements/sec against the published > ${PUBLISHED_SETTLEMENTS_PER_SEC}`);
  line(`    [${everySettled ? 'PASS' : 'BELOW TARGET'}] every offered order settled exactly once (${settled}/${orderIds.length})`);
  line(`    [${busyless ? 'PASS' : 'BELOW TARGET'}] no SQLITE_BUSY, locked or 5xx response appeared`);
  if (driftAfter === null) {
    line('    [NO DATA] reconciliation could not be read (is tools/reconcile present and DATABASE_URL free?)');
  } else {
    line(`    [${driftAfter ? 'PASS' : 'BELOW TARGET'}] post-run reconciliation reports ${driftAfter ? '0 drifting accounts' : 'DRIFT'}`);
  }
  line('');
  line('  WHAT THESE NUMBERS DO NOT MEAN');
  line('    1. THIS IS NOT A LATENCY MEASUREMENT. It reports throughput only; a latency');
  line('       distribution from a Node client needs the four-process calibration in');
  line('       tools/loadtest, and none of that applies to a count.');
  line('    2. THE POOL SIZE IS THE SERVER\'S, NOT THIS TOOL\'S. The criterion names 8 because');
  line('       db.rs sets it; this tool never opens the database except to seed and to run the');
  line('       reconciliation gate, both through the sqlite3 CLI.');
  line('    3. LOCAL NUMBERS ARE NOT THE DEPLOYMENT\'S. See docs/load-test.md for how to take');
  line('       those: a machine separate from the server, with the server pinned to --cpus=0.2.');
  line('');
  process.stdout.write(`${out.join('\n')}\n`);

  const result = {
    settled,
    offered: orderIds.length,
    settlementsPerSec: Number(perSec.toFixed(2)),
    wallMs,
    elapsedNs,
    concurrency,
    accounts,
    perAccount,
    published: { settlementsPerSec: PUBLISHED_SETTLEMENTS_PER_SEC, poolSize: 8, source: 'docs/benchmark.md#scenario-3' },
    statuses: Object.fromEntries(statuses),
    reconciliation: { before: driftBefore, after: driftAfter },
    verdicts: { throughputOk, everySettled, busyless, reconciliationOk: driftAfter === true, allOk },
  };
  if (args['--json']) {
    fs.writeFileSync(args['--json'], `${JSON.stringify(result, null, 2)}\n`);
    process.stdout.write(`\nsettle-bench: wrote ${args['--json']}\n`);
  }

  // Exported for the `finally` in the caller rather than read from a closure, because `main`
  // is where both are known and `process.exit` inside it would skip a `finally` in the SAME
  // function. Set BEFORE the exit so the cleanup has them.
  process.exitCode = allOk ? 0 : 1;
}

// The cleanup runs even when the measurement throws, so a failed run does not leave seeded
// accounts in the operator's listing. `orderIds` is resolved through a hoisted read because
// the seeding happens inside a try that can exit before main() finishes.
/**
 * Removes every row this run seeded.
 *
 * A BENCHMARK THAT LEAVES ROWS BEHIND CORRUPTS THE NEXT READING. Seeded accounts appear in the
 * operator's account listing and in the admin audit surface, and 50 of them per run is a
 * listing nobody can read. The tag is per-run and unique, so this can only ever delete its own.
 *
 * BEST-EFFORT, and the failure is REPORTED rather than swallowed: a cleanup that silently did
 * nothing would leave the next run's account count wrong with no explanation.
 */
function cleanup(dbPath, runTag) {
  if (!dbPath || !runTag) return;
  try {
    const script =
      `DELETE FROM ledger WHERE account_id IN (SELECT id FROM accounts WHERE id LIKE '00000000-0000-4000-8000-%');`
      + ` DELETE FROM topups WHERE order_id LIKE '${runTag}-%';`
      + ` DELETE FROM wallets WHERE account_id IN (SELECT id FROM accounts WHERE id LIKE '00000000-0000-4000-8000-%');`
      + ` DELETE FROM accounts WHERE id LIKE '00000000-0000-4000-8000-%';`;
    // STDIN, for the same reason the seed uses it: a cleanup script grows with the run.
    execFileSync('sqlite3', [dbPath], { encoding: 'utf8', input: script, stdio: ['pipe', 'pipe', 'pipe'] });
  } catch (err) {
    process.stderr.write(`settle-bench: WARNING - could not remove the seeded rows (${err.message}).\n`);
    process.stderr.write(`settle-bench:   accounts left behind have ids starting 00000000-0000-4000-8000-.\n`);
  }
}

// The cleanup runs whether or not the measurement threw, so a failed run does not leave 50
// seeded accounts in the operator's listing. `runTag` is stashed on `globalThis` by `main`
// because the seeding happens inside a `try` that can `process.exit` before `main` returns.
main()
  .then(() => {})
  .catch((err) => {
    process.stderr.write(`settle-bench: ${err && err.stack ? err.stack : err}\n`);
  })
  .finally(() => {
    cleanup(process.env.SETTLE_BENCH_DB, process.env.SETTLE_BENCH_TAG);
  });
