# Alert-delivery check

Verifies [`tools/alert/alert.sh`](../alert/alert.sh) — the tool that **tells someone**
something is wrong.

**Why it matters more than its size suggests.** Every other gate in this repository
*detects* a problem; this one has to *deliver* it. Its documentation is dense with
deliberate distinctions that a tidy-up can silently collapse:

> `tools/alert/README.md:111` — *"A throttled alert is **exit 1, not exit 0**. 'Suppressed
> on purpose' and 'delivered' are different facts and must not be conflated."*
> `:114` — *"The cooldown is recorded **only AFTER** the channel accepted the alert.
> Recording it first would let a failed delivery suppress its own retry for the whole
> window — the worst of both: nobody was told, and nothing will try again for 15 minutes."*
> `:99` — the cooldown exists because *"a small business with thin margins cannot afford
> alert fatigue."*

```sh
sh tools/alert-check/check.sh
```

Hermetic: it uses the **file** channel (`ALERT_SINK_FILE`) and a per-scenario state
directory, so there is no network and no credentials.

## What it proves

| Behaviour | Assertion |
| --- | --- |
| Delivered | exit **0** |
| **Throttled** | exit **1**, *not* 0 — collapsing it would make an alert nobody received indistinguishable from one that was |
| A different alert key | exit **0** — keys do not throttle each other |
| Unknown alert id | exit **4** |
| **A failed delivery** | does **not** record a cooldown |
| A successful delivery | **does** record a cooldown |

## The ordering property is the one that matters

Its failure is **invisible in normal operation**: alerts still arrive, but one failed
delivery silences that incident for the whole window — *"nobody was told, and nothing will
try again."*

So it is tested by attempting **twice** against a failing channel and asserting **both**
attempts reached it (exit 3 each). If the first had recorded a cooldown, the second would
be throttled (exit 1) and the tool would look healthy while being useless.

The positive direction is asserted too: a *successful* delivery **must** record the
cooldown. Without it, "no state file was written" would pass on a tool that never writes
state — a throttle that never engages and pages on every run.

## A cooldown that could not be recorded must not be CLAIMED

The state write was guarded with `|| true`. A state directory that existed while the
**file** could not be written therefore produced the worst possible report:

```
alert: DELIVERED 'ledger_drift' via file (key 'ledger_drift', cooldown 900s)
```

…with nothing on disk. The throttle never engages, so **every run of the check pages
again** — the exact outcome `ALERT_COOLDOWN_SECONDS` exists to prevent — and the log tells
an operator the opposite. The `else` branch already warned when the *directory* was
unwritable; the *file* case was silent, and it is the same fact.

The tool now says so:

```
alert: warning: cooldown is NOT effective: NOT RECORDED - could not write <path>
alert:   the alert WAS delivered, but the next run will page again: throttling is OFF
alert: DELIVERED 'ledger_drift' via file (key 'ledger_drift', cooldown 900s REQUESTED BUT NOT RECORDED)
```

Exit stays **0**: the alert *was* delivered, so reporting a failure would be wrong. What
changes is that the run states plainly that throttling is off — an operational condition
someone has to fix before the next run pages repeatedly.

**A silent absence is debuggable; a false attestation is not.**

## Mutation-tested

| Mutation | Caught by |
| --- | --- |
| Throttled exit changed `1` → `0` | the throttled assertion |
| The cooldown recorded **before** delivery | the two-attempt ordering test |
| The throttle check deleted | the throttled assertion |
| An unknown alert id no longer rejected | the usage assertion |
| A failed state write no longer sets the status | the not-recorded assertions |