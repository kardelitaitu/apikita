#!/bin/sh
# apikita wind-down report check.
#
# WHY THIS EXISTS. `tools/wind-down/report.sh` implements the payout ELIGIBILITY
# boundary, and docs/decisions.md:66 settles that boundary as "STRICTLY greater than
# 2 x rate". An off-by-one there does not crash: it quietly pays one account the
# automatic rail that should have been paid on request, or withholds one that should
# have been automatic. Nobody notices until a customer does. So the boundary is
# asserted from BOTH sides, against the real migrated schema, rather than reasoned
# about in a comment.
#
# WHAT IT PROVES, in the order that matters:
#
#   1. THE CLASSIFICATION IS RIGHT. The rail predicate is "EVER settled a midtrans
#      top-up => bank_transfer, else stablecoin" (docs/decisions.md:65). Two failure
#      modes are asserted separately, because a predicate that ignores `status` and one
#      that ignores `rail` look identical from the outside:
#        - a midtrans top-up that is only PENDING must NOT make the account Indonesian
#          (the rule is monotone over SETTLED top-ups), so that account is stablecoin;
#        - a crypto-settled account must NOT be dragged to bank_transfer.
#   2. THE BOUNDARY IS RIGHT. balance == 2*rate is SUB-threshold; balance == 2*rate+1
#      is ELIGIBLE. Only asserting one side would accept an off-by-one.
#   3. THE ARITHMETIC IS RIGHT. Stablecoin units are floor(balance*1e6/rate), and the
#      fixture includes a balance whose division has a .5 remainder so TRUNCATION is
#      observable rather than assumed.
#   4. THE TOOL REFUSES. The frozen rate is a required input with no default
#      (docs/decisions.md:64 -- a live lookup is unavailable when you are shutting
#      down), so absent/garbage/zero rates and a bad DATABASE_URL must each exit
#      non-zero and SAY WHY. This repo's culture is that a detector which only ever
#      passes is not trusted.
#   5. IT IS READ-ONLY. The database file must be BYTE-IDENTICAL after a run. The
#      payout SQL is one edit away from moving money, and `-readonly` is the only
#      thing between the two.
#
# It builds a REAL migrated database from server/migrations, the way
# tools/drill-check/check.sh does -- a hand-written minimal schema would let the
# report pass against a shape the server never produces. A migration that does not
# apply is a HARD FAILURE, never a skip (the false green recorded at
# tools/drill-check/check.sh:92-104).
#
# Skips LOUDLY (exit 3) when sqlite3 is absent, never 0.
#
# Usage: sh tools/wind-down-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
REPORT="$REPO/tools/wind-down/report.sh"
WORK="${TMPDIR:-/tmp}/apikita-wind-down-check-$$"
cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
mkdir -p "$WORK" || { echo "wind-down-check: cannot create $WORK" >&2; exit 2; }

command -v sqlite3 >/dev/null 2>&1 || {
    echo "wind-down-check: SKIPPED - sqlite3 is not on PATH, so the report was NOT verified" >&2
    exit 3
}

[ -f "$REPORT" ] || { echo "wind-down-check: missing $REPORT" >&2; exit 3; }

FAILED=0
fail() { echo "wind-down-check: FAIL - $1" >&2; FAILED=1; }

# --- 1. Build the migrated database -----------------------------------------
DB="$WORK/wind-down.db"
DSN="sqlite://$DB"
MIGRATION_DIR="$REPO/server/migrations"

[ -d "$MIGRATION_DIR" ] || { echo "wind-down-check: SKIPPED - no $MIGRATION_DIR to build from" >&2; exit 3; }

