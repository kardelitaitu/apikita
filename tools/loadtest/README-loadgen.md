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

Against `/health` on a 32-logical-core Ryzen 9 7950X, loopback:

| concurrency | floor p99 | server p99 |
| --- | --- | --- |
| 1 | 1.131 ms | 1.106 ms |
| 4 | 1.290 ms | 1.236 ms |
| 8 | 1.231 ms | 1.572 ms |
| 16 | 1.377 ms | 1.597 ms |
| 32 | 1.892 ms | 1.910 ms |

Two things follow. The floor is **effectively flat against rate** at fixed concurrency (10–1000 req/s all
land within 1.1–1.4 ms), so it is client-side queueing rather than throughput. And **past concurrency
~16 the floor crosses the bar** — a run at 32 workers cannot produce a p99 verdict about the server, and
the tool says so rather than reporting its own contention as latency.

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
