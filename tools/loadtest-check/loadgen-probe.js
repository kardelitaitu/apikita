#!/usr/bin/env node
//
// loadgen-probe — runs the REAL `server/src/bin/loadgen` binary and requires its CALIBRATION to hold.
//
// WHY THIS IS A FILE, and why it is not a source read. `tools/loadtest-check/check.sh` states the rule
// in its own header: "a source-shape rule can be satisfied by code that does not work". `loadgen.rs`
// is a 366-line file whose honest-looking structure — a floor measured before the server, a refusal
// when the floor is at or above the target, a reachability check, a warm-up the comment says is
// discarded — is exactly the kind of thing a reader (or a grep) confirms and a run falsifies. So this
// probe SPAWNS THE BINARY and reads its exit code and its `--json` payload.
//
// MEASURED, and it is the reason this file earns its place: `--measure-floor --json` DOES NOT FIRE
// THE FLOOR REFUSAL. The text path exits 4 correctly; the JSON path, with the identical arguments,
// printed a payload whose own `p99_ms` was 28.575 against `target_p99_ms` of 0.001 — a floor two
// thousand times above the bar, which the tool's own text mode calls UNRESOLVABLE — and exited 0.
// Three consecutive runs, exits 0/0/0, p99 10.2 / 28.6 / 12.2 ms. The cause is structural: the `--json`
// branch `println!`s and `return`s at line ~317, before the `args.measure_floor` verdict block that
// carries the `process::exit(4)`. An automated consumer — CI, a script, the very matrix that fills the
// row in docs/load-test.md — reads JSON, not the human text. So the honesty mechanism this binary is
// built around is invisible to precisely the audience most likely to act on it.
//
// That finding is REPORTED here, not fixed: `server/src/bin/loadgen.rs` is under concurrent edit by
// another agent, and this probe's job is to be the thing that goes red when the refusal is not honoured
// on the interface its callers use.
//
// WHAT IT ASSERTS, and each one is a PROPERTY PROVOKED AND OBSERVED:
//   1. The floor refusal fires. An absurd bar (--target-p99-ms 0.001) must exit 4. Asserted on the
//      TEXT path, which is the one that currently works — see the finding above. A check written
//      against the JSON path would be RED TODAY, correctly, and is reported separately rather than
//      folded in, so the gate's verdict distinguishes "the mechanism is broken" from "the mechanism
//      does not cover this interface".
//   2. A reachable target with a sane bar does NOT refuse — exit 0. Without this, "always exit 4"
//      would pass check 1. The pair is the falsification: the SAME binary must refuse one bar and
//      accept the other.
//   3. An unreachable target fails LOUDLY — exit 3, never 0. A run against nothing must not look
//      like a measurement. This one needs NO server, and is the reason the no-server path of this
//      probe is still worth running.
//   4. Bad arguments are refused — exit 2, not 0. A typo must not produce a run measuring something
//      else. Needs no server.
//   5. `--json` is parseable, carries every documented field, reports a NON-ZERO sample count on a
//      successful run, and its `achieved_rps` is within a sane factor of `offered_rps`. A payload
//      that parses but says `samples: 0` is the "no data reported as a pass" shape.
//   6. The warm-up RUNS. MEASURED: passing `--warmup N` lengthens the run by exactly N seconds
//      (5.07s -> 7.06s -> 10.02s for warmup 0/2/5), which is what an implemented warm-up does.
//      A warm-up that was parsed and then ignored would leave the wall time at the `--seconds`
//      figure. This is a REAL assertion with a real falsification.
//
// WHAT IT DELIBERATELY DOES NOT ASSERT, because a check that cannot fail is worse than none:
//
//   * "a larger warm-up lowers the reported sample count". IT IS FALSE. MEASURED: with `--seconds 5`
//     fixed, warmup 0/2/5 reported 1008 / 1024 / 1008 samples — indistinguishable — because
//     `measure_ends = warmup_ends + seconds` makes the warm-up ADDITIVE to the measurement window
//     rather than carved out of it. The run gets longer; the measured window does not shrink. A probe
//     asserting the count differs would fail against a working binary, which is the opposite of useful.
//   * "a larger warm-up lowers the measured tail". MEASURED once at rate 50 as a 10x gap (p95 121 ms
//     vs 13 ms) — and it did NOT reproduce: three further pairs gave 0.9x, 1.0x, 2.0x. The first
//     reading was host noise. Gating on it would make this gate flaky, and a flaky gate gets muted.
//     So the discard's EFFECT is not asserted; only that the warm-up window physically runs.
//
// Usage: node tools/loadtest-check/loadgen-probe.js <path/to/loadgen.exe> [base-url]
//   base-url defaults to http://127.0.0.1:17432 (the port the gate's own recipe starts a server on).
// Exit: 0 every check that could run held, 1 one did not, 3 wrong arguments or the binary is missing.
//       Checks that need a server are SKIPPED, loudly, when none is reachable — printed, never silent.
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');

