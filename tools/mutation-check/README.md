# mutation-check

A gate that audits the gates: **a gate which requires an optional tool must be able to say "I could not
run", and must not report that as a pass.**

## The defect it found

`sqlite-probes-check` had this:

```sh
if ! command -v python3 >/dev/null 2>&1; then
    echo "sqlite-probes-check: SKIPPED - python3 is not on PATH, so nothing was verified"
    exit 0
fi
```

The message names the failure mode the repository's tooling exists to avoid — a green run that verified
nothing — and then **returns 0**, which every other gate here treats as a pass. On a runner without
python3, the other fourteen gates would go green while the SQLite probes never ran once.

MEASURED rather than assumed: this was the **only** gate under `tools/` that requires an optional tool and
cannot skip. The other thirteen exit 3.

## What it asserts

1. **A gate that shells out to an optional tool exits 3 when the tool is absent.** The tools are
   enumerated — `docker`, `node`, `sqlite3`, `python3`, `curl`, `jq` — and a gate naming one without an
   `exit 3` is reported by name.
2. **Every gate documents its exit codes and its usage**, so a caller can act on the verdict rather than
   guess.

Not asserted, deliberately: what the gate checks or whether its subject is right. That is the gate's own
business, and a check on it would duplicate every other README here.

## This gate's own check was audited, because a grep can be fooled

`exit 3` is found with a regex, and round 27 of this repository's audit found four anchors that matched text
inside a comment — a check that reports ok for a reason that is not there. So this gate's check was verified
rather than assumed, in two ways:

**Statically.** Every gate's `exit 3` occurrences were listed with their line number and whether that line
is a comment. No gate's only occurrence is prose.

**Behaviourally, which is what settled it.** Every optional-tool gate was run twice — once normally, and once
under a stripped `PATH` holding the coreutils the gates need and *not* the tool under test. Two controls
guard each run: **does the stripped shell execute at all**, and **is the tool genuinely absent**. Without
those, a result of `0` is indistinguishable from a run that never happened.

**Result: 12 of 12 gates exit 3, each with a message naming the missing tool.**

The first attempt at that measurement reported twelve defects because its stripped `PATH` was empty — a
harness fault reading as "nothing failed", which is the defect this repository writes gates against. The
controls are what distinguish the two.

`rollback-check` still does more than this gate does: it asserts the skip path **in its own suite**
(`with no sqlite3 the drill must skip loudly with exit 3`), so a future edit that breaks the skip fails the
gate itself rather than waiting for an audit.

## Two false starts, recorded because they are the reason the gate is shaped this way

**It began as a mutation-harness checker.** Round 42 believed a mutation result that a checklist would have
rejected, so this gate was to certify mutation harnesses — scripts that break a subject and restore it. It
named `relay-check/behaviour.sh`, and that script failed six of seven checks. Reading the subject showed
why: it is a **behavioural probe** — it runs a real nginx container and times stream arrival, never mutates
a file, and has no reason to restore one. A check that no script under `tools/` performs a
mutate-and-restore at all confirmed it. The check was sound and the subject was invented.

**Its second version was too broad.** Rewritten against what is true of every gate, it reported
`relay-check: never exits 3`. But `relay-check` is a static check that documents `Exit: 0/1/2` and needs no
external tool, so it has no reason to skip. The rule had measured "exits 3" rather than "can skip when it
cannot run".

Both are the same defect the gate was built to catch — **a gate built on a guessed subject reports false
violations.**

## Usage

```
sh tools/mutation-check/check.sh [tool-dir...]
```

With no argument it audits every gate under `tools/`. With names it audits only those, which is how a new
one can be checked before being committed.

* **0** — every gate can skip loudly and documents itself
* **1** — a violation, naming the gate
* **3** — `node` is missing, or no gate was found
