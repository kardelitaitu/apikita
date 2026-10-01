#!/bin/sh
# Ledger reconciliation gate for apikita launch Gate 2.
#
# Gate 2 runs BOTH money checks here, because one cannot see the other:
#
#   1. wallet/ledger drift - reconcile.sql: wallets.balance_idr vs
#      SUM(ledger.delta_idr) per account. A FULL OUTER JOIN, so it also catches a
#      ledger account with NO wallets row (the cache missing entirely).
#   2. stranded reservation holds - the reserve_<uuid> predicate of
#      server/src/bin/hold-sweep.rs, copied verbatim below. A negative reserve
#      row with no positive row under the same ref is invisible to check 1: the
#      debit and its missing release are both absent from the sum, so the wallet
#      still equals its ledger sum and check 1 returns no row.
#
# Applies both queries against the SQLite database $DATABASE_URL names, using the
# sqlite3 CLI. There is no database server: SQLite is a file the API opens.
#
# Exit codes:
#   0  both checks passed
#   1  drift detected: at least one account where
#      balance_idr <> SUM(ledger.delta_idr), or ledger money with no wallets row
#   2  DATABASE_URL is not set (unset, empty, or whitespace only), is not a
#      SQLite URL, or names an in-memory database (which cannot be reconciled
#      from outside the process)
#   3  sqlite3 is not installed / not on PATH
#   4  sqlite3 ran but failed (unreadable file, SQL error)
#   5  a stranded reservation hold is older than the bound (money unaccounted)
#   6  the database file DATABASE_URL names does not exist
#
# 0-4 and 5 keep their 0.0.1 meanings verbatim; 6 is the one addition the SQLite
# port forced. The port had wanted 5 for a missing database file, but 5 was
# already the stranded hold here, and a code that means two things is worse than
# a new one: an existing consumer that only knows 0-5 still treats 6 as a
# failure, because every non-zero code is one. See README.md.
#
# VERIFIED PORTED (this header previously reported the OPPOSITE as a known gap):
# $SQL_FILE (reconcile.sql) now uses SQLite casts - CAST(w.balance_idr AS TEXT)
# and CAST(COALESCE(SUM(l.delta_idr), 0) AS TEXT) - and the drift query runs
# under the sqlite3 CLI. Measured against a scratch database migrated from
# server/migrations: clean sheet exits 0, injected drift exits 1, ledger money
# with no wallets row exits 1, and an old stranded hold exits 5. One dependency
# that statement carries: the query is a FULL OUTER JOIN, which SQLite has
# supported only since 3.39.0 (2022-06-25) - on an older CLI the query fails and
# the script exits 4 loudly, never a false pass.

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
SQL_FILE="$SCRIPT_DIR/reconcile.sql"

# --- sqlite3 availability ----------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "reconcile: sqlite3 is not installed or not on PATH" >&2
    echo "reconcile: install the SQLite command-line shell (sqlite3) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
# A whitespace-only value is not a DSN: a blind prefix match would turn it into a
# relative path that happens to be a plausible filename. Treat it as unset.
if [ -z "$DATABASE_URL" ]; then
    echo "reconcile: DATABASE_URL is not set" >&2
    echo "reconcile: export DATABASE_URL='sqlite://data/server.db'" >&2
    exit 2
fi
case "$DATABASE_URL" in
    *[![:space:]]*) ;;
    *)
        echo "reconcile: DATABASE_URL is set but contains only whitespace" >&2
        echo "reconcile: export DATABASE_URL='sqlite://data/server.db'" >&2
        exit 2
        ;;
esac

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
    exit 6
fi

# --- Bound for the hold check ------------------------------------------------
# Mirrors hold-sweep's default (server/src/bin/hold-sweep.rs,
# DEFAULT_MAX_HOLD_AGE_SECONDS = 900). A hold younger than this may be a request
# still in flight, so it is reported but does not fail the gate; older than this
# is an incident.
HOLD_MAX_AGE_SECONDS="${HOLD_MAX_AGE_SECONDS:-900}"
case "$HOLD_MAX_AGE_SECONDS" in
    *[!0-9]*|'')
        echo "reconcile: HOLD_MAX_AGE_SECONDS='$HOLD_MAX_AGE_SECONDS' is not a positive integer; using 900" >&2
        HOLD_MAX_AGE_SECONDS=900
        ;;
esac

TMP="${TMPDIR:-/tmp}"
ERR="$TMP/reconcile.$$.err"
HOLD_OUT="$TMP/reconcile-hold.$$.out"
HOLD_ERR="$TMP/reconcile-hold.$$.err"
HOLD_SQL="$TMP/reconcile-hold.$$.sql"
trap 'rm -f "$ERR" "$HOLD_OUT" "$HOLD_ERR" "$HOLD_SQL"' EXIT HUP INT TERM