const exe = process.argv[2];
const baseUrl = process.argv[3] || 'http://127.0.0.1:17432';
if (!exe) {
  console.error('loadgen-probe: usage: loadgen-probe.js <path/to/loadgen.exe> [base-url]');
  process.exit(3);
}
if (!fs.existsSync(exe)) {
  // A missing binary is not a violation — it is an unbuildable gate. check.sh treats 3 as
  // "a file is unreadable", the same class as a missing node.
  console.error(`loadgen-probe: no loadgen binary at ${exe}`);
  console.error('loadgen-probe:   build it first: (cd server && cargo build --bin loadgen)');
  process.exit(3);
}

const problems = [];
const skipped = [];
const need = (cond, msg) => { if (!cond) problems.push(msg); };

/**
 * Runs the binary and returns its exit code and raw stdout.
 *
 * THE BINARY IS INVOKED DIRECTLY, NEVER THROUGH A SHELL, and that is a MEASURED requirement rather
 * than a preference. On Windows this gate is run from Git Bash, whose MSYS layer rewrites a bare
 * `/health` argument into a Windows path (`C:/Program Files/Git/health`) on the way to a non-MSYS
 * child. The binary then requests a URL nobody serves and the run reports a reachability failure —
 * a confusing "builder error" that has nothing to do with what was asked. Because the probe passes
 * `/health` on many of these command lines, `shell: false` (the spawnSync default, made explicit
 * here) is load-bearing: no shell, no path mangling.
 */
function run(args) {
  const res = spawnSync(exe, args, {
    shell: false,
    encoding: 'utf8',
    timeout: 120000,
    maxBuffer: 16 * 1024 * 1024,
  });
  if (res.error) {
    if (res.error.code === 'ETIMEDOUT') {
      problems.push(`loadgen ${args.join(' ')} did not finish within 120s`);
      return { code: null, out: '', err: '' };
    }
    problems.push(`could not spawn ${exe}: ${res.error.message}`);
    return { code: null, out: '', err: '' };
  }
  return { code: res.status, out: res.stdout || '', err: res.stderr || '' };
}

// --- is there a server? ---------------------------------------------------------
//
// The probe is designed to be USEFUL WITH NO SERVER, not merely to survive one. Three of the six
// properties (unreachable target, bad arguments, and — once the JSON path is fixed — the refusal)
// need nothing listening. The rest are SKIPPED with the reason printed, following the precedent in
// check.sh (the sampler probe's non-Windows case) and tools/alert/probe.sh, which prints
// "SKIPPED ... This alert is NOT being checked" rather than a green tick over nothing.
//
// The reachability test is the BINARY ITSELF pointed at the base URL with a valid command line: if
// the server is up it exits 0, and if it is down it exits 3. That way "is the server there" is asked
// with the same instrument the checks use, rather than with a second, differently-behaving probe.
const probe = run(['--base-url', baseUrl, '--path', '/health', '--seconds', '1', '--warmup', '0',
  '--rate', '20', '--target-p99-ms', '500', '--json']);