migrate_failed=""
for f in "$MIGRATION_DIR"/*.sql; do
    if ! sqlite3 -bail "$DB" < "$f" >/dev/null 2>"$WORK/mig.err"; then
        migrate_failed=$(basename "$f")
        break
    fi
done
if [ -n "$migrate_failed" ]; then
    fail "the migration $migrate_failed did not apply, so every assertion below would run against a PARTIAL schema: $(head -1 "$WORK/mig.err" 2>/dev/null)"
    echo "wind-down-check: the report contract is BROKEN (see above)" >&2
    exit 1
fi

# Guard the fixture: the tables the report reads must EXIST, or a later seed failure
# would look like a report bug.
for t in accounts wallets topups; do
    n=$(sqlite3 "$DB" "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='$t';")
    [ "$n" = "1" ] || fail "the migrated database has no '$t' table, so the fixture cannot be seeded"
done
if [ "$FAILED" -ne 0 ]; then
    echo "wind-down-check: the report contract is BROKEN (see above)" >&2
    exit 1
fi

# --- 2. Seed the fixture -----------------------------------------------------
# rate = 16000, so 2*rate = 32000 and the boundary is exact.
#
#   account                    balance   top-ups                          expect
#   -------------------------  --------  -------------------------------  ------------------
#   acct-midtrans-above         100000   settled midtrans                 ELIGIBLE bank
#   acct-crypto-above            50000   settled crypto                   ELIGIBLE stable
#   acct-both-above              60000   settled crypto + settled midtrans ELIGIBLE bank
#   acct-pending-above           40000   PENDING midtrans only            ELIGIBLE stable
#   acct-notopup-above           33000   (none)                           ELIGIBLE stable
#   acct-threshold-plus-one      32001   (none)                           ELIGIBLE stable  <- boundary
#   acct-exact-threshold         32000   settled midtrans                 SUB              <- boundary
#   acct-below                   10000   settled midtrans                 SUB
#   acct-one                         1   (none)                           SUB
#   acct-zero                        0   (none)                           neither
sqlite3 -bail "$DB" <<'SEED' || { echo "wind-down-check: could not seed the fixture" >&2; exit 3; }
INSERT INTO accounts (id, status, is_operator, created_at, updated_at) VALUES
  ('acct-midtrans-above',     'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-crypto-above',       'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-both-above',         'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-pending-above',      'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-notopup-above',      'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-threshold-plus-one', 'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-exact-threshold',    'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-below',              'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-one',                'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('acct-zero',               'active', 0, '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');

INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES
  ('acct-midtrans-above',     100000, '2026-01-01T00:00:00+00:00'),
  ('acct-crypto-above',        50000, '2026-01-01T00:00:00+00:00'),
  ('acct-both-above',          60000, '2026-01-01T00:00:00+00:00'),
  ('acct-pending-above',       40000, '2026-01-01T00:00:00+00:00'),
  ('acct-notopup-above',       33000, '2026-01-01T00:00:00+00:00'),
  ('acct-threshold-plus-one',  32001, '2026-01-01T00:00:00+00:00'),
  ('acct-exact-threshold',     32000, '2026-01-01T00:00:00+00:00'),
  ('acct-below',               10000, '2026-01-01T00:00:00+00:00'),
  ('acct-one',                     1, '2026-01-01T00:00:00+00:00'),
  ('acct-zero',                    0, '2026-01-01T00:00:00+00:00');

INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at, settled_at) VALUES
  ('t-mid-1',  'acct-midtrans-above',    100000, 'ord-mid-1',  'settled', 'midtrans', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('t-cry-1',  'acct-crypto-above',       50000, 'ord-cry-1',  'settled', 'crypto',   '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('t-both-1', 'acct-both-above',         10000, 'ord-both-1', 'settled', 'crypto',   '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('t-both-2', 'acct-both-above',         50000, 'ord-both-2', 'settled', 'midtrans', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('t-pend-1', 'acct-pending-above',      40000, 'ord-pend-1', 'pending', 'midtrans', '2026-01-01T00:00:00+00:00', NULL),
  ('t-exact-1','acct-exact-threshold',    32000, 'ord-exact-1','settled', 'midtrans', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00'),
  ('t-below-1','acct-below',              10000, 'ord-below-1','settled', 'midtrans', '2026-01-01T00:00:00+00:00', '2026-01-01T00:00:00+00:00');
SEED

# Guard the seed itself: an INSERT that silently did nothing would make every
# assertion below vacuous.
n_wallets=$(sqlite3 "$DB" "SELECT COUNT(*) FROM wallets;")
[ "$n_wallets" = "10" ] || fail "the fixture has $n_wallets wallets, expected 10 - the assertions would be vacuous"
n_topups=$(sqlite3 "$DB" "SELECT COUNT(*) FROM topups;")
[ "$n_topups" = "7" ] || fail "the fixture has $n_topups top-ups, expected 7"
n_pending=$(sqlite3 "$DB" "SELECT COUNT(*) FROM topups WHERE status='pending' AND rail='midtrans';")
[ "$n_pending" = "1" ] || fail "the fixture has no PENDING midtrans top-up, so the status half of the rail predicate would be untested"
if [ "$FAILED" -ne 0 ]; then
    echo "wind-down-check: the report contract is BROKEN (see above)" >&2
    exit 1
fi

RATE=16000

# Writes stdout+stderr to $WORK/report.out and prints the exit code on stdout, so the
# caller can read the script's OWN status. `out=$(... | tr -d '\r')` would report tr's
# exit (always 0) and make a refusal look like a pass.
run_report() {
    env DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="$RATE" sh "$REPORT" >"$WORK/report.out" 2>&1
    printf '%s' "$?"
}

# --- 3. READ-ONLY: the file must be byte-identical afterwards ----------------
cp "$DB" "$WORK/before.db"
rc=$(run_report)
out=$(tr -d '\r' < "$WORK/report.out")
if [ "$rc" -ne 0 ]; then
    fail "a reportable database must exit 0 (there being money to pay out is not a failure), got $rc"
fi
cmp -s "$WORK/before.db" "$DB" || fail "the database file CHANGED during a run - the report is not read-only"
# And the -readonly flag must be doing the work, not luck: a write through this exact
# invocation must FAIL. If it succeeded, the flag is not in effect and the check above
# proves nothing about a future edit.
if sqlite3 -readonly -bail "$DB" "UPDATE wallets SET balance_idr = 0;" >/dev/null 2>&1; then
    fail "a write through 'sqlite3 -readonly' SUCCEEDED - the read-only guarantee is not in effect"
fi
cmp -s "$WORK/before.db" "$DB" || fail "the database file changed after the write-refusal probe"

# THE ASSERTION THAT ACTUALLY BINDS THE REPORT. The two probes above test the sqlite3
# CLI's own flag, NOT the report's invocation of it -- so a mutation that drops
# `-readonly` from report.sh SURVIVED the first version of this check (measured: the
# mutation was applied and the check still exited 0). The guarantee that matters is
# that a WRITE INSIDE THE REPORT'S OWN SQL fails rather than moving money, because the
# payout SQL is one careless edit away from being exactly that write.
#
# So: copy the report, inject a write into its SQL, and require the run to FAIL and the
# database to be UNCHANGED. With `-readonly` in place the write errors out (exit 4);
# without it the write succeeds, the balance moves, and this fails loudly.
INJ="$WORK/report-injected.sh"
sed 's/ORDER BY balance_idr DESC;/ORDER BY balance_idr DESC; UPDATE wallets SET balance_idr = 0;/' \
    "$REPORT" > "$INJ"
# Guard the fixture: if the anchor did not match, the injected file is identical to the
# original and the assertion below would test nothing.
cmp -s "$REPORT" "$INJ" && fail "could not inject a write into the report's SQL, so the read-only guarantee would be untested"

env DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="$RATE" sh "$INJ" >"$WORK/inj.out" 2>&1
inj_rc=$?
if [ "$inj_rc" -eq 0 ]; then
    fail "a WRITE injected into the report's own SQL SUCCEEDED (exit 0) - the report is not read-only in practice"
fi
cmp -s "$WORK/before.db" "$DB" || fail "the injected write CHANGED the database - the report can move money"
# And the refusal must be sqlite3's read-only error, not some unrelated failure that
# would also have produced a non-zero code.
if ! grep -qi 'readonly\|read-only' "$WORK/inj.out"; then
    fail "the injected write failed, but not with a read-only error - the guarantee may be accidental (got: $(head -2 "$WORK/inj.out" | tr '\n' ' '))"
fi

# A second run must produce identical output: a report that drifts between runs on
# unchanged data cannot be diffed against the payout it authorises.
rc2=$(run_report)
out2=$(tr -d '\r' < "$WORK/report.out")
[ "$out" = "$out2" ] || fail "two runs over unchanged data produced different output"

# --- 4. The eligible list, the rail classification, and the boundary ---------
elig_rows=$(printf '%s\n' "$out" | sed -n '/^wind-down: STEP 3/,/^wind-down: STEP 4/p' | grep '^acct-' || true)
sub_rows=$(printf '%s\n' "$out" | sed -n '/^wind-down: STEP 4/,/^wind-down: SENSITIVITY/p' | grep '^acct-' || true)

# The STEP 3 / STEP 4 banners must actually delimit the two lists. If sed found
# nothing, every row assertion below would compare against an empty string and fail
# for the wrong reason - or worse, a `grep -q` on a specific row would pass vacuously.
[ -n "$elig_rows" ] || fail "could not locate the STEP 3 eligible list in the output"
[ -n "$sub_rows" ] || fail "could not locate the STEP 4 sub-threshold list in the output"

in_elig() { printf '%s\n' "$elig_rows" | grep -qF "$1"; }
in_sub()  { printf '%s\n' "$sub_rows"  | grep -qF "$1"; }

# --- classification ----------------------------------------------------------
in_elig "acct-midtrans-above|100000|bank_transfer|-" \
    || fail "a settled-midtrans account above the threshold must be bank_transfer"
in_elig "acct-both-above|60000|bank_transfer|-" \
    || fail "an account with BOTH a settled crypto and a settled midtrans top-up must be bank_transfer (the rule is 'EVER settled midtrans')"
in_elig "acct-crypto-above|50000|stablecoin|3125000" \
    || fail "a crypto-settled account must be stablecoin with floor(50000*1000000/16000)=3125000 units"
# THE status HALF. A predicate that dropped `t.status = 'settled'` would classify this
# account as bank_transfer and every other assertion here would still pass.
in_elig "acct-pending-above|40000|stablecoin|2500000" \
    || fail "a PENDING midtrans top-up must NOT make the account Indonesian - the rule is monotone over SETTLED top-ups, so this is stablecoin"
in_elig "acct-notopup-above|33000|stablecoin|2062500" \
    || fail "an account with no top-up at all must be stablecoin"

# --- the boundary, from BOTH sides -------------------------------------------
# 32001 is ELIGIBLE and 32000 is SUB. Asserting only one of these would accept an
# off-by-one in either direction, which is the whole reason this check exists.
in_elig "acct-threshold-plus-one|32001|stablecoin|2000062" \
    || fail "balance 2*rate+1 (32001) must be ELIGIBLE - the rule is strictly greater, so this is the lowest eligible balance"
in_sub "acct-exact-threshold|32000" \
    || fail "balance exactly 2*rate (32000) must be SUB-threshold, NOT eligible - the rule is 'strictly greater'"
in_elig "acct-exact-threshold" \
    && fail "balance exactly 2*rate (32000) appeared in the ELIGIBLE list - that is an off-by-one that pays automatically what the threshold says is on request"

# floor(), not round(). 32001*1000000/16000 = 2000062.5; the units above assert the
# truncation. State the reason here so a future 'fix' to round-half-up fails loudly.
in_elig "2000063" \
    && fail "the units for balance 32001 were ROUNDED (2000063) instead of FLOORED (2000062) - rounding up can create money"

# --- the other sub-threshold rows and the excluded one -----------------------
in_sub "acct-below|10000" || fail "a below-threshold non-zero balance must be listed as sub-threshold"
in_sub "acct-one|1"       || fail "a balance of 1 IDR is > 0 and must be listed as sub-threshold"
in_elig "acct-zero" && fail "a zero balance must not be eligible"
in_sub  "acct-zero" && fail "a zero balance must not be sub-threshold (the rule is balance_idr > 0)"

# --- 5. The counts and totals, and the claim that it is NOT a gate -----------
printf '%s\n' "$out" | grep -qF "ELIGIBLE (automatic payout)         : 6 account(s), 315001 IDR" \
    || fail "the pre-flight must report 6 eligible accounts totalling 315001 IDR"
printf '%s\n' "$out" | grep -qF "SUB-THRESHOLD (on request, not lost): 3 account(s), 42001 IDR" \
    || fail "the pre-flight must report 3 sub-threshold accounts totalling 42001 IDR"
printf '%s\n' "$out" | grep -qF "bank_transfer : 2 account(s), 160000 IDR" \
    || fail "the per-rail breakdown must report 2 bank_transfer accounts totalling 160000 IDR"
printf '%s\n' "$out" | grep -qF "stablecoin    : 4 account(s), 155001 IDR - 9687562 USDC units in total" \
    || fail "the per-rail breakdown must report 4 stablecoin accounts and 9687562 units in total"

# The rate must be reported AS SUPPLIED, with its provenance, not merely used.
printf '%s\n' "$out" | grep -qF "closure_usd_idr_rate = 16000 IDR per USD" \
    || fail "the report must state the frozen rate it used"
printf '%s\n' "$out" | grep -qF "SUPPLIED by the operator" \
    || fail "the report must say the rate was supplied, not looked up (docs/decisions.md:64)"
printf '%s\n' "$out" | grep -qF "automatic threshold = balance_idr > 2 * 16000 = 32000 IDR" \
    || fail "the report must state the derived threshold, so an operator can check the boundary by hand"

# NOT A GATE. The exit code above is 0 with eligible rows present; the text must not
# contradict that by reading like a green light for a payout.
printf '%s\n' "$out" | grep -qF "THIS TOOL PAYS NOBODY" \
    || fail "the report must state plainly that it pays nobody"
printf '%s\n' "$out" | grep -qF "docs/wind-down.md" \
    || fail "the report must point at docs/wind-down.md for the payout steps"
printf '%s\n' "$out" | grep -qF "OPERATIONAL DATA, NOT SOURCE" \
    || fail "the report must warn that its output is sensitive operational data (account ids and balances)"

# --- 6. The refusals ---------------------------------------------------------
# Each is asserted with its OWN exit code, because collapsing them sends an operator
# to the wrong fix.
refuse() {
    # $1 = description, $2 = expected exit, $3 = expected message fragment, then env prefix
    desc="$1"; want="$2"; msg="$3"; shift 3
    # NOT `out=$(env ... | tr ...)`: `$?` after a pipeline is the exit of the LAST
    # command, so it would read tr's 0 and every refusal below would look like a pass.
    # That is exactly the silent-pass shape this file exists to catch. Redirect to a
    # file instead, so the code read is the script's own.
    env "$@" sh "$REPORT" >"$WORK/refuse.out" 2>&1
    rrc=$?
    r=$(tr -d '\r' < "$WORK/refuse.out")
    if [ "$rrc" -ne "$want" ]; then
        fail "$desc: expected exit $want, got $rrc"
    fi
    case "$r" in
        *"$msg"*) ;;
        *) fail "$desc: exited $rrc but did not say why (expected '$msg' in: $r)" ;;
    esac
}

# The rate: absent, garbage, and zero. A rate is required because it is FROZEN --
# there is no default and no lookup (docs/decisions.md:64).
# `env -u` (POSIX) so "unset" is genuinely unset even if the caller exported it.
refuse "rate unset"          7 "CLOSURE_USD_IDR_RATE is not set"      -u CLOSURE_USD_IDR_RATE DATABASE_URL="$DSN"
refuse "rate is not a number" 7 "is not a positive integer"           DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="sixteen-thousand"
refuse "rate has decimals"    7 "is not a positive integer"           DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="16000.5"
refuse "rate is zero"         7 "a rate must be positive"             DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="0"
refuse "rate is empty"        7 "CLOSURE_USD_IDR_RATE is not set"     DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE=""
refuse "rate is negative"     7 "is not a positive integer"           DATABASE_URL="$DSN" CLOSURE_USD_IDR_RATE="-16000"

# DATABASE_URL: absent, whitespace, not SQLite, in-memory, missing file.
refuse "DATABASE_URL unset"       2 "DATABASE_URL is not set"        -u DATABASE_URL CLOSURE_USD_IDR_RATE="$RATE"
refuse "DATABASE_URL whitespace"  2 "contains only whitespace"       DATABASE_URL="   " CLOSURE_USD_IDR_RATE="$RATE"
refuse "DATABASE_URL is postgres" 2 "is not a SQLite URL"            DATABASE_URL="postgres://nope" CLOSURE_USD_IDR_RATE="$RATE"
refuse "DATABASE_URL in-memory"   2 "does not name a file"           DATABASE_URL="sqlite::memory:" CLOSURE_USD_IDR_RATE="$RATE"
refuse "database file missing"    6 "no such database file"          DATABASE_URL="sqlite://$WORK/absent.db" CLOSURE_USD_IDR_RATE="$RATE"

# --- 7. The internal consistency guard ---------------------------------------
# The report refuses (exit 8) rather than printing a figure it cannot stand behind.
# Reached by removing the wallets row an eligible account's total was summed from
# while leaving the account in scope is not possible through the tool's own SQL -- the
# two queries would move together -- so the guard is exercised at the level it can be:
# the boundary predicate itself, by running against a database whose ONLY wallet sits
# exactly on the threshold. There the eligible list must be empty and the report must
# still exit 0, proving an empty result is a report and not an error.
ONLY="$WORK/only-exact.db"
cp "$DB" "$ONLY"
sqlite3 -bail "$ONLY" "DELETE FROM wallets WHERE account_id <> 'acct-exact-threshold';" >/dev/null 2>&1
env DATABASE_URL="sqlite://$ONLY" CLOSURE_USD_IDR_RATE="$RATE" sh "$REPORT" >"$WORK/only.out" 2>&1
rc3=$?
out3=$(tr -d '\r' < "$WORK/only.out")
if [ "$rc3" -ne 0 ]; then
    fail "a database whose only balance is exactly at the threshold must still exit 0 with an empty eligible list, got $rc3"
fi
printf '%s\n' "$out3" | grep -qF "ELIGIBLE (automatic payout)         : 0 account(s), 0 IDR" \
    || fail "with one wallet exactly at the threshold the eligible count must be 0, not 1"
printf '%s\n' "$out3" | grep -qF "(none)" \
    || fail "an empty eligible list must be shown as '(none)' rather than as a blank the operator has to interpret"

if [ "$FAILED" -ne 0 ]; then
    echo "wind-down-check: the wind-down report contract is BROKEN (see above)" >&2
    exit 1
fi

echo "wind-down-check: OK - the report classifies rails by SETTLED midtrans, splits the"
echo "wind-down-check:      threshold strictly (32000 sub, 32001 eligible), floors the"
echo "wind-down-check:      stablecoin units, refuses a missing/garbage/zero rate and a bad"
echo "wind-down-check:      DATABASE_URL, and leaves the database byte-identical"
exit 0
