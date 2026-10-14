#!/bin/sh
# apikita SQLite backup - the tooling docs/backup-and-restore.md specifies.
#
# What it does, in order:
#   1. resolve DATABASE_URL (falls back to the local development database)
#   2. require the sqlite3 CLI
#   3. REFUSE to continue unless BACKUP_ENCRYPTION_KEY is set (exit 6). A silent
#      plaintext downgrade is the failure mode this tool exists to prevent.
#   4. take the copy with SQLite's .backup (the online backup API), then VERIFY it:
#      the 16-byte SQLite header, then PRAGMA integrity_check over the whole file.
#      An empty, truncated or corrupt copy is a FAILURE (exit 5), never a warning:
#      the classic silent backup is one that "succeeds" and restores nothing.
#   5. encrypt to the artifact (openssl AES-256-CBC, PBKDF2, 200k iters) and
#      VERIFY the ciphertext by decrypting it back and running the same header +
#      integrity_check on the result (exit 7).
#   6. run the offsite hook OFFSITE_CMD with the artifact path as its argument
#      (exit 8 on failure). Unset OFFSITE_CMD is exit 1: "offsite copy missing"
#      is an alert in docs/backup-and-restore.md ("Offsite is not a detail").
#   7. prune artifacts older than BACKUP_RETENTION_DAYS, never the newest (exit 9)
#
# WHY .backup AND NOT cp
#   SQLite's correct copy primitive for a database another process may be writing
#   is the online backup API, which the CLI exposes as the .backup dot-command. It
#   takes a read lock for the duration of each step and, crucially, it reads the
#   database THROUGH the WAL - so it sees committed transactions that are still
#   sitting in the -wal file. A raw cp of a live WAL database does not: it copies
#   the main file (which may be missing committed frames) and either drops the -wal
#   or leaves a mismatched pair. VACUUM INTO would also be consistent, but it
#   requires its output path not to exist and it rewrites the whole database, which
#   is a different tool for a different job. .backup is the primitive that matches
#   what this script needs: a consistent snapshot of a live database.
#
# Exit codes (3 and 4 keep tools/reconcile/reconcile.sh's meanings):
#   0  backup complete: copy taken, verified, encrypted, offsite copy made
#   1  local backup written and verified, but NO offsite copy (OFFSITE_CMD unset)
#   2  DATABASE_URL is set but is not a sqlite:// URL, or names an in-memory database
#   3  the sqlite3 CLI is not available (not installed / not on PATH)
#   4  the .backup ran but failed (unreadable file, disk full, lock held)
#   5  the copy is empty or is not a readable SQLite database - it is not a backup
#   6  BACKUP_ENCRYPTION_KEY is not set (refusing to write a plaintext dump)
#   7  encryption failed, or the encrypted artifact did not decrypt to an intact database
#   8  the offsite hook (OFFSITE_CMD) failed
#   9  destination directory is not writable, or retention pruning failed
#
# There is no container fallback any more, and adding one would be dead code:
# docker-compose.yml has no database service to exec into (docker compose config
# --services -> nginx, scheduler), so "docker compose exec postgres ..." could only
# ever fail at runtime. A missing sqlite3 is exit 3, loudly.
#
# The encryption key is read from the environment and passed to openssl as
# "env:BACKUP_ENCRYPTION_KEY", so it never appears in the process list.
#
# Verified vs assumed, and the human decisions still open (offsite provider, key
# custody): see README.md next to this file. Nothing here invents a provider.

set -u

SCRIPT_DIR=$(cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

BACKUP_DIR="${BACKUP_DIR:-$REPO_ROOT/tmp/backups}"
BACKUP_RETENTION_DAYS="${BACKUP_RETENTION_DAYS:-30}"
# The fallback is this repo's local development database, spelled as an absolute
# path on purpose. "sqlite://data/server.db" (docs/local-development.md) is
# relative to the SERVER's working directory; a cron job resolving it against its
# own cwd would back up nothing, or worse, create an empty database and "succeed".
BACKUP_DEFAULT_DATABASE_URL="${BACKUP_DEFAULT_DATABASE_URL:-sqlite://$REPO_ROOT/server/data/server.db}"
ARTIFACT_PREFIX="${ARTIFACT_PREFIX:-apikita}"

fail() { echo "backup: $*" >&2; }

# --- the sqlite3 CLI ---------------------------------------------------------
if ! command -v sqlite3 >/dev/null 2>&1; then
    fail "the sqlite3 CLI is not installed or not on PATH"
    fail "  looked for: sqlite3 on PATH. There is no container fallback:"
    fail "  docker-compose.yml has no database service (services: nginx, scheduler)"
    fail "  install the SQLite command-line shell and retry"
    exit 3
fi

# --- the DSN -----------------------------------------------------------------
DSN="${DATABASE_URL:-}"
case "$DSN" in
    *[![:space:]]*) ;;
    *)
        fail "DATABASE_URL is not set (unset, empty or whitespace only)"
        fail "using the local development database instead:"
        fail "  $BACKUP_DEFAULT_DATABASE_URL"
        fail "  (docs/local-development.md) - LOCAL dev database only, never production"
        DSN="$BACKUP_DEFAULT_DATABASE_URL"
        ;;
