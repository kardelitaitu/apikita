# Postgres backup (the tooling `docs/backup-and-restore.md` specifies)

[`docs/backup-and-restore.md`](../../docs/backup-and-restore.md) states the backup strategy;
this directory is the part of it that runs. The doc's daily-full-logical-dump layer
is implemented here as a `pg_dump -Fc` artifact that is **verified before it is
declared a success**, encrypted at rest, handed to a pluggable offsite hook, and
pruned on a retention window.

**An untested backup is a belief** (the doc's first line, and this tool's design
rule). A dump that `pg_dump` exited 0 on is not a backup until `pg_restore --list`
reads it - and neither is a ciphertext until it decrypts back into an archive that
lists. Both checks are exit-code gates here, never warnings.

## Files

- `backup.sh` - a POSIX shell runner. Dumps, verifies, encrypts, verifies again,
  runs the offsite hook, prunes. Documented exit-code contract below.

## How to run

```sh
export DATABASE_URL='postgres://postgres:dev@localhost:5432/apikita'   # local dev only
export BACKUP_ENCRYPTION_KEY='<key from your secret manager>'          # REQUIRED
export OFFSITE_CMD='rclone copy --sftp-host ... "$1" remote:apikita-backups'
sh tools/backup/backup.sh
```

### Environment

| Variable | Default | Meaning |
| -------- | ------- | ------- |
| `DATABASE_URL` | - | The DSN to dump. Falls back to the documented local dev DSN when unset (see below). |
| `BACKUP_ENCRYPTION_KEY` | - | **Required.** Passphrase for the artifact. Absent means exit 6 - never a plaintext dump. |
| `BACKUP_DIR` | `<repo>/tmp/backups` | Destination directory for artifacts (gitignored). |
| `BACKUP_RETENTION_DAYS` | `30` | Prune artifacts older than this. The doc's daily-dump retention. |
| `OFFSITE_CMD` | - | Offsite hook. Unset means exit 1: the backup is local only. |
| `BACKUP_DEFAULT_DATABASE_URL` | `postgres://postgres:dev@localhost:5432/apikita` | The documented fallback DSN (`docs/local-development.md`). |
| `ARTIFACT_PREFIX` | `apikita` | Artifact filename prefix. Also scopes retention pruning. |
| `COMPOSE_FILE` / `CONTAINER_SERVICE` | `<repo>/docker-compose.yml` / `postgres` | The container fallback for `pg_dump`. |

Artifacts are named `<prefix>-<UTC timestamp>.dump.enc`, e.g.
`apikita-20260925T210104Z.dump.enc`. The name carries the time; the mtime is what
retention uses.

### DATABASE_URL is optional on purpose

Unset, empty, or whitespace-only `DATABASE_URL` falls back to the **local
development** DSN with a loud warning on stderr. `tools/reconcile/reconcile.sh`
exits 2 in that case, deliberately: it is a gate, and a gate that silently picks a
database is a gate that can pass against the wrong one. A backup tool has the
opposite need - it runs from cron with no environment, and the alternative to the
documented default is no backup at all. **The default is local dev; pointing this
at production is an explicit `DATABASE_URL`.**

A `DATABASE_URL` that is set but is not a `postgres://`/`postgresql://` DSN is exit
2: an obviously wrong value is never silently replaced by the default.

## Exit codes

Codes 3 and 4 keep their meanings from `tools/reconcile/reconcile.sh`, so an
operator learns one table. The rest are this tool's own.

| Code | Meaning |
| ---- | ------- |
| `0`  | Complete - dump written, verified, encrypted, verified again, offsite copy made, retention applied. |
| `1`  | Local backup written and verified, but **no offsite copy** (`OFFSITE_CMD` unset). The doc is explicit that a backup on the same host as the database is not a backup, so this is not a pass. |
| `2`  | `DATABASE_URL` is set but is not a `postgres://` / `postgresql://` DSN. |
| `3`  | `pg_dump`/`pg_restore` not available: not on PATH, and no usable container fallback. |
| `4`  | `pg_dump` ran but failed (connection, permissions, disk). |
| `5`  | **The dump is empty or `pg_restore --list` cannot read it.** It is not a backup. |
| `6`  | `BACKUP_ENCRYPTION_KEY` is not set - refused before writing anything. |
| `7`  | Encryption failed, or the encrypted artifact did not decrypt into a listable archive. |
| `8`  | The offsite hook failed. The local artifact is kept; the offsite copy is missing. |
| `9`  | Destination not writable, or retention pruning failed. |

