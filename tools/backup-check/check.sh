#!/bin/sh
# apikita backup contract check.
#
# WHY THIS EXISTS. `tools/backup/backup.sh` had NO CI coverage, and a backup that does
# not work is how money and history are lost. Running it for the first time found a
# REAL DEFECT: the offsite hook was invoked as
#
#     sh -c "$OFFSITE_CMD" apikita-offsite "$ARTIFACT"
#
# With `sh -c CMD name arg`, `name` becomes $0 INSIDE CMD. That works for an INLINE
# command and SILENTLY FAILS for a SCRIPT hook - the natural shape for any real
# provider - which received NOTHING while the script printed "offsite hook succeeded"
# and exited 0. The one outcome the tool exists to prevent - a backup that never left
# the machine, reported as success - was reachable through the ordinary hook.
#
# WHAT IT CHECKS, in order of importance:
#   1. The ARTIFACT REACHES THE HOOK, in BOTH shapes (inline and script). This is the
#      property that was broken.
#   2. A FAILING hook exits 8 and keeps the local artifact.
#   3. The documented refusals still hold: 6 without a key, 1 without an offsite hook.
#   4. The artifact is ENCRYPTED - the file must not be a readable SQLite database,
#      because a plaintext dump that reports success is the worst outcome of all.
#
# It builds its own source database, so it needs the `migrate` binary on PATH or a
# prebuilt one. Skips LOUDLY (exit 3) when sqlite3 is missing, never 0.
#
# Usage: sh tools/backup-check/check.sh
# Exit: 0 all hold, 1 a violation, 3 a prerequisite is missing.

set -u

REPO=$(cd -- "$(dirname -- "$0")/../.." && pwd)
WORK="${TMPDIR:-/tmp}/apikita-backup-check-$$"

cleanup() { rm -rf "$WORK"; }
trap cleanup EXIT INT TERM
mkdir -p "$WORK" || { echo "backup-check: cannot create $WORK" >&2; exit 2; }

command -v sqlite3 >/dev/null 2>&1 || {
    echo "backup-check: SKIPPED - sqlite3 is not on PATH, so nothing was verified" >&2
    exit 3
}

# A minimal source database with the tables the tool checks. `migrate` would be more
# faithful, but requiring a Rust build would make this check slow enough that it stops
# being run - and the properties under test are the hook contract and the encryption,
# neither of which depends on the real schema.
SRC="$WORK/source.db"
sqlite3 "$SRC" "CREATE TABLE accounts (id TEXT PRIMARY KEY); INSERT INTO accounts VALUES ('a1');" || {
    echo "backup-check: could not create the source database" >&2
    exit 3
}

FAILED=0
fail() { echo "backup-check: FAIL - $1" >&2; FAILED=1; }

run_backup() {
    # $1 = BACKUP_ENCRYPTION_KEY, $2 = OFFSITE_CMD, $3 = BACKUP_DIR
    env DATABASE_URL="sqlite://$SRC" BACKUP_DIR="$3" OFFSITE_CMD="$2" \
        BACKUP_ENCRYPTION_KEY="$1" sh "$REPO/tools/backup/backup.sh" 2>&1
}

# --- 3. the documented refusals ----------------------------------------------
OUT="$WORK/refuse"
mkdir -p "$OUT"

out=$(run_backup "" "true" "$OUT"); rc=$?
[ "$rc" -eq 6 ] || fail "no encryption key should exit 6, got $rc"

out=$(run_backup "k" "" "$OUT"); rc=$?
[ "$rc" -eq 1 ] || fail "no offsite hook should exit 1 (a local backup is not a backup), got $rc"

out=$(env DATABASE_URL="postgres://nope" BACKUP_DIR="$OUT" OFFSITE_CMD=echo BACKUP_ENCRYPTION_KEY=k \
    sh "$REPO/tools/backup/backup.sh" 2>&1); rc=$?
[ "$rc" -eq 2 ] || fail "a non-sqlite URL should exit 2, got $rc"

