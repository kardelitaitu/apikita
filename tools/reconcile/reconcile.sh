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
# Exit codes (0-4 unchanged; 5 is additive):
#   0  both checks passed
#   1  drift detected: at least one account where
#      balance_idr <> SUM(ledger.delta_idr), or ledger money with no wallets row
#   2  DATABASE_URL is not set (unset, empty, or whitespace only)
#   3  psql is not installed / not on PATH
#   4  psql ran but failed (connection, permissions, SQL error)
#   5  a stranded reservation hold is older than the bound (money unaccounted)
#
# Rows are read from psql STDOUT only. psql STDERR is diagnostics - a NOTICE or
# WARNING on a healthy database must never be counted as drift.

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
SQL_FILE="$SCRIPT_DIR/reconcile.sql"

# --- psql availability -------------------------------------------------------
if ! command -v psql >/dev/null 2>&1; then
    echo "reconcile: psql is not installed or not on PATH" >&2
    echo "reconcile: install the PostgreSQL client (psql) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
# A whitespace-only value is not a DSN: passing it to psql makes psql fall back
# to a local socket and exit 4, which is not the documented meaning. Treat it as
# unset.
if [ -z "$DATABASE_URL" ]; then
    echo "reconcile: DATABASE_URL is not set" >&2
    echo "reconcile: export DATABASE_URL='postgres://user:pass@host:5432/db'" >&2
    exit 2
fi
case "$DATABASE_URL" in
    *[![:space:]]*) ;;
    *)
        echo "reconcile: DATABASE_URL is set but contains only whitespace" >&2
        echo "reconcile: export DATABASE_URL='postgres://user:pass@host:5432/db'" >&2
        exit 2
        ;;
esac

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
OUT="$TMP/reconcile.$$.out"
ERR="$TMP/reconcile.$$.err"
HOLD_OUT="$TMP/reconcile-hold.$$.out"
HOLD_ERR="$TMP/reconcile-hold.$$.err"
HOLD_SQL="$TMP/reconcile-hold.$$.sql"
trap 'rm -f "$OUT" "$ERR" "$HOLD_OUT" "$HOLD_ERR" "$HOLD_SQL"' EXIT HUP INT TERM

# The stranded-hold predicate, copied verbatim from server/src/bin/hold-sweep.rs
# (`stranded_holds`), which mirrors db::unpaired_hold_rows. If that predicate
# changes, change this one in the same commit - two definitions of "stranded" is
# how a detector stops being trusted.
cat > "$HOLD_SQL" <<'HOLDSQL'
SELECT l.account_id, a.pb_user_id, l.ref AS reservation_ref,
       SUM(l.delta_idr)::bigint AS amount_idr,
       MIN(l.created_at) AS held_at,
       EXTRACT(EPOCH FROM (now() - MIN(l.created_at)))::bigint AS age_seconds
FROM ledger l
JOIN accounts a ON a.id = l.account_id
WHERE l.ref LIKE 'reserve_%'
  AND l.delta_idr < 0
GROUP BY l.account_id, a.pb_user_id, l.ref
HAVING NOT EXISTS (
    SELECT 1 FROM ledger m
    WHERE m.account_id = l.account_id
      AND m.ref = l.ref
      AND m.delta_idr > 0
)
ORDER BY held_at ASC;
HOLDSQL

# --- Check 1: wallet/ledger drift -------------------------------------------
# ON_ERROR_STOP=1 matters: without it psql prints a SQL error to stderr and still
# exits 0, and since stderr is no longer counted as drift that would be a silent
# pass. With it, a broken query is a genuine psql failure -> exit 4.
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -t -A -F'|' -f "$SQL_FILE" >"$OUT" 2>"$ERR"
STATUS=$?

if [ "$STATUS" -ne 0 ]; then
    echo "reconcile: psql failed (exit $STATUS):" >&2
    [ -s "$ERR" ] && cat "$ERR" >&2
    [ -s "$OUT" ] && cat "$OUT" >&2
    exit 4
