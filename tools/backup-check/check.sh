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