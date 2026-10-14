#!/usr/bin/env node
//
// vcpu-pin - run a command inside a Windows Job Object with a HARD CPU rate cap.
//
// WHY THIS EXISTS. `docs/load-test.md` says the two published targets are stated per **0.2 vCPU**, and
// that a p99 measured on an unconstrained box is not evidence about the deployment because the bar
// assumes the process is CPU-starved. The documented way to get that shape is `docker run --cpus=0.2`.
// MEASURED on the development host: Docker Desktop is not running, so that route is unavailable here.
//
// THE OBVIOUS SUBSTITUTE IS WRONG, and the size of the error is the point. Pinning a process to ONE
// logical CPU is the usual answer, but on a 32-core box that is 1.0 core-equivalents - **five times**
// the documented 0.2. Reporting it as "the 0.2 vCPU figure" would be the same laundering the load-test
// harness already refuses to do for its p99: quoting a measurement taken under conditions the target
// does not describe.
//
// `JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP` is the real analogue. It caps a process tree at a percentage
// of TOTAL system CPU, which is what `--cpus` does, and it needs no Docker and no reboot.
//
// USAGE
//   node tools/vcpu-pin/vcpu-pin.js --cpu 0.2 -- node server.js
//   node tools/vcpu-pin/vcpu-pin.js --help
//
// Exit: the child's exit code, or 3 if the cap could not be applied. IT REFUSES TO RUN UNCAPPED -
// a pin that silently did nothing would make every figure taken through it a fiction, which is the
// failure mode this tool exists to prevent.
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const TOTAL_CORES = os.cpus().length;

const USAGE = `vcpu-pin - run a command under a hard CPU rate cap (the --cpus=0.2 analogue)

  --cpu <n>       core-equivalents to allow, e.g. 0.2. Required.
  --allow-uncapped  run anyway if the cap cannot be applied (records that it failed)
  --help          this text

The cap is a fraction of TOTAL system CPU, so on a ${TOTAL_CORES}-core box '--cpu 0.2' means
${(0.2 / TOTAL_CORES * 100).toFixed(3)}% - which is why pinning to one whole core (${(1 / TOTAL_CORES * 100).toFixed(2)}%)
is NOT a substitute: it is five times too much.
`;

function parseArgs(argv) {
  const out = { cpu: null, allowUncapped: false, cmd: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const a = argv[i];
    if (a === '--') { out.cmd = argv.slice(i + 1); break; }
    if (a === '--cpu') out.cpu = parseFloat(argv[++i]);
    else if (a === '--allow-uncapped') out.allowUncapped = true;
    else if (a === '--help' || a === '-h') out.help = true;
    else { out.bad = a; break; }
  }
  return out;
}

/**
 * The PowerShell that creates the job, sets the cap, and launches the child inside it.
 *
 * THE STRUCT SIZE IS 8 BYTES, NOT 12, and getting that wrong is silent. `SetInformationJobObject`
 * with the wrong `dwLength` returns FALSE with `ERROR_BAD_LENGTH` (24) — and MEASURED, the child still
 * runs, so a caller that does not check the return gets an uncapped process and a comment saying the
 * cap was applied. That is the failure this tool exists to prevent, and it is why the check is here
 * rather than left to the API to fail loudly.
 *
 * The native shape is:
 *
 *     DWORD ControlFlags;                            // 4
 *     union { DWORD CpuRate; DWORD Weight; };        // 4   <- a UNION, not two fields
 *
 * Three `UInt32` fields is 12 and fails; two is 8 and works.
 *
 * The rate control is set BEFORE assignment. There is a small window where a freshly spawned child
 * runs uncapped; it is documented rather than hidden because closing it needs CREATE_SUSPENDED plus a
 * remote thread resume, which is a lot of machinery for a window measured in milliseconds on a
 * process whose first action is loading.
 */
