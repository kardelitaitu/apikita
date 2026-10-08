#!/usr/bin/env node
//
// verdict-probe — drives the load test's verdict functions directly, and requires them to FLIP.
//
// WHY THIS IS A FILE AND NOT A `node -e` INSIDE check.sh, which is what it was first. The assertions
// below contain `$(...)`-shaped JavaScript — template literals, `m.verdictCpu(null, 0.2, 40)` — and
// inside a single-quoted heredoc those are fine but in a plain `sh` command substitution the
// parenthesis is a SYNTAX ERROR in the shell BEFORE node ever runs. It failed as
// "syntax error near unexpected token `('", which is at least loud; a version that happened to parse
// would have run an empty program and exited 0, reporting every verdict as earned.
//
// So the assertions live in a file the shell only has to name. `check.sh` runs this and reads its
// exit code, and the rule it enforces is the one this repository keeps re-learning: A GUARD THAT
// CANNOT FAIL IS WORSE THAN NO GUARD.
//
// THE RULE, stated as the thing being tested: a verdict is EARNED when a measurement crossing its
// threshold changes the verdict. Both directions are asserted for every verdict, because a verdict
// pinned to PASS and a verdict pinned to FAIL are the same defect with the sign flipped, and testing
// only one of them accepts the other.
//
// Usage: node tools/loadtest-check/verdict-probe.js <path/to/loadtest.js>
// Exit: 0 every verdict flips as required, 1 one did not, 3 the module could not be loaded.
'use strict';

const harnessPath = process.argv[2];
if (!harnessPath) {
  console.error('verdict-probe: needs the path to loadtest.js');
  process.exit(3);
}

let m;
try {
  // eslint-disable-next-line import/no-dynamic-require
  m = require(harnessPath);
} catch (err) {
  console.error(`verdict-probe: could not load ${harnessPath}: ${err.message}`);
  process.exit(3);
}

const problems = [];
const need = (cond, msg) => { if (!cond) problems.push(msg); };

// --- Key Validation Latency (p99) ---------------------------------------------
//
// Inside the bar, outside it, exactly on it, and with no measurement at all.
const inside = m.verdictP99(1.0, 1.5);
const outside = m.verdictP99(3.0, 1.5);
need(inside.kind === 'PASS', `verdictP99(1.0, 1.5) returned ${inside.kind}, expected PASS`);
need(inside.ok === true, 'verdictP99(1.0, 1.5).ok should be true');
need(outside.kind === 'BELOW TARGET', `verdictP99(3.0, 1.5) returned ${outside.kind}, expected BELOW TARGET`);
need(outside.ok === false, 'verdictP99(3.0, 1.5).ok should be false');

// Exactly on the bar is INSIDE it: the row reads `<= 1.5`, not `< 1.5`. An off-by-one comparison
// here would reject a run that met the published target exactly.
need(m.verdictP99(1.5, 1.5).ok === true, 'verdictP99 at exactly the bar must pass: the published row is "<="');

// NO MEASUREMENT IS NOT A PASS. This is the shape the 614 ms port-exhaustion run produced, and the
// shape a stopped server produces.
const noData = m.verdictP99(null, 1.5);
need(noData.kind === 'NO DATA' && noData.ok === false,
  `verdictP99(null, 1.5) returned ${noData.kind}/ok=${noData.ok}, expected NO DATA and not-ok`);
need(m.verdictP99(Number.NaN, 1.5).ok === false, 'verdictP99(NaN) must not pass');

// THE FALSIFICATION, mechanically: the SAME measurement must pass a loose bar and fail a tight one.
need(m.verdictP99(1.2, 1.5).kind === 'PASS' && m.verdictP99(1.2, 1.0).kind === 'BELOW TARGET',
  'the same 1.2 ms measurement must PASS at a 1.5 ms bar and FAIL at a 1.0 ms bar');

