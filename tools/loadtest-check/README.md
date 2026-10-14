# loadtest-check

Proves the load test's verdicts are earned by measurement, and that its **calibration** works.

```
sh tools/loadtest-check/check.sh
```

Exit `0` every assertion holds, `1` a violation, `3` `node` is missing or a file is unreadable.

## Why this exists

[`tools/benchmark-verdicts/`](../benchmark-verdicts/README.md) already asserts that every `[PASS]` the
**benchmark binary** prints is earned — and it does it by **parsing the source** of one file. Both
properties are wrong for this tool:

- The load test is `tools/loadtest/loadtest.js`, which that gate never opens.
- **A source-shape rule can be satisfied by code that does not work.**

MEASURED, and it is the reason this gate exists rather than a widened version of the other one. The
first pid sampler built for the load test was rejected by the **Windows PowerShell parser** — three
successive attempts, each for a different reason (`chr(32)` is a VB function that PowerShell does not
have; a line continuation assembled by JS concatenation became PowerShell's `++` operator; escaped
quotes did not survive the shell boundary). Every one returned `null` from every call, and the harness
reports a null sample downstream as:

```
[NO DATA] the server process was never sampled, so nothing was compared
```

— for a **healthy server**, on every run, forever. That text reads as the tool declining to measure.
It would have passed every source-shape check ever written, and it is exactly the class of defect this
repository keeps finding. **So this gate runs things.**

## What it asserts

| # | assertion | how, and why not a read |
| --- | --- | --- |
| 1 | both thresholds equal `docs/benchmark.md`'s rows | both sides are read; a drift either way fails. Also the row's `at N req/s`, which the tool tells an operator to re-run at |
| 2 | `verdictP99` and `verdictCpu` **flip** when the measurement crosses the bar | the functions are **called**. Both directions, because a verdict pinned to PASS and one pinned to FAIL are the same defect with the sign flipped |
| 3 | exactly on the bar passes | the row reads `<= 1.5`, not `< 1.5`; an off-by-one would reject a run that met the target |
| 4 | no measurement, and an **unresolvable** one, never report `ok` | the "a PASS that cannot fail" rule, asserted by execution |
| 5 | `percentileFromHistogram` reads back a value known by construction | a hand-built histogram of 1000 samples at 0.5 ms |
| 6 | the merge is a **sum** | a merge that took a maximum would still look plausible; the total catches it |
| 7 | **the pid sampler reads a live process and sees its CPU move** | a spinner is spawned and its burn is observed — **the assertion that catches the null sampler** |
| 8 | the two histogram boundary arrays are identical | they are duplicated across the parent/worker process boundary; a difference mis-places the tail without changing the total |
| 9 | an unknown flag is refused | `docs/benchmark.md` records a documented invocation whose flags were silently ignored — a command that SUCCEEDS while doing something else |

The helper probes (`verdict-probe.js`, `sampler-probe.js`, `bounds-probe.js`) are separate files rather
than inline `node -e` in `check.sh`, and each split is a fix rather than tidiness:

- **`verdict-probe.js`** — the assertions contain `$(...)`-shaped JavaScript, and in a plain `sh`
  command substitution the parenthesis is a **shell syntax error before node runs**. MEASURED: it
  failed loudly. A version that happened to parse would have run an empty program and exited 0,
  reporting every verdict as earned.
- **`sampler-probe.js`** — runs the real sampler, which is not exported (it is Windows-specific), so
  the probe loads the module's own sampler block and drives it.
- **`bounds-probe.js`** — balances braces rather than slicing at `)();`. MEASURED: the slice produced
  an expression with a paren unclosed and stderr was piped to `/dev/null`, so the gate reported
  "could not read the boundary arrays" — an error naming the **comparison** when the failure was in
  the **extraction**. `server/src/doc_claims.rs` records the same lesson in its own header: enforcing
  this needs brace matching, "which is a parser rather than a matcher".

## Mutations it was tested against

MEASURED, all ten caught, gate green before and after:

| mutation | result |
| --- | --- |
| `verdictP99` pinned to `return true \|\| measuredMs <= targetMs` | **caught** |
| `verdictCpu` pinned to `return true \|\| pct <= targetPct` | **caught** |
| the `NO DATA` verdict returns `ok: true` | **caught** |
| the `UNRESOLVABLE` verdict returns `ok: true` | **caught** |
| `PUBLISHED_CPU_TARGET_PCT` 40 → 41 | **caught** |
| `PUBLISHED_CPU_RATE_RPS` 200 → 250 | **caught** |
| `PUBLISHED_P99_LATENCY_MS` 1.5 → 1.6 | **caught** |
| the worker's histogram boundaries drift (×1.08 → ×1.09) | **caught** |
| the worker's top boundary becomes `Infinity` (JSON carries it as `null`) | **caught** |
| the unknown-flag refusal is removed | **caught** |

## What it does NOT catch

- **A threshold made vacuous.** `--target-p99-ms 999999` is still a comparison. The gate asserts the
  *document's* figure is compiled in, not that the figure is a good one — the limitation
  `benchmark-verdicts` records in the same words.
- **The sampling interval's adequacy on a slower host.** The tool measures the host's CPU quantum at
  runtime and refuses an interval below four times it, but `check.sh` does not reproduce that on a
  machine it does not own.
- **Anything about the p99 on a platform where the sampler does not apply.** The sampler reads
  `Win32_Process`, so the assertion is **SKIPPED on non-Windows** — loudly, with the reason printed. A
  silent skip is a green tick over nothing, which is the failure mode this gate exists to avoid.
- **Whether the load test's numbers are good.** This gate proves the verdicts are *earned* and the
  *calibration works*. It says nothing about whether the server is fast.

## Related

- [`tools/loadtest/`](../loadtest/README.md) — the harness itself, the measured numbers, and the
  client-side latency floor that shapes its design.
- [`docs/benchmark.md`](../../docs/benchmark.md) — the matrix these two thresholds come from.
- [`server/src/doc_claims.rs`](../../server/src/doc_claims.rs) — the Rust half,
  `the_loadtest_thresholds_are_the_thresholds_the_document_publishes`.
