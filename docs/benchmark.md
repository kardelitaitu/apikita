# Server Benchmarking & Capacity Limits

Empirical methodology and performance targets to measure the actual operational limits of the ApiKita Rust gateway, specifically tuned for a **0.2 vCPU / 256MB–512MB RAM** Northflank container.

> Architecture: [`docs/architecture.md`](architecture.md) · Failover & Key Pool: [`docs/failover.md`](failover.md) · Data Model: [`docs/website/02-data-model.md`](website/02-data-model.md)

---

## 1. Objectives & Critical Limits to Test

We must answer five concrete questions through empirical testing:
1. **What is the peak Request-Per-Second (RPS) ceiling on 0.2 vCPU?**
2. **What is the maximum concurrent in-flight streaming capacity before RAM exceeds 256 MB or stream jitter occurs?**
3. **What is the database transaction ceiling under concurrent wallet debits and ledger appends?**
4. **How does the 100-key router perform when 10% of keys are throttled (HTTP 429) simultaneously?**
5. **Does the ledger remain 100% reconciled ($\sum \text{delta\_idr} \equiv \text{balance\_idr}$) under high concurrency?**

---

## 2. The Four Benchmark Scenarios

### Scenario 1: Hot-Path Key Authentication & Pre-Flight (CPU Bound)
* **Target Endpoint:** `POST /v1/chat/completions` (rejected at pre-flight or evaluated up to proxy boundary).
* **Workload:** 1,000 to 10,000 requests/second from multiple threads.
* **What it stresses:**
  - In-memory SHA-256 hashing speed.
  - In-memory key metadata cache (60s TTL lookup).
  - Pre-flight arithmetic and context window validation.
* **Pass Criteria:**
  - $> 5,000\text{ req/sec}$ throughput on 0.2 vCPU.
  - $p99 \le 2\text{ ms}$.

### Scenario 2: High-Concurrency SSE Streaming Passthrough (I/O & Memory Bound)
* **Target Endpoint:** Active streaming proxy pipeline.
* **Upstream Condition:** Mock upstream server simulating 30 tokens/second over 10 seconds (300 chunks per request).
* **Workload:** Incrementally ramp concurrent connections: $50 \rightarrow 100 \rightarrow 250 \rightarrow 500 \rightarrow 1,000$ active streams.
* **What it stresses:**
  - `tokio` async task scheduling and socket buffer allocation.
  - Memory consumption (verifying $< 35\text{ KB}$ per stream).
  - CPU usage under active chunk forwarding.
* **Pass Criteria:**
  - 500 concurrent streams maintained without dropped connections.
  - Memory consumption stays under $100\text{ MB}$.
  - CPU usage remains $\le 60\%$ on 0.2 vCPU.

