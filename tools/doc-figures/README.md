# doc-figures

Every number a document states about a config key or a Rust constant, checked against the code that
defines it.

```
sh tools/doc-figures/check.sh
```

Exit `0` every figure agrees, `1` a mismatch or a scan shorter than its floor, `3` `node` is missing.

## Why this exists

`docs/benchmark.md` published a capacity figure of **`connection pool size = 10`** — twice, once as
what Scenario 3 "stresses" and once inside a pass criterion, *"> 100 settlements/sec with connection
pool size = 10"* — while `db::POOL_MAX_CONNECTIONS` is **8**.

MEASURED at the time this tool was written:

| check | result |
| --- | --- |
| `git log -S 'max_connections(10)' -- server/src/db.rs` | **no commit** — the code has never opened 10 |
| `docs/plans/sqlite-migration.md` (the spec that chose the value) | states **8**, twice, with the contention measurement taken at 8 |
| `server/src/bin/benchmark.rs` | sets no pool at all, so "a different scenario" is not a reading |

So the published figure was never true, and **the pass criterion built on it was never achievable**.

## Why nothing caught it, which is the part worth keeping

- The citation guard over `docs/` checks **line references**. `docs/benchmark.md` is excluded from
  that guard by a triage reason about **citations** — *"a record of one measurement, not a contract"*.
  That reason is correct and it still leaves the document's **numbers** unread, because a number
  stated as fact is a different kind of claim from a line reference.
- The nearest guard, `the_benchmark_doc_describes_a_command_the_binary_actually_has`, couples that
  document to the benchmark **binary's CLI**, not to the configuration.

Between a reason that did not apply and a sibling guard pointed elsewhere, the digit had no owner.

## What it checks

For every `.md` file under `docs/`, every statement shaped `<key> = <number>` where `<key>` is a real
key in `config/apikita.toml`, and every `<CONSTANT> = <number>` where `<CONSTANT>` is a real
`pub const` in `server/src`. Each is compared to the value in the code.

MEASURED: **10 comparable statements across 47 documents.**

## What it deliberately allows

A statement marked as a **non-shipped setting**. `config.rs` reads `0` as "off" for both
`credit_expiry_months` and `key_metadata_cache_seconds`, and three documents state `<key> = 0` while
describing exactly that:

- `docs/terms-of-service.md` — *"`credit_expiry_months = 0` disables the window"*
- `docs/decisions.md` — the same disabling value, in the register
- `docs/website/02-data-model.md` — *"With the cache disabled (`key_metadata_cache_seconds = 0`)"*

The filter is a list of phrases (`disables`, `set to 0`, `with the cache`), and **it is the reason the
floor matters**: a filter can hide a whole class of row, so a scan that finds fewer than 8 comparable
statements **fails** rather than passing quietly.

## The bug in the first version of this tool

The filter was one regex, `/\b(disabl|...)\b/i`, and it matched **nothing** for the word "disables": a
trailing `\b` after the stem `disabl` requires a non-word character next, and the next character is
`e`. So the phrase it was written for slipped through and `docs/terms-of-service.md` was reported as a
mismatch. The filter is now a list of substrings, and the floor would have caught a filter that
suppressed everything — which is why the floor is asserted rather than printed.

## What it is not

It does not replace a dedicated guard. `a_published_pool_size_is_the_pool_size_the_code_opens` reads
one document against one constant and fails for a reason specific to that claim. This tool is the
**sweep** that finds statements nobody has written a guard for; a finding here should usually become
one.

## Related

- [`docs/ci-cd.md`](../../docs/ci-cd.md) — the pipeline table this gate appears in.
- [`tools/ci-docs-check/`](../ci-docs-check/README.md) — asserts the CI document names this step.