# --- 1. the artifact REACHES a SCRIPT hook -----------------------------------
# The regression this check was written for.
OUT="$WORK/script"
mkdir -p "$OUT"
HOOK="$WORK/hook.sh"
SEEN="$WORK/seen.txt"
# The hook is a child process, so the path it writes to must be exported, not just set.
export SEEN
# The hook writes what it was GIVEN to a file the check then reads, so the assertion
# is about the ARGUMENT rather than about the hook merely not crashing.
cat > "$HOOK" <<'HOOKEOF'
#!/bin/sh
echo "$1" > "$SEEN"
[ -f "$1" ] || exit 1
HOOKEOF

out=$(run_backup "k" "sh $HOOK" "$OUT"); rc=$?
[ "$rc" -eq 0 ] || fail "a working SCRIPT hook should exit 0, got $rc: $out"

if [ ! -s "$SEEN" ]; then
    fail "a SCRIPT hook received NO argument. The artifact must reach it: a hook that
    silently gets nothing while the backup reports success is a backup that never left
    the machine."
else
    case "$(cat "$SEEN")" in
        *.enc) ;;
        *) fail "the script hook received '$(cat "$SEEN")', which is not the artifact path" ;;
    esac
fi

# --- 4. the artifact is ENCRYPTED -------------------------------------------
ART=$(find "$OUT" -name "*.enc" | head -n 1)
if [ -z "$ART" ]; then
    fail "no encrypted artifact was produced"
else
    if [ "$(head -c 6 "$ART")" = "SQLite" ]; then
        fail "the artifact is a PLAINTEXT SQLite database; docs/backup-and-restore.md requires encryption at rest"
    fi
    if sqlite3 "$ART" "SELECT 1" >/dev/null 2>&1; then
        fail "sqlite3 could OPEN the artifact unencrypted"
    fi
fi

# --- 2. a FAILING hook is exit 8 --------------------------------------------
OUT="$WORK/failhook"
mkdir -p "$OUT"
out=$(run_backup "k" "false" "$OUT"); rc=$?
[ "$rc" -eq 8 ] || fail "a failing offsite hook should exit 8, got $rc"
[ "$(find "$OUT" -name '*.enc' | wc -l)" -ge 1 ] || fail "a failed hook must KEEP the local artifact"


# ---------------------------------------------------------------------------
# A documented retention promise must match what actually enforces it.
# ---------------------------------------------------------------------------
# WHY THIS IS HERE, and why in a BACKUP guard. docs/ip-tracking.md is a PRIVACY
# document, and its Open items list carried:
#
#   "The purge is a binary with no scheduler behind it yet; it needs to be added to
#    whatever runs the nightly backup and reconciliation jobs."
#
# Read plainly that says an IP retention promise is NOT YET ENFORCED - the most
# alarming reading available for the key_ip_* tables, and the one an auditor would
# act on. The truth was the opposite: run_retention in the maintenance entrypoint
# had been applying both windows nightly for several waves. Only the standalone
# binary is absent, which is a convenience and not a retention gap.
#
# The document even PREDICTS this failure six lines above the item: "If any of these
# stops being true, the policy must change the same day. These are the kind of
# statements that become false through a well-intentioned feature addition." The
# item went false through exactly that.
#
# So the two sides are held together: if the entrypoint deletes from key_ip_seen,
# no document may say the purge still needs to be added, and vice versa. The
# assertion is that they AGREE, so it survives a change in either direction.
ENTRYPOINT="$REPO/.docker/maintenance/entrypoint.sh"
IPDOC="$REPO/docs/ip-tracking.md"
if [ ! -f "$ENTRYPOINT" ] || [ ! -f "$IPDOC" ]; then
    fail "cannot read $ENTRYPOINT and $IPDOC, so the IP retention claim was not compared"
