# benchmark-verdicts

Every `[PASS]` the benchmark binary prints must be earned by a comparison.

```
sh tools/benchmark-verdicts/check.sh
```

Exit `0` every verdict is earned, `1` a literal verdict, `3` `node` is missing or the source is
unreadable.

## Why this exists

MEASURED over two rounds: **three of the benchmark's four scenarios printed `[PASS]` from a fixed
string that no measurement could change.**

| scenario | the verdict, as written | could it fail? |
| --- | --- | --- |
| 3 — 100-key pool | `[PASS] 100-key router absorbs 10% throttles without client failure` | **no** — printed beside `Success Rate: 37.76%`, against a documented target of `>= 99.9%` |
| 2 — Midtrans SHA-512 | `[PASS] Instantaneous webhook validation` | **no** — "instantaneous" is an adjective doing a number's job |
| 4 — SSE streams | `[PASS] 0.2 vCPU easily handles {} concurrent active streams` | **no** — the `{}` is the scenario's *input*, so the verdict clause is fixed text |
| 1 — key auth | `[PASS] Exceeds 0.2 vCPU threshold` / `[WARN] Below target` | **yes** — branches on `ops_per_sec` |

And one printed **result** was not a measurement:

```rust
println!("    Estimated Socket RAM: {:>10.2} MB (well within 256MB limit)",
    (concurrency * 35) as f64 / 1024.0
);
```

Both operands are constants, and `35` is **the document's own pass criterion** (`< 35 KB / stream`).
The harness took the target, multiplied it by its input, and printed the product as a passing
reading. Nothing about the run entered it — not the bytes the tasks accumulated, not elapsed time,
not RSS.

## Why a guard rather than a convention

The `benchmark` binary has **no test coverage** — no `#[test]`, no doctest — and the only check
coupling it to anything (`the_benchmark_doc_describes_a_command_the_binary_actually_has`) reads its
**CLI**, never its output. Three of four scenarios drifted the same way with nothing watching.

## The rule

A verdict line is **earned** if either:

- it interpolates a format placeholder, so the printed text carries a computed value; **or**
- it is one arm of an `if`/`else` whose condition compares something numerically.

The key-auth `[PASS]` has no placeholder and is still correct, because line 83 branches on
`ops_per_sec` — which is why the rule has two arms rather than one.

## What it does NOT catch, and this was measured

The tool was falsified against its own subject. Two mutations survive, and **the second was
mis-described in an earlier version of this file** — the correction matters more than the admission.

- **A threshold made vacuous: `if ops_per_sec >= MIN_SIGS_PER_SEC` → `>= 0` is not caught.** `>= 0`
  is still a comparison; the rule cannot tell a meaningful threshold from a meaningless one.

- **An `else` arm made unreachable is not caught, when the arm's verdict carries its value.** This was
  written as *"deleting an `else` arm is not caught"*, which overstates the gap in one direction and
  understates it in another. MEASURED, mutation by mutation:

  | mutation | verdict affected | caught? |
  | --- | --- | --- |
  | `} else {` → `} if false {`, key-auth | `[WARN] Below target` — **no value in it** | **caught** |
  | `} else {` → `} if false {`, signatures | `[BELOW FLOOR] {ops_per_sec} ...` | not caught |
  | `} else {` → `} if false {`, pool | `[BELOW TARGET] success rate {success_pct:.2}%` | not caught |

  The distinction is real: a verdict that **carries its measurement** is still earned under rule 1, so
  barring it does not make it a literal. What the mutation breaks is **reachability** — the verdict
  still prints, still carries the number, and can no longer be reported. That is a **different defect**,
  and catching it needs reachability analysis this tool does not do.

  So the honest statement is: **this catches a verdict that cannot change; it does not catch a verdict
  that can change but can no longer be reached.** The earlier wording implied the tool was weaker than
  it is on the first count and stronger than it is on the second.

What it catches is the shape that **actually occurred three times**: a verdict with no value and no
branch anywhere near it. That is a narrow rule, and being narrow is deliberate — a rule that guessed
at threshold quality would fire on correct code, the failure mode this repository has now recorded in
four separate tools.

It also cannot tell whether the value a verdict interpolates is the *relevant* measurement. Scenario
4's `[PASS] {concurrency} concurrent streams finished` satisfies the rule by interpolating its input;
what makes it a verdict is the comparison against `MIN_STREAMS_TARGET` beside it.

## Falsified against the defects it was written for

| mutation | result |
| --- | --- |
| reintroduce the round-17 literal `[PASS]` | **caught** |
| reintroduce the fixed-clause streams verdict | **caught** |
| change a threshold to `>= 0` | not caught — see above |
| bar a value-carrying arm behind `if false` | not caught — see above |
| bar a valueless arm behind `if false` | **caught** |

## Related

- [`docs/benchmark.md`](../../docs/benchmark.md) — the targets and the metrics matrix these verdicts
  are meant to compare against. Its §4 already states the limitation this tool partly closes: *"a
  `[PASS]`/`[WARN]` verdict against a **hardcoded** target."*
- [`docs/ci-cd.md`](../../docs/ci-cd.md) — the pipeline table this gate appears in.
