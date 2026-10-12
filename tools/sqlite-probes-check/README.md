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
2. **The port probe reports ≥ 28 probes.** `[ERROR]` rows are *expected* — they are the Postgres-syntax
   probes, where SQLite rejecting the statement IS the documented result. The count is the signal: a
   suite that silently shrinks is how coverage disappears.
3. **The four `STRICT` probes are present, by name.** So a future edit cannot quietly remove the one
   measurement the plan's most important line depends on.
4. **The version precondition holds on this host**, reported as a failure about *the host* rather than
   about the assertion.

## Proving it can fail

| provocation | result |
| --- | --- |
| the four `STRICT` probes dropped from the suite | **caught** — reports 24 probes and names the missing invariant |
| a probe file deleted | **caught** — "the plan cites a result nothing can reproduce" |
| a probe made to crash | **caught** — "$p exited 1" |

Each message names the condition rather than the symptom, because a gate that fails without saying what
broke sends the next reader back to the source.

## What it does NOT check

It does not re-derive the probes' *values* — the table in `sqlite-probes/README.md` records what was
measured, and this gate confirms the suite still runs and has not shrunk. A probe whose result silently
changes from `PASS` to a different pass is caught by the named rows, not by a full transcript comparison.
