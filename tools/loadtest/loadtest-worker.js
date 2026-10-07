#!/usr/bin/env node
//
// loadtest-worker — ONE client process of the load generator, driven by `loadtest.js`.
//
// ===========================================================================
// WHY THIS IS A SEPARATE PROCESS, WHICH IS THE CENTRAL FINDING OF THIS TOOL
// ===========================================================================
//
// MEASURED, and it is the reason a naive version of this harness would have printed a number that
// meant nothing. A single Node process driving a loopback client, timed against a handler that does
// NOTHING (`--path /health` answers from memory), recorded:
//
//     p50 1.208 ms   p90 1.801 ms   p95 2.095 ms   p99 3.172 ms
//
// with ZERO server-side work on the measured path. The published bar for this row is `<= 1.5 ms`.
// So ONE Node process cannot resolve the published target at all — its own event loop and
// `process.hrtime` costs exceed the bar before the server is asked anything. A single-process
// harness would have reported "p99 3.2 ms, BELOW TARGET" against a server that was answering in
// microseconds, and every reader would have believed it.
//
// Spreading the SAME work across four processes measured:
//
//     p50 ~0.59 ms           p95 ~1.5 ms           p99 ~2.4 ms
//
// — the floor is PER PROCESS, not per host, because a Node client is one event loop and its cost
// scales with the requests that loop has to schedule. This file is that fix, and the fix is the
// whole reason the harness is shaped as a parent plus N workers rather than one clever process.
//
// WHAT IT STILL DOES NOT SOLVE, stated because the four-process figure is also above the bar: TWO
// Node processes on one host cannot certify a 1.5 ms p99 either. Loopback round-trip overhead and
// `process.hrtime` resolution are a floor that no amount of process-splitting removes. The harness
// therefore grades its own resolution before reporting a verdict — see `resolutionFloors` in
// `loadtest.js` — rather than printing a percentile whose meaning it has not established.
//
// ===========================================================================
// PROTOCOL
// ===========================================================================
//
// The parent writes a config object as JSON on stdin and the worker replies with a result object as
// JSON on stdout. Nothing else crosses. Streaming the per-request latencies back over a pipe would
// put the transport in the measurement path, so the WORKER reduces to a histogram before replying
// and the parent merges histograms — conservative for percentiles and involving no IPC per request.
//
'use strict';

const http = require('node:http');

/**
 * Bucket boundaries in MICROSECONDS, geometric from 1 us.
 *
 * A histogram rather than a raw array because the merged distribution across N workers has to be
 * built without shipping millions of sample values, and because a percentile from a histogram is
 * the only kind that survives that merge. The buckets are dense where the matrix's thresholds live
 * (0.1 ms to 20 ms) and coarse above it, which is where a bucket's width can no longer change a
 * verdict: the width at 500 ms is ~1%, far below anything a `<= 1.5 ms` or `<= 20 ms` bar can see.
 *
 * `Number.MAX_VALUE` is NOT used as the last bound — `JSON.stringify` turns it into `null` — so the
 * top boundary is a finite 1e9 us (1000 s), and no observed latency can escape the histogram.
 */
const BOUNDS_US = (() => {
  const out = [];
  let v = 1;
  while (v < 1e9) { out.push(v); v = Math.ceil(v * 1.08); }
  out.push(1e9);
  return out;
})();

function bucketize(micros) {
  let lo = 0;
  let hi = BOUNDS_US.length - 1;
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (micros <= BOUNDS_US[mid]) hi = mid; else lo = mid + 1;
  }
  return lo;
}

async function readStdin() {
  const chunks = [];
  for await (const c of process.stdin) chunks.push(c);
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function run(cfg) {
  const url = new URL(cfg.path, cfg.baseUrl);
  const body = cfg.body;
  const agent = new http.Agent({ keepAlive: cfg.keepAlive, maxSockets: Math.max(cfg.concurrency, 1) });
  const hist = new Int32Array(BOUNDS_US.length).fill(0);
  const statuses = new Map();
  let completed = 0;
  let errors = 0;
  let inFlight = 0;
  let peakInFlight = 0;
  let longestMicros = 0;
  let startedAt = 0;
  let endedAt = 0;
  // Warmup samples are taken but not histogrammed, so the distribution the parent merges is the
  // steady-state one. MEASURED need: the first request on every connection pays connect + the TLS
  // no-op + axum's first accept, and including it moves a 5,000-sample p99 by several percent.
  const warmupUntil = () => (startedAt === 0 ? Infinity : startedAt + cfg.warmupMs);

  const oneRequest = () =>
    new Promise((resolve) => {
      const t0 = process.hrtime.bigint();
      inFlight += 1;
      peakInFlight = Math.max(peakInFlight, inFlight);
      const req = http.request(
        {
          host: url.hostname, port: url.port, path: url.pathname + url.search, method: 'POST', agent,
          headers: { ...cfg.headers, 'Content-Length': Buffer.byteLength(body) },
        },
        (res) => {
          res.resume();
          res.on('end', () => {
            const micros = Number(process.hrtime.bigint() - t0) / 1000;
            const now = Date.now();
            if (now >= warmupUntil()) {
              hist[bucketize(micros)] += 1;
              if (micros > longestMicros) longestMicros = micros;
            }
            statuses.set(res.statusCode, (statuses.get(res.statusCode) || 0) + 1);
            completed += 1;
            inFlight -= 1;
            resolve();
          });
        },
      );
      req.on('error', (err) => {
        errors += 1;
        inFlight -= 1;
        const key = `error:${err.code || 'unknown'}`;
        statuses.set(key, (statuses.get(key) || 0) + 1);
        resolve();
      });
      req.end(body);
    });

  startedAt = Date.now();
  const stopAt = startedAt + cfg.durationMs;

  if (cfg.rps > 0) {
    // RATE MODE: this worker's share of the offered rate. A token bucket, so a stalled server shows
    // up as a backlog the parent sums and reports rather than as load that silently never happened.
    const perTick = cfg.rps / options.workers / 1000;
    let budget = 0;
    let last = Date.now();
    let backlog = 0;
    while (Date.now() < stopAt) {
      await sleep(1);
      const now = Date.now();
      budget += (now - last) * perTick;
      last = now;
      while (budget >= 1 && backlog < cfg.concurrency) {
        budget -= 1;
        backlog += 1;
        oneRequest().then(() => { backlog -= 1; });
      }
    }
    const drainUntil = Date.now() + 5000;
    while (inFlight > 0 && Date.now() < drainUntil) await sleep(10);
  } else {
    await Promise.all(
      Array.from({ length: Math.max(cfg.concurrency, 1) }, async () => {
        while (Date.now() < stopAt) await oneRequest();
      }),
    );
  }

  endedAt = Date.now();
  agent.destroy();

  return {
    histogram: Array.from(hist),
    statuses: Object.fromEntries([...statuses.entries()].sort()),
    completed,
    errors,
    peakInFlight,
    longestMicros,
    // The worker's OWN CPU for the load window, so the parent can compare generator cost against
    // server cost and refuse a latency verdict that is really a client measurement.
    workerCpuSeconds: process.cpuUsage().user / 1e6 + process.cpuUsage().system / 1e6,
    loadMs: endedAt - startedAt,
  };
}

/** Set from the config so the rate split can divide by the process count. */
const options = { workers: 1 };

readStdin().then(async (cfg) => {
  options.workers = cfg.workers || 1;
  const result = await run(cfg);
  process.stdout.write(JSON.stringify(result));
}).catch((err) => {
  process.stderr.write(`loadtest-worker: ${err && err.stack ? err.stack : err}\n`);
  process.exit(3);
});
