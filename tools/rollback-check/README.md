# Rollback drill check

Proves [`tools/rollback/drill.sh`](../rollback/README.md) **can fail** and **does refuse**,
not merely that it passes when everything works.

A drill that only ever passes is indistinguishable from a drill that does nothing -- the
argument every `*-check` directory in this repository exists to make. The hazard here is
sharper than usual: the drill's whole claim is *"the restored database is back on the
schema the OLD binary expects"*, and a comparison that silently always reports MATCH would
make that claim unconditionally true.

```sh
sh tools/rollback-check/check.sh
```

## What it asserts

| Assertion | Why it matters |
| --- | --- |
| A **clean** source rolls back to PASS, restoring the pre-migration version | The happy path, with the version assertion that makes this drill distinct from the restore drill |
| A **deliberately wrong** restored schema version makes the drill FAIL | The assertion that binds the drill's defining claim |
| The guard **refuses** live-looking targets, **accepts** scratch ones, and refuses **ambiguous** ones | All three directions: a guard that refuses everything is not a guard |
| A migration that damages **nothing** exits **8**, never 0 | A fixture that cannot express detectable damage must not certify a clean rollback |
| A migration that fails **loudly** is not accepted as a rehearsal | A migration that errors never ships, so there is nothing to roll back |
| A **corrupted** restored database makes the drill fail | The step-7 assertions, driven through their own inputs |
| A missing `sqlite3` skips **loudly (exit 3)**, never 0 | A silent pass on a machine that ran nothing |
| Teardown is honest | The scratch file survives neither a passing nor a failing run |

## An empty reading is not a mismatch

A design point worth stating, because it was found in review rather than by a test.

The drill's spot-check and row-count comparisons read a value from each of two databases
and compare them. If one **query fails**, the variable is empty and a naive comparison
reports *"the totals differ"* — sending the reader after bad **data** when the real fault
is a broken **measurement**. Those are different diagnoses:

- wrong reading → the restored database is wrong, which is what the drill exists to catch
- empty reading → the drill could not tell, which is its own defect

The drill now separates them: a non-numeric reading exits 4 with
`SPOT-CHECK COULD NOT MEASURE`, and only a genuine difference exits 1 with
`SPOT-CHECK FAILED`. Measured symptom that motivated this:
`wallets_total source=5700 restored=` reported as a spot-check failure.

## What this check does NOT cover -- read this before trusting it

Measured with a verified mutation battery (8 mutations, each confirmed to have landed).
**Two of the drill's assertions are mutation-covered; four are not.**

| Drill assertion | Covered? |
| --- | --- |
| schema-version comparison | **yes** |
| exit-8 precondition | **yes** |
| restored `integrity_check` | no |
| restored-drift assertion | no |
| wallet spot-check | no -- reachable, and unpinned (see below) |
| row-count comparison | no -- and unreachable in the desync scenario |
| `LOW == LIVE_LOW` refusal branch | no |

**Why the four are structural rather than missing tests:**

- **integrity** -- the drill checks a file `.restore` just wrote from a snapshot it
  produced itself. Nothing in its interface can make integrity fail, so the assertion is
  defensive and cannot be driven from outside.
- **restored drift, spot-check, row counts** -- all three compare the SNAPSHOT against the
  RESTORED COPY OF THE SAME SNAPSHOT, so they are equal by construction. The
  `ROLLBACK_INJECT_RESTORED` and `ROLLBACK_INJECT_SPOTCHECK` hooks *do* make the drill
  fail, but they trip an EARLIER assertion first, so the later comparisons are never
  reached.

  **WHICH earlier assertion, corrected: it is the SPOT-CHECK, not the restored-drift.**
  This paragraph used to say "they trip the restored-drift assertion first", which is wrong
  in a way that changes the shape of the gap. The drill runs its three comparisons in the
  order restored-drift, spot-check, row-count, and the `SPOTCHECK` hook edits the SNAPSHOT
  (which only the spot-check and the row count read), so it fires the SPOT-CHECK and exits
  from inside that block.

  Measured one mutation at a time on a stable baseline, which separates the three rows of
  this table from each other:

      neuter `SRC_BAL != DST_BAL`   (spot-check)  -> check SURVIVED
      neuter `SRC_ROWS != DST_ROWS` (row count)   -> check SURVIVED
      neuter BOTH                                 -> check FAILED

  So the spot-check is **reachable and unpinned** -- the desync injection also deletes a row,
  so the `A|B` match in the desync block is satisfied by the row-count message while the
  spot-check comparison is dead. The row count is **unreachable**. The two are not a pair
  and this table previously listed them as one line; they are now stated separately above.

  An attempt to "fix" the `A|B` match by requiring both messages was made and reverted: with
  the spot-check binding, the row-count message is never printed, so requiring it asserts an
  unreachable thing and the check goes red on a clean tree.
