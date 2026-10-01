# Verification tools

Everything under `tools/` exists to **check** something. Most of it runs in CI; the CI
document ([`docs/ci-cd.md`](../docs/ci-cd.md)) says which stage calls what.

**Why this index exists.** These tools had no index at all: a search for every directory
below in the root `README.md` and `AGENTS.md` returned **zero hits for all fourteen**. The
root README indexes the documentation and the tooling was invisible, so the only way to
discover that a reconciliation gate or a restore drill existed was to list this directory
or read the workflow. **The tools are where the verification actually lives**, so they are
worth one page.

This file is checked by [`tools/ci-docs-check/`](ci-docs-check/README.md): every directory
here must appear, and every name listed must exist.

## Money

| Tool | What it is for | Runs in CI |
| --- | --- | --- |
| [`reconcile/`](reconcile/README.md) | **The launch Gate 2 money check.** Wallet/ledger drift and stranded holds. *"The single best guard against silently wrong money."* | Yes — mounted into the scheduler smoke |
| [`reconcile-check/`](reconcile-check/README.md) | Proves the money gate can actually **fail** on drift, and passes a consistent ledger. | Yes |
| [`backup/`](backup/README.md) | Takes, verifies, encrypts and ships a backup, with a documented exit-code contract. | No — run on the host or by cron |
| [`backup-check/`](backup-check/README.md) | Proves the artifact **reaches the offsite hook**, and that the artifact is encrypted. | Yes |
| [`drill/`](drill/README.md) | The restore drill: proves a backup restores and reconciles. *"An untested backup is a belief."* | No — run before launch and on a cadence |
| [`drill-check/`](drill-check/README.md) | Proves the drill **refuses every live-looking target before deleting anything**. | Yes |
| [`rollback/`](rollback/README.md) | The bad-migration rollback drill: snapshots before a migration, proves the damage is **detectable**, restores, and asserts the schema came **back**. Fills the open item in [`docs/deployment.md`](../docs/deployment.md) — *"Restore from the snapshot; do not hand-write a reverse migration."* | No — run before launch and after a migration scare |
| [`rollback-check/`](rollback-check/README.md) | Proves the rollback drill **can fail** — and states plainly which of its assertions are **not** mutation-covered. | Yes |
| [`wind-down/`](wind-down/README.md) | The read-only half of the wind-down runbook: who is owed what, and in which rail. **It pays nobody.** | No — run at closure, before Step 5 |
| [`wind-down-check/`](wind-down-check/README.md) | Proves the payout report splits the threshold **strictly**, floors the stablecoin units, and refuses a missing or implausible frozen rate. | Yes |

## Data

| Tool | What it is for | Runs in CI |
| --- | --- | --- |
| [`sqlite-probes/`](sqlite-probes/README.md) | Validates the shipped schema against the plan, and runs 32 invariant probes against it. | Yes |

## Delivery — the things customers notice

| Tool | What it is for | Runs in CI |
| --- | --- | --- |
| [`alert/`](alert/README.md) | **Tells someone** something is wrong. A cooldown so one incident pages once. | No — run by cron |
| [`alert-check/`](alert-check/README.md) | Proves throttling is exit 1 not 0, and that a failed delivery never silences its own retry. | Yes |
| [`relay-check/`](relay-check/README.md) | Proves the edge relay **actually streams SSE**, the failure the doc calls worse than no relay. | Yes |

## Contracts about the repository itself

| Tool | What it is for | Runs in CI |
| --- | --- | --- |
| [`compose-check/`](compose-check/README.md) | Validates the deployment definition and the invariants its comments call load-bearing. | Yes |
| [`ci-docs-check/`](ci-docs-check/README.md) | Asserts the CI document describes the pipeline that actually runs — including this file. | Yes |

## Local scaffolding — not checks

These exist to make local development and testing possible. They assert nothing and are
deliberately not CI stages.

| Tool | What it is for |
| --- | --- |
| [`fake-upstream/`](fake-upstream/README.md) | A stand-in provider, so the proxy can be exercised with no upstream key and no spend. |
| [`fake-midtrans/`](fake-midtrans/README.md) | A stand-in payment callback sender, so the webhook path can be driven locally. |

## A pattern worth knowing

The `*-check` directories exist because a gate that **cannot fail** is worse than no gate:
the tick beside it means something. Each of them was written by running the tool until it
broke, and several found real defects — a buffered relay, a backup hook that reported
success while copying nothing, a money gate whose first half hid its second. Their READMEs
list the mutations they were tested against, because a check that has never been mutated is
a check whose blind spots are unknown.

**That sentence was false for two of the eight until it was checked against them.**
`relay-check` and `backup-check` listed no mutations at all — the same "documented but never
verified" shape the checks themselves exist to catch. Both were then mutated and both
refused correctly (`proxy_buffering on`, `gzip on`, a deleted `/events` block; and an offsite
hook whose failure is swallowed), and the measured results are now in those two READMEs.
Nothing tests this paragraph — it was found by reading it against the directory listing,
which is why the finding is recorded here rather than assumed to stay true.