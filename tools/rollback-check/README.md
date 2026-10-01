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

## What this check does NOT cover -- read this before trusting it

Measured with a verified mutation battery (8 mutations, each confirmed to have landed).
**Two of the drill's assertions are mutation-covered; four are not.**

| Drill assertion | Covered? |
| --- | --- |
| schema-version comparison | **yes** |
| exit-8 precondition | **yes** |
| restored `integrity_check` | no |
| restored-drift assertion | no |
| wallet spot-check / row counts | no |
| `LOW == LIVE_LOW` refusal branch | no |

**Why the four are structural rather than missing tests:**

- **integrity** -- the drill checks a file `.restore` just wrote from a snapshot it
  produced itself. Nothing in its interface can make integrity fail, so the assertion is
  defensive and cannot be driven from outside.
- **restored drift, spot-check, row counts** -- all three compare the SNAPSHOT against the
  RESTORED COPY OF THE SAME SNAPSHOT, so they are equal by construction. The
  `ROLLBACK_INJECT_RESTORED` and `ROLLBACK_INJECT_SPOTCHECK` hooks *do* make the drill
  fail, but they trip the restored-drift assertion first, so the later comparisons are
  never reached.
- **the `LOW == LIVE_LOW` branch** -- the explicit live-looking refusals are separate code
  paths, so neutering this one leaves them working.

These are covered by **reading the drill**, not by this check. If that is not good enough,
the right fix is to **delete the redundant assertions** rather than to add a test that
appears to cover them -- a check whose README implies total coverage is the exact defect
this directory exists to catch.

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

## CI

Wired as the step `Check the rollback drill` in `.github/workflows/ci.yml`, after the
restore-drill check and before the CI-documentation check (so the latter can see it).
`tools/ci-docs-check/check.sh` fails if this directory is not indexed in `tools/README.md`
or if the step name is absent from `docs/ci-cd.md`.
