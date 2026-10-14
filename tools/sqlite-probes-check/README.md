# sqlite-probes-check — the probes behind a shipped plan still run

```sh
sh tools/sqlite-probes-check/check.sh
```

Runs in CI. Exit 1 if the probe evidence is broken, 0 (with a loud SKIPPED) if `python3` is absent.

## Why it exists

`docs/plans/sqlite-migration.md` cites [`tools/sqlite-probes/`](../sqlite-probes/README.md) as the
evidence for the whole SQLite port — and the port is **shipped**. Nothing re-ran them: three Python
scripts with no `check.sh`, no CI step, and no assertion that the environment they run in can support
the schema they describe.

## The hole this closed

**The plan's most important invariant was measured by nothing.** `sqlite-migration.md:1441` says every
table is `STRICT`, calls it *"the single most important line in this appendix"*, and gives the reason:
without it `INTEGER` is only *affinity*, so a `REAL` lands in a money column silently. The same section
states the requirement it creates — **SQLite ≥ 3.37**.

MEASURED before this gate:

- the probe suite had **no `STRICT` probe at all**
- the SQLite version was **printed on the banner and never checked**

So a host with an older library would run all 24 probes green and then fail to create the first table.
That is this repository's recurring defect in its quieter form: not a check that cannot fail, but a
measurement that cannot notice the one precondition it depends on.

## What was added

Four probes in `sqlite-port-probe.py`:

| probe | what it establishes |
| --- | --- |
| SQLite version supports `STRICT` (≥ 3.37) | **asserted, not printed** — the precondition for the schema existing at all |
| `STRICT` rejects a wrong type | TEXT cannot land in an `INTEGER` column |
| **`STRICT` rejects a REAL in a money column** | the exact failure §4.9 names: a float reaching the wallet |
| control: a **non**-`STRICT` table accepts both | without it, a rejected value would not be evidence about `STRICT` at all |

The version comparison uses `sqlite_version_info`, **not** the string: `"3.9.0" > "3.37.0"` is true
lexicographically, which would pass exactly the host that cannot run the schema.

MEASURED on this host: SQLite **3.45.1**, the plan's floor is 3.37, `STRICT` rejects
`cannot store REAL value in INTEGER column s.amount_idr`, and the shipped schema carries **24 tables
with 32 `STRICT` occurrences** — so the invariant is load-bearing, not decorative.

## What the gate asserts, and why each is a behaviour

1. **All three probes run and exit 0.** A probe that crashes cites nothing.
2. **Every probe emits at least one `[PASS]`/`[FAIL]`/`[ERROR]` verdict** — because "ran (exit 0)" is not
   the same as "checked something". MEASURED counts on this host: **28 / 8 / 39**.
3. **The port probe reports ≥ 28 probes.** `[ERROR]` rows are *expected* — they are the Postgres-syntax
   probes, where SQLite rejecting the statement IS the documented result. The count is the signal: a
   suite that silently shrinks is how coverage disappears.
4. **The four `STRICT` probes are present, by name.** So a future edit cannot quietly remove the one
   measurement the plan's most important line depends on.
5. **The version precondition holds on this host**, reported as a failure about *the host* rather than
   about the assertion.

### The probe that printed verdicts it never checked

Assertion 2 exists because of a specific finding. `sqlite-timestamp-probe.py` printed `(want N)` beside
every measurement and compared none of them — **MEASURED: patching it so a value contradicted its own
printed `want` still exited 0.** It documents the timestamp hazard that `docs/plans/sqlite-migration.md`
calls *"the finding most likely to have caused a production incident"*, while being unable to notice that
hazard changing.

It now has **8 assertions and a real exit code**, and its section 3 was corrected while doing so: the line
read `(want 1)` under the heading *"Same instant, two formats, is NOT equal"*. The two strings differ at
position 10, where `'T'` (0x54) sorts above `' '` (0x20), so they are **never** equal as text — the
expectation contradicted its own heading and could not be satisfied. It reads `(want 0)` now, with the
reason recorded beside it.

**That is two defects in one 137-line file, both of the same shape: a stated expectation that the code
never enforced.**

## Proving it can fail

| provocation | result |
| --- | --- |
| the four `STRICT` probes dropped from the suite | **caught** — reports 24 probes and names the missing invariant |
| a probe file deleted | **caught** — "the plan cites a result nothing can reproduce" |
| a probe made to crash | **caught** — "$p exited 1" |
| **a probe's verdict lines removed** (back to printing only) | **caught** — "prints no verdict at all, so its exit code says only that Python ran" |

Each message names the condition rather than the symptom, because a gate that fails without saying what
broke sends the next reader back to the source.

**One was verified by accident, which is worth recording.** While testing the last provocation, a
`git checkout --` on the *uncommitted* probe restored it to its committed, assertion-free state — and this
gate immediately failed and named the probe. That is the end-to-end behaviour working on a real
regression rather than an injected one.

**And `tools/shell-hazards/check.sh` caught a hazard in this gate while it was being written:**
`n=$(python3 ... | grep -c ...)` captures the *last* stage's status, so a crashing probe would have been
counted from whatever `grep` happened to see. It captures the output first now.

## What it does NOT check

It does not re-derive the probes' *values* — the table in `sqlite-probes/README.md` records what was
measured, and this gate confirms the suite still runs and has not shrunk. A probe whose result silently
changes from `PASS` to a different pass is caught by the named rows, not by a full transcript comparison.