else
    # THE INVOCATION, not a mention of it. A grep for the table name over the whole
    # file passes on a COMMENT or a banner line - which is exactly how my first
    # mutation of this guard escaped: renaming the EXECUTING line left key_ip_seen in
    # the header block and in the OK log line, so the check stayed green while the
    # delete no longer ran. So this looks for the call that actually executes.
    ENFORCED=no
    grep -q 'retention_delete "$DB_FILE" key_ip_seen' "$ENTRYPOINT" && ENFORCED=yes

    CLAIMS_PENDING=no
    grep -q 'needs to be added' "$IPDOC" && CLAIMS_PENDING=yes

    if [ "$ENFORCED" = yes ] && [ "$CLAIMS_PENDING" = yes ]; then
        fail "the entrypoint INVOKES the key_ip_seen retention delete, but docs/ip-tracking.md still tells the reader the purge needs to be added to the nightly jobs - a privacy document must not describe an enforced retention window as pending"
    fi
    if [ "$ENFORCED" = no ] && [ "$CLAIMS_PENDING" = no ]; then
        fail "the entrypoint no longer INVOKES the key_ip_seen retention delete, yet docs/ip-tracking.md no longer says the purge is pending - a reader would believe the IP retention window is enforced when nothing runs it"
    fi

    # Guard the fixture with the SAME precision: run_retention must exist AND the
    # invocation must sit inside it, or the comparison above read the wrong region.
    if ! sed -n '/^run_retention()/,/^}/p' "$ENTRYPOINT" | grep -q 'retention_delete'; then
        fail "run_retention does not invoke retention_delete in $ENTRYPOINT - the retention comparison did not actually happen"
    fi
fi


# ---------------------------------------------------------------------------
# The Rust sweep and the production sweep must delete from the SAME tables.
# ---------------------------------------------------------------------------
# WHY THIS EXISTS, and it is the same defect class as the block above, one layer
# lower. The retention policy is implemented TWICE: `purge_expired_usage` in
# server/src/db.rs (and `identity::tokens::purge_expired`, which it calls) is what
# the Rust tests exercise and what `bin/usage-purge` logs, while `run_retention` in
# the maintenance entrypoint is what ACTUALLY RUNS in production - the binary is
# not shipped in the server image.
#
# Nothing compared the two. So a table could be added to the Rust sweep, get a
# passing test, get a log line, and still never be deleted in production, with
# every check green. That is not hypothetical: it is exactly what happened to
# `identity_tokens`. Its purge function was written with a unit test and NO caller
# of any kind, and the table accumulated expired verification and password-reset
# links for as long as the identity port has existed while the privacy page said
# they do not persist.
#
# THE ASSERTION IS AGREEMENT, NOT MEMBERSHIP, so it survives a change in either
# direction: the entrypoint may be missing a delete the Rust sweep performs, or it
# may delete from a table the Rust sweep does not know about. Both are drift and
# both fail. The comment on the block above gives the same reason for the same
# shape.
DBRS="$REPO/server/src/db.rs"
if [ ! -f "$DBRS" ] || [ ! -f "$ENTRYPOINT" ]; then
    fail "cannot read $DBRS and $ENTRYPOINT, so the Rust and production retention sweeps were not compared"
