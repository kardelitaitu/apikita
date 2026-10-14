# vcpu-pin

Run a command under a **hard CPU rate cap** — the Windows analogue of `docker run --cpus=0.2`.

```
node tools/vcpu-pin/vcpu-pin.js --cpu 0.2 -- node server.js
node tools/vcpu-pin/vcpu-pin.js --help
```

Exit: the child's exit code, or `3` if the cap could not be applied.

## Why this exists

`docs/load-test.md` states that the two published benchmark targets are quoted per **0.2 vCPU**, and
that a figure measured on an unconstrained box is not evidence about the deployment because the bar
assumes the process is CPU-starved. The documented way to get that shape is `docker run --cpus=0.2`.

**MEASURED on the development host: Docker Desktop is not running**, so that route is unavailable
there. This tool is the substitute that does not need a container runtime.

## The obvious substitute is wrong, and the size of the error is the point

Pinning a process to **one logical CPU** is the usual answer. On a 32-core box that is **1.0
core-equivalents — five times** the documented 0.2. Reporting it as "the 0.2 vCPU figure" would be the
same laundering the load-test harness already refuses to do for its p99: quoting a measurement taken
under conditions the target does not describe.

`JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP` caps a process tree at a *percentage of total system CPU*, which
is what `--cpus` does.

## It refuses to run uncapped

If the cap cannot be applied, this exits `3` rather than running the child anyway. A pin that silently
did nothing would make every figure taken through it a fiction. `--allow-uncapped` exists for debugging
and prints that the number is **not** a capped figure.

## Verified against prediction

Measured on a 32-core host against a single-threaded child that reports its own wall time. A cap of `C`
cores should make a one-core job take `1/C` as long again:

| `--cpu` | rate | effective cores | predicted | **measured** |
| --- | --- | --- | --- | --- |
| 1.0 | 3% | 0.96 | 1.04× | **1.01×** |
| 0.5 | 2% | 0.64 | 1.56× | **1.64×** |
| 0.25 | 1% | 0.32 | 3.13× | **3.61×** |

All within 15% of prediction. **The cap binds.**

## Two things that cost a round each, recorded so they cost nobody else one

**1. The struct is 8 bytes, not 12.** `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` is:

```
DWORD ControlFlags;                        // 4
union { DWORD CpuRate; DWORD Weight; };    // 4   <- a UNION, not two fields
```

Three `UInt32` fields is 12 and `SetInformationJobObject` returns FALSE with `ERROR_BAD_LENGTH` (24).
**MEASURED: the child still runs**, so a caller that does not check the return gets an uncapped process
and a comment claiming the cap was applied.

**2. `PROCESS_ALL_ACCESS`, not the documented minimum.** The API documents
`PROCESS_SET_QUOTA | PROCESS_SET_INFORMATION` (0x600) as sufficient for `AssignProcessToJobObject`.
MEASURED on a `Start-Process` child, that fails with **`ERROR_ACCESS_DENIED` (5)**. An access-rights
sweep on the same pid: 0x400, 0x600, 0x401 and 0x1600 all fail; only `0x1F0FFF` succeeds. The failure is
easy to misread as a problem with the caller's privileges rather than with the handle.

## Granularity, stated because it limits what can be expressed

Windows CPU rate control takes **hundredths of a percent**, but the round-trip through the API here is
whole percent. On a 32-core box that is **0.32 core-equivalents minimum** — so `--cpu 0.2` is **not
expressible** and is refused with the arithmetic, rather than silently rounded.

### The refusal is correct, and it is NOT the end of the road

This file used to say *"to measure the documented 0.2 vCPU shape, use a box with fewer cores"*. That was
true of the Windows Job Object and wrong as guidance, because there is a route that needs no different
hardware at all.

**cgroup v2's `cpu.max` expresses a fractional quota exactly, on any core count.** It takes
`<quota_us> <period_us>`, so `20000 100000` **is** 0.20 of one CPU — not rounded, not substituted.

MEASURED, and this is the whole argument: a 3-second spin loop pinned that way consumed
**`usage_usec 622639` = 0.208 cores**, with **`nr_throttled 31` of 31 periods**. The cap bit, and the
figure landed on the target.

```sh
# Requires root: /sys/fs/cgroup is rw but not writable by an unprivileged user.
mkdir -p /sys/fs/cgroup/loadtest
echo 20000 100000 > /sys/fs/cgroup/loadtest/cpu.max     # 0.20 cores, exactly
echo $$ > /sys/fs/cgroup/loadtest/cgroup.procs          # from INSIDE the process to be capped
```

**One trap, because it cost a measurement:** `echo $$` in a subshell writes the *parent's* pid, so the
process you meant to cap never enters the cgroup. The first attempt here reported `usage_usec 1008`
against a 2-second spin — which reads like "the cap works wonderfully" and actually means "nothing was
capped". Use `$BASHPID`, or `echo` from inside the process being measured, and **check `nr_throttled`
is non-zero** before believing the run.

**So the two tools are complementary, not redundant:**

| | expresses 0.2 vCPU on 32 cores? | needs privilege? |
| --- | --- | --- |
| Job Object (`vcpu-pin.js`) | **no** — 1% floor, 60% over | no |
| cgroup v2 `cpu.max` | **yes** — exactly | **root** |

Prefer the Job Object where 1% granularity suffices and on Windows, where cgroups do not exist. Reach
for the cgroup when the target is fractional and a privileged shell is available. Only fall back to
"fewer cores" when neither is true.

## Related

- [`docs/load-test.md`](../../docs/load-test.md) — the procedure this exists to support, and the reason
  the box shape matters.
- [`tools/loadtest/`](../loadtest/README.md) — the generator that takes the measurement.
