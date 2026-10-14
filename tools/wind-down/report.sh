#!/bin/sh
# Wind-down closure payout REPORT for apikita. READ-ONLY. It PAYS NOBODY.
#
# WHY THIS EXISTS. docs/decisions.md:71 promises: "Deliverable is docs/wind-down.md
# plus a read-only report." The runbook was written; the report was not. A grep for
# 'wind-down' across tools/ returned nothing, so the runbook's payout SQL was left as
# markdown an operator hand-copies under pressure -- exactly the failure mode this
# codebase guards against everywhere else (the runbook itself is the only doc whose
# SQL is meant to be RUN, and nothing executed it).
#
# WHAT IT IS. A self-describing, read-only report over the SQLite file $DATABASE_URL
# names, covering three of the runbook's steps in one run:
#
#   * a PRE-FLIGHT summary -- how many balances are in scope and how much money
#   * Step 3, the ELIGIBILITY query (docs/wind-down.md:79-99) -- every wallet above
#     the frozen threshold, with its payout rail and, for a stablecoin rail, the
#     payout units
#   * Step 4, the SUB-THRESHOLD list (docs/wind-down.md:124-131) -- balances above
#     zero but at or below the threshold
#
# The sub-threshold rows are NOT forfeited. They are paid on request with the company
# covering the transfer fee: the threshold decides what is AUTOMATIC, not what is
# OWED (docs/decisions.md:68). The report prints them separately for that reason.
#
# THIS TOOL IS NOT A GATE. There being money to pay out is not a failure, so it exits
# 0 whenever it manages to print. It never writes a ledger row, never moves money and
# never changes a balance -- see the -readonly note below. THE PAYOUT STEPS ARE IN
# docs/wind-down.md; this tool stops before Step 5.
#
# ---------------------------------------------------------------------------
# THE FROZEN RATE IS A REQUIRED INPUT, NEVER LOOKED UP
# ---------------------------------------------------------------------------
# docs/decisions.md:64 -- the rate is captured ONCE at wind-down start from Bank
# Indonesia JISDOR and frozen for every payout, because balances are frozen at the
# same instant and "a live FX API is precisely what is unavailable when you are
# shutting down" (docs/wind-down.md:41). The runbook says "substitute the captured
# value, never a live lookup". So the rate arrives in $CLOSURE_USD_IDR_RATE and is
# refused loudly when absent or not a positive integer. There is NO default: a
# defaulted rate would silently value every balance at a number nobody chose.
#
# The threshold rule is STRICTLY GREATER than 2 * rate (docs/decisions.md:66,
# docs/wind-down.md:27). A balance of exactly 2 * rate is therefore SUB-threshold,
# and that boundary is asserted by tools/wind-down-check/check.sh rather than assumed.
#
# ---------------------------------------------------------------------------
# HOW THE RUNBOOK'S SQL IS REPRODUCED
# ---------------------------------------------------------------------------
# Both queries below are the runbook's, verbatim, inside a CTE; the rate is BOUND as
# a named parameter (`.parameter set :closure_usd_idr_rate`) rather than textually
# substituted, so the SQL in this file is character-for-character the SQL in
# docs/wind-down.md. Two deliberate additions, both noted at their site:
#   1. an ORDER BY (the runbook has none) so two runs of the same data diff cleanly;
#   2. a payout_units column on the Step 3 list, computed exactly as Step 5 does it:
#        units = floor(balance_idr * 1_000_000 / closure_usd_idr_rate)
#      USDC has 6 decimals and integer division truncates toward zero, which for
#      positive operands IS floor -- so the payout can never create money. It is
#      printed only for a stablecoin rail; a bank transfer pays balance_idr whole.
#
# ---------------------------------------------------------------------------
# Exit codes:
#   0  report produced. INCLUDING when eligible balances exist -- there being money to
#      pay out is not a failure, and this is NOT a gate
#   2  DATABASE_URL is not set (unset, empty, or whitespace only), is not a SQLite
#      URL, or names an in-memory database (which cannot be reported on from outside
#      the process)
#   3  sqlite3 is not installed / not on PATH
#   4  sqlite3 ran but failed (unreadable file, SQL error)
#   6  the database file DATABASE_URL names does not exist
#   7  CLOSURE_USD_IDR_RATE is not set, or is not a positive integer
#   8  the report is INTERNALLY INCONSISTENT: the SQL aggregates and the rows this run
#      printed disagree, so one of the two figures would be wrong. It refuses to print
#      a number an operator would act on rather than guessing which one
#
# 1 and 5 are deliberately UNASSIGNED. 1 is "a check failed" in this repo's house
# convention (tools/reconcile/reconcile.sh:20) and this is not a gate, so nothing here
# may ever be read as one. 5 is reconcile.sh's stranded-hold code, left free so the two
# tools cannot be confused if an operator ever wraps both in one script.
#
# Usage:
#   export DATABASE_URL='sqlite://data/server.db'
#   export CLOSURE_USD_IDR_RATE='<the frozen JISDOR rate>'
#   sh tools/wind-down/report.sh

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)

