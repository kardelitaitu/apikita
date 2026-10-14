# loadgen — a native client for the p99 row

## The problem it solves

`docs/load-test.md:71` publishes a **key-validation p99 target of 1.5 ms**. `docs/benchmark.md` says the
in-process `benchmark` binary cannot verify it (no HTTP client, no histogram), so the row has been blank
since it was written.

Filling it with the Node harness (`tools/loadtest/`) is impossible, and the reason is measured rather
than argued. Against a handler doing **no work** (`/health`):

| generator | p50 | p95 | p99 |
| --- | --- | --- | --- |
| one Node process | 1.208 ms | 2.095 ms | **3.172 ms** |
| four Node processes | ~0.59 ms | ~1.5 ms | **~2.4 ms** |

A no-op handler reported at 2.4 ms is the **instrument**, not the server. The Node harness handles this
honestly — it measures its own floor first and refuses a verdict when the floor is at or above the target
— which is why the row stayed blank rather than being filled with a fabrication.

**A 16-core host and Docker both fail to fix this.** The constraint is the measuring instrument, not
capacity; a client whose own floor exceeds the bar cannot resolve that bar on any machine. That is worth
stating because the reflex is to reach for hardware.

## What it is

`server/src/bin/loadgen.rs`. A Rust HTTP client: no event loop, no GC, no millisecond-resolution timer
between the request and the reading.

```sh
cargo build --bin loadgen

# ALWAYS the floor first, at the SAME concurrency as the run you intend.
./target/debug/loadgen --base-url http://127.0.0.1:8080 --path /health --measure-floor --concurrency 8

# Then the real run.
./target/debug/loadgen --base-url http://127.0.0.1:8080 --path /v1/chat/completions --rate 200 --concurrency 8
```

## The measured floor, and why the flag must match

**Linux**, on a 32-logical-core Ryzen 9 7950X, loopback. **THE OPERATING SYSTEM IS PART OF THE
CONDITION AND THIS TABLE OMITTED IT**, which a re-measurement exposed: the same tool on Windows, same
CPU class, reports a floor of **0.750 ms at concurrency 8 and 0.672 ms at 32** - flat, and BOTH below
the 1.500 ms bar - where this table has 1.231 ms and 1.892 ms, crossing it.

So the conclusion below (*"past concurrency ~16 the floor crosses the bar"*) is a property of THAT
HOST, not of the tool and not of loopback. The honest shape is a range across machines, and a run that
needs the boundary should MEASURE it rather than import it from here.

`loadgen` is OS-aware in the one place that matters and says so at run time: `host_load()` reads
`/proc/loadavg` under `#[cfg(target_os = "linux")]` and returns *"NOT AVAILABLE on this platform - a
floor taken here cannot state its host state"* everywhere else. A Windows floor is a number without its
load context, which is the second reason not to compare it to a Linux one.

| concurrency | floor p99 | server p99 |
| --- | --- | --- |
| 1 | 1.131 ms | 1.106 ms |
| 4 | 1.290 ms | 1.236 ms |
| 8 | 1.231 ms | 1.572 ms |
| 16 | 1.377 ms | 1.597 ms |
| 32 | 1.892 ms | 1.910 ms |

Two things were said to follow from that table, and **BOTH ARE WITHDRAWN — by
`docs/load-test.md` itself, which this README had not caught up with.** The table above is kept as the
record of what was first measured; the conclusions read differently now:

1. *"The floor is effectively flat against rate"* — TRUE, and stronger than stated. The verified
   measurement in `docs/load-test.md` gives medians of **0.664 / 0.698 / 0.658 / 0.681 / 0.675 ms** at
   concurrency 1/4/8/16/32 — flat across the whole range, not merely across rate at one concurrency.
2. *"Past concurrency ~16 the floor crosses the bar, so a run at 32 workers cannot produce a verdict"*
   — **RETRACTED.** MEASURED, and the retraction is in `docs/load-test.md`: 7 trials per concurrency
   gave non-monotonic medians, concurrency 32 landed **UNDER** the 1.5 ms bar, and the quoted 1.892 ms
   was a one-shot artifact on a host running a crypto-miner (the same command read 13.7–23.7 ms under
   that load). **A single run on one host is a coin flip."

What replaces it is a RULE rather than a table: every figure must be a **median of repeated interleaved
trials, with the host's CPU stated**. This round's independent reading on Windows — same CPU class,
no `/proc/loadavg`, so no host-load figure at all — reported 0.750 ms at concurrency 8 and 0.672 ms at
32, which corroborates the flat conclusion rather than the crossing one.

## What makes it trustworthy, and where each property came from

Every one of these is copied from the Node harness rather than invented, because the failure mode of a
latency tool is a confident number that measures the tool:

1. **The floor is measured before the server** and a verdict is **refused** when the floor is at or above
   the target. Same rule, same honesty.
2. **A real histogram** — every sample retained and sorted, nearest-rank percentile. A p99 from a mean is
   a fabrication.
3. **Warm-up is discarded and SAID to be discarded**, with the duration printed, so a figure can be
   attributed rather than trusted.
4. **Achieved rate is printed beside the offered rate.** An offered rate nobody achieved is not a result.
5. **A reachability pre-check**, so a run against nothing fails loudly instead of reporting fast
   connection refusals as excellent latency.
6. **The provenance the row needs** — box, vCPU, date — is printed, because `docs/load-test.md:66`
   refuses a figure without those three.

## Bugs found in this tool by measuring it

Recorded because each is a live example of the failure mode the tool guards against.

- **The flag parser consumed the value after a bare flag.** `i += 2` unconditionally meant
  `--measure-floor --concurrency 4` failed with "unknown argument: 4", and every following argument
  shifted. Fixed by making each arm report how many arguments it consumed.
- **`--measure-floor` ignored `--concurrency` and `--rate`.** It hardcoded 50 req/s at the default
  concurrency, so `--measure-floor --concurrency 32` validated a *different shape* than the run it was
  gating — the exact defect the floor exists to prevent. The first reading I took from it (3.312 ms) was
  wrong for this reason, and I nearly reported it as a conclusion.
- **A measurement taken while the machine was at 99% CPU read 20–37 ms.** Three subagents were running
  load tests on the same box. The floor is a property of the client *and the host*, so a contended host
  inflates it — which is precisely what the tool's own refusal rule is for. **Do not measure while
  anything else load-generates on the same machine.**

## What this does NOT do

It does not sample the server's CPU. The Node harness does, via `--pid`. The CPU row
(`docs/load-test.md:72`) has its own instrument and this one is scoped to the p99 row. Stated so the
absence is not mistaken for an oversight.