esac
case "$DSN" in
    sqlite://*) DB_PATH=${DSN#sqlite://} ;;
    sqlite:*)   DB_PATH=${DSN#sqlite:} ;;
    *)
        fail "DATABASE_URL is not a sqlite:// URL: '$DSN'"
        fail "  expected e.g. sqlite://data/server.db (there is no database server any more)"
        exit 2
        ;;
esac
DB_PATH=${DB_PATH%%\?*}
if [ -z "$DB_PATH" ] || [ "$DB_PATH" = ":memory:" ]; then
    fail "DATABASE_URL does not name a file: '$DSN'"
    fail "  an in-memory database cannot be backed up from outside the process"
    exit 2
fi
# A relative path is relative to the SERVER's working directory
# (docs/local-development.md: "relative to server/"), not to this script's cwd.
case "$DB_PATH" in
    /*|?:[\\/]*) ;;
    *) DB_PATH="$REPO_ROOT/server/$DB_PATH" ;;
esac
if [ ! -f "$DB_PATH" ]; then
    fail "no such database file: $DB_PATH"
    fail "  create it with 'cargo run --bin migrate' (from server/)"
    exit 4
fi

# --- the encryption key, BEFORE anything is written --------------------------
# Checked here, not at the point of use: a copy that has already been written must
# never be left on disk unencrypted because the key turned out to be absent.
if [ -z "${BACKUP_ENCRYPTION_KEY:-}" ]; then
    fail "BACKUP_ENCRYPTION_KEY is not set"
    fail "refusing to write an UNENCRYPTED dump: docs/backup-and-restore.md requires"
    fail "backups be encrypted at rest, with the key held separately from the backup"
    fail "  export BACKUP_ENCRYPTION_KEY='<key from your secret manager>'"
    exit 6
fi

# --- destination -------------------------------------------------------------
if ! mkdir -p "$BACKUP_DIR" 2>/dev/null || [ ! -w "$BACKUP_DIR" ]; then
    fail "destination directory is not writable: $BACKUP_DIR"
    exit 9
fi

case "$BACKUP_RETENTION_DAYS" in
    ''|*[!0-9]*)
        fail "BACKUP_RETENTION_DAYS='$BACKUP_RETENTION_DAYS' is not a non-negative integer; using 30"
        BACKUP_RETENTION_DAYS=30
        ;;
esac

TMP="${TMPDIR:-/tmp}"
STAMP=$$
RAW="$TMP/$ARTIFACT_PREFIX.$STAMP.dump"
DEC="$TMP/$ARTIFACT_PREFIX.$STAMP.verify.dump"
TMP_ART="$TMP/$ARTIFACT_PREFIX.$STAMP.enc"
ERRA="$TMP/$ARTIFACT_PREFIX.$STAMP.err"
PRUNE_LIST="$TMP/$ARTIFACT_PREFIX.$STAMP.prune"
trap 'rm -f "$RAW" "$DEC" "$TMP_ART" "$ERRA" "$PRUNE_LIST"' EXIT HUP INT TERM

# The raw copy is plaintext and contains emails and every balance: keep it
# unreadable to anyone but the owner while it exists.
umask 077

TS=$(date -u +%Y%m%dT%H%M%SZ)
# The artifact name carries the process stamp too: the timestamp has ONE-SECOND
# resolution, so two runs completing in the same second would mv their
# ciphertext to the SAME path - the second silently overwriting the first, both
# reporting success, one backup gone. The stamp (the same pid-unique STAMP the
# temp files use) makes the name unique per run.
ARTIFACT="$BACKUP_DIR/$ARTIFACT_PREFIX-$TS-$STAMP.dump.enc"

# An artifact is only usable if the 16-byte SQLite header is there and
# integrity_check agrees. This replaces "pg_restore --list": the same question -
# "is this a real, intact database?" - asked of a different format.
is_sqlite_file() {
    [ -f "$1" ] || return 1
    head -c 16 -- "$1" 2>/dev/null | grep -q 'SQLite format 3' || return 1
    return 0
}
integrity_ok() {
    sqlite3 -readonly -bail -noheader "$1" "PRAGMA integrity_check;" 2>/dev/null | head -1
}

# ---------------------------------------------------------------------------
# sqlite_dot <db-file> <dot-command> <dir of the command's path argument>
#
# The sqlite3 CLI's dot-commands resolve their path argument with the C library,
# so an MSYS/Git-Bash absolute path like /c/dev/... is NOT translated and the
# command fails with "cannot open". cd'ing into the argument's directory and using
# a bare basename is the one form that works on every host. The database file
# itself is opened by the CLI (which does translate), so it is passed absolutely.
# ---------------------------------------------------------------------------
sqlite_dot() {
    _db="$1"; _cmd="$2"; _argdir="$3"
    ( cd -- "$_argdir" && sqlite3 -bail "$_db" "$_cmd" )
}

# --- take the copy -----------------------------------------------------------
# .backup writes a consistent snapshot of the LIVE database into the temp file.
# It is run against the source by its absolute path (the CLI translates that) and
# the output path is given as a bare basename from its own directory (which it
# does not).
RAW_DIR=$(dirname -- "$RAW"); RAW_BASE=$(basename -- "$RAW")
sqlite_dot "$DB_PATH" ".backup '$RAW_BASE'" "$RAW_DIR" >/dev/null 2>"$ERRA"
STATUS=$?
if [ "$STATUS" -ne 0 ]; then
    fail "the .backup of $DB_PATH failed (exit $STATUS):"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 4
fi

SIZE=$(wc -c < "$RAW" | tr -d ' ')
if [ "$SIZE" -eq 0 ]; then
    fail "the backup produced an EMPTY copy - not a backup"
    exit 5
fi

if ! is_sqlite_file "$RAW"; then
    fail "the copy is not a SQLite database (no 'SQLite format 3' header): $RAW"
    fail "a copy that cannot be read is a FAILURE, not a warning"
    exit 5
fi
COPY_INTEGRITY=$(integrity_ok "$RAW")
if [ "$COPY_INTEGRITY" != "ok" ]; then
    fail "PRAGMA integrity_check on the copy says: '${COPY_INTEGRITY:-<no output>}' (not 'ok')"
    fail "the copy is corrupt - it is not a backup"
    exit 5
fi

# --- encrypt -----------------------------------------------------------------
# AES-256-CBC + PBKDF2-HMAC-SHA256 (200000 iterations, random salt). This is real
# symmetric encryption, not obfuscation - and not authenticated encryption: see
# README.md, "Encryption", for what that does and does not guarantee.
if ! openssl enc -aes-256-cbc -pbkdf2 -iter 200000 -salt \
        -pass env:BACKUP_ENCRYPTION_KEY -in "$RAW" -out "$TMP_ART" 2>"$ERRA"; then
    fail "encryption failed:"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 7
fi

# Verify the ciphertext, not just the plaintext: decrypt it back and check the
# result is still an intact database. A wrong key, a truncated ciphertext, or a
# corrupt copy all fail here.
if ! openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
        -pass env:BACKUP_ENCRYPTION_KEY -in "$TMP_ART" -out "$DEC" 2>"$ERRA"; then
    fail "the encrypted artifact did not decrypt (wrong key, or corrupt ciphertext):"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 7
fi
if ! is_sqlite_file "$DEC"; then
    fail "the decrypted artifact is not a SQLite database - the backup is not restorable"
    exit 7
fi
DEC_INTEGRITY=$(integrity_ok "$DEC")
if [ "$DEC_INTEGRITY" != "ok" ]; then
    fail "the decrypted artifact failed integrity_check: '${DEC_INTEGRITY:-<no output>}' (not 'ok')"
    fail "the backup is not restorable"
    exit 7
fi

# Publish atomically: a reader never sees a half-written artifact under the final
# name, and the plaintext never lands in the destination directory.
if ! mv "$TMP_ART" "$ARTIFACT"; then
    fail "could not publish the artifact to $ARTIFACT"
    exit 9
fi
rm -f "$RAW" "$DEC"

ESIZE=$(wc -c < "$ARTIFACT" | tr -d ' ')
SHA=$(openssl dgst -sha256 -r "$ARTIFACT" 2>/dev/null | cut -d' ' -f1)
echo "backup: $ARTIFACT"
echo "backup: copy $SIZE bytes -> artifact $ESIZE bytes, integrity_check=ok, sha256=$SHA"
echo "backup: copied with .backup (SQLite online backup API) from $DB_PATH; encrypted AES-256-CBC/PBKDF2-200k"

# --- offsite -----------------------------------------------------------------
# The hook is the pluggable seam: any provider (rclone, aws-cli, s3cmd, rsync,
# restic) fits, and the provider itself is a human decision still open in
# docs/backup-and-restore.md ("Offsite storage provider and encryption key
# custody"). No credentials are invented here.
EXIT_CODE=0
if [ -z "${OFFSITE_CMD:-}" ]; then
    fail "OFFSITE_CMD is not set: the backup is LOCAL ONLY"
    fail "  a backup on the same host as the database is not a backup"
    fail "  (docs/backup-and-restore.md, 'Offsite is not a detail') - exit 1"
    EXIT_CODE=1
else
    # THE ARTIFACT IS APPENDED AS AN ARGUMENT, and it is dispatched through "$@".
    #
    # WHY THIS IS NOT `sh -c "$OFFSITE_CMD" apikita-offsite "$ARTIFACT"`, which is what
    # it used to be: with `sh -c CMD name arg`, `name` becomes $0 INSIDE CMD. That
    # works for an INLINE command ("rclone copy $1 remote:") and SILENTLY FAILS for a
    # SCRIPT hook ("sh /usr/local/bin/upload.sh"), because the artifact lands in the
    # outer sh's $0 while the script itself receives nothing. Measured both forms:
    #
    #   inline -> $1 is the artifact
    #   script -> $1 is EMPTY
    #
    # A script hook is the NATURAL shape for any real provider (rclone, aws-cli, a
    # wrapper that reads credentials), so the silent form was the common one - and the
    # script still printed "hook succeeded" and exited 0, while the offsite copy had
    # NOT been made. That is precisely the outcome this tool exists to prevent: exit 1
    # is documented to mean "no offsite copy", and here there was none with exit 0.
    #
    # The artifact is SINGLE-QUOTED into the command string, which satisfies both
    # shapes with no duplication:
    #   inline -> "rclone copy $1 remote:" becomes "...copy '/path/x.enc' remote:",
    #             so $1 is the artifact (the inner shell expands it).
    #   script -> "sh hook.sh" becomes "sh hook.sh '/path/x.enc'", so the hook
    #             receives it as its own $1.
    # An earlier attempt used `"$OFFSITE_CMD \"$@\"" _ "$ARTIFACT"`, which fixed the
    # script form but gave an INLINE hook the artifact TWICE - once from $@ and once
    # from the $1 it already referenced. Caught by running it, not by reading it.
    #
    # The path is single-quoted, so a path containing a single quote would break the
    # command. BACKUP_DIR is operator-controlled and a quote in it is pathological,
    # but the failure would be a confusing syntax error rather than a clear refusal,
    # so it is rejected up front instead.
    case "$ARTIFACT" in
        *"'"*)
            fail "the artifact path contains a single quote, which cannot be passed to the offsite hook safely: $ARTIFACT"
            exit 9
            ;;
    esac
    if sh -c "$OFFSITE_CMD '$ARTIFACT'"; then
        echo "backup: offsite hook succeeded: $OFFSITE_CMD"
    else
        fail "offsite hook FAILED: $OFFSITE_CMD $ARTIFACT"
        fail "  the local artifact is kept; the offsite copy is MISSING"
        exit 8
    fi
fi

# --- retention ---------------------------------------------------------------
# Prune artifacts older than the retention window, but NEVER the newest one:
# deleting the only surviving backup to satisfy a retention rule is worse than
# keeping an old one. The artifact just written is newest by construction.
NEWEST=$(ls -1t "$BACKUP_DIR"/"$ARTIFACT_PREFIX"-*.dump.enc 2>/dev/null | head -n 1)
PRUNED=0
KEPT_NEWEST=0
if find "$BACKUP_DIR" -maxdepth 1 -type f -name "$ARTIFACT_PREFIX-*.dump.enc" \
        -mtime +"$BACKUP_RETENTION_DAYS" > "$PRUNE_LIST" 2>/dev/null; then
    while IFS= read -r old || [ -n "$old" ]; do
        [ -z "$old" ] && continue
        if [ "$old" = "$NEWEST" ]; then
            KEPT_NEWEST=1
            continue
        fi
        if rm -f "$old"; then
            echo "backup: pruned (older than ${BACKUP_RETENTION_DAYS}d): $old"
            PRUNED=$((PRUNED + 1))
        else
            fail "could not prune $old"
            exit 9
        fi
    done < "$PRUNE_LIST"
else
    fail "retention scan failed for $BACKUP_DIR"
    exit 9
fi

echo "backup: retention ${BACKUP_RETENTION_DAYS}d: pruned $PRUNED artifact(s), kept the newest"
[ "$KEPT_NEWEST" -eq 1 ] && echo "backup:   (the newest artifact is older than the window and was kept anyway)"
exit "$EXIT_CODE"
