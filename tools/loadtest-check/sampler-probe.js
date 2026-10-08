#!/usr/bin/env node
//
// sampler-probe — runs the load test's REAL pid sampler against a process this script owns, and
// requires it to see that process burn CPU.
//
// WHY THIS FILE EXISTS, and it is a defect this repository would otherwise have shipped. The pid
// sampler in `tools/loadtest/loadtest.js` shells out to PowerShell for each CPU sample. Its first
// version built that command by string concatenation with escaped quotes, which the `pwsh -Command`
// PARSER rejected at the shell boundary; its second used `chr(32)`, which is a VB function Windows
// PowerShell does not have; its third assembled a line continuation into `[char]32++(...)`, which
// PowerShell read as its increment operator and refused. All three produced `null` from every sample
// — and `null` is reported downstream as "[NO DATA] the server process was never sampled", which for
// a HEALTHY server looked like the tool declining to measure rather than the tool being broken.
//
// No amount of reading the source would have found any of the three. Running it found all three in
// about a minute. So the gate runs it, every time, on a process whose CPU behaviour is known by
// construction: this script spawns a child that spins, and requires the sampler's delta to be
// positive and to be in the right order of magnitude.
//
// Usage: node tools/loadtest-check/sampler-probe.js <path/to/loadtest.js>
// Exit: 0 the sampler reads a live process and its CPU moved, 1 it did not, 3 the module or the
//       exported sampler could not be loaded.
'use strict';

const fs = require('node:fs');
const { spawn } = require('node:child_process');

const harnessPath = process.argv[2];
if (!harnessPath) {
  console.error('sampler-probe: needs the path to loadtest.js');
  process.exit(3);
}

/**
 * Loads the module WITHOUT running its `main`.
 *
 * `loadtest.js` is a script that also exports its verdict functions for this gate, and its
 * `require.main === module` guard keeps `main` from firing on import — so a plain `require` is
 * enough, and it is what `check.sh` uses for the verdict assertions.
 *
 * THE PID SAMPLER IS NOT EXPORTED, and deliberately: it is Windows-specific (`Win32_Process`) and
 * exporting it would invite a caller to use it from a platform where it cannot work. So this probe
 * reads the function out of the source by evaluating the module's own prefix — the same technique
 * `.agents/`-style scratch probes use — and the gate is the only thing ever doing it. That is a
 * deliberate trade: `check.sh` runs this file, and a broken `sampleProcess` fails here rather than in
 * a load test somebody trusted.
 */
function loadSampler(path) {
  const src = fs.readFileSync(path, 'utf8');
  const cut = src.indexOf('async function sampleWhile');
  if (cut === -1) {
    // The sampler block moved. Fail loudly rather than evaluating the wrong region of the file.
    throw new Error('could not find the sampler block in loadtest.js (sampleWhile is gone or renamed)');
  }
  const body = src.slice(src.indexOf('\n', src.indexOf('#!')), cut);
  const mod = { exports: {} };
  // eslint-disable-next-line no-new-func
  new Function('module', 'exports', 'require', '__dirname',
    `${body}\nmodule.exports = { sampleProcess };`)(mod, mod.exports, require, __dirname);
  return mod.exports;
}

let sampler;
try {
  sampler = loadSampler(harnessPath);
} catch (err) {
  console.error(`sampler-probe: could not load the sampler: ${err.message}`);
  process.exit(3);
}
if (typeof sampler.sampleProcess !== 'function') {
  console.error('sampler-probe: the module exposes no sampleProcess');
  process.exit(3);
}

// A child that burns CPU for long enough to be unambiguous, and no longer.
//
// THE SPIN MUST OUTLAST THE PROBE'S OWN SCHEDULE, and it did not.
//
// MEASURED: this file reported `the sampler returned null on the second read of a live process` about
// one run in eight under load. The null was CORRECT - the child really was gone. The child is spawned
// and starts spinning immediately, while the probe's timeline is: a BLOCKING first sample, then a
// `setTimeout` of 2000ms, then the second sample. So the child's budget is measured from SPAWN, and the
// probe only reaches its second read at `firstSampleMs + 2000ms`. MEASURED on an idle host the first
// sample takes 515-1100ms, leaving under a second of margin; on a loaded host (which is exactly when a
// full tool sweep runs it, after a dozen other guards) the first sample alone exceeds the 2000ms delay
// and the child has already exited.
//
// The old value tuned the two figures to be nearly equal, which is a race by construction. The fix is
// not a bigger timeout on the read but a child that cannot expire before the probe is done with it:
// the spin is now several times the delay, so the margin is the SPIN rather than the difference
// between two closely-matched numbers.
const SPIN_MS = 15000;
const SECOND_READ_DELAY_MS = 2000;
const child = spawn(process.execPath, [
  '-e',
  `const t=Date.now()+${SPIN_MS};while(Date.now()<t){for(let i=0;i<20000;i++);}`,
], { stdio: 'ignore' });

