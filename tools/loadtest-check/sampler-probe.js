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
const SPIN_MS = 4000;
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

setTimeout(() => {
  const second = sampler.sampleProcess(child.pid);
  child.kill();
  if (second === null) {
    console.error('sampler-probe: FAIL - the sampler returned null on the second read of a live process');
    process.exit(1);
  }
  const delta = second.cpuSeconds - first.cpuSeconds;
  // The child spun for SPIN_MS of wall time. A single-threaded spin on a machine with other work
  // running lands somewhere around that figure; the assertion is deliberately a BAND rather than an
  // equality, because this is a machine under test, not a stopwatch.
  const expected = SPIN_MS / 1000;
  if (delta <= expected * 0.25) {
    console.error(
      `sampler-probe: FAIL - the sampler saw only ${delta.toFixed(3)} CPU-seconds of a child that\n` +
      `sampler-probe:   spun for ~${expected.toFixed(1)}s. Either the sampler is reading the wrong\n` +
      `sampler-probe:   process or the host's CPU quantum is larger than the sampling window, and\n` +
      `sampler-probe:   EITHER makes every CPU figure the load test prints too small - and small\n` +
      `sampler-probe:   passes a "<= 40%" bar.`,
    );
    process.exit(1);
  }
  console.log(
    `loadtest-check:   ok - sampled a live process twice, saw ${delta.toFixed(3)} CPU-seconds of a ` +
    `~${expected.toFixed(1)}s spin (RSS ${(second.rssBytes / 2 ** 20).toFixed(1)} MB)`,
  );
  process.exit(0);
}, 2000);