# The stranded-hold predicate, copied verbatim from server/src/bin/hold-sweep.rs
# (`stranded_holds`), which mirrors db::unpaired_hold_rows. If that predicate
# changes, change this one in the same commit - two definitions of "stranded" is
# how a detector stops being trusted.
#
# Timestamps are RFC3339 text, so the age is an integer subtraction between times.
#
# `strftime('%s', 'now')` READS SQLite's clock here, and the project rule is
# "every timestamp is WRITTEN from Rust, never by SQL" (docs/architecture.md) -- so
# say plainly why this is not a violation, because the comment here used to cite that
# rule as though it justified the call. The rule exists because SQLite's
# `CURRENT_TIMESTAMP` and `datetime('now')` emit a SPACE-SEPARATED form that does not
# compare correctly against the RFC3339 values the code binds, and the columns carry a
# GLOB CHECK so the wrong form cannot be stored at all. Neither is in play: this
# expression is never STORED, and `strftime('%s', ...)` yields integer seconds rather
# than any date string, so no format can leak into a column.
#
# What it does mean is that this age is measured against the READER's clock rather
# than the one that wrote the row. For a report-only diagnostic that is acceptable; a
# WRITER must not do this. The shipped WRITERS do not: no migrated column declares
# `DEFAULT CURRENT_TIMESTAMP` -- the only occurrences in `server/migrations/` are the
# comments stating that rule -- and `validate-migration-schema.py` asserts it over
# sqlite_master. Two SQL-side clocks remain in tools/, both outside that check and both
# outside any customer data: this one, and the `installed_on ... DEFAULT
# CURRENT_TIMESTAMP` column in the table `rollback/drill.sh` SYNTHESISES to stand in for
# sqlx's own `_sqlx_migrations` -- a scratch database the drill creates and deletes.
cat > "$HOLD_SQL" <<'HOLDSQL'
SELECT l.account_id,
       (SELECT i.email FROM identities i
         WHERE i.account_id = l.account_id
         ORDER BY i.email_verified DESC, i.created_at ASC LIMIT 1) AS email,
       l.ref AS reservation_ref,
       CAST(SUM(l.delta_idr) AS INTEGER) AS amount_idr,
       MIN(l.created_at) AS held_at,
       CAST(strftime('%s', 'now') - strftime('%s', MIN(l.created_at)) AS INTEGER) AS age_seconds
FROM ledger l
JOIN accounts a ON a.id = l.account_id
WHERE l.ref LIKE 'reserve_%'
  AND l.delta_idr < 0
GROUP BY l.account_id, l.ref
HAVING NOT EXISTS (
    SELECT 1 FROM ledger m
    WHERE m.account_id = l.account_id
      AND m.ref = l.ref
      AND m.delta_idr > 0
)
ORDER BY held_at ASC;
HOLDSQL

# --- Run the queries ---------------------------------------------------------
# -readonly: reconciliation must never write. A stray UPDATE in reconcile.sql
#   should fail rather than silently move money. On a WAL database this needs the
#   -shm file to be creatable; if the volume forbids it, run the gate against a
#   backup copy instead (docs/backup-and-restore.md).
# -bail: stop at the first error and exit non-zero for it, instead of continuing
#   past a failed statement and reporting a clean sheet.
#
# The SQL is fed on stdin because the sqlite3 CLI has no `-f` option - that is
# psql's spelling, and the original script used it.
#
# Check 1: wallet/ledger drift. sqlite3's default list output is already
# pipe-separated with no header, matching the old `psql -t -A -F'|'` shape.
DRIFT=$(sqlite3 -readonly -bail "$DB_PATH" <"$SQL_FILE" 2>"$ERR")
STATUS=$?

if [ "$STATUS" -ne 0 ]; then
    echo "reconcile: sqlite3 failed (exit $STATUS):" >&2
    [ -s "$ERR" ] && cat "$ERR" >&2
    [ -n "$DRIFT" ] && printf '%s\n' "$DRIFT" >&2
    exit 4
fi

# Check 2: stranded reservation holds. stdout only - a diagnostic on stderr must
# never be counted as a hold row.
HOLD_ROWS=$(sqlite3 -readonly -bail "$DB_PATH" <"$HOLD_SQL" 2>"$HOLD_ERR")
HOLD_STATUS=$?

if [ "$HOLD_STATUS" -ne 0 ]; then
    echo "reconcile: sqlite3 failed (exit $HOLD_STATUS) running the stranded-hold check:" >&2
    [ -s "$HOLD_ERR" ] && cat "$HOLD_ERR" >&2
    exit 4