### Scenario 3: Database & Ledger Contention (Database I/O Bound)
* **Target Endpoint:** `POST /webhooks/midtrans` and usage settlement `debit_usage_transaction`.
* **Workload:** 100 concurrent workers hammering wallet credits and proxy usage debits across 50 distinct accounts.
* **What it stresses:**
  - Single-writer serialisation on `topups` and `wallets` — `BEGIN IMMEDIATE` plus a
    conditional-`UPDATE` claim (the SQLite replacement for `SELECT ... FOR UPDATE`,
    which SQLite does not have).
  - Connection pool saturation (`max_connections = 8` on 0.2 vCPU — the value
    `db.rs`'s `init_pool` actually sets).
  - Append-only write throughput on `ledger` and `usage_daily` upserts.
* **Pass Criteria:**
  - Zero `SQLITE_BUSY` failures and zero write-transaction deadlocks.
  - Zero balance discrepancies: post-test reconciliation query returns 0 errors.
  - $> 100\text{ settlements/sec}$ with connection pool size = 8.

> **These two figures said `10` until they were corrected, and nothing could have caught it.**
> MEASURED: the code has never shipped `max_connections(10)` — `git log -S 'max_connections(10)'
> -- server/src/db.rs` returns no commit — and the plan that specified the SQLite migration
> (`plans/sqlite-migration.md`) states **8**, twice, alongside the contention measurement taken at 8.
> The benchmark binary does not set a pool of its own, so there is no "different scenario" reading:
> the number was simply wrong, and a pass criterion of *"> 100 settlements/sec at pool size 10"* was
> never achievable against the shipped configuration.
>
> **Why it survived.** This document is triaged in `doc_claims.rs` as *"a record of one measurement,
> not a contract"* — a reason about **line citations**, which is what that triage decides. A
> configuration number stated as fact is a different kind of claim, and the triage reason does not
> cover it. The nearest guard, `the_benchmark_doc_describes_a_command_the_binary_actually_has`,
> couples this file to the **CLI**, not to the config, so nothing was ever looking at this digit.
> `a_published_pool_size_is_the_pool_size_the_code_opens` now reads this line and the constant.


### Scenario 4: 100-Key Pool Under 429 Infiltration (Router Resilience)
* **Target Endpoint:** Proxy router with 100 simulated upstream keys.
* **Failure Injection:** Mock upstream randomly responds with HTTP 429 on 10% of requests.
* **What it stresses:**
  - Least-loaded key selection algorithm (`min_by_key`).
  - Granular 5-second per-key cooldown.
  - Client-side transparent retry within `max_key_attempts = 3`.
* **Pass Criteria:**
  - End-user success rate $\ge 99.9\%$.
  - Circuit breaker does not trip (endpoint remains closed/healthy).
  - Throttled keys automatically resume traffic after 5s cooldown.

---

## 3. Metrics Matrix & Thresholds

| Metric | Target (0.2 vCPU) | Warning Threshold | Critical Failure |
| :--- | :--- | :--- | :--- |
| **Max Concurrent Streams** | $\ge 250$ | $< 100$ | $< 50$ or OOM Crash |
| **In-Flight Stream Memory** | $< 35\text{ KB / stream}$ | $> 75\text{ KB / stream}$ | Total RAM $> 200\text{ MB}$ |
| **Key Validation Latency (p99)** | $\le 1.5\text{ ms}$ | $> 5.0\text{ ms}$ | $> 20\text{ ms}$ |
| **Streaming TTFT (Time to First Token)** | Added delay $\le 10\text{ ms}$ | $> 50\text{ ms}$ | $> 200\text{ ms}$ |
| **Ledger Drift** | **Strictly 0 IDR** | — | Any drift $> 0$ |
| **CPU Saturation at 200 req/s** | $\le 40\%$ | $> 75\%$ | $100\%$ (Throttling) |

---

## 4. Benchmark Tooling & Execution

We provide a dedicated benchmarking harness binary in `server/src/bin/benchmark.rs`:

```bash
cargo run --release --bin benchmark
```

**It takes no arguments**, and that is not an oversight to fix by adding a parser — it is a
different kind of tool from what this section used to describe. It is an **in-process capacity
suite**: one OS thread (`current_thread`, standing in for a 0.2 vCPU container) running four
synthetic scenarios — key-auth maths, Midtrans SHA-512 verification, 100-key pool routing under a
10% 429 rate, and concurrent SSE stream simulation at 500 and 1,000 streams. It never starts a
server and never opens the database.

> ~~`cargo run --release --bin benchmark -- --concurrency 250 --duration 60s --scenarios
> streaming,ledger`~~ — **the flags do nothing.** MEASURED: that exact command runs to completion,
> because `cargo` passes the arguments and the binary ignores them. The output is the fixed four
> scenarios, not the requested `streaming,ledger`. A documented invocation that silently succeeds
> while doing something else is worse than one that fails, which is why the line is struck rather
> than corrected.

> ~~The benchmark runner outputs: Summary table: Throughput (RPS), Latency percentiles (p50, p90,
> p95, p99), Error rate. Resource telemetry: Peak RSS RAM, CPU user/system time.~~ — **not
> produced.** The binary has no HTTP client, no latency histogram and no RSS/CPU sampling. What it
> actually prints per scenario is a line of `Throughput` in scenario-native units (`ops/sec`,
> `sigs/sec`, `req/sec`, `tokens/sec`), an average latency where the scenario has one, and a
> `[PASS]`/`[WARN]` verdict against a **hardcoded** target. Neither the units nor the thresholds
> are configurable.
>
> **It follows that these numbers are not a load test and must not be quoted as one.** An
> in-process loop over a hash function measures the hash function; it says nothing about latency
> under real request concurrency, connection handling, or the SQLite write path. Real figures come
> from the drill and the reconcile tools against a running stack.

> ~~Automated Ledger Invariant Check~~ — **not implemented.** This line claimed the
> benchmark runs `verify_wallet_reconciliation` across all modified accounts at
> completion. It does not: `bin/benchmark.rs` never calls that function, and the only
> callers of it are its own tests in `db.rs`. The money invariant is checked by
> `tools/reconcile/reconcile.sh` against the real database, which is where it belongs —
> a benchmark process holds a synthetic wallet that the reconcile query would not
> recognise. The claim is struck rather than deleted so the next reader knows it was
> checked, not missed.

**A second defect, in the binary itself and fixed with this note.** Scenario 4 printed
`[PASS] 0.2 vCPU easily handles 500 concurrent active streams` from a **literal 500**, so the
1,000-stream run reported its result under the 500-stream heading. The message now interpolates the
count that ran. The lesson is the same one this section keeps recording: a number written twice
drifts, and the copy nobody re-reads is the one a reader trusts.