fi

# --- Check 2: stranded reservation holds ------------------------------------
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -t -A -F'|' -f "$HOLD_SQL" >"$HOLD_OUT" 2>"$HOLD_ERR"
HOLD_STATUS=$?

if [ "$HOLD_STATUS" -ne 0 ]; then
    echo "reconcile: psql failed (exit $HOLD_STATUS) running the stranded-hold check:" >&2
    [ -s "$HOLD_ERR" ] && cat "$HOLD_ERR" >&2
    [ -s "$HOLD_OUT" ] && cat "$HOLD_OUT" >&2
    exit 4
fi

# --- psql diagnostics -------------------------------------------------------
# Warnings, notices and the like. Surfaced, never counted as drift.
if [ -s "$ERR" ]; then
    echo "reconcile: psql diagnostics on stderr (diagnostic, NOT drift):" >&2
    cat "$ERR" >&2
fi
if [ -s "$HOLD_ERR" ]; then
    echo "reconcile: psql diagnostics on stderr from the hold check (diagnostic, NOT drift):" >&2
    cat "$HOLD_ERR" >&2
fi

# --- Count drifting rows ----------------------------------------------------
# Only non-empty STDOUT lines count. The column header is skipped so a stray
# header can never be mistaken for a drifting account.
ROWS=0
DRIFT=""
while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
        ''|'account_id|balance_idr|ledger_sum') continue ;;
    esac
    ROWS=$((ROWS + 1))
    if [ -z "$DRIFT" ]; then
        DRIFT="$line"
    else
        DRIFT="$DRIFT
$line"
    fi
done < "$OUT"

# --- Count stranded holds ---------------------------------------------------
HOLDS=0
OVER_BOUND=0
HOLD_ROWS=""
while IFS= read -r line || [ -n "$line" ]; do
    case "$line" in
        ''|'account_id|pb_user_id|reservation_ref|amount_idr|held_at|age_seconds') continue ;;
    esac
    HOLDS=$((HOLDS + 1))
    age=${line##*|}
    case "$age" in
        ''|*[!0-9]*) age=0 ;;
    esac
    if [ "$age" -gt "$HOLD_MAX_AGE_SECONDS" ]; then
        OVER_BOUND=$((OVER_BOUND + 1))
    fi
    if [ -z "$HOLD_ROWS" ]; then
        HOLD_ROWS="$line"
    else
        HOLD_ROWS="$HOLD_ROWS
$line"
    fi
done < "$HOLD_OUT"

# --- Report the hold check on EVERY run -------------------------------------
# Silence here would be the silent pass this gate exists to prevent: a stranded
# hold is invisible to the drift query, so it must be stated even when the drift
# check is clean.
echo "reconcile: HOLD SWEEP - $HOLDS stranded reservation hold(s) (a negative reserve_ row with no positive row under the same ref); $OVER_BOUND older than ${HOLD_MAX_AGE_SECONDS}s"
if [ "$HOLDS" -gt 0 ]; then
    echo "reconcile:   account_id|pb_user_id|reservation_ref|amount_idr|held_at|age_seconds"
    printf '%s\n' "$HOLD_ROWS"
fi
echo "reconcile: HOLD SWEEP REQUIRED - the drift query above is structurally blind to a stranded"
echo "reconcile:   hold (debit and missing release are both out of the sum). The authoritative"
echo "reconcile:   sweep is server/src/bin/hold-sweep.rs; it needs DATABASE_URL, is report-only"
echo "reconcile:   (never moves money unless --release is passed) and defaults to a 900s bound:"
echo "reconcile:     DATABASE_URL='<dsn>' cargo run --manifest-path server/Cargo.toml --bin hold-sweep"
echo "reconcile:   Nothing schedules it yet (no CI workflow, no compose service). Run it, and"
echo "reconcile:   treat a hold unpaired at two consecutive sweeps as an incident."

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