# --- sqlite3 availability ----------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    echo "wind-down: sqlite3 is not installed or not on PATH" >&2
    echo "wind-down: install the SQLite command-line shell (sqlite3) and retry" >&2
    exit 3
fi

# --- DATABASE_URL ------------------------------------------------------------
# A whitespace-only value is not a DSN: a blind prefix match would turn it into a
# relative path that happens to be a plausible filename. Treat it as unset.
if [ -z "$DATABASE_URL" ]; then
    echo "wind-down: DATABASE_URL is not set" >&2
    echo "wind-down: export DATABASE_URL='sqlite://data/server.db'" >&2
    exit 2
fi
case "$DATABASE_URL" in
    *[![:space:]]*) ;;
    *)
        echo "wind-down: DATABASE_URL is set but contains only whitespace" >&2
        echo "wind-down: export DATABASE_URL='sqlite://data/server.db'" >&2
        exit 2
        ;;
esac

# A `case`, not a blind prefix strip: a leftover Postgres URL must be refused loudly
# rather than quietly rewritten into a relative path that happens to be a plausible
# filename (the reason recorded at tools/reconcile/reconcile.sh:73-84).
case "$DATABASE_URL" in
    sqlite://*) DB_PATH=${DATABASE_URL#sqlite://} ;;
    sqlite:*)   DB_PATH=${DATABASE_URL#sqlite:} ;;
    *)
        echo "wind-down: DATABASE_URL is not a SQLite URL: $DATABASE_URL" >&2
        echo "wind-down: expected e.g. sqlite://data/server.db" >&2
        exit 2
        ;;
esac

# Drop any sqlx query string (`?mode=rwc`); it is not part of the filename.
DB_PATH=${DB_PATH%%\?*}

if [ -z "$DB_PATH" ] || [ "$DB_PATH" = ":memory:" ]; then
    echo "wind-down: DATABASE_URL does not name a file: $DATABASE_URL" >&2
    echo "wind-down: an in-memory database cannot be reported on from outside the process" >&2
    exit 2
fi

if [ ! -f "$DB_PATH" ]; then
    echo "wind-down: no such database file: $DB_PATH" >&2
    echo "wind-down: create it with 'cargo run --bin migrate'" >&2
    exit 6
fi

# --- The frozen rate ---------------------------------------------------------
if [ -z "$CLOSURE_USD_IDR_RATE" ]; then
    echo "wind-down: CLOSURE_USD_IDR_RATE is not set" >&2
    echo "wind-down: it is the FROZEN JISDOR rate captured at wind-down start and there is NO default" >&2
    echo "wind-down: export CLOSURE_USD_IDR_RATE='<IDR per USD, a positive integer>'" >&2
    exit 7
fi
case "$CLOSURE_USD_IDR_RATE" in
    *[!0-9]*|'')
        echo "wind-down: CLOSURE_USD_IDR_RATE='$CLOSURE_USD_IDR_RATE' is not a positive integer" >&2
        echo "wind-down: expected e.g. CLOSURE_USD_IDR_RATE=16240 (no decimals, no separators)" >&2
        exit 7
        ;;