function capScript(cpuEquivalents) {
  const pct = Math.max(1, Math.round((cpuEquivalents / TOTAL_CORES) * 100));
  return `
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public class JobCpuCap {
  [StructLayout(LayoutKind.Sequential)]
  public struct JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
    public UInt32 ControlFlags;
    public UInt32 CpuRate;
  }
  [DllImport("kernel32.dll", CharSet=CharSet.Unicode)]
  public static extern IntPtr CreateJobObject(IntPtr a, string n);
  [DllImport("kernel32.dll", SetLastError=true)]
  public static extern bool SetInformationJobObject(IntPtr h, int c, IntPtr i, uint l);
  [DllImport("kernel32.dll")]
  public static extern bool AssignProcessToJobObject(IntPtr job, IntPtr proc);
  [DllImport("kernel32.dll")]
  public static extern IntPtr OpenProcess(uint access, bool inherit, int pid);
  [DllImport("kernel32.dll")]
  public static extern bool CloseHandle(IntPtr h);
  public static bool Apply(IntPtr job, IntPtr proc, uint rate) {
    var info = new JOBOBJECT_CPU_RATE_CONTROL_INFORMATION();
    info.ControlFlags = 0x1 | 0x4;   // ENABLE | HARD_CAP
    info.CpuRate = rate;             // HUNDREDTHS of a percent: 200 = 2.00%
    int len = Marshal.SizeOf(typeof(JOBOBJECT_CPU_RATE_CONTROL_INFORMATION));
    IntPtr p = Marshal.AllocHGlobal(len);
    Marshal.StructureToPtr(info, p, false);
    bool ok = SetInformationJobObject(job, 15, p, (uint)len);   // 15 = CpuRateControlInformation
    Marshal.FreeHGlobal(p);
    if (!ok) return false;
    return AssignProcessToJobObject(job, proc);
  }
}
'@
$job = [JobCpuCap]::CreateJobObject([IntPtr]::Zero, $null)
if ($job -eq [IntPtr]::Zero) { Write-Error 'CreateJobObject failed'; exit 3 }
$proc = Start-Process -FilePath '{CMD0}' -ArgumentList @({ARGS}) -PassThru -NoNewWindow
# PROCESS_ALL_ACCESS, NOT the documented minimum, and this cost a round to find.
#
# MEASURED: with the minimal rights the API documents (PROCESS_SET_QUOTA | PROCESS_SET_INFORMATION
# = 0x600), AssignProcessToJobObject returns FALSE with ERROR_ACCESS_DENIED (5) on a plain
# Start-Process child. An access-rights sweep on the same pid: 0x400, 0x600, 0x401 and 0x1600 all
# fail with 5; only 0x1F0FFF succeeds. That is not what the documentation implies, and the failure is
# easy to mistake for a permissions problem with the caller rather than with the handle.
#
# The minimal rights may well be sufficient for a process created some other way; this is the value
# that is MEASURED to work for the child this tool actually starts.
$h = [JobCpuCap]::OpenProcess(0x1F0FFF, $false, $proc.Id)
if ($h -eq [IntPtr]::Zero) { Write-Error 'OpenProcess failed'; exit 3 }
$applied = [JobCpuCap]::Apply($job, $h, ${pct * 100})
if (-not $applied) { Write-Error 'the CPU rate cap was NOT applied'; exit 3 }
Write-Host "vcpu-pin: cap applied - ${cpuEquivalents} core-equivalents = ${pct}% of ${TOTAL_CORES} cores (rate ${pct * 100})"
$proc.WaitForExit()
exit $proc.ExitCode
`;
}

