# Load test — what to run, where, and what the numbers mean

`docs/benchmark.md` publishes six metrics-matrix rows. **Four are verified in code.** Two are not, and
cannot be by the in-process binary:

| row | target | why the in-process binary cannot own it |
| --- | --- | --- |
| Key Validation Latency (p99) | $\le 1.5$ ms | the in-process binary has no HTTP client and no latency histogram — see "Measuring the p99" below for the client that does |
| CPU Saturation at 200 req/s | $\le 40\%$ | the binary has no RSS/CPU sampling |

The document already says this and says it plainly: *"these numbers are not a load test and must not be
quoted as one... Real figures come from the drill and the reconcile tools against a running stack."* This
page is the missing half — **how to take those two measurements.**

## What you need before starting

| thing | why | how to check |
| --- | --- | --- |
| The server running | the whole point | `curl -s localhost:8080/health` returns `{"status":"healthy","database":"connected"}` |
| A migrated database | the request path reads it | `DATABASE_URL=... cargo run --bin migrate` |
| **A machine with the same CPU as production** | the target is stated per 0.2 vCPU | see "Why the box matters" below |
| A load generator | concurrency, not a loop | [`tools/loadtest/`](../tools/loadtest/README.md) — `node tools/loadtest/loadtest.js --help` |
| `ps`/`/proc` access to the server PID | CPU sampling | the harness takes `--pid`; the same host is the simple case |

**Do not run this against production.** The generator writes wallet rows through the proxy path. Use a
restored copy — `tools/drill/drill.sh` produces exactly that, and `tools/reconcile/reconcile.sh` is how
you confirm the copy is sound before and after.

## Why the box matters, and this is the part people skip

Both targets are stated **for 0.2 vCPU**. A p99 measured on a 16-core workstation is not evidence about
the deployment, because the published bar assumes the process is CPU-starved. MEASURED in an earlier
round: the whole benchmark suite runs on **one OS thread** precisely to model that container.

So: **run the generator on a different machine from the server**, and pin the server to 0.2 vCPU
(`--cpus=0.2` under Docker, or `cpulimit`). A generator sharing the CPU it is measuring reports its own
contention as the server's latency.

## Measuring the p99 — and why a Node client cannot

**The 1.5 ms bar is below the floor of a Node load generator, so no amount of hardware fixes the Node
route.** MEASURED (`tools/loadtest/loadtest.js:40-57`) against a handler doing NO work:

| generator | p50 | p95 | p99 |
| --- | --- | --- | --- |
| one Node process | 1.208 ms | 2.095 ms | **3.172 ms** |
| four Node processes | ~0.59 ms | ~1.5 ms | **~2.4 ms** |

A no-op handler answering from memory, reported at 2.4 ms: that is the **instrument**, not the server.
`tools/loadtest/` handles this honestly — it measures its own floor first and refuses a verdict when the
floor is at or above the bar — which is why this row is still blank rather than filled with a
fabrication.

**A 16-core host and Docker both fail to fix this**, and it is worth saying why because the reflex is to
reach for hardware: the constraint is the *measuring instrument*, not capacity. A client whose own floor
exceeds the bar cannot resolve that bar on any machine.

### The native client

`server/src/bin/loadgen.rs` is a Rust HTTP client with no event loop, no GC and no dependency on a
millisecond-resolution timer between the request and the reading. Build and use it:

```sh
cargo build --bin loadgen

# ALWAYS measure the floor first, at the SAME concurrency as the run you intend to make.
./target/debug/loadgen --base-url http://127.0.0.1:8080 --path /health --measure-floor --concurrency 8

# Then the real run.
./target/debug/loadgen --base-url http://127.0.0.1:8080 --path /v1/chat/completions --rate 200 --concurrency 8
```

It refuses a verdict when its own floor is at or above the target, retains every sample for a real
percentile rather than a mean, and **prints the warm-up it discarded** — so a figure it produces can be
attributed rather than trusted.

**The floor must be measured at the SAME CONCURRENCY as the run**, and the reason is not a tidy scaling
curve — it is that the floor is dominated by **variance**. The first table published here claimed a
monotonic rise with concurrency (1.131 / 1.290 / 1.231 / 1.377 / **1.892** ms at concurrency 1/4/8/16/32),
and that was wrong.

**MEASURED, and it retracts the row above:** on the same host, 7 trials at each concurrency gave medians
of **1.30 / 2.06 / 2.07 / 1.25 / 1.42 ms** — noisy, non-monotonic, and **concurrency 32 landed UNDER the
1.5 ms bar**. Individual runs swung **1.14–7.77 ms for an identical configuration**. The quoted 1.892 ms
was a one-shot artifact, and two conclusions drawn from it are withdrawn: the floor does not rise
reliably with concurrency, and "past concurrency 16 it crosses the bar" was not reproducible.

**What replaces it is a rule rather than a table: a single run on this host is a coin flip, so every
figure recorded here must be a median of repeated interleaved trials, with the host's CPU stated.** The
table above was taken while a crypto-miner was running — the same command read **13.7–23.7 ms** under
that load and ~1.4 ms quiet, a 20× swing from the environment alone.

