# shell-hazards

Three shell constructs that destroy a measurement, checked across every script under `tools/`.

```
sh tools/shell-hazards/check.sh
```

Exit `0` all hold, `1` a hazard, `3` `node` is missing or too few scripts were found.

## Why this exists

Rounds 4-10 of the work on this repository each found a guard or a harness that **failed to measure
what it claimed**. Each reason went into `docs/testing.md`. Notes are not guards: the same mistake was
made three times in three rounds, in three different forms. Three of those forms are shell-level and
are checked mechanically here.

### 1. A capture that discards the verdict

```sh
OUT=$(some_detector ... | tail -1)     # $? is tail's, not the detector's
```

`$?` across a pipeline is the **last** command's. `tail` succeeded, so the harness sees `0` and cannot
fail regardless of what the detector found. MEASURED in this repository:

```sh
$ DATABASE_URL="sqlite:///tmp/nope.db" sh tools/reconcile/reconcile.sh >/dev/null 2>&1; echo $?
6
$ DATABASE_URL="sqlite:///tmp/nope.db" sh tools/reconcile/reconcile.sh 2>&1 | tail -1 >/dev/null; echo $?
0
```

The detector reported a missing database with exit 6. Piped into `tail`, the shell reports 0.

### 2. A flag set inside a pipeline subshell

```sh
echo "$ITEMS" | while read -r x; do FAILED=1; done    # FAILED is lost
```

The loop runs in a subshell, so the assignment does not survive it. The message still **prints**,
which is what makes it look like it worked. MEASURED: a guard in this repository named a defect it had
found and exited 0 anyway.

### 3. `set -e`

Every gate here runs detectors whose **non-zero exit is the finding** — `reconcile.sh` returns 1 for
drift, 2 for a bad DSN, 5 for a stranded hold, 6 for a missing file. MEASURED: adding `-e` to the
existing `set -u` line aborts three gates on a **clean** tree.

```sh
   tools/alert-check/check.sh     with set -eu: exit 1
   tools/reconcile-check/check.sh with set -eu: exit 1
   tools/backup-check/check.sh    with set -eu: exit 6
```

`set -u` catches an unset variable, which is always a bug. `set -e` turns an expected result into a
fatal one. The two flags are not a pair.

## What it does not flag, which is the whole design

Hazard 1 has **five** live instances in this repository and **every one is correct**:

| site | why it is safe |
| --- | --- |
| `alert-check`'s `PROBE_NAMED` | a floor (`-lt 4`), so a failed `probe.sh` yields 0 and fails |
| `drill.sh`'s `DUMP_SHA` | an empty hash is **omitted** from the record rather than written blank |
| `drill.sh`'s `SRC_SPOT`, `SCR_SPOT` | `[ -z ]` — a failed `sqlite3` gives empty stdout, which fails |
| `rollback/drill.sh`'s `INTEG` | `[ "$INTEG" != "ok" ]` — fails closed, since empty is not "ok" |

A check that flagged those would be a false-positive machine. **The distinguishing property is not
whether the exit code was discarded, but whether the captured value is validated before it is
trusted.** Each of the five is, and each is listed by name in `check.js` **with its validation** — so
deleting one of those validations **fails**, rather than passing because the site sits on an
exemption list.

## The first run found its own author's mistake

The `DUMP_SHA` entry asserted a validation — `[ -n "$DUMP_SHA" ]` — that did not exist where it was
claimed. The real guard is `[ -n "${DUMP_SHA:-}" ]` further down the file, and the pattern was
corrected to match it. That is the argument for the tool rather than for careful reading: a claim
about code, written from memory, was wrong, and the check caught it within a minute of being written.

## What it is not

It does not run the scripts; it reads them. A hazard that is syntactically invisible — the wrong
semantics on a correct-looking line — will not be caught here. It covers the three shapes that were
actually made in this repository, and each is falsified in `check.js`'s tests as noted above.

## Related

- [`docs/testing.md`](../../docs/testing.md) — the full set of traps, including the ones that are not
  shell-level.
- [`docs/ci-cd.md`](../../docs/ci-cd.md) — the pipeline table this gate appears in.