const serverUp = probe.code === 0;
// The floor checks run loadgen AGAINST THE LIVE TARGET, so they are available exactly when the
// server is. This predicate was `serverUp || probe.code === 3` in the first version, and MEASURED
// that made the no-server path WORSE THAN USELESS: a dead server is precisely the case that exits 3,
// so the probe took its "available" branch and then reported two FAILURES — "the floor refusal
// exited 3, expected 4" and "a 500 ms bar exited 3, expected 0" — for a server that was simply not
// running. A probe that fails a CI box for having no server is a probe that gets muted, and a muted
// gate is the thing this directory exists to avoid. The live checks SKIP; the reachability and
// argument checks above still run, which is what keeps this probe useful on a bare runner.
const floorAvailable = serverUp;

if (!serverUp) {
  skipped.push(
    `the server at ${baseUrl} is not answering (loadgen exited ${probe.code}), so every check that `
    + 'needs a live target is SKIPPED and NOT passed',
  );
}

// --- 3. an unreachable target fails loudly (NO SERVER NEEDED) --------------------
//
// Run FIRST among the checks, because it is the one that must hold even on a CI box with nothing
// deployed. Port 1 is closed by convention (a privileged port nothing binds), so this is a genuine
// connection refusal rather than a timeout. The failure it guards is the worst shape a measurement
// tool has: connection refusals are FAST, so a client that ignored the error would report a
// beautiful, clean, empty run — and 0 is the one exit code that reads as success.
{
  const r = run(['--base-url', 'http://127.0.0.1:1', '--path', '/health', '--seconds', '2', '--json']);
  need(r.code === 3,
    `a run against a closed port (http://127.0.0.1:1) exited ${r.code}, expected 3. `
    + 'A target nobody serves must fail loudly; exit 0 there is a measurement of nothing '
    + `reported as a measurement. Output: ${(r.out + r.err).trim().slice(0, 200)}`);
  if (r.code === 3) {
    console.log('loadtest-check:   ok - a closed port exits 3, so a run against nothing is refused '
      + 'rather than reported');
  }
}

// --- 4. bad arguments are refused (NO SERVER NEEDED) ----------------------------
//
// Two shapes, because they fail in different places in the parser: an UNKNOWN flag (the match's
// fall-through arm) and a KNOWN flag with an unparseable VALUE (the `parse().map_err` arms). A
// parser that handled one and not the other would still let a typo through in the uncovered shape.
{
  const unknown = run(['--base-url', baseUrl, '--definitely-not-a-flag']);
  need(unknown.code === 2,
    `an unknown flag exited ${unknown.code}, expected 2. A run that succeeds while measuring `
    + `something other than what was asked is the defect docs/benchmark.md records. Output: `
    + `${(unknown.out + unknown.err).trim().slice(0, 200)}`);

  const badValue = run(['--rate', 'not-a-number']);
  need(badValue.code === 2,
    `"--rate not-a-number" exited ${badValue.code}, expected 2. A typed flag whose value does not `
    + `parse must be refused, not defaulted. Output: ${(badValue.out + badValue.err).trim().slice(0, 200)}`);

  if (unknown.code === 2 && badValue.code === 2) {
    console.log('loadtest-check:   ok - an unknown flag and an unparseable value both exit 2');
  }
}

