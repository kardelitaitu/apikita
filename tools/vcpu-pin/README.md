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

**To measure the documented 0.2 vCPU shape, use a box with fewer cores**: 0.2 vCPU on a 16-core host is
1.25%, which is expressible. The refusal names this.

## Related

- [`docs/load-test.md`](../../docs/load-test.md) — the procedure this exists to support, and the reason
  the box shape matters.
- [`tools/loadtest/`](../loadtest/README.md) — the generator that takes the measurement.