esac
# Normalise away leading zeros (so 016240 is not read as bad octal), then refuse zero:
# every digit test above accepts "0", and a zero rate would make the threshold zero and
# every balance eligible.
RATE=$(printf '%s' "$CLOSURE_USD_IDR_RATE" | sed 's/^0*//')
if [ -z "$RATE" ] || [ "$RATE" = "0" ]; then
    echo "wind-down: CLOSURE_USD_IDR_RATE='$CLOSURE_USD_IDR_RATE' is zero; a rate must be positive" >&2
    exit 7
fi

THRESHOLD=$((RATE * 2))

# The runbook's threshold expression, written ONCE and interpolated into every query
# below, so there is a single textual definition of "eligible" in this file.
T="(2 * :closure_usd_idr_rate)"

TMP="${TMPDIR:-/tmp}"
PRE_SQL="$TMP/wind-down-pre.$$.sql"
ELIG_SQL="$TMP/wind-down-elig.$$.sql"
SUB_SQL="$TMP/wind-down-sub.$$.sql"
ERR="$TMP/wind-down.$$.err"
trap 'rm -f "$PRE_SQL" "$ELIG_SQL" "$SUB_SQL" "$ERR"' EXIT HUP INT TERM

# --- The queries -------------------------------------------------------------
# PRE-FLIGHT. Six aggregates in one row, pipe-separated. Every predicate here uses $T,
# the same definition the two lists below use, so the summary cannot describe a
# different question from the rows.
cat > "$PRE_SQL" <<PRE
.parameter set :closure_usd_idr_rate $RATE
SELECT
  (SELECT COUNT(*) FROM wallets),
  (SELECT COALESCE(SUM(balance_idr), 0) FROM wallets),
  (SELECT COUNT(*) FROM wallets w WHERE w.balance_idr > $T),
  (SELECT COALESCE(SUM(balance_idr), 0) FROM wallets w WHERE w.balance_idr > $T),
  (SELECT COUNT(*) FROM wallets w WHERE w.balance_idr > 0 AND w.balance_idr <= $T),
  (SELECT COALESCE(SUM(balance_idr), 0) FROM wallets w WHERE w.balance_idr > 0 AND w.balance_idr <= $T);
PRE

# STEP 3, docs/wind-down.md:85-99. The CTE body is the runbook's query verbatim --
# including the rail predicate, which is copied exactly as decisions.md:65 states it.
# The outer SELECT adds only the ORDER BY and the payout_units column described above.
cat > "$ELIG_SQL" <<ELIG
.parameter set :closure_usd_idr_rate $RATE
WITH eligible AS (
  -- Frozen rate: substituted as a bound parameter, never a live lookup.
  -- 2 * rate, because the rule is "more than USD 2.00" (strictly greater).
  SELECT w.account_id,
         w.balance_idr,
         CASE WHEN EXISTS (
           SELECT 1 FROM topups t
            WHERE t.account_id = w.account_id
              AND t.status = 'settled'
              AND t.rail = 'midtrans'
         ) THEN 'bank_transfer' ELSE 'stablecoin' END AS payout_rail
    FROM wallets w
   WHERE w.balance_idr > $T
)
SELECT account_id,
       balance_idr,
       payout_rail,
       CASE WHEN payout_rail = 'bank_transfer' THEN '-'
            ELSE CAST(balance_idr * 1000000 / :closure_usd_idr_rate AS TEXT) END AS payout_units
  FROM eligible
 ORDER BY balance_idr DESC;
ELIG

# STEP 4, docs/wind-down.md:126-131, verbatim, plus an ORDER BY for a diffable report.
cat > "$SUB_SQL" <<SUB
.parameter set :closure_usd_idr_rate $RATE
SELECT w.account_id, w.balance_idr
  FROM wallets w
 WHERE w.balance_idr > 0
   AND w.balance_idr <= $T
 ORDER BY w.balance_idr DESC;
SUB

