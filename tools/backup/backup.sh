#!/bin/sh
# apikita Postgres backup - the tooling docs/backup-and-restore.md specifies.
#
# What it does, in order:
#   1. resolve DATABASE_URL (falls back to the documented local dev DSN)
#   2. locate pg_dump/pg_restore: host PATH, else the "postgres" compose service
#   3. REFUSE to continue unless BACKUP_ENCRYPTION_KEY is set (exit 6). A silent
#      plaintext downgrade is the failure mode this tool exists to prevent.
#   4. pg_dump -Fc into a temp file, then VERIFY it with "pg_restore --list".
#      An empty or unlistable dump is a FAILURE (exit 5), never a warning: a
#      truncated dump is the classic backup that "succeeds" and restores nothing.
#   5. encrypt to the artifact (openssl AES-256-CBC, PBKDF2, 200k iters) and
#      VERIFY the ciphertext decrypts back into a listable archive (exit 7).
#   6. run the offsite hook OFFSITE_CMD with the artifact path as its argument
#      (exit 8 on failure). Unset OFFSITE_CMD is exit 1: "offsite copy missing"
#      is an alert in docs/backup-and-restore.md ("Offsite is not a detail").
#   7. prune artifacts older than BACKUP_RETENTION_DAYS, never the newest (exit 9)
#
# Exit codes (3 and 4 keep tools/reconcile/reconcile.sh's meanings):
#   0  backup complete: dump written, verified, encrypted, offsite copy made
#   1  local backup written and verified, but NO offsite copy (OFFSITE_CMD unset)
#   2  DATABASE_URL is set but is not a postgres:// / postgresql:// DSN
#   3  pg_dump / pg_restore not available (no host client and no usable container)
#   4  pg_dump ran but failed (connection, permissions, disk)
#   5  the dump is empty or cannot be listed by pg_restore - it is not a backup
#   6  BACKUP_ENCRYPTION_KEY is not set (refusing to write a plaintext dump)
#   7  encryption failed, or the encrypted artifact did not decrypt to a listable archive
#   8  the offsite hook (OFFSITE_CMD) failed
#   9  destination directory is not writable, or retention pruning failed
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
DEFAULT_DATABASE_URL="${BACKUP_DEFAULT_DATABASE_URL:-postgres://postgres:dev@localhost:5432/apikita}"
COMPOSE_FILE="${COMPOSE_FILE:-$REPO_ROOT/docker-compose.yml}"
CONTAINER_SERVICE="${CONTAINER_SERVICE:-postgres}"
ARTIFACT_PREFIX="${ARTIFACT_PREFIX:-apikita}"

fail() { echo "backup: $*" >&2; }

# --- the DSN -----------------------------------------------------------------
DSN="${DATABASE_URL:-}"
case "$DSN" in
    *[![:space:]]*) ;;
    *)
        fail "DATABASE_URL is not set (unset, empty or whitespace only)"
        fail "using the documented local development DSN instead:"
        fail "  $DEFAULT_DATABASE_URL"
        fail "  (docs/local-development.md:124) - LOCAL dev stack only, never production"
        DSN="$DEFAULT_DATABASE_URL"
        ;;
esac
case "$DSN" in
    postgres://*|postgresql://*) ;;
    *)
        fail "DATABASE_URL is not a postgres:// or postgresql:// DSN: '$DSN'"
        exit 2
        ;;
esac

# --- pg_dump / pg_restore ----------------------------------------------------
# This host has no PostgreSQL client; the local stack runs one inside the
# "postgres" compose service. Prefer a host client when one exists, else exec in
# the container. list_archive() reads the archive from stdin so it works for both.
if command -v pg_dump >/dev/null 2>&1 && command -v pg_restore >/dev/null 2>&1; then
    dump_db() { pg_dump "$@"; }
    list_archive() { pg_restore --list; }
    DUMP_TOOL="pg_dump (host PATH)"