// --- 1 and 2. the floor refusal, and its opposite --------------------------------
//
// THE PAIR IS THE ASSERTION. A binary pinned to refuse passes check 1 alone; one pinned to accept
// passes check 2 alone. Only both together show the verdict TRACKS the bar.
//
// The absurd bar is 0.001 ms. MEASURED, this host's real loopback floor is 8-29 ms, so the bar is
// three to four orders of magnitude below anything the client can observe — the refusal is not
// marginal, it is the only honest answer. The sane bar is 500 ms, which any working loopback
// measurement is far below.
if (!floorAvailable) {
  // Not a separate line from the live-run skip above: they share one cause, and two messages for one
  // absence reads as two problems. The floor verdict is evaluated INSIDE loadgen after its
  // reachability check, so with nothing listening there is no invocation that can reach it.
  skipped.push('the floor refusal and the sane-bar acceptance both require loadgen to reach the '
    + 'target (the verdict is evaluated after its reachability check), and nothing is listening');
} else {
  const absurd = run(['--base-url', baseUrl, '--path', '/health', '--measure-floor',
    '--target-p99-ms', '0.001']);
  need(absurd.code === 4,
    `--measure-floor --target-p99-ms 0.001 exited ${absurd.code}, expected 4 (UNRESOLVABLE). `
    + 'A floor at or above the target must be refused; an exit 0 here means the honesty mechanism '
    + `does not fire. Output: ${(absurd.out + absurd.err).trim().slice(0, 300)}`);
  if (absurd.code === 4) {
    console.log('loadtest-check:   ok - an absurd bar (0.001 ms) is refused with exit 4, so the '
      + 'floor verdict fires');
  }

  const sane = run(['--base-url', baseUrl, '--path', '/health', '--seconds', '3', '--warmup', '1',
    '--target-p99-ms', '500', '--json']);
  need(sane.code === 0,
    `a reachable target with a 500 ms bar exited ${sane.code}, expected 0. Without this the `
    + `refusal check above is satisfied by a binary pinned to refuse. Output: `
    + `${(sane.out + sane.err).trim().slice(0, 300)}`);
  if (sane.code === 0) {
    console.log('loadtest-check:   ok - a reachable target with a 500 ms bar is NOT refused (exit 0), '
      + 'so the refusal tracks the bar');
  }
}