function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.help) { console.log(USAGE); process.exit(0); }
  if (args.bad) { console.error(`vcpu-pin: unknown option ${args.bad}\n`); console.log(USAGE); process.exit(3); }
  if (!args.cpu || !(args.cpu > 0)) { console.error('vcpu-pin: --cpu <n> is required\n'); console.log(USAGE); process.exit(3); }
  if (args.cmd.length === 0) { console.error('vcpu-pin: no command given (use -- <cmd> ...)'); process.exit(3); }

  // THE REFUSAL TESTS THE EXACT PERCENTAGE, NOT THE ROUNDED ONE, and the first version got this wrong
  // in the one direction that matters. MEASURED: `--cpu 0.2` on a 32-core host is 0.625%, which
  // `Math.round` sends to 1% - so a check on the ROUNDED value passed and the tool applied a cap of
  // **0.32 core-equivalents while announcing 0.2**. That is a 60% overstatement of the allowance,
  // produced by the tool whose entire purpose is to stop exactly this kind of quiet substitution.
  //
  // The cap is therefore compared as a RATE against what was asked for, and a request that cannot be
  // expressed within the granularity is refused rather than rounded up - because rounding UP gives the
  // child MORE CPU than the caller asked for, which makes every figure taken under it optimistic.
  const exactPct = (args.cpu / TOTAL_CORES) * 100;
  const pct = Math.round(exactPct);
  // THE TEST IS WHETHER ROUNDING CHANGES THE ALLOWANCE MEANINGFULLY, not whether the rate is small.
  //
  // MEASURED: `--cpu 0.2` on a 32-core host is 0.625%, which rounds to 1% - so the child gets 0.320
  // cores while the banner says 0.2, a 60% overstatement. A first fix tested `exactPct < 0.5`, which
  // 0.625 is not, so it still applied the cap and still mislabelled it. The condition has to compare
  // the APPLIED allowance against the REQUESTED one.
  //
  // Rounding UP always gives the child MORE CPU than asked for, which makes every figure taken under
  // it optimistic. More than 10% over is refused; anything else is reported.
  const appliedCores = (pct * TOTAL_CORES) / 100;
  const overBy = (appliedCores - args.cpu) / args.cpu;
  if (overBy > 0.10) {
    console.error(`vcpu-pin: ${args.cpu} core-equivalents is ${exactPct.toFixed(3)}% of ${TOTAL_CORES} cores.`);
    console.error(`   The nearest expressible rate is ${pct}%, which is ${appliedCores.toFixed(3)} cores.`);
    console.error(`   That is ${(overBy * 100).toFixed(0)}% MORE than asked for, so it is refused rather than`);
    console.error('   substituted: an allowance larger than the target makes every figure taken under it');
    console.error('   optimistic, which is the substitution this tool exists to prevent.');
    console.error('   To measure the documented 0.2 vCPU shape, use a host with fewer cores:');
    console.error('   0.2 vCPU on a 16-core box is 1.25%, which IS expressible.');
    process.exit(3);
  }
  if (Math.abs(exactPct - pct) > 0.01) {
    console.error(`vcpu-pin: note - ${args.cpu} requested, applied as ${pct}% = ${appliedCores.toFixed(3)} cores.`);
  }

  const [cmd0, ...rest] = args.cmd;
  const script = capScript(args.cpu)
    .replace('{CMD0}', cmd0.replace(/'/g, "''"))
    .replace('{ARGS}', rest.map((a) => `'${a.replace(/'/g, "''")}'`).join(','));

  console.log(`vcpu-pin: ${args.cpu} core-equivalents on a ${TOTAL_CORES}-core host = ${pct}%`);

  // THE SCRIPT GOES TO A FILE, NOT TO `-Command`. MEASURED: a multi-line `-Command` string containing
  // a here-string was PRINTED VERBATIM instead of executed - the caller saw the whole script on stdout
  // and the child never ran under the cap. A temp file is the shape PowerShell documents for this and
  // is what makes the here-string terminator behave.
  const tmp = path.join(os.tmpdir(), `vcpu-pin-${process.pid}.ps1`);
  fs.writeFileSync(tmp, script);
  let r;
  try {
    r = spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', tmp], {
      stdio: 'inherit',
    });
  } finally {
    try { fs.unlinkSync(tmp); } catch { /* the file is a temp; a failed unlink is not fatal */ }
  }

  if (r.status === 3) {
    console.error('');
    console.error('vcpu-pin: THE CAP WAS NOT APPLIED. Refusing to report an uncapped run as capped.');
    if (args.allowUncapped) {
      console.error('vcpu-pin: --allow-uncapped given, so running WITHOUT the cap. The number is NOT');
      console.error('vcpu-pin:   a 0.2 vCPU figure and must not be recorded as one.');
      const rr = spawnSync(cmd0, rest, { stdio: 'inherit' });
      process.exit(rr.status ?? 1);
    }
    process.exit(3);
  }
  process.exit(r.status ?? 1);
}

main();