Codes `1`, `5`, `6`, `7` and `8` are distinct from each other deliberately: "no
offsite copy" is an alert with a different fix from "the dump is unreadable", and
collapsing them into a generic failure is how the wrong thing gets investigated.

## What is verified, and how

| Step | Check | Failure |
| ---- | ----- | ------- |
| Dump is non-empty | byte count of the raw dump | exit 5 |
| Dump is a real archive | `pg_restore --list` reads it | exit 5 |
| Artifact decrypts | the ciphertext decrypts back | exit 7 |
| Decrypted artifact is a real archive | `pg_restore --list` reads it | exit 7 |
| Offsite copy exists | the hook exits 0 | exit 8 |
| Retention ran | `find` scan succeeded | exit 9 |

The plaintext dump is written to `$TMPDIR` with `umask 077`, verified, encrypted,
and deleted - it never lands in `BACKUP_DIR`, and it is removed by an `EXIT` trap
even on failure. The artifact is published with `mv`, so a reader never sees a
half-written file under the final name.

This is the answer to the doc's monitoring line "a backup job that reports success
while producing a tiny file is the classic silent failure": a truncated or
empty dump cannot reach exit 0, because the listing step - not the size alone - is
the gate. The tool still prints the artifact's size and SHA-256 so the size alert
the doc asks for can be built on top.

## Encryption

**Real symmetric encryption, not obfuscation:** `openssl enc -aes-256-cbc -pbkdf2
-iter 200000 -salt`, i.e. AES-256 in CBC mode with a key derived from
`BACKUP_ENCRYPTION_KEY` by PBKDF2-HMAC-SHA256 at 200,000 iterations and a random
per-artifact salt (OpenSSL 3.2.4, the version on this host). The passphrase is
passed as `-pass env:BACKUP_ENCRYPTION_KEY`, so it never appears in the process
list or in shell history.

**The limitation, stated plainly: AES-256-CBC is not authenticated encryption.**
OpenSSL's `enc` does not support AEAD ciphers (`aes-256-gcm` fails with "AEAD
ciphers not supported"), so there is no MAC over the ciphertext. The consequences,
honestly:

- **Confidentiality holds.** Without the key the artifact is not readable - which
  is the threat the doc names ("an unencrypted dump in object storage is a breach
  waiting for a misconfiguration").
- **Tamper detection does not.** A party who can modify the offsite object could
  flip ciphertext bits and the tool would not detect it before `pg_restore` does
  something undefined. The mitigations are outside this script and are the
  provider's job: object versioning/immutability, and treating the offsite bucket
  as write-only from the backup host. The SHA-256 printed at write time is for
  the size/anomaly alerting in the doc, not for authentication.
- `age` is **not** installed on this host. `gpg` **is** (`/usr/bin/gpg`), and
  `gpg --symmetric` would give authenticated encryption. `openssl` was chosen
  because it is the standard tool here, is present, and covers the encryption
  *and* the SHA-256 the size/anomaly alert wants in a single dependency - not
  because GPG is unavailable. Moving to an authenticated cipher is a deliberate,
  worthwhile change, not a limitation of this host.

Restoring, once the key is at hand:

```sh
openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 -pass env:BACKUP_ENCRYPTION_KEY \
  -in apikita-20260925T210104Z.dump.enc > backup.dump
pg_restore --clean --if-exists -d scratch backup.dump   # docs/backup-and-restore.md
```

## Offsite: a hook, and a decision nobody has made

`OFFSITE_CMD` is invoked as `sh -c "$OFFSITE_CMD" apikita-offsite <artifact path>`,
so the artifact is the hook's `$1`. Exit 0 means copied; anything else is exit 8
and an alert. Any provider fits without touching this script:

```sh
OFFSITE_CMD='rclone copy "$1" b2:apikita-backups/$(date -u +%Y-%m-%d)/'
OFFSITE_CMD='aws s3 cp "$1" s3://apikita-backups/ --sse AES256'
OFFSITE_CMD='rsync -a "$1" backup-host:/srv/apikita-backups/'
```

**No provider is configured, and this tool does not pretend otherwise.** Unset
`OFFSITE_CMD` is exit 1 with the reason on stderr. `docs/backup-and-restore.md`
still carries an unchecked open item - **"Offsite storage provider and encryption
key custody"** - and nothing in this directory resolves it. That doc also asks for
a **weekly** offsite copy with 90-day retention while this script runs the hook on
**every** run; daily-or-better satisfies "weekly" but the schedule is a decision
for whoever wires the cron job, not a default to invent here.

### Still a human decision

| Decision | Why it is not this script's to make |
| -------- | ----------------------------------- |
| **Which offsite provider** | Costs money, needs an account and credentials, and the doc's "a different provider from the one running Postgres" is a policy choice. |
| **Key custody** | Where `BACKUP_ENCRYPTION_KEY` lives (secret manager, hardware, split custody), who can read it, and the rotation/escrow plan. **Losing it makes every artifact useless** - the doc says so. |
| **Scheduling** | Nothing runs this yet: no cron, no compose service, no CI job. `tools/reconcile/` has the same gap. |
| **PITR (RPO 15 minutes)** | **This tool does not meet the doc's RPO.** A daily logical dump loses up to 24 hours; the 15-minute RPO comes from continuous WAL archiving / managed PITR, which the doc's open item assigns to `wal-g` or a managed offering. Nothing here implements it. |
| **The restore drill** | The doc's quarterly rehearsal, with the ledger-reconciliation gate, is a separate procedure (`tools/reconcile/reconcile.sh` covers the ledger half on a live DB). This tool proves an artifact is *readable*, not that it *restores*. |
| **PocketBase** | The doc ranks it below Postgres but it is not backed up here at all. |

## Verified status

Verified against the **local** PostgreSQL 16 dev stack (compose service
`postgres`, database `apikita`), on 2026-09-25, with every exit code exercised for
real - no fabricated output:

| # | Scenario | Result |
| - | -------- | ------ |
| a | Real backup of the local dev DB | exit 0; 113,861-byte dump -> 113,888-byte artifact, 89 TOC entries; `pg_restore --list` on the decrypted artifact lists the archive |
| a2 | Same, `OFFSITE_CMD` unset | exit 1; the local artifact is still written and verified |
| b | `DATABASE_URL` at an unreachable host | exit 4, `pg_dump: error: connection to server ... failed` |
| c | `BACKUP_ENCRYPTION_KEY` unset | exit 6; **zero files** written to the destination |
| d | A 40-day-old artifact with `BACKUP_RETENTION_DAYS=30` | pruned, exit 0; the newest artifact and unrelated files untouched |

**Mutation-checked.** Each guard was removed and the failure re-run, to prove the
guard - not something incidental - is what stops it:

- dump guards removed -> exit 0 with a **32-byte artifact wrapping an empty dump**
  that `pg_restore` rejects (`input file is too short`): the silent success the
  verification step exists to prevent.
- key check removed **and** a plaintext fallback added -> exit 0 with a 113,861-byte
  artifact whose first five bytes are `PGDMP`: **the database was written to disk
  unencrypted, and reported as a success.** This is the silent downgrade exit 6
  prevents.
- age filter removed -> a 5-day-old artifact, well inside the 30-day window, was
  deleted.
- newest-artifact guard removed -> the newest artifact was deleted too, leaving an
  empty backup directory.

### Not verified

- **Any real offsite provider.** The hook was exercised with `cp` and `true` only.
  No credentials exist here, and none were invented.
- **A restore.** No scratch database was provisioned, so the doc's ledger
  reconciliation gate has not been run against a restored artifact. `pg_restore
  --list` proves the archive is intact and readable; it does not prove the data
  restores.
- **Encryption against a hostile adversary.** No key-rotation, wrong-key, or
  tamper case beyond a wrong passphrase (which fails closed at exit 7).
- **A schedule.** Nothing invokes this automatically.

## Notes for whoever wires this up

- **This host has no `psql`/`pg_dump`.** The script prefers a host client when one
  exists, and otherwise execs into the `postgres` compose service - which is how
  every result above was produced. When the container fallback is used,
  `DATABASE_URL` is resolved **inside the compose network**: `localhost:5432`
  (the documented default) and the service name `postgres` both reach the local
  database, but a host-only address (e.g. `host.docker.internal`) may not.
- The container fallback also needs Docker and the compose file. On a host without
  Docker, install the PostgreSQL client - the tool then uses it directly.
- `docs/backup-and-restore.md` is the contract and was **not edited**. Its open
  item "Offsite storage provider and encryption key custody" should be ticked only
  when a provider is chosen and the key has a custodian - not because this script
  exists.