# --- Run ---------------------------------------------------------------------
# -readonly: this tool READS. A stray write anywhere in the SQL below -- or a future
#   edit that adds one -- fails rather than silently moving money. On a WAL database
#   this needs the -shm file to be creatable; where the volume forbids it, run the
#   report against a backup copy instead (docs/backup-and-restore.md).
# -bail: stop at the first error rather than continuing past a failed statement.
# The SQL is fed on stdin because the sqlite3 CLI has no `-f` option -- that is psql's
# spelling. sqlite3's default list output is already pipe-separated with no header.
run_sql() {
    # $1 = sql file, $2 = label
    _out=$(sqlite3 -readonly -bail "$DB_PATH" <"$1" 2>"$ERR")
    _rc=$?
    if [ "$_rc" -ne 0 ]; then
        echo "wind-down: sqlite3 failed (exit $_rc) running the $2 query:" >&2
        [ -s "$ERR" ] && cat "$ERR" >&2
        [ -n "$_out" ] && printf '%s\n' "$_out" >&2
        exit 4
    fi
    if [ -s "$ERR" ]; then
        echo "wind-down: sqlite3 diagnostics on stderr ($2; diagnostic, NOT a result):" >&2
        cat "$ERR" >&2
    fi
    # A Windows sqlite3 emits CRLF, and command substitution keeps the CR of every
    # line but the last (the real bug recorded at tools/reconcile/reconcile.sh:185-191,
    # where a CR left in a parsed field made a gate fail OPEN). Strip before parsing
    # ANY field below. A no-op on LF-only output.
    printf '%s\n' "$_out" | tr -d '\r'
}

# Strip leading zeros so a value like 08 is not read as bad octal; empty means 0.
norm_int() {
    _v=$(printf '%s' "$1" | sed 's/^0*//')
    [ -n "$_v" ] || _v=0
    printf '%s' "$_v"
}

PRE_RAW=$(run_sql "$PRE_SQL" "pre-flight")
ELIG_RAW=$(run_sql "$ELIG_SQL" "eligibility")
SUB_RAW=$(run_sql "$SUB_SQL" "sub-threshold")

inconsistent() {
    echo "wind-down: INTERNALLY INCONSISTENT - $1" >&2
    echo "wind-down: refusing to print a figure an operator would act on. One of the two" >&2
    echo "wind-down: numbers is wrong and this run cannot tell you which." >&2
    exit 8
}

# --- Parse the pre-flight row ------------------------------------------------
PRE_LINES=$(printf '%s\n' "$PRE_RAW" | grep -c . || true)
[ "$PRE_LINES" -eq 1 ] || inconsistent "the pre-flight query returned $PRE_LINES rows, expected 1"
PRE_FIELDS=$(printf '%s\n' "$PRE_RAW" | tr '|' '\n' | grep -c . || true)
[ "$PRE_FIELDS" -eq 6 ] || inconsistent "the pre-flight row has $PRE_FIELDS fields, expected 6"

IFS='|' read -r W_COUNT W_TOTAL E_COUNT E_TOTAL S_COUNT S_TOTAL <<PRELINE
$PRE_RAW
PRELINE
W_COUNT=$(norm_int "$W_COUNT"); W_TOTAL=$(norm_int "$W_TOTAL")
E_COUNT=$(norm_int "$E_COUNT"); E_TOTAL=$(norm_int "$E_TOTAL")
S_COUNT=$(norm_int "$S_COUNT"); S_TOTAL=$(norm_int "$S_TOTAL")

# --- Cross-check the aggregates against the rows actually printed ------------
# The point of printing a count and then the rows is that the operator can act on the
# rows. If the two disagree, the report is lying in one of the two places.
E_ROWS=$(printf '%s\n' "$ELIG_RAW" | grep -c . || true)
S_ROWS=$(printf '%s\n' "$SUB_RAW" | grep -c . || true)
[ "$E_ROWS" -eq "$E_COUNT" ] || inconsistent "the pre-flight says $E_COUNT eligible accounts but the Step 3 list printed $E_ROWS rows"
[ "$S_ROWS" -eq "$S_COUNT" ] || inconsistent "the pre-flight says $S_COUNT sub-threshold accounts but the Step 4 list printed $S_ROWS rows"