// --- 5 and 6. the JSON payload, the warm-up, and the rate ------------------------
if (!serverUp) {
  skipped.push('the --json payload, the sample count, the achieved-vs-offered rate and the warm-up '
    + 'are all read from a live run, and no server is answering');
} else {
  // --- 5. the payload parses and says something ---------------------------------
  //
  // `samples: 0` on a successful run is the failure this catches: it is the JSON spelling of the
  // "[NO DATA]" that the Node harness reports, and a consumer that reads only `p99_ms` would take
  // the `NaN` that a percentile of no samples returns as a number.
  const jsonRun = run(['--base-url', baseUrl, '--path', '/health', '--seconds', '3', '--warmup', '1',
    '--rate', '50', '--target-p99-ms', '500', '--json']);

  let payload = null;
  if (jsonRun.code !== 0) {
    problems.push(`a successful run exited ${jsonRun.code}, expected 0; its output was `
      + `${(jsonRun.out + jsonRun.err).trim().slice(0, 300)}`);
  } else {
    try {
      payload = JSON.parse(jsonRun.out.trim());
    } catch (err) {
      // THE PARSE IS ASSERTED SEPARATELY from the fields, because a payload that does not parse
      // cannot be reported field-by-field and the reader needs to know which failure it is.
      problems.push(`--json did not emit parseable JSON (${err.message}); output: `
        + `${jsonRun.out.trim().slice(0, 300)}`);
    }
  }

  if (payload) {
    const FIELDS = ['kind', 'url', 'offered_rps', 'achieved_rps', 'samples', 'errors',
      'p50_ms', 'p95_ms', 'p99_ms', 'target_p99_ms', 'seconds', 'concurrency'];
    const missing = FIELDS.filter((f) => !(f in payload));
    need(missing.length === 0,
      `the --json payload is missing ${missing.join(', ')}. A consumer reading a field that is not `
      + `there gets undefined, which compares as NaN and passes any check. Payload: `
      + `${JSON.stringify(payload).slice(0, 300)}`);

    need(Number.isInteger(payload.samples) && payload.samples > 0,
      `a successful run reported samples=${JSON.stringify(payload.samples)}, expected a positive `
      + 'integer. A payload that parses but carries no samples is a measurement that did not happen '
      + 'reported in the shape of one that did.');

    // The rate sanity band. The generator is CLOSED-LOOP: a soft one, because the point is to catch a
    // rate that is off by an order of magnitude (offered ignored, or achieved counted over the wrong
    // window), not to police a busy CI host. `achieved > offered * 2` is arithmetically impossible for
    // a closed-loop generator and can only mean the denominator is wrong.
    if (Number.isFinite(payload.achieved_rps) && Number.isFinite(payload.offered_rps)
        && payload.offered_rps > 0) {
      const ratio = payload.achieved_rps / payload.offered_rps;
      need(ratio >= 0.2 && ratio <= 2,
        `achieved_rps ${payload.achieved_rps} against offered_rps ${payload.offered_rps} is a ratio `
        + `of ${ratio.toFixed(3)}; expected within [0.2, 2]. A closed-loop generator cannot exceed `
        + 'its own offered rate, so a figure above 2x means the rate is computed over the wrong window.');
    } else {
      problems.push(`the --json payload carries a non-finite or non-positive rate: `
        + `offered=${JSON.stringify(payload.offered_rps)} achieved=${JSON.stringify(payload.achieved_rps)}`);
    }

    if (missing.length === 0 && payload.samples > 0) {
      console.log(`loadtest-check:   ok - --json parses, carries all ${FIELDS.length} fields, `
        + `reports ${payload.samples} samples and ${payload.achieved_rps}/${payload.offered_rps} req/s`);
    }
  }

  // --- 6. the warm-up RUNS -------------------------------------------------------
  //
  // WHAT IS ASSERTED IS THE WALL TIME, NOT THE SAMPLE COUNT, and the reason is MEASURED rather than
  // assumed. The obvious formulation — "a bigger warm-up discards more samples, so `samples` must
  // fall" — IS FALSE FOR THIS BINARY. `measure_ends = warmup_ends + seconds`, so the warm-up is
  // ADDITIVE: the measured window keeps its full `--seconds` and the run simply takes longer.
  // MEASURED with --seconds 5 and --rate 200: warmup 0/2/5 gave 1008 / 1024 / 1008 samples — the same
  // within noise — while the wall time went 5.07s / 7.06s / 10.02s. A probe asserting the count
  // differs would be RED against a correct binary, and the "discard" it claims to verify is not the
  // property this binary has.
  //
  // The wall time IS a real assertion with a real falsification: if the warm-up were parsed and then
  // ignored (the classic version of this defect — a flag accepted, a value stored, nothing done), the
  // run would finish in the `--seconds` figure regardless. Requiring the measured `seconds` to exceed
  // `--warmup + --seconds`'s floor means the warm-up window physically elapsed.
  //
  // The test runs the warm-up at 0 and at 4 with the SAME measured window, so the only difference is
  // the discarded prefix. Both runs must also succeed, or a crash would look like a long run.
  const noWarm = run(['--base-url', baseUrl, '--path', '/health', '--seconds', '3', '--warmup', '0',
    '--rate', '50', '--target-p99-ms', '500', '--json']);
  const bigWarm = run(['--base-url', baseUrl, '--path', '/health', '--seconds', '3', '--warmup', '4',
    '--rate', '50', '--target-p99-ms', '500', '--json']);

  if (noWarm.code !== 0 || bigWarm.code !== 0) {
    problems.push(`the warm-up comparison needs two successful runs and got exits `
      + `${noWarm.code} and ${bigWarm.code}`);
  } else {
    let a = null;
    let b = null;
    try { a = JSON.parse(noWarm.out.trim()); b = JSON.parse(bigWarm.out.trim()); } catch (err) {
      problems.push(`the warm-up comparison could not parse a payload: ${err.message}`);
    }
    if (a && b) {
      // The warm-up run must be LONGER than the bare one by roughly the warm-up duration. The margin
      // is one second, generous enough for scheduling jitter and tight enough that an ignored
      // warm-up (a difference of zero) cannot slip through.
      const grew = b.seconds - a.seconds;
      need(grew >= 3,
        `--warmup 4 reported ${b.seconds.toFixed(2)}s against --warmup 0's ${a.seconds.toFixed(2)}s `
        + `(a difference of ${grew.toFixed(2)}s), expected at least 3s. The warm-up is parsed but the `
        + 'run is not longer, so the discard window never elapses and the flag does nothing.');

      // Both runs must have MEASURED something. This is what stops a warm-up so large it starves the
      // measured window from passing: a run that discarded everything and reported nothing would
      // otherwise satisfy the wall-time check alone.
      need(a.samples > 0 && b.samples > 0,
        `a warm-up run reported no samples (warmup 0: ${a.samples}, warmup 4: ${b.samples}); `
        + 'the measurement window must still produce samples after the warm-up is discarded');

      if (grew >= 3 && a.samples > 0 && b.samples > 0) {
        console.log(`loadtest-check:   ok - the warm-up window elapses: --warmup 4 ran `
          + `${grew.toFixed(2)}s longer than --warmup 0 for the same measured window `
          + `(${a.samples} / ${b.samples} samples)`);
        console.log('loadtest-check:        (the sample COUNT is deliberately NOT required to differ:');
        console.log('loadtest-check:        the warm-up is additive to --seconds here, so it cannot)');
      }
    }
  }
}