- **the `LOW == LIVE_LOW` branch** -- the explicit live-looking refusals are separate code
  paths, so neutering this one leaves them working.

These are covered by **reading the drill**, not by this check. If that is not good enough,
the right fix is to **delete the redundant assertions** rather than to add a test that
appears to cover them -- a check whose README implies total coverage is the exact defect
this directory exists to catch.

**AND NO OTHER GATE COVERS THEM EITHER, which was worth checking rather than assuming.**
`tools/drill-check/` also exercises this drill, and its header claims to test that "a
DRIFTED source FAILS" -- so it looked as though the restored-drift assertion might be pinned
there even though it is not pinned here. It is not: neutering `if [ "$RESTORED_RC" -ne 0 ]`
at `drill.sh:643` leaves **both** `rollback-check` and `drill-check` green.

That makes the restored-drift the most consequential of the four. It is the assertion that
fails when the RESTORED database does not reconcile, which is the failure the drill exists to
catch; `docs/backup-and-restore.md` states the reconciliation result as a **pass criterion**
("Zero rows from the reconciliation query is the gate"); and deleting the assertion is
invisible to every automated gate. `drill-check`'s drifted-source test exercises the
*precondition* path (step 5, on the damaged source), not this one (step 7, on the restored
copy) -- two different checks that both read as "drift".

Nothing here is broken. What this note is for is that a reader who greps for a guard on the
restored-drift assertion finds two gate directories and, on both, a passing check. The
protection is a sentence in this README, and it is now a sentence that says so explicitly.


## A note on measurement, because it cost two rounds

An earlier version of this battery reported all eight mutations caught. **That reading was
wrong.** `pgrep` does not exist on this host, so the script's settle loop returned
immediately, concurrent runs collided on the shared scratch tree, and the check failed for
contention -- which made *every* mutation look caught.

A mutation battery is only trustworthy when (a) the baseline passes and (b) each mutation
is verified to have landed. Both are done for the table above; the mutations are applied
with an explicit landed-check and the baseline is asserted at the end.

## Running it

```sh
sh tools/rollback-check/check.sh     # exit 0 = all assertions held
```

It builds throwaway databases under `.agents/` and removes them via `trap ... EXIT INT
TERM`. Needs `sqlite3` and POSIX `sh`; no network, no service container, no `DATABASE_URL`,
no Rust build.

### Run ONE AT A TIME

**This check is not safe to run concurrently with itself**, and the failure it produces does
not look like a collision. Measured, twice, on a clean copy of the tree:

| | result |
| --- | --- |
| two runs, serial | `0`, `0` |
| two runs, started together | **`1`, `1`** |
| serial again afterwards | `0` — it does not persist |

Both concurrent runs fail, and they fail with *specific, alarming* diagnostics rather than an
obvious resource error — for example *"the un-injected control run must PASS, got exit 4"* and
*"the wrong-version run exited 7 but did not report a SCHEMA VERSION MISMATCH"*. A reader who
sees that would reasonably conclude the drill is broken.

**Why.** `check.sh` builds its scratch under one fixed root per repository
(`ROLLBACK_CHECK_WORK`, defaulting to `.agents/rollback-check-work`), with no per-process
isolation, so two runs share a working tree and each sees the other's half-built databases.
The override exists — `ROLLBACK_CHECK_WORK=/some/other/dir` gives a run its own space — which
is the way to parallelise if you must.

**Why this is written here rather than left to the note above.** The note at
["A note on measurement"](#a-note-on-measurement-because-it-cost-two-rounds) records what
contention did to *one mutation battery's* numbers. This is the property itself, and it
belongs beside "Running it", where someone about to run the check will see it. A check that
fails under a parallel harness, with a message that blames the system under test, is worth one
sentence of warning.

## CI

Wired as the step `Check the rollback drill` in `.github/workflows/ci.yml`, after the
restore-drill check and before the CI-documentation check (so the latter can see it).
`tools/ci-docs-check/check.sh` fails if this directory is not indexed in `tools/README.md`
or if the step name is absent from `docs/ci-cd.md`.