# --- Per-rail breakdown, accumulated from the Step 3 rows --------------------
# Derived from the rows rather than from a second query, so the breakdown and the list
# cannot describe different account sets. A `while read` over a heredoc runs in THIS
# shell (unlike a pipeline), so the counters survive the loop.
BANK_COUNT=0; BANK_TOTAL=0
STABLE_COUNT=0; STABLE_TOTAL=0; STABLE_UNITS=0
E_SUM=0
if [ "$E_ROWS" -gt 0 ]; then
    while IFS='|' read -r aid bal rail units; do
        [ -n "$aid" ] || continue
        bal=$(norm_int "$bal")
        E_SUM=$((E_SUM + bal))
        case "$rail" in
            bank_transfer)
                BANK_COUNT=$((BANK_COUNT + 1))
                BANK_TOTAL=$((BANK_TOTAL + bal))
                ;;
            stablecoin)
                STABLE_COUNT=$((STABLE_COUNT + 1))
                STABLE_TOTAL=$((STABLE_TOTAL + bal))
                STABLE_UNITS=$((STABLE_UNITS + $(norm_int "$units")))
                ;;
            *)
                inconsistent "a Step 3 row carried an unknown payout_rail '$rail'"
                ;;
        esac
    done <<ELIGLINES
$ELIG_RAW
ELIGLINES
fi

S_SUM=0
if [ "$S_ROWS" -gt 0 ]; then
    while IFS='|' read -r aid bal; do
        [ -n "$aid" ] || continue
        S_SUM=$((S_SUM + $(norm_int "$bal")))
    done <<SUBLINES
$SUB_RAW
SUBLINES
fi

[ "$E_SUM" -eq "$E_TOTAL" ] || inconsistent "the Step 3 rows sum to $E_SUM IDR but the pre-flight total says $E_TOTAL IDR"
[ "$S_SUM" -eq "$S_TOTAL" ] || inconsistent "the Step 4 rows sum to $S_SUM IDR but the pre-flight total says $S_TOTAL IDR"
[ "$E_COUNT" -eq "$((BANK_COUNT + STABLE_COUNT))" ] || inconsistent "the rail breakdown covers $((BANK_COUNT + STABLE_COUNT)) accounts but there are $E_COUNT eligible ones"
# The two predicates PARTITION the wallets: everything is either above the threshold,
# or above zero and at/below it, or exactly zero (which adds nothing). If the total
# does not split that way, a wallet is in neither list and its money is unaccounted for.
[ "$W_TOTAL" -eq "$((E_SUM + S_SUM))" ] || inconsistent "the wallet total is $W_TOTAL IDR but the two lists cover only $((E_SUM + S_SUM)) IDR - some balance is in neither list"

ZERO_COUNT=$((W_COUNT - E_COUNT - S_COUNT))