// --- CPU Saturation ------------------------------------------------------------
//
// The verdict takes CORE-EQUIVALENTS and a vCPU denominator, so it cannot be a literal: the same
// reading is a different verdict at a different denominator, and that is asserted.
const cpuIn = m.verdictCpu(0.05, 0.2, 40);   // 0.05 / 0.2 = 25%
const cpuOut = m.verdictCpu(0.20, 0.2, 40);  // 0.20 / 0.2 = 100%
need(cpuIn.kind === 'PASS' && cpuIn.pct === 25,
  `verdictCpu(0.05, 0.2, 40) gave ${cpuIn.kind}/${cpuIn.pct}, expected PASS/25`);
need(cpuOut.kind === 'BELOW TARGET' && cpuOut.pct === 100,
  `verdictCpu(0.20, 0.2, 40) gave ${cpuOut.kind}/${cpuOut.pct}, expected BELOW TARGET/100`);
need(m.verdictCpu(null, 0.2, 40).ok === false, 'verdictCpu(null) must not pass');
need(m.verdictCpu(0.05, 0.2, 10).kind === 'BELOW TARGET', 'a 25% reading must FAIL at a 10% bar');
// The DENOMINATOR moves the verdict, which is why the tool prints which one it used.
need(m.verdictCpu(0.05, 0.2, 40).kind === 'PASS' && m.verdictCpu(0.05, 1.0, 40).kind === 'PASS'
  && m.verdictCpu(0.5, 0.2, 40).kind === 'BELOW TARGET' && m.verdictCpu(0.5, 1.0, 40).kind === 'BELOW TARGET',
  'verdictCpu must divide by the vCPU denominator it is given');

// --- An unresolvable p99 is NEVER a pass ----------------------------------------
//
// This is the instrument-floor verdict. It exists because a single Node process measured a p99 of
// 3.172 ms against a handler doing NO WORK, while the published bar is 1.5 ms — so on this host a
// p99 cannot be attributed to the server at all. The one thing this verdict must never do is return
// ok, whatever measurement it is handed.
for (const v of [null, 0.1, 5, 1000, Number.NaN]) {
  const u = m.verdictUnresolvable(v, 1.5, 2.4, 'test');
  need(u.ok === false, `verdictUnresolvable(${v}) reported ok=true - an unresolvable measurement must never pass`);
  need(u.kind === 'UNRESOLVABLE', `verdictUnresolvable(${v}) kind was ${u.kind}, expected UNRESOLVABLE`);
}

// --- The histogram, against an answer known by construction ---------------------
const bounds = m.HISTOGRAM_BOUNDS;
need(Array.isArray(bounds) && bounds.length > 50, `HISTOGRAM_BOUNDS looks wrong: ${JSON.stringify(bounds).slice(0, 60)}`);
need(bounds[bounds.length - 1] < Number.MAX_VALUE,
  'the top histogram boundary must be FINITE: JSON.stringify turns Infinity/MAX_VALUE into null, and a worker replying null would lose every sample above the last real bucket');

const bucketOf = (us) => { for (let i = 0; i < bounds.length; i += 1) if (us <= bounds[i]) return i; return bounds.length - 1; };
const hist = new Array(bounds.length).fill(0);
for (let i = 0; i < 1000; i += 1) hist[bucketOf(500)] += 1; // every sample at 0.5 ms
const p50 = m.percentileFromHistogram(hist, bounds, 50);
need(p50.ms >= 0.5 && p50.ms < 0.5 * 1.09,
  `a histogram holding 1000 samples at 0.5 ms reported p50 ${p50.ms} ms; expected the bucket containing 0.5 ms (upper bound, within one ~8% bucket)`);
need(p50.count === 1000, `percentileFromHistogram reported ${p50.count} samples, expected 1000`);

// An empty histogram must NOT report a number. A percentile of nothing is not zero.
const empty = m.percentileFromHistogram(new Array(bounds.length).fill(0), bounds, 99);
need(!Number.isFinite(empty.ms), `an empty histogram reported p99 ${empty.ms}; a percentile of no samples must not be a finite number`);

// The merge is a SUM. A merge that took a maximum, or one worker's histogram, would still look
// plausible — the total is what catches it.
const merged = m.mergeHistograms([hist, hist, hist]);
need(merged.reduce((a, b) => a + b, 0) === 3000,
  `mergeHistograms summed to ${merged.reduce((a, b) => a + b, 0)}; expected 3000`);