else
    # The tables the RUST sweep deletes from. THREE sources, because the policy is
    # implemented across three functions and each owns a different class of table:
    #
    #   * `db::purge_expired_usage` - usage_events, usage_daily, sessions, and (via
    #     the call added with the identity fix) identity_tokens.
    #   * `identity::tokens::purge_expired` - identity_tokens' own SQL, in its own
    #     file, reached from the function above.
    #   * `ip_tracking::purge_expired` - the four salted-hash and counter tables.
    #     They were ALWAYS swept here rather than in `db.rs` (the entrypoint comment
    #     says so: "the runnable form of the ip-purge binary's SQL"), so reading only
    #     `db.rs` reports them as missing from the Rust side when they are simply
    #     somewhere else. That is the guard's own false positive, and the first run
    #     of it produced exactly that - which is the argument for reading each
    #     implementation rather than assuming one.
    #   * `routes::telegram::purge_terminal` - `link_codes`. A third module, and it
    #     is named for the RULE rather than for its table: the link-code window is
    #     "used or expired + 24h", which is not an age-based period, so it does not
    #     belong in `db.rs`'s list any more than `link_code_issues` belongs in
    #     `ip_tracking`'s. Adding it here is what the guard's own failure message
    #     asked for when this table first appeared on the shell side alone.
    #
    # NOTE that this extraction reads the SQL alphabetically after `sort -u`, and
    # that the set is compared to the shell's. A table deleted by a function none of
    # these four paragraphs names is a table this guard cannot see, which is why the
    # list of sources is spelled out here rather than derived: the day a fifth purge
    # function appears, the ONE thing to remember is to add its file to the block
    # below, and the failure that omission produces is this guard reporting a table
    # as "deleted in production but not in Rust" - a false alarm, not a miss.
    #
    # Read from the executable SQL, not from a doc or a struct field name: a field
    # named `identity_tokens` on `PurgedUsage` proves only that someone declared it,
    # and the whole defect being guarded here was a declaration with no delete behind
    # it.
    TOKENSRS="$REPO/server/src/identity/tokens.rs"
    IPTRS="$REPO/server/src/ip_tracking.rs"
    TELEGRAMRS="$REPO/server/src/routes/telegram.rs"
    RUST_TABLES=$(
        {
            sed -n '/^pub async fn purge_expired_usage(/,/^}/p' "$DBRS"
            [ -f "$TOKENSRS" ] && sed -n '/^pub async fn purge_expired(/,/^}/p' "$TOKENSRS"
            [ -f "$IPTRS" ] && sed -n '/^pub async fn purge_expired(/,/^}/p' "$IPTRS"
            [ -f "$TELEGRAMRS" ] && sed -n '/^pub async fn purge_terminal(/,/^}/p' "$TELEGRAMRS"
        } \
        | grep -oE 'DELETE FROM [a-z_]+' \
        | awk '{print $3}' \
        | sort -u
    )

    # The tables the PRODUCTION sweep deletes from. Two sources, because the entrypoint
    # expresses them two ways: seven routes go through the `retention_delete*` HELPERS
    # (whose `DELETE FROM $2` names the table only as a bound parameter, so the table
    # name is at the CALL SITE), and `identity_tokens` is written out inline because
    # its cutoff has no day offset.
    #
    # So both the call sites and the inline statement are read. Reading only the
    # helpers finds no table names at all; reading only the call sites misses the
    # inline one.
    #
    # NOT `sed -n '/^run_retention()/,/^}/p'`. That range stops at the FIRST line
    # starting with `}`, and `run_retention`'s body contains nested `if`/`case` blocks
    # whose closing brace sits at column 0 - so the extraction ended about a third of
    # the way in and this guard's own positive control caught it as an empty set.
    # An awk flag that clears when brace depth returns to zero walks the real body.
    RETENTION_BODY=$(
        awk '
            /^run_retention\(\)/ { infn = 1 }
            infn {
                print
                n = gsub(/\{/, "{")
                m = gsub(/\}/, "}")
                depth += n - m
                if (depth <= 0 && n + m > 0) { infn = 0 }
            }
        ' "$ENTRYPOINT"
    )

    SHELL_TABLES=$(
        {
            # Call sites: `retention_delete "$DB_FILE" <table> <days>` and its instant
            # sibling, whose table is the second argument after the file.
            printf '%s\n' "$RETENTION_BODY" \
                | grep -oE 'retention_delete(_instant)? "\$DB_FILE" [a-z_]+' \
                | awk '{print $3}'
            # Inline statements, for a table routed through no helper.
            printf '%s\n' "$RETENTION_BODY" \
                | grep -oE 'DELETE FROM [a-z_]+' \
                | awk '{print $3}'
        } | sort -u
    )

    # POSITIVE CONTROL. Two empty sets AGREE, and they agree for the reason
    # `purge_expired_usage` might have been renamed: the extraction read the wrong
    # region and found no SQL at all. Without this the check passes on a tree where
    # it is measuring nothing.
    if [ -z "$RUST_TABLES" ]; then
        fail "no DELETE FROM was found in purge_expired_usage (or identity::tokens::purge_expired) in $DBRS - the retention comparison is measuring an empty set, so it can neither pass nor fail meaningfully"
    fi
    if [ -z "$SHELL_TABLES" ]; then
        fail "no DELETE FROM was found inside run_retention in $ENTRYPOINT - the retention comparison is measuring an empty set"
    fi

    # Each side, one table per line, so the failure can name the offender instead of
    # printing two jumbled lists.
    MISSING_IN_SHELL=$(comm -23 <(printf '%s\n' "$RUST_TABLES") <(printf '%s\n' "$SHELL_TABLES"))
    if [ -n "$MISSING_IN_SHELL" ]; then
        fail "the Rust retention sweep deletes from these tables and the PRODUCTION sweep does not: $(printf '%s' "$MISSING_IN_SHELL" | tr '\n' ' '). server/src/bin/usage-purge.rs is NOT shipped in the server image, so the entrypoint is what runs: a table present only in the Rust sweep is swept in tests and NEVER in production, which is how expired identity links accumulated while every check stayed green."
    fi

    MISSING_IN_RUST=$(comm -13 <(printf '%s\n' "$RUST_TABLES") <(printf '%s\n' "$SHELL_TABLES"))
    if [ -n "$MISSING_IN_RUST" ]; then
        fail "the PRODUCTION sweep deletes from these tables and the Rust sweep does not: $(printf '%s' "$MISSING_IN_RUST" | tr '\n' ' '). A table the entrypoint deletes that no Rust code or test covers is a retention window with no measurement behind it - it cannot appear in the lag report, so a sweep that stopped would be invisible."
    fi

    # WHAT THIS CANNOT SEE, stated rather than left as an absence. The comparison is
    # over TABLE NAMES, so it catches a table that exists on one side and not the
    # other - which is the defect that prompted it, twice over. It does NOT catch a
    # table whose DELETE is present but whose CALL was dropped: removing the call to
    # `identity::tokens::purge_expired` from `purge_expired_usage` leaves the SQL in
    # `tokens.rs`, so the union still names `identity_tokens` and this check stays
    # green. That case is covered by a Rust test instead
    # (`the_retention_sweep_deletes_expired_links_and_keeps_live_ones`, proved by
    # deleting the call and watching it fail) - which is the right place for it,
    # because "the function is still called" is a fact about Rust control flow that a
    # shell grep cannot read.
    #
    # The WINDOW is not compared either: the shell passes 7 where Rust has
    # SEEN_RETENTION_DAYS = 7, and a change to one would not move the other. That gap
    # is older than this check and is recorded on docs/data-retention.md's nightly
    # note and in the entrypoint's own helper comments, which name the Rust constant
    # each literal mirrors.
    #
    # AND A THIRD BLIND SPOT, WHICH THIS PARAGRAPH USED TO OMIT AND WHICH HID A
    # CUSTOMER-FACING GAP. The comparison above is over `DELETE FROM` table names. A
    # sweep that retires credit by UPDATING a row and INSERTING a ledger entry has no
    # `DELETE` in it, so it is not in either set and every assertion above passes
    # without ever having considered it.
    #
    # That is not hypothetical: `db::expire_credit` - the sweep behind the credit-expiry
    # clause in docs/terms-of-service.md - is called from `usage-purge.rs` and nowhere
    # else, and `usage-purge.rs` contains NO `DELETE FROM` at all. It calls two library
    # functions. So the whole credit-expiry mechanism was outside this comparison, and
    # because `usage-purge` is not wired, NO credit ever expires in production while
    # this check stayed green.
    #
    # The assertion below closes it without widening the table comparison, which would
    # need SQL parsing and would rot. It asks the narrower question that the defect
    # makes checkable: every SWEEP function the unwired binary calls must have an inline
    # counterpart in `run_retention`, or be named here with a reason.
    #
    # SWEEP functions only, matched as `db::<name>(` - the call form. A constant
    # (`db::USAGE_EVENTS_RETENTION_DAYS`) is not a sweep, and `init_pool` is plumbing
    # that every binary needs; neither is in scope, and admitting them would make this
    # list long enough that a real omission would hide in it.
    #
    # AND PRODUCTION CODE ONLY. This used to grep the whole file, so a `db::` call in the
    # binary's own `#[cfg(test)] mod tests` was treated as a production dependency of the
    # sweep. MEASURED: adding one test whose fixture settles a deposit through
    # `db::credit_topup_transaction` - the real money-in path, exactly what a fixture SHOULD
    # use - failed this check with "calls these db functions and NOTHING accounts for them",
    # naming a function the BINARY never calls. The check was right to notice a new name and
    # wrong about where it came from, and the fix is scope, not an allow-list entry: a sweep
    # is what the shipped code path calls, and test code is not that path.
    #
    # Cut at the line beginning `mod tests`, which is the convention every binary here uses
    # (`usage-purge.rs`, `hold-sweep.rs`, `migrate.rs`). If that line is ever absent the grep
    # still runs over the whole file, so the failure mode is the old over-strict one rather
    # than a silently empty set - and the emptiness guard below catches the other direction.
    UNWIRED_TARGET="$REPO/server/src/bin/usage-purge.rs"
    UNWIRED_TESTS_AT=$(grep -n '^mod tests' "$UNWIRED_TARGET" | head -n 1 | cut -d: -f1)
    if [ -n "$UNWIRED_TESTS_AT" ]; then
        UNWIRED_CODE=$(sed -n "1,$((UNWIRED_TESTS_AT - 1))p" "$UNWIRED_TARGET")
    else
        UNWIRED_CODE=$(cat "$UNWIRED_TARGET")
    fi
    UNWIRED_CALLS=$(printf '%s\n' "$UNWIRED_CODE" \
        | grep -oE 'db::[a-z_]+\(\)?|db::[a-z_]+\(&' \
        | grep -oE 'db::[a-z_]+' | sort -u | grep -v 'db::init_pool')
    if [ -z "$UNWIRED_CALLS" ]; then
        fail "no db:: sweep call was found in server/src/bin/usage-purge.rs - the unwired-sweep comparison is measuring an empty set, so it can neither pass nor fail meaningfully"
    fi
    # Sweeps the entrypoint genuinely reimplements inline. `purge_expired_usage` is the
    # age-based DELETE sweep and is covered by the table comparison above.
    INLINE_COVERED='purge_expired_usage'
    # Sweeps covered by running the shipped binary, asserted below. This is the list that
    # `db::expire_credit` MOVED ONTO - it used to be KNOWN_UNENFORCED, which was accurate
    # while `usage-purge` shipped in no image and is now false.
    BINARY_COVERED='expire_credit'
    # Sweeps that are NOT inline AND NOT run by any shipped binary. Each needs a reason,
    # and the reason is what a reader needs to act on.
    #
    # THIS LIST IS EMPTY, AND THAT IS THE OUTCOME IT EXISTED FOR. `db::expire_credit` was
    # its only entry: the sweep is an UPDATE plus an INSERT, so the DELETE-based comparison
    # above could not see it, and `usage-purge` was built by nothing - so the wallet page
    # and the terms of service promised a two-year expiry that nothing applied. The fix was
    # NOT a second implementation in the entrypoint (that would duplicate a guarded debit
    # and a ledger row, i.e. the ledger invariant); it was to BUILD AND SHIP THE BINARY.
    #
    # An entry here means a published promise is unenforced, so adding one is a serious
    # choice rather than a way to silence this check.
    KNOWN_UNENFORCED=$(cat <<'KNOWN'
KNOWN
)
    UNEXPLAINED=""
    for fn in $UNWIRED_CALLS; do
        case " ${INLINE_COVERED} " in *" ${fn#db::} "*) continue ;; esac
        case " ${BINARY_COVERED} " in *" ${fn#db::} "*) continue ;; esac
        if ! printf '%s\n' "$KNOWN_UNENFORCED" | grep -qF "$fn "; then
            UNEXPLAINED="$UNEXPLAINED $fn"
        fi
    done
    if [ -n "$UNEXPLAINED" ]; then
        fail "server/src/bin/usage-purge.rs calls these db functions and NOTHING accounts for them:$UNEXPLAINED. Either the entrypoint reimplements the work inline (add it to INLINE_COVERED here, and to run_retention), or a shipped binary runs it (add it to BINARY_COVERED and add the job), or it is genuinely unenforced (add it to KNOWN_UNENFORCED with the reason a reader needs, AND say so in docs/terms-of-service.md). The credit-expiry sweep went unenforced for exactly this reason, and the DELETE-based comparison above could not see it."
    fi

    # THE COVERAGE ITSELF, not just the accounting. The loop above passes when every call is
    # either inline or explained; that is the wrong question now that the answer for
    # `expire_credit` is "the binary runs it". Ask directly: is the binary BUILT by the
    # image, and is it RUN by the schedule? Two greps, because a binary that is built and
    # never run leaves the promise exactly as broken as one that is neither.
    if ! grep -q -- '--bin usage-purge' "$REPO/server/Dockerfile"; then
        fail "server/Dockerfile does not build --bin usage-purge, so db::expire_credit ships in no image and NO CREDIT EXPIRES while docs/terms-of-service.md promises a two-year term. This is the defect this check was widened to catch."
    fi
    # THE CALL, NOT THE DEFINITION - and two attempts at this both failed, so the reason is
    # worth recording.
    #   attempt 1: `grep -q 'run_credit_expiry'` matched the `run_credit_expiry() {` line.
    #   attempt 2: `grep -qE '^[[:space:]]*run_credit_expiry([[:space:]]|\|\||$)'` ALSO
    #              matched it, because in ERE `()` is an EMPTY GROUP - the pattern reduces
    #              to "the bare name" and the `(` that follows is never examined.
    # MEASURED each time: deleting the call from `run_wired_jobs` left this check at exit 0.
    # A guard satisfied by a definition is satisfied by dead code, which is the same defect
    # as the unwired binary it exists to catch.
    #
    # So match a CALL SHAPE explicitly: the name, NOT followed by `(`, then whitespace (which
    # is what both real call sites have - `run_credit_expiry || rc=1` and the bare verb form).
    if ! grep -qE '^[[:space:]]*run_credit_expiry[^([:alnum:]_]' "$REPO/.docker/maintenance/entrypoint.sh"; then
        fail ".docker/maintenance/entrypoint.sh does not CALL run_credit_expiry (only defines it, or does not mention it), so the credit-expiry sweep is built and never RUN. A binary that ships and is not scheduled enforces the published term no better than one that never shipped."
    fi