# --- Report ------------------------------------------------------------------
echo "wind-down: CLOSURE PAYOUT REPORT (READ-ONLY)"
echo "wind-down: =========================================================================="
echo "wind-down: THIS TOOL PAYS NOBODY. It reads and prints. It writes no ledger row, moves"
echo "wind-down: no money and changes no balance. The payout steps are in docs/wind-down.md"
echo "wind-down: (Steps 5-8); this report stops before Step 5 and takes no action on what"
echo "wind-down: it finds."
echo "wind-down:"
echo "wind-down: FROZEN RATE"
echo "wind-down:   closure_usd_idr_rate = $RATE IDR per USD"
echo "wind-down:   source: SUPPLIED by the operator via \$CLOSURE_USD_IDR_RATE. It was NOT"
echo "wind-down:     looked up. docs/decisions.md:64 freezes the rate once at wind-down start"
echo "wind-down:     (Bank Indonesia JISDOR on the wind-down date); a live FX lookup is"
echo "wind-down:     exactly what is unavailable when you are shutting down."
echo "wind-down:   automatic threshold = balance_idr > 2 * $RATE = $THRESHOLD IDR (STRICTLY greater,"
echo "wind-down:     so a balance of exactly $THRESHOLD is sub-threshold and is NOT paid automatically)"
echo "wind-down:"
echo "wind-down: PRE-FLIGHT"
echo "wind-down:   wallets rows                        : $W_COUNT"
echo "wind-down:   total balance_idr                   : $W_TOTAL IDR"
echo "wind-down:   ELIGIBLE (automatic payout)         : $E_COUNT account(s), $E_TOTAL IDR"
echo "wind-down:   SUB-THRESHOLD (on request, not lost): $S_COUNT account(s), $S_TOTAL IDR"
echo "wind-down:   zero balance (in neither list)      : $ZERO_COUNT account(s)"
echo "wind-down:"
echo "wind-down: PAYOUT RAIL BREAKDOWN (eligible rows only)"
echo "wind-down:   bank_transfer : $BANK_COUNT account(s), $BANK_TOTAL IDR - pay balance_idr whole, no conversion"
echo "wind-down:   stablecoin    : $STABLE_COUNT account(s), $STABLE_TOTAL IDR - $STABLE_UNITS USDC units in total"
echo "wind-down:   Every customer is Indonesian today, so this normally reads bank_transfer for"
echo "wind-down:   everyone. That is the CORRECT answer, not a stub: no crypto rail is"
echo "wind-down:   implemented, so no account has ever settled a non-midtrans top-up"
echo "wind-down:   (docs/wind-down.md:101-122)."
echo "wind-down:"
echo "wind-down: STEP 3 - ELIGIBLE FOR AUTOMATIC PAYOUT (balance_idr > $THRESHOLD)"
echo "wind-down:   columns: account_id|balance_idr|payout_rail|payout_units"
echo "wind-down:   payout_units is '-' for bank_transfer (IDR is already whole) and is"
echo "wind-down:   floor(balance_idr * 1000000 / $RATE) for stablecoin, rounded DOWN so a"
echo "wind-down:   payout can never create money."
if [ "$E_ROWS" -gt 0 ]; then
    printf '%s\n' "$ELIG_RAW"
else
    echo "wind-down:   (none)"
fi
echo "wind-down:"
echo "wind-down: STEP 4 - SUB-THRESHOLD (0 < balance_idr <= $THRESHOLD)"
echo "wind-down:   columns: account_id|balance_idr"
echo "wind-down:   These are NOT forfeited and are NOT held back for a fee. Pay them on"
echo "wind-down:   request and cover the transfer fee: the threshold decides what is"
echo "wind-down:   AUTOMATIC, not what is OWED (docs/decisions.md:68). Anything genuinely"
echo "wind-down:   unclaimed stays a retained liability -- never recognised as revenue."
if [ "$S_ROWS" -gt 0 ]; then
    printf '%s\n' "$SUB_RAW"
else
    echo "wind-down:   (none)"
fi
echo "wind-down:"
echo "wind-down: SENSITIVITY - THIS OUTPUT IS OPERATIONAL DATA, NOT SOURCE"
echo "wind-down:   It contains account ids and balances. Whether the database is encrypted"
echo "wind-down:   at rest or plaintext makes NO difference to that: this is decrypted, live"
echo "wind-down:   data the moment it is read. tools/drill/README.md frames the drill log the"
echo "wind-down:   same way ('drill logs are operational records, not source'). Treat this"
echo "wind-down:   output the way you treat the database itself: keep it out of git, out of"
echo "wind-down:   tickets and out of chat, and delete it when the payout is done."
echo "wind-down:"
echo "wind-down: NEXT: docs/wind-down.md. Step 5 pays (bank transfer whole, stablecoin at the"
echo "wind-down:   units above); Step 6 writes one ledger row per paid account AND zeroes the"
echo "wind-down:   wallet -- both are mandatory or wallet = SUM(delta_idr) breaks; Step 7 runs"
echo "wind-down:   tools/reconcile/reconcile.sh and must return zero rows."
echo "wind-down: OK - report complete: $E_COUNT eligible, $S_COUNT sub-threshold, 0 accounts changed"

# The runbook is the authority for what happens next; say so if it is not where expected.
RUNBOOK="$SCRIPT_DIR/../../docs/wind-down.md"
if [ ! -f "$RUNBOOK" ]; then
    echo "wind-down: WARNING - cannot find the runbook at $RUNBOOK" >&2
fi

exit 0