**AND THE FLOOR'S COMPOSITION MATTERED MORE THAN ITS LEVEL.** Instrumenting the original generator
split each request slot into *timer lateness* versus *actual RTT*: the sleep-timer overshoot was
**p50 ~7–8 ms / p99 ~15–16 ms** — an order of magnitude larger than the latency being measured, since
RTT itself was p99 1.34 ms at concurrency 8. It was never request cost. It was N independent per-worker
sleep schedules waking late and colliding, which is why the figure swung so wildly.

The generator was subsequently rebuilt on **one shared release schedule** (a token bucket, so lateness
does not compound with N) over **pre-connected `TcpStream`s** instead of a `reqwest` call per request.
The author reports a floor of **0.48–0.78 ms, flat across concurrency 1→32**.

**THAT FIGURE IS NOT VERIFIED IN THIS DOCUMENT.** It was measured on a host the author's own report
describes as contaminated, and I could not reproduce it: every attempt since has found the machine
carrying unrelated load (a separate project's build process holding ~18 cores). The instrument refuses
to take a floor reading on a busy host, which is the correct behaviour and also why the claim stands
unconfirmed. **Treat it as reported, not established, until it is measured on a quiet box with the CPU
stated.**

## The run

Three phases, because one number cannot distinguish the three failures it could be:

1. **Warm-up.** Drive the target for ~30s and discard it. The first requests pay for connection setup
   and cache population; including them measures start-up, not steady state.
2. **Steady state.** The measurement window. This is where p50/p95/p99 come from.
3. **Saturation.** Ramp until the p99 crosses the target. **The number worth recording is the throughput
   at which it crosses**, not the peak throughput — a service that meets its latency at 200 req/s and
   falls over at 2,000 is a different product from one that does not.

## What the two numbers mean, and what they do not

**The p99 is about the request path, not the model.** What is being timed is key validation, the hold,
the upstream call and the settlement — the gateway's own work. Upstream latency dominates the
end-to-end figure and is **not** what the 1.5 ms target is about. Report the two separately or the
number is uninterpretable.

**The CPU figure is only meaningful with the throughput it was taken at.** "40% CPU" alone says nothing;
`40% at 200 req/s` is the claim the document makes, and the row states it that way for that reason.

**An idle measurement is not a measurement.** CPU read while nothing is in flight is the baseline, and
reporting it as the figure would be the same defect class as the `[PASS]` that could not fail: a number
presented as a result that nothing in the run produced.

## Recording the result

Fill this in with **real** values and note the date, the box, and the vCPU pin — a figure without those
three is not reproducible and the next reader cannot tell whether it still holds.

| metric | target | measured | box | vCPU | date |
| --- | --- | --- | --- | --- | --- |
| Key validation p99 | $\le 1.5$ ms | *not yet measured* | — | — | — |
| CPU at 200 req/s | $\le 40\%$ | *not yet measured* | — | — | — |

The two rows above are **deliberately empty**, and stay empty for the reason given below. What the
harness HAS produced so far is in a different table, because it is a different claim:

**Measured on the DEVELOPMENT host, which is NOT the deployment shape.** Windows 11 Pro, 32 logical
cores, 61.7 GB RAM, node v22.23.2, release `apikita-server`, loopback, against a migrated scratch
SQLite. `POST /v1/chat/completions` with a well-formed but unknown Bearer key — refused `401` at key
lookup, so no upstream, no wallet and no write path. Taken with `tools/loadtest/loadtest.js`; the
full transcript, the falsification commands and the limits are in
[`tools/loadtest/README.md`](../tools/loadtest/README.md).

| run | offered | achieved | p99 | server cores | % of 0.2 vCPU |
| --- | --- | --- | --- | --- | --- |
| published shape | 200 req/s | 200 req/s | **1.559 ms** | 0.045 | **22.28%** |
| published shape | 200 req/s | 200 req/s | **1.145 ms** | — | — |
| saturation | tight | 78,710 req/s | 1.443 ms | 11.681 | 5,840% |

**Do not put those in the table above.** They do not satisfy this page's own rules — the generator
shared the box with the server, and the server was not pinned to 0.2 vCPU — so they are not evidence
about the deployment. What they ARE is evidence about the INSTRUMENT, and that is worth recording:

* **A p99 that straddles the bar across runs at identical load is not resolving the bar.** These runs
  land at 1.145 ms and 1.559 ms against a 1.5 ms target, and the difference between them is the
  host's loopback client, not the server. The harness now measures its own client-side floor first and
  **refuses to report a p99 verdict when that floor is at or above the bar** — MEASURED, one Node
  process reported a p99 of **3.172 ms against a handler doing no work at all**.
* **The CPU row is a normalisation, not a reading.** It is a percentage, so it needs a denominator,
  and the only one this document's matrix is coherent against is the `0.2 vCPU` its header names. The
  harness divides by that, prints which denominator it used, and prints core-equivalents beside it.

So the two rows above remain blank **not** because nothing was measured, but because what was measured
cannot answer the question they ask. A figure from the wrong box in those cells would be worse than the
gap — and now there is a tool whose output says which of the two it is.


## If the measurement misses the target

That is a **finding**, not a failure of the harness. Record it, then look at where the time goes before
changing anything — the document's warning and critical columns ($> 5.0$ ms and $> 20$ ms) exist so that
a miss has a severity rather than just a sign.