elif command -v docker >/dev/null 2>&1 && [ -f "$COMPOSE_FILE" ]; then
    dump_db() { docker compose -f "$COMPOSE_FILE" exec -T "$CONTAINER_SERVICE" pg_dump "$@"; }
    list_archive() { docker compose -f "$COMPOSE_FILE" exec -T "$CONTAINER_SERVICE" pg_restore --list; }
    DUMP_TOOL="docker compose exec -T $CONTAINER_SERVICE pg_dump"
else
    fail "pg_dump/pg_restore are not on PATH, and no usable container fallback exists"
    fail "  looked for: docker on PATH + $COMPOSE_FILE"
    fail "install the PostgreSQL client, or run this where the stack runs"
    exit 3
fi

# --- the encryption key, BEFORE anything is written --------------------------
# Checked here, not at the point of use: a dump that has already been written
# must never be left on disk unencrypted because the key turned out to be absent.
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
LIST="$TMP/$ARTIFACT_PREFIX.$STAMP.list"
ERRA="$TMP/$ARTIFACT_PREFIX.$STAMP.err"
PRUNE_LIST="$TMP/$ARTIFACT_PREFIX.$STAMP.prune"
trap 'rm -f "$RAW" "$DEC" "$TMP_ART" "$LIST" "$ERRA" "$PRUNE_LIST"' EXIT HUP INT TERM

# The raw dump is plaintext and contains emails and every balance: keep it
# unreadable to anyone but the owner while it exists.
umask 077

TS=$(date -u +%Y%m%dT%H%M%SZ)
ARTIFACT="$BACKUP_DIR/$ARTIFACT_PREFIX-$TS.dump.enc"

# --- dump, and verify the DUMP itself ----------------------------------------
dump_db -Fc -d "$DSN" > "$RAW" 2>"$ERRA"
STATUS=$?
if [ "$STATUS" -ne 0 ]; then
    fail "pg_dump failed (exit $STATUS):"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 4
fi

SIZE=$(wc -c < "$RAW" | tr -d ' ')
if [ "$SIZE" -eq 0 ]; then
    fail "pg_dump exited 0 but produced an EMPTY dump - not a backup"
    exit 5
fi

if ! list_archive < "$RAW" > "$LIST" 2>"$ERRA"; then
    fail "the dump is not a readable custom-format archive (pg_restore --list failed):"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    fail "a dump that cannot be listed is a FAILURE, not a warning"
    exit 5
fi
TOC=$(grep -c '^[0-9]' "$LIST" 2>/dev/null || echo 0)

# --- encrypt -----------------------------------------------------------------
# AES-256-CBC + PBKDF2-HMAC-SHA256 (200000 iterations, random salt). This is
# real symmetric encryption, not obfuscation - and not authenticated encryption:
# see README.md, "Encryption", for what that does and does not guarantee.
if ! openssl enc -aes-256-cbc -pbkdf2 -iter 200000 -salt \
        -pass env:BACKUP_ENCRYPTION_KEY -in "$RAW" -out "$TMP_ART" 2>"$ERRA"; then
    fail "encryption failed:"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 7
fi

# Verify the ciphertext, not just the plaintext: decrypt it back and list the
# result. A wrong key, a truncated ciphertext, or a corrupt archive all fail here.
if ! openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
        -pass env:BACKUP_ENCRYPTION_KEY -in "$TMP_ART" -out "$DEC" 2>"$ERRA"; then
    fail "the encrypted artifact did not decrypt (wrong key, or corrupt ciphertext):"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
    exit 7
fi
if ! list_archive < "$DEC" > "$LIST" 2>"$ERRA"; then
    fail "the decrypted artifact is not a readable archive - the backup is not restorable:"
    [ -s "$ERRA" ] && cat "$ERRA" >&2
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
echo "backup: dump $SIZE bytes -> artifact $ESIZE bytes, $TOC TOC entries, sha256=$SHA"
echo "backup: dumped with $DUMP_TOOL; encrypted AES-256-CBC/PBKDF2-200k"

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
    if sh -c "$OFFSITE_CMD" apikita-offsite "$ARTIFACT"; then
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