// --- the finding, reported rather than folded into a pass ------------------------
//
// The JSON path does not carry the floor refusal. This is asserted ONLY where a server is reachable
// (with no server the reachability check exits 3 first, so the refusal is not reachable at all), and
// it is reported as a PROBLEM — because it is a defect in the binary under test, on the interface an
// automated consumer uses, and a probe that stayed green through it would be the "check that cannot
// fail" this directory exists to avoid.
if (floorAvailable) {
  const jsonFloor = run(['--base-url', baseUrl, '--path', '/health', '--measure-floor',
    '--target-p99-ms', '0.001', '--json']);
  let jsonFloorPayload = null;
  try { jsonFloorPayload = JSON.parse(jsonFloor.out.trim()); } catch { /* reported below */ }

  if (jsonFloor.code === 0 && jsonFloorPayload
      && Number.isFinite(jsonFloorPayload.p99_ms)
      && jsonFloorPayload.p99_ms >= jsonFloorPayload.target_p99_ms) {
    problems.push(
      `--measure-floor --json reported a floor of ${jsonFloorPayload.p99_ms} ms against a `
      + `${jsonFloorPayload.target_p99_ms} ms target and exited 0. The text path refuses this exact `
      + 'run with exit 4 (UNRESOLVABLE). The `--json` branch returns before the floor verdict, so an '
      + 'automated consumer is told a run succeeded whose own payload says the instrument cannot see '
      + 'past the bar. Fix: emit the floor verdict and exit 4 on the --json path too.',
    );
    console.error('loadtest-check: FAIL - --measure-floor --json does not fire the floor refusal: '
      + `p99_ms ${jsonFloorPayload.p99_ms} >= target_p99_ms ${jsonFloorPayload.target_p99_ms}, `
      + 'exit 0 (the text path exits 4 for the same arguments)');
  } else if (jsonFloor.code === 4) {
    console.log('loadtest-check:   ok - --measure-floor --json also refuses with exit 4');
  }
}

// --- report ---------------------------------------------------------------------
for (const s of skipped) {
  console.error(`loadtest-check:   SKIPPED - ${s}`);
  console.error('loadtest-check:   A skip stated out loud is not a pass; the checks above still ran.');
}
for (const p of problems) console.error(`loadtest-check: FAIL - ${p}`);
if (problems.length) process.exit(1);
process.exit(0);