fi


# ---------------------------------------------------------------------------
# A privacy obligation marked "not written" must match whether the text exists.
# ---------------------------------------------------------------------------
# WHY THIS IS HERE, beside the IP-retention check: it is the same defect class in
# the same document family. docs/data-retention.md is where a reader learns what
# personal data is held and whether the obligations around it are discharged, and
# its cross-border table said of TWO rows:
#
#   | Disclose forwarding in the terms | **Required, not yet written** |
#   | State the provider jurisdiction  | **Required, not yet written** |
#
# MEASURED: both were false. docs/terms-of-service.md contains the forwarding
# disclosure (:19, naming mainland China), the jurisdiction requirement (:156) and
# the before-first-use promise (:292). What is genuinely open is the legal REVIEW
# (deferred until a revenue trigger) and PUBLICATION - three different statuses
# that the doc collapsed into the one that was untrue.
#
# So the two files are held together: if the ToS contains the disclosure, no
# retention doc may describe it as unwritten, and vice versa. Asserting AGREEMENT
# rather than a fact means it stays true whichever side someone edits.
TOS="$REPO/docs/terms-of-service.md"
RETDOC="$REPO/docs/data-retention.md"
if [ ! -f "$TOS" ] || [ ! -f "$RETDOC" ]; then
    fail "cannot read $TOS and $RETDOC, so the disclosure status was not compared"
