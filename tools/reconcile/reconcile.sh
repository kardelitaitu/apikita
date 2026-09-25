#!/bin/sh
# Ledger reconciliation gate for apikita launch Gate 2.
#
# Applies tools/reconcile/reconcile.sql against the database $DATABASE_URL names,
# using the sqlite3 CLI. Prints any accounts whose wallet balance disagrees with
# the sum of their ledger deltas, and exits non-zero so it can gate CI (e.g.
# launch Gate 2: "The reconciliation query returns zero rows on production data").
#
# Exit codes:
#   0  reconciliation passed (zero drifting accounts)
#   1  drift detected: at least one account where
#      balance_idr <> SUM(ledger.delta_idr)
#   2  DATABASE_URL is not set, is not a SQLite URL, or names an in-memory
#      database (which cannot be reconciled from outside the process)
#   3  sqlite3 is not installed / not on PATH
#   4  sqlite3 ran but failed (unreadable file, SQL error)
#   5  the database file DATABASE_URL names does not exist

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
SQL_FILE="$SCRIPT_DIR/reconcile.sql"

# --- sqlite3 availability ----------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "reconcile: sqlite3 is not installed or not on PATH" >&2
    echo "reconcile: install the SQLite command-line shell (sqlite3) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
if [ -z "$DATABASE_URL" ]; then
    echo "reconcile: DATABASE_URL is not set" >&2
    echo "reconcile: export DATABASE_URL='sqlite://data/server.db'" >&2
    exit 2
fi

# A `case`, not a blind prefix strip: a leftover Postgres URL must be refused
# loudly rather than quietly rewritten into a relative path that happens to be a
# plausible filename.
case "$DATABASE_URL" in
    sqlite://*) DB_PATH=${DATABASE_URL#sqlite://} ;;
    sqlite:*)   DB_PATH=${DATABASE_URL#sqlite:} ;;
    *)
        echo "reconcile: DATABASE_URL is not a SQLite URL: $DATABASE_URL" >&2
        echo "reconcile: expected e.g. sqlite://data/server.db" >&2
        exit 2
        ;;
esac

# Drop any sqlx query string (`?mode=rwc`); it is not part of the filename.
DB_PATH=${DB_PATH%%\?*}

if [ -z "$DB_PATH" ] || [ "$DB_PATH" = ":memory:" ]; then
    echo "reconcile: DATABASE_URL does not name a file: $DATABASE_URL" >&2
    echo "reconcile: an in-memory database cannot be reconciled from outside the process" >&2
    exit 2
fi

if [ ! -f "$DB_PATH" ]; then
    echo "reconcile: no such database file: $DB_PATH" >&2
    echo "reconcile: create it with 'cargo run --bin migrate'" >&2
    exit 5
fi

# --- Run the query -----------------------------------------------------------
# -readonly: reconciliation must never write. A stray UPDATE in reconcile.sql
#   should fail rather than silently move money. On a WAL database this needs the
#   -shm file to be creatable; if the volume forbids it, run the gate against a
#   backup copy instead (docs/backup-and-restore.md).
# -bail: stop at the first error and exit non-zero for it, instead of continuing
#   past a failed statement and reporting a clean sheet.
#
# The SQL is fed on stdin because the sqlite3 CLI has no `-f` option — that is
# psql's spelling, and the original script used it.
OUTPUT=$(sqlite3 -readonly -bail "$DB_PATH" <"$SQL_FILE" 2>&1)
STATUS=$?

if [ "$STATUS" -ne 0 ]; then
    echo "reconcile: sqlite3 failed (exit $STATUS):" >&2
    echo "$OUTPUT" >&2
    exit 4
fi

# Count drifting rows (non-empty lines of output). `grep -c` exits 1 when it
# matches nothing, which is the passing case, so it must not fail the script.
ROWS=$(printf '%s\n' "$OUTPUT" | grep -c . || true)

if [ "$ROWS" -gt 0 ]; then
    echo "reconcile: DRIFT DETECTED - $ROWS account(s) where balance_idr <> SUM(ledger.delta_idr)" >&2
    echo "$OUTPUT" >&2
    exit 1
fi

echo "reconcile: OK - wallet balances reconcile with the ledger (0 drifting accounts)"
exit 0