const first = sampler.sampleProcess(child.pid);
if (first === null) {
  // THE DEFECT THIS PROBE EXISTS FOR, reported with the failure mode named rather than as a bare
  // "sampler returned null".
  console.error(
    'sampler-probe: FAIL - the sampler returned null for a process that is definitely alive.\n' +
    'sampler-probe:   That is the exact failure three earlier versions of it had, and downstream it\n' +
    'sampler-probe:   reads as "[NO DATA] the server process was never sampled" - a healthy server\n' +
    'sampler-probe:   the harness silently declines to measure. Check the PowerShell the sampler\n' +
    'sampler-probe:   builds, by running it by hand against any live pid.',
  );
  child.kill();
  process.exit(1);
}
if (!Number.isFinite(first.cpuSeconds) || first.cpuSeconds < 0) {
  console.error(`sampler-probe: FAIL - the sampler returned a non-finite CPU reading: ${JSON.stringify(first)}`);
  child.kill();
  process.exit(1);
}
if (!Number.isFinite(first.rssBytes) || first.rssBytes <= 0) {
  console.error(`sampler-probe: FAIL - the sampler returned a non-positive RSS: ${JSON.stringify(first)}`);
  child.kill();
  process.exit(1);
}

const secondReadAt = Date.now();
setTimeout(() => {
  const second = sampler.sampleProcess(child.pid);
  const elapsedMs = Date.now() - secondReadAt;
  child.kill();
  if (second === null) {
    // THE MESSAGE NOW CARRIES THE DIAGNOSIS, because "returned null" alone sent a reader looking for a
    // broken sampler when the cause is a child that had already exited. The elapsed figure is what
    // distinguishes the two: a null with a SHORT elapsed is a sampler fault, a null with an elapsed
    // near the spin means the child outlived its welcome - which the constants above make impossible.
    console.error(
      'sampler-probe: FAIL - the sampler returned null on the second read of a live process.\n' +
      `sampler-probe:   ${elapsedMs}ms elapsed since the first read, against a ${SPIN_MS}ms spin - so the\n` +
      'sampler-probe:   child should still have been running. That is the exact failure three earlier\n' +
      'sampler-probe:   versions of it had, and downstream it reads as "[NO DATA] the server process\n' +
      'sampler-probe:   was never sampled" - a healthy server the harness silently declines to\n' +
      'sampler-probe:   measure. Check the PowerShell the sampler builds, by running it by hand.',
    );
    process.exit(1);
  }
  const delta = second.cpuSeconds - first.cpuSeconds;
  // THE BAND IS OVER THE PROBE'S OWN WINDOW, not over the child's whole life.
  //
  // The child spins in one thread while this process waits, so in ~elapsedMs of wall time it should
  // accrue about that many CPU-seconds, and never more than the spin allows. The check is deliberately
  // a BAND: the host is under test, not a stopwatch, and a busy machine gives the child less than it
  // asked for. What it must exclude is the two ways this can be wrong in the direction that MATTERS -
  // reading the wrong process (near zero) or reading a counter that does not advance.
  const expected = elapsedMs / 1000;
  const floor = expected * 0.25;
  const ceiling = SPIN_MS / 1000 + 1;
  if (delta <= floor) {
    console.error(
      `sampler-probe: FAIL - the sampler saw only ${delta.toFixed(3)} CPU-seconds across ${elapsedMs}ms\n` +
      `sampler-probe:   of a spinning child (floor ${floor.toFixed(3)}). Either the sampler is reading\n` +
      'sampler-probe:   the wrong process or the host CPU quantum is larger than the sampling window,\n' +
      'sampler-probe:   and EITHER makes every CPU figure the load test prints too small - and small\n' +
      'sampler-probe:   passes a "<= 40%" bar.',
    );
    process.exit(1);
  }
  if (delta > ceiling) {
    console.error(
      `sampler-probe: FAIL - the sampler reported ${delta.toFixed(3)} CPU-seconds of a child that could\n` +
      `sampler-probe:   have burned at most ~${(SPIN_MS / 1000).toFixed(1)}. A figure above the child's own\n` +
      'sampler-probe:   ceiling means the two reads were of DIFFERENT processes, or the counter is not\n' +
      'sampler-probe:   monotonic - and a large CPU figure fails a "<= 40%" bar for the wrong reason.',
    );
    process.exit(1);
  }
  console.log(
    `loadtest-check:   ok - sampled a live process twice, saw ${delta.toFixed(3)} CPU-seconds across ` +
    `${elapsedMs}ms of a ~${(SPIN_MS / 1000).toFixed(1)}s spin (RSS ${(second.rssBytes / 2 ** 20).toFixed(1)} MB)`,
  );
  process.exit(0);
}, SECOND_READ_DELAY_MS);