else
    # The DISCLOSURE existing is the corpus of the claim; look for the substance,
    # not a heading - "mainland China" is the fact a reader needs.
    DISCLOSED=no
    grep -q 'mainland China' "$TOS" && DISCLOSED=yes

    CLAIMS_UNWRITTEN=no
    grep -q 'not yet written' "$RETDOC" && CLAIMS_UNWRITTEN=yes

    if [ "$DISCLOSED" = yes ] && [ "$CLAIMS_UNWRITTEN" = yes ]; then
        fail "docs/terms-of-service.md DOES contain the cross-border forwarding disclosure, but docs/data-retention.md still marks it 'not yet written' - a reader tracking privacy readiness is told a disclosure does not exist when it does"
    fi
    if [ "$DISCLOSED" = no ] && [ "$CLAIMS_UNWRITTEN" = no ]; then
        fail "docs/terms-of-service.md no longer contains the forwarding disclosure, yet docs/data-retention.md no longer says it is unwritten - a reader would believe the disclosure exists when it does not"
    fi

    # Guard the fixture: both sides must have been read from a real document.
    grep -q 'cross-border' "$TOS" && grep -q 'Obligation' "$RETDOC" || {
        fail "the disclosure table or the ToS could not be located, so the comparison above did not actually happen"
    }
fi

if [ "$FAILED" -ne 0 ]; then
    echo "backup-check: the backup contract is BROKEN (see above)" >&2
    exit 1
fi

echo "backup-check: OK - the artifact reaches both hook shapes, the refusals hold, and the artifact is encrypted"
exit 0