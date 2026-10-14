#!/bin/sh
# Documentation-figure check - every number a document states about a config key or a RUST const
# must be the number the code defines.
#
# WHY THIS EXISTS. `docs/benchmark.md` published a capacity figure of "connection pool size = 10"
# and a pass criterion built on it, while `db::POOL_MAX_CONNECTIONS` is 8. MEASURED at the time:
# `git log -S 'max_connections(10)' -- server/src/db.rs` returns no commit, so the published figure
# was never true and the criterion was never achievable.
#
# Nothing could have caught it, and the reason is structural rather than careless. The citation
# guard over `docs/` checks LINE references. `benchmark.md` is excluded from that guard by a triage
# reason about citations - "a record of one measurement, not a contract" - so the document's line
# numbers are unread, and its NUMBERS were unread for a different reason that the triage reason does
# not cover. The nearest guard couples the document to the benchmark BINARY's CLI, not to the config.
# Between a reason that did not apply and a sibling pointed elsewhere, the digit had no owner.
#
# WHAT IT CHECKS. For every markdown file under docs/, every `<config_key> = <number>` and
# `<CONSTANT> = <number>` whose left-hand side is a real key in `config/apikita.toml` or a real
# `pub const` in the server, compared against the value in the code.
#
# WHAT IT DELIBERATELY ALLOWS, because a sweep that flags these reports drift that is not there:
# a statement marked as a NON-SHIPPED setting. `config.rs` reads `0` as "off" for both
# `credit_expiry_months` and `key_metadata_cache_seconds`, and three documents state `<key> = 0`
# while describing exactly that. The filter is a list of phrases ("disables", "set to 0",
# "with the cache"), and it is the reason the floor below matters: a filter can hide a class of row.
#
# THE FLOOR IS PART OF THE CONTRACT. MEASURED: 10 comparable statements across 47 documents. If the
# scan finds fewer than 8, it is not reading the tree and the run FAILS rather than passing quietly -
# the same rule tools/alert-check and the doc guards state for their own counts.
#
# Usage: sh tools/doc-figures/check.sh
# Exit: 0 every figure agrees, 1 a mismatch or a short scan, 3 node is missing.
set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
TOOL="$REPO/tools/doc-figures/check.js"

if ! command -v node >/dev/null 2>&1; then
    echo "doc-figures: node is required and was not found on PATH" >&2
    exit 3
fi

if [ ! -f "$TOOL" ]; then
    echo "doc-figures: $TOOL is missing" >&2
    exit 3
fi

exec node "$TOOL"
