#!/bin/sh
# Ledger reconciliation gate for apikita launch Gate 2.
#
# Applies tools/reconcile/reconcile.sql against $DATABASE_URL using psql.
# Prints any accounts whose wallet balance disagrees with the sum of their
# ledger deltas, and exits non-zero so it can gate CI (e.g. launch Gate 2:
# "The reconciliation query returns zero rows on production data").
#
# Exit codes:
#   0  reconciliation passed (zero drifting accounts)
#   1  drift detected: at least one account where
#      balance_idr <> SUM(ledger.delta_idr)
#   2  DATABASE_URL is not set
#   3  psql is not installed / not on PATH
#   4  psql ran but failed (connection, permissions, SQL error)

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
SQL_FILE="$SCRIPT_DIR/reconcile.sql"

# --- psql availability -------------------------------------------------------
if ! command -v psql >/dev/null 2>&1; then
    echo "reconcile: psql is not installed or not on PATH" >&2
    echo "reconcile: install the PostgreSQL client (psql) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
if [ -z "$DATABASE_URL" ]; then
    echo "reconcile: DATABASE_URL is not set" >&2
    echo "reconcile: export DATABASE_URL='postgres://user:pass@host:5432/db'" >&2
    exit 2
fi

# --- Run the query ----------------------------------------------------------
OUTPUT=$(psql "$DATABASE_URL" -t -A -F'|' -f "$SQL_FILE" 2>&1)
STATUS=$?

if [ "$STATUS" -ne 0 ]; then
    echo "reconcile: psql failed (exit $STATUS):" >&2
    echo "$OUTPUT" >&2
    exit 4
fi

# Count drifting rows (non-empty lines of output).
ROWS=$(printf '%s\n' "$OUTPUT" | grep -c . || true)

if [ "$ROWS" -gt 0 ]; then
    echo "reconcile: DRIFT DETECTED - $ROWS account(s) where balance_idr <> SUM(ledger.delta_idr)" >&2
    echo "$OUTPUT" >&2
    exit 1
fi

echo "reconcile: OK - wallet balances reconcile with the ledger (0 drifting accounts)"
exit 0