// --- The published constants are the document's figures --------------------------
//
// These are also compared against docs/benchmark.md by check.sh, in shell, without a Rust toolchain.
// Asserted here as well because a constant that exists but is not the one the verdicts close over
// would satisfy a source read while the verdict used something else.
need(m.PUBLISHED_P99_LATENCY_MS === 1.5,
  `PUBLISHED_P99_LATENCY_MS is ${m.PUBLISHED_P99_LATENCY_MS}; the metrics matrix publishes 1.5 ms`);
need(m.PUBLISHED_CPU_TARGET_PCT === 40,
  `PUBLISHED_CPU_TARGET_PCT is ${m.PUBLISHED_CPU_TARGET_PCT}; the metrics matrix publishes 40%`);
need(m.PUBLISHED_CPU_RATE_RPS === 200,
  `PUBLISHED_CPU_RATE_RPS is ${m.PUBLISHED_CPU_RATE_RPS}; the matrix row reads "at 200 req/s"`);
need(m.PUBLISHED_VCPU === 0.2,
  `PUBLISHED_VCPU is ${m.PUBLISHED_VCPU}; docs/benchmark.md's header states 0.2 vCPU`);

// --- The sampling interval against the host's CPU quantum ------------------------
//
// WHY THIS IS ASSERTED HERE RATHER THAN LEFT TO THE TOOL. The refusal used to live inline in `main`,
// behind the live-server reachability check. MEASURED: running the tool with a deliberately tiny
// interval and a dead server stops at "is not answering" first, and running it with no server stops
// at "--base-url is required" - so NO hermetic gate could reach it. That matters because the rule is
// the subtle kind: too short an interval makes most samples read "no CPU consumed", the CPU figure
// comes out SMALL, and small passes a <= 40% bar. A rule whose failure direction is a false PASS is
// the one worth pinning, and it now has a pure function to pin.
need(m.SAMPLE_INTERVAL_QUANTA > 1,
  `SAMPLE_INTERVAL_QUANTA is ${m.SAMPLE_INTERVAL_QUANTA}; a single sample can straddle a quantum ` +
  'boundary and see two, so the multiplier must be greater than 1');

const quantum = 15.625; // the measured value the tool's own doc comment records
const tooShort = m.sampleIntervalIsUsable(quantum, quantum);
need(tooShort.ok === false,
  'an interval of exactly one quantum was accepted; at that period most samples read "no CPU ' +
  'consumed", the figure comes out small, and small PASSES the published bar');
const fourQuanta = m.sampleIntervalIsUsable(quantum * 4, quantum);
need(fourQuanta.ok === true,
  `an interval of exactly ${m.SAMPLE_INTERVAL_QUANTA}x the quantum was rejected; the boundary is ` +
  'inclusive because the tool says "at least" this figure');
const justUnder = m.sampleIntervalIsUsable(quantum * 4 - 0.01, quantum);
need(justUnder.ok === false,
  'an interval just under the multiplier was accepted, so the boundary is not where it claims');
need(m.sampleIntervalIsUsable(undefined, quantum).ok === false,
  'an undefined interval was accepted');
need(m.sampleIntervalIsUsable(1000, 0).ok === false,
  'a zero quantum was accepted, which would make every interval look usable');
need(tooShort.minimumMs === Math.ceil(quantum * m.SAMPLE_INTERVAL_QUANTA),
  `the refusal does not report the minimum interval it demands (got ${tooShort.minimumMs}); the ` +
  'operator is told to raise a number they cannot compute');

for (const p of problems) console.error(`check.sh: FAIL - ${p}`);
if (problems.length) process.exit(1);
console.log('loadtest-check:   ok - both verdicts pass inside their bar and fail outside it, in both');
console.log('loadtest-check:        directions; no-data never passes; an unresolvable p99 never passes;');
console.log('loadtest-check:        the histogram merges by sum and reads back a known value;');
console.log('loadtest-check:        and a too-short sampling interval is refused, which no live-server');
console.log('loadtest-check:        run of the tool can be made to demonstrate.');
