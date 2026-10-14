# settle-bench

Drives the **settlement** path and reports settlements per second — the third pass criterion of
`docs/benchmark.md` Scenario 3, which had no owner.

```
node tools/settle-bench/settle-bench.js --base-url http://127.0.0.1:8080 \
  --server-key <midtrans server key> --db server/data/server.db \
  --accounts 50 --per-account 4 --concurrency 16
```

Exit `0` every criterion met, `1` one not met, `3` a prerequisite is missing (`--base-url`,
`--server-key`, `--db`, a non-integer count, or a seed the `sqlite3` CLI could not apply).

## Why this exists

Scenario 3 publishes three criteria:

```
zero SQLITE_BUSY failures and zero write-transaction deadlocks
zero balance discrepancies (post-test reconciliation returns 0)
> 100 settlements/sec with connection pool size = 8
```

**Two of the three appeared in no file except that document line.** `server/src/doc_claims.rs`
pins the scenario BY NAME as the one the harness cannot run — *"its criteria are unowned"* — and
that is an honest record rather than a fix. This closes the third criterion, the one with no
other owner. The other two are owned elsewhere: reconciliation by `tools/reconcile` (the launch
Gate 2 money check), and SQLITE_BUSY by `db::busy_timeout`, whose doc comment records the
measurement behind the value.

## Why it is not a `--path` on `tools/loadtest`

That harness sends **one fixed body** from several worker processes, which is exactly right for
a latency distribution and impossible here: every webhook must carry a **different `order_id`**
and a signature computed over that specific body, because the unique `order_id` is the
idempotency key the credit is keyed on. A replay of one body would settle once and return
`already_settled` forever after — a throughput figure for a database doing nothing.

## What it does, in order

1. **Seeds** pending top-ups across N distinct accounts, through the `sqlite3` CLI. Distinct
   accounts matter: one account serialises on a single wallet row, which measures row
   contention rather than the single-writer property Scenario 3 is about.
2. **Reads reconciliation before**, through the shipped `tools/reconcile/reconcile.sh` rather
   than a query written here. A second copy of that invariant would be the duplicate-rule that
   drifts.
3. **Drives** signed settlement notifications at a chosen concurrency.
4. **Reads reconciliation after**, so "no drift" is a delta rather than a coincidence.
5. **Deletes every row it seeded**, in a `finally`, so a repeated run does not accumulate
   accounts in the operator's listing. A cleanup that fails says so rather than passing.

## The measured run

Ran against a local server with the shipped pool size, 20 accounts at 3 pending top-ups each,
60 distinct settlements at concurrency 8:

```
    Settlements        : 60 of 60 offered
    Wall time          : 0.29 s at concurrency 8
    Settlements/sec    : 209.1
    Published bar      : > 100 settlements/sec, pool size = 8
```

ALL FOUR CRITERIA PASS, and three of them are the tool's verdicts rather than a reading: the count
against the published bar, every offered order settling exactly once, no `SQLITE_BUSY`/locked/5xx in
any response, and a post-run reconciliation delta of 0. A second run reported **reconciliation
before: clean**, which is what the `finally` cleanup is for - a repeatable tool that accumulates
accounts is a tool whose second measurement is not the same measurement.

### What this figure does and does not establish

It establishes the criterion has an owner that RUNS, which is the whole point of the tool: before it
landed the figure existed in exactly one place, a document line, and `doc_claims.rs` pinned the
scenario by name as the one the harness cannot run. It does NOT establish a capacity number for a
deployment - this is a dev-profile binary on a developer machine with no `--cpus` pin, which is the
condition `docs/load-test.md` describes.

Two figures from two runs on the same machine (209.1 then 178.6 at a smaller account count) are
**a tool that reports a measurement, not a constant**. Anyone quoting this README's number as a
capacity should quote the run conditions with it.

## What it deliberately does not do

- **No latency distribution.** That is `tools/loadtest`'s job, and its four-process calibration
  warnings do not apply to a count.
- **No pool of its own.** The criterion names `8` because `db.rs` sets it; this tool never opens
  the database except to seed and to reconcile, both through the `sqlite3` CLI.
- **No claim about the deployment.** See [`docs/load-test.md`](../../docs/load-test.md) for how to
  take those numbers: a machine separate from the server, with the server pinned to `--cpus=0.2`.

## The defect it found on its first run

Driving the settlement path with a seeded order whose `topups.id` was not a UUID made the server
**panic inside the handler task**: the connection closed with no HTTP response and no log line.
`topups.id` is `TEXT PRIMARY KEY` with no foreign key and no format constraint, so the value is
storable, and `Row::get::<Hyphenated, _>` panics decoding it. Four sites in the crate used
`try_get` and seventeen used `get`, and the difference was invisible until something exercised it.

It is fixed — a malformed id is now an `AppError::Database`, answered as a logged 500 that names
the order — and `a_non_uuid_topup_id_is_an_error_rather_than_a_panic` pins it, mutation-verified.