fi

# A Windows sqlite3 emits CRLF, and command substitution keeps the CR of every
# line but the last. The age field parsed with ${line##*|} would then end in a
# CR, fail the *[!0-9]* guard, and be forced to 0 - never over the bound. Since
# the rows are ORDER BY held_at ASC, an over-bound hold followed by any newer
# hold was silently missed: the gate exited 0 instead of 5, failing OPEN. The
# strip is a no-op on LF-only output (ids and refs never contain a CR).
HOLD_ROWS=$(printf '%s\n' "$HOLD_ROWS" | tr -d '\r')

# --- sqlite3 diagnostics -----------------------------------------------------
# Surfaced, never counted as drift.
if [ -s "$ERR" ]; then
    echo "reconcile: sqlite3 diagnostics on stderr (diagnostic, NOT drift):" >&2
    cat "$ERR" >&2
fi
if [ -s "$HOLD_ERR" ]; then
    echo "reconcile: sqlite3 diagnostics on stderr from the hold check (diagnostic, NOT drift):" >&2
    cat "$HOLD_ERR" >&2
fi

# --- Count drifting rows -----------------------------------------------------
# Only non-empty STDOUT lines count. `grep -c` exits 1 when it matches nothing,
# which is the passing case, so it must not fail the script.
ROWS=$(printf '%s\n' "$DRIFT" | grep -c . || true)

# --- Count stranded holds ---------------------------------------------------
HOLDS=$(printf '%s\n' "$HOLD_ROWS" | grep -c . || true)
OVER_BOUND=0
if [ "$HOLDS" -gt 0 ]; then
    while IFS= read -r line; do
        [ -n "$line" ] || continue
        age=${line##*|}
        case "$age" in
            ''|*[!0-9]*) age=0 ;;
        esac
        if [ "$age" -gt "$HOLD_MAX_AGE_SECONDS" ]; then
            OVER_BOUND=$((OVER_BOUND + 1))
        fi
    done <<HOLDLINES
$HOLD_ROWS
HOLDLINES
fi

# --- Report the hold check on EVERY run -------------------------------------
# Silence here would be the silent pass this gate exists to prevent: a stranded
# hold is invisible to the drift query, so it must be stated even when the drift
# check is clean.
echo "reconcile: HOLD SWEEP - $HOLDS stranded reservation hold(s) (a negative reserve_ row with no positive row under the same ref); $OVER_BOUND older than ${HOLD_MAX_AGE_SECONDS}s"
if [ "$HOLDS" -gt 0 ]; then
    echo "reconcile:   account_id|email|reservation_ref|amount_idr|held_at|age_seconds"
    printf '%s\n' "$HOLD_ROWS"
fi
echo "reconcile: HOLD SWEEP REQUIRED - the drift query above is structurally blind to a stranded"
echo "reconcile:   hold (debit and missing release are both out of the sum)."
echo "reconcile:   This detector RUNS ON A SCHEDULE: the scheduler's run_hold_sweep applies the"
echo "reconcile:   SAME predicate as server/src/bin/hold-sweep.rs nightly with a 900s bound, and"
echo "reconcile:   is report-only. Treat a hold unpaired at TWO CONSECUTIVE sweeps as an incident."
echo "reconcile:   What is NOT automated is the RELEASE - returning money to an account stays a"
echo "reconcile:   deliberate operator action, never a timer:"
echo "reconcile:     DATABASE_URL='<dsn>' cargo run --manifest-path server/Cargo.toml --bin hold-sweep --release"

# --- Verdict ----------------------------------------------------------------
if [ "$ROWS" -gt 0 ]; then
    echo "reconcile: DRIFT DETECTED - $ROWS account(s) where the wallet cache disagrees with the authoritative ledger" >&2
    echo "reconcile: columns: account_id|balance_idr|ledger_sum  ('NO WALLET ROW' = ledger money with no wallets row)" >&2
    printf '%s\n' "$DRIFT" >&2
    exit 1
fi

if [ "$OVER_BOUND" -gt 0 ]; then
    echo "reconcile: STRANDED HOLD - $OVER_BOUND hold(s) unpaired for more than ${HOLD_MAX_AGE_SECONDS}s: money left a wallet and came back nowhere" >&2
    printf '%s\n' "$HOLD_ROWS" >&2
    exit 5
fi

echo "reconcile: OK - wallet balances reconcile with the ledger (0 drifting accounts); 0 holds over the ${HOLD_MAX_AGE_SECONDS}s bound"
exit 0
