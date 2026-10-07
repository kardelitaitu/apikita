# loadtest

The real HTTP load test — the measurement two of `docs/benchmark.md`'s six metrics-matrix rows
publish and nothing in this repository could take.

```
node tools/loadtest/loadtest.js --base-url http://127.0.0.1:8080 --pid 1234 --rps 200 --seconds 30
```

Exit `0` every verdict passed, `1` a verdict is `[BELOW TARGET]`, `3` a prerequisite is missing (no
`--base-url`, nothing answering, no `--pid`, or a sampling interval below this host's CPU quantum).

`sh tools/loadtest-check/check.sh` gates it. Exit `0` every assertion holds, `1` a violation, `3`
`node` is missing or a file is unreadable.

## Why this exists

`docs/benchmark.md` publishes six rows. **Four are verified in code. Two were verified by nothing:**

| row | target | who owned it before |
| --- | --- | --- |
| Key Validation Latency (p99) | `<= 1.5 ms` | **nobody** |
| CPU Saturation at 200 req/s | `<= 40%` | **nobody** |

`server/src/bin/benchmark.rs` cannot produce either, and says so about itself — it is an in-process
capacity suite on one OS thread with no HTTP client, no latency histogram and no sampling, and the
document's §4 records that *"these numbers are not a load test and must not be quoted as one."* So
the document published two bars, an operator read them, and no code measured them. That is the same
defect class as the `max_connections = 10` figure this repository already records: not a wrong
number, a number with **no owner**. This tool is the owner.

## The finding that shapes the whole design

The obvious harness — one Node process, `http.Agent({ keepAlive: true })`, time each response — was
built and measured first, and it is wrong in a way that is invisible without a control.

**MEASURED, against a handler doing no work at all (`POST /health`):**

| generator | p50 | p95 | p99 |
| --- | --- | --- | --- |
| one Node process | 1.208 ms | 2.095 ms | **3.172 ms** |
| four Node processes | ~0.59 ms | ~1.5 ms | **~2.4 ms** |

The published bar is `<= 1.5 ms`. **One process reports a p99 roughly twice the bar against a server
that answered in microseconds** — a confident, plausible, entirely fabricated `[BELOW TARGET]`. The
floor is per *process*, because a Node client is one event loop and its scheduling cost scales with
the requests that loop drives.

So the generator is a parent plus N worker **processes** (`loadtest-worker.js`), each reducing to its
own histogram, and the parent merges them. **And the fix is incomplete, which is reported rather than
papered over:** even four processes measure a ~2.4 ms p99 against a no-op handler. The tool therefore
measures that floor on every run and **refuses to report a p99 verdict when the floor is at or above
the bar** — `[UNRESOLVABLE]`, which is `ok: false` and a non-zero exit, so "I declined to compare"
cannot be read as "it passed".

## What it measures, and what it deliberately does not

**The p99 verdict is a round trip, not the server's key-validation time.** The published row describes
compute — SHA-256, the 60s key-metadata cache lookup, pre-flight arithmetic. What this times also
contains the client's request build, the kernel's TCP path at both ends, the server's accept and body
parse, and any queueing. The floor printed on every run *is* this host's share of it. Separating the
two needs a timer inside the server; without one, a `[PASS]` is a statement about the round trip and
the output says so in a numbered block on every run, not only on the runs that look wrong.

**The CPU percentage is a normalisation, not a reading.** A percentage needs a denominator and the
document states one in its own header — a `0.2 vCPU` Northflank container, an explicit fifth of a
core. The row's warning (`> 75%`) and critical (`100%, "Throttling"`) columns are container-throttling
semantics, which is only coherent against a core *share*. So the tool divides by 0.2 vCPU, **names the
denominator it used**, and prints the raw core-equivalents beside it so a reader who disagrees can
redo the division. It does not pick whichever denominator makes the row pass.

**Neither number is the deployed system's.** The matrix is denominated in 0.2 vCPU; a run on a 32-core
host measures a different machine, and the tool prints the machine facts so a reader can see that.

## Measured on the build host

Windows 11 Pro, 32 logical cores, 61.7 GB RAM, node v22.23.2, `rustc 1.98.0`, release
`apikita-server` against a migrated scratch SQLite on loopback. Hot path:
`POST /v1/chat/completions` with a **well-formed but unknown** Bearer key, which is refused with a
deterministic `401 unauthenticated` at key lookup — SHA-256 plus the metadata-cache miss, no upstream,
no wallet, no write path.

| run | offered | achieved | p50 | p95 | p99 | server cores | % of 0.2 vCPU | verdicts |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| published shape | 200 req/s | 200 req/s | 0.665 ms | 0.908 ms | **1.559 ms** | 0.045 | **22.28%** | p99 `[BELOW TARGET]`, CPU `[PASS]` |
| published shape | 200 req/s | 200 req/s | 0.719 ms | 1.035 ms | **1.145 ms** | — | — | p99 `[PASS]` |
| saturation | tight | 78,710 req/s | 0.777 ms | 1.145 ms | **1.443 ms** | 11.681 | 5,840% | p99 `[PASS]`, CPU `[BELOW TARGET]` |

Peak RSS stayed at **11.3–18.7 MB** across all of it.

The p99 **landing on both sides of 1.5 ms across runs at the same load** is itself the finding, and it
is why the floor is printed: at 1.1–1.6 ms against a 1.5 ms bar, this host's loopback client is the
dominant term, and the difference between these runs is not the server. **The p99 row should be read
as "this host's round trip straddles the bar", not as "the server passes" or "the server fails".**
Note also that the CPU row and the p99 row are taken at *different* loads above — `<= 40%` is a claim
about 200 req/s, and 5,840% is the saturation figure, which is a different quantity and is labelled
as one in the output.

**These are not the deployment's numbers and must not be quoted as such.** See
`docs/load-test.md` for how to take those: a machine separate from the server, with the server pinned
to `--cpus=0.2`.

## Falsifying the verdicts

A verdict that cannot be made to fail on demand is a verdict nobody has tested. `--target-p99-ms` and
`--target-cpu-pct` override the published constants, so the same measurement flips:

```
# p99 1.145 ms at the published 1.5 ms bar -> PASS
node tools/loadtest/loadtest.js --base-url http://127.0.0.1:18099 --rps 200 --pid <pid> --target-p99-ms 1.5
# ... and at a 0.5 ms bar -> BELOW TARGET. MEASURED.
node tools/loadtest/loadtest.js --base-url http://127.0.0.1:18099 --rps 200 --pid <pid> --target-p99-ms 0.5
```

MEASURED, both directions, on the same server:

| override | measurement | verdict |
| --- | --- | --- |
| `--target-p99-ms 2.0` | p99 0.840 ms | `[PASS]` |
| `--target-p99-ms 0.5` | p99 0.908 ms | `[BELOW TARGET]` |
| `--target-cpu-pct 5` | 39.06% | `[BELOW TARGET]` |

The gate performs the same flip mechanically on every run, by calling the verdict functions directly —
see `verdict-probe.js` below.

## The gate: `tools/loadtest-check/`

`tools/benchmark-verdicts/check.sh` already asserts that every `[PASS]` the **benchmark binary** prints
is earned, and it does it by **parsing the source**. Both properties are wrong here: this tool is a
different file, and **a source-shape rule can be satisfied by code that does not work.** MEASURED: the
first pid sampler built for this harness was rejected by the Windows PowerShell parser and returned
`null` from every call — which the harness reported downstream as `[NO DATA] the server process was
never sampled`, for a healthy server, forever. It would have passed every source-shape check ever
written.

So this gate **runs things**:

| assertion | how | why it is not a source read |
| --- | --- | --- |
| both thresholds equal `docs/benchmark.md`'s rows | reads both sides | a drift in either direction fails |
| `verdictP99` and `verdictCpu` **flip** | calls them | a pinned verdict fails at the call |
| no measurement, and an unresolvable one, never pass | calls them | the "cannot fail" defect, asserted |
| the pid sampler reads a live process and its CPU moves | spawns a spinner | **this is the assertion that catches the null sampler** |
| the two histogram boundary arrays are identical | extracts and compares | the duplication cannot drift |
| an unknown flag is refused | runs the parser | a typo cannot silently measure something else |

### Mutations it was tested against

MEASURED, all ten caught, gate green before and after:

| mutation | result |
| --- | --- |
| `verdictP99` pinned to `return true \|\| ...` | **caught** |
| `verdictCpu` pinned to `return true \|\| ...` | **caught** |
| `NO DATA` returns `ok: true` | **caught** |
| `UNRESOLVABLE` returns `ok: true` | **caught** |
| `PUBLISHED_CPU_TARGET_PCT` 40 → 41 | **caught** |
| `PUBLISHED_CPU_RATE_RPS` 200 → 250 | **caught** |
| `PUBLISHED_P99_LATENCY_MS` 1.5 → 1.6 | **caught** |
| the worker's histogram boundaries drift (×1.08 → ×1.09) | **caught** |
| the worker's top boundary becomes `Infinity` (JSON sends `null`) | **caught** |
| the unknown-flag refusal is removed | **caught** |

The Rust half of the threshold coupling is
`the_loadtest_thresholds_are_the_thresholds_the_document_publishes` in `server/src/doc_claims.rs`,
which also asserts the two constants are not **swapped** — an equality check alone cannot catch a
`40` read as a latency and a `1.5` read as a percentage. MEASURED against three mutations there
(each constant drifted, and the pair exchanged), all caught.

## What it does NOT catch

- **A threshold made vacuous.** `--target-p99-ms 999999` still looks like a comparison. The gate
  asserts the *document's* figure is the one compiled in, not that the figure is a good one.
- **An unresolvable p99 is reported, not fixed.** On a host where the client floor exceeds the bar,
  this tool says so and stops. It cannot take the measurement the row wants; only a server-side timer
  can, and that does not exist.
- **Capacity.** This is one host's worth of client processes. The server is never driven to its limit,
  so every core-equivalents figure is a **lower bound** on per-request cost, not a ceiling.
- **The non-hot-path scenarios.** `docs/benchmark.md` Scenarios 2–4 (SSE, ledger, key-pool 429) are
  untouched by this tool.

## Related

- [`docs/benchmark.md`](../../docs/benchmark.md) — the matrix, and §4's own statement that the
  in-process binary is not a load test.
- [`docs/load-test.md`](../../docs/load-test.md) — how to take the two measurements against a real
  deployment shape, and why the box matters.
- [`tools/benchmark-verdicts/`](../benchmark-verdicts/README.md) — the sibling gate for the in-process
  binary's `[PASS]` lines.
