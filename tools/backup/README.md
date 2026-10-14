# SQLite backup (the tooling `docs/backup-and-restore.md` specifies)

[`docs/backup-and-restore.md`](../../docs/backup-and-restore.md) states the backup strategy;
this directory is the part of it that runs. The doc's daily-full-logical-dump layer
is implemented here as a SQLite snapshot that is **verified before it is declared a
success**, encrypted at rest, handed to a pluggable offsite hook, and pruned on a
retention window.

**An untested backup is a belief** (the doc's first line, and this tool's design
rule). A file the copy step exited 0 on is not a backup until it is read back -
the 16-byte SQLite header **and** `PRAGMA integrity_check` - and neither is a
ciphertext until it decrypts back into a database that passes the same two checks.
Both are exit-code gates here, never warnings.

## Files

- `backup.sh` - a POSIX shell runner. Copies, verifies, encrypts, verifies again,
  runs the offsite hook, prunes. Documented exit-code contract below.

## How to run

```sh
export DATABASE_URL='sqlite://data/server.db'                    # local dev only
export BACKUP_ENCRYPTION_KEY='<key from your secret manager>'    # REQUIRED

# Either shape works. Both are tested by tools/backup-check/.
export OFFSITE_CMD='rclone copy "$1" remote:apikita-backups'                 # inline
export OFFSITE_CMD='sh /usr/local/bin/upload-offsite.sh'                     # a script

sh tools/backup/backup.sh
```

**Both hook shapes are supported, and that is now enforced.** The artifact is appended
to `OFFSITE_CMD` before it runs, so an INLINE command sees it as `$1` and a SCRIPT
receives it as its own first argument. This was **broken for the script shape**: the
hook was invoked as `sh -c "$OFFSITE_CMD" apikita-offsite "$ARTIFACT"`, where the
`name` argument becomes `$0` *inside* the command - so a script got **nothing**,
while the backup printed *"offsite hook succeeded"* and exited 0. The one outcome this
tool exists to prevent, a backup that never left the machine, was reachable through
the ordinary hook. `tools/backup-check/check.sh` now runs both shapes in CI.

### Environment

| Variable | Default | Meaning |
| -------- | ------- | ------- |
| `DATABASE_URL` | - | The SQLite database to copy. Falls back to the local development database when unset (see below). |
| `BACKUP_ENCRYPTION_KEY` | - | **Required.** Passphrase for the artifact. Absent means exit 6 - never a plaintext dump. |
| `BACKUP_DIR` | `<repo>/tmp/backups` | Destination directory for artifacts (gitignored). **Not `BACKUP_DEST`.** |
| `BACKUP_RETENTION_DAYS` | `30` | Prune artifacts older than this. The doc's daily-dump retention. |
| `OFFSITE_CMD` | - | Offsite hook. Unset means exit 1: the backup is local only. |
| `BACKUP_DEFAULT_DATABASE_URL` | `sqlite://<repo>/server/data/server.db` | The fallback when `DATABASE_URL` is unset. An **absolute** path on purpose - see below. |
| `ARTIFACT_PREFIX` | `apikita` | Artifact filename prefix. Also scopes retention pruning. |

Artifacts are named `<prefix>-<UTC timestamp>-<pid>.dump.enc`, e.g.
`apikita-20260926T033819Z-4123.dump.enc`. The name carries the time and the
process id of the run that made it: the timestamp has one-second resolution, so
two runs completing in the same second would otherwise overwrite each other's
artifact while both reported success. The mtime is what retention uses.

### DATABASE_URL is optional on purpose

Unset, empty, or whitespace-only `DATABASE_URL` falls back to the **local
development** database with a loud warning on stderr.
`tools/reconcile/reconcile.sh` exits 2 in that case, deliberately: it is a gate,
and a gate that silently picks a database is a gate that can pass against the
wrong one. A backup tool has the opposite need - it runs from cron with no
environment, and the alternative to the documented default is no backup at all.
**The default is local dev; pointing this at production is an explicit
`DATABASE_URL`.**

A `DATABASE_URL` that is set but is not a `sqlite://` URL is exit 2: an obviously
wrong value is never silently replaced by the default. A leftover `postgres://`
URL is refused **by name** with a `case`, not prefix-stripped into a plausible
filename.

**The fallback is an absolute path.** `sqlite://data/server.db` is relative to the
**server's** working directory (`docs/local-development.md`: "relative to
`server/`"). A cron job resolving that against its own cwd would back up nothing,
or worse, create an empty database and "succeed". The script therefore resolves a
relative path against `server/`, and the built-in default is absolute.

## Exit codes

Codes 3 and 4 keep their meanings from `tools/reconcile/reconcile.sh`, so an
operator learns one table. The rest are this tool's own.

| Code | Meaning |
| ---- | ------- |
| `0`  | Complete - copy taken, verified, encrypted, verified again, offsite copy made, retention applied. |
| `1`  | Local backup written and verified, but **no offsite copy** (`OFFSITE_CMD` unset). The doc is explicit that a backup on the same host as the database is not a backup, so this is not a pass. |
| `2`  | `DATABASE_URL` is set but is not a `sqlite://` URL, or names an in-memory database. |
| `3`  | The `sqlite3` CLI is not available: not installed, not on `PATH`, and no container fallback (there is none - see below). |
| `4`  | The `.backup` ran but failed (unreadable file, disk full, lock held), or the database file does not exist. |
| `5`  | **The copy is empty, or is not a readable, intact SQLite database.** It is not a backup. |
| `6`  | `BACKUP_ENCRYPTION_KEY` is not set - refused before writing anything. |
| `7`  | Encryption failed, or the encrypted artifact did not decrypt into an intact database. |
| `8`  | The offsite hook failed. The local artifact is kept; the offsite copy is missing. |
| `9`  | Destination not writable, or retention pruning failed. |

Codes `1`, `5`, `6`, `7` and `8` are distinct from each other deliberately: "no
offsite copy" is an alert with a different fix from "the copy is unreadable", and
collapsing them into a generic failure is how the wrong thing gets investigated.

**Changed from the PostgreSQL version:** `2` is about a `sqlite://` URL rather
than a `postgres://` DSN; `3` is about the `sqlite3` CLI rather than
`pg_dump`/`pg_restore`; `4` also covers a missing database file; and `5`/`7` are
about the SQLite header and `integrity_check` rather than `pg_restore --list`.

## The copy: `.backup`, not `cp`, and not `VACUUM INTO`

**`.backup` was chosen, and the reason is the whole point of a backup.**

The CLI implements `.backup` with SQLite's **online backup API**. It is
transactionally consistent against a database another process may be writing, and
crucially it **reads through the WAL**: it sees committed transactions that are
still sitting in the `-wal` file. A raw `cp` of a live WAL database does not - it
copies the main file, which may be missing committed frames, and either drops the
`-wal` or leaves a mismatched pair. **A backup that silently omits committed
transactions is the worst outcome this tool can produce**, so `cp` is never used.

**`VACUUM INTO` is the other correct primitive, and it was rejected for a
different reason.** It is also consistent, but it *rewrites the whole database* -
it is a compaction, not a snapshot - and it refuses to run if the output path
already exists, which turns an idempotent retry into a special case. `.backup`
is the primitive that matches what this script needs: a faithful snapshot of a
live database, byte-for-byte at the page level, with the WAL folded in.

The copy is written to `$TMPDIR` with `umask 077`, verified, encrypted, and
deleted - it never lands in `BACKUP_DIR`, and it is removed by an `EXIT` trap even
on failure. The artifact is published with `mv`, so a reader never sees a
half-written file under the final name.

## What is verified, and how

| Step | Check | Failure |
| ---- | ----- | ------- |
| Copy is non-empty | byte count of the raw copy | exit 5 |
| Copy is a real database | the 16-byte `SQLite format 3` header | exit 5 |
| Copy is intact | `PRAGMA integrity_check` = `ok` over the whole file | exit 5 |
| Artifact decrypts | the ciphertext decrypts back | exit 7 |
| Decrypted artifact is a real, intact database | header + `integrity_check` | exit 7 |
| Offsite copy exists | the hook exits 0 | exit 8 |
| Retention ran | `find` scan succeeded | exit 9 |

This is the answer to the doc's monitoring line "a backup job that reports success
while producing a tiny file is the classic silent failure": a truncated or empty
copy cannot reach exit 0, because the header and `integrity_check` - not the size
alone - are the gate. The tool still prints the artifact's size and SHA-256 so the
size alert the doc asks for can be built on top.

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
  flip ciphertext bits and the tool would not detect it. The mitigations are
  outside this script and are the provider's job: object versioning/immutability,
  and treating the offsite bucket as write-only from the backup host. The SHA-256
  printed at write time is for the size/anomaly alerting in the doc, not for
  authentication.
- `age` is **not** installed on this host. `gpg` **is**, and `gpg --symmetric`
  would give authenticated encryption. `openssl` was chosen because it is the
  standard tool here, is present, and covers the encryption *and* the SHA-256 the
  size/anomaly alert wants in a single dependency - not because GPG is
  unavailable. Moving to an authenticated cipher is a deliberate, worthwhile
  change, not a limitation of this host.

Restoring, once the key is at hand:

```sh
BACKUP_ENCRYPTION_KEY='<key>' openssl enc -d -aes-256-cbc -pbkdf2 -iter 200000 \
  -pass env:BACKUP_ENCRYPTION_KEY -in apikita-20260926T033819Z-4123.dump.enc > backup.db
# then, to prove it restores (docs/backup-and-restore.md):
sh tools/drill/drill.sh --target apikita_drill_scratch.db --dump backup.db
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
| **Which offsite provider** | Costs money, needs an account and credentials, and the doc's "a different provider from the one running the database" is a policy choice. |
| **Key custody** | Where `BACKUP_ENCRYPTION_KEY` lives (secret manager, hardware, split custody), who can read it, and the rotation/escrow plan. **Losing it makes every artifact useless** - the doc says so. |
| **Scheduling** | Nothing runs this yet: no cron, no compose service, no CI job. `tools/reconcile/` has the same gap. |
| **PITR (RPO 15 minutes)** | **This tool does not meet the doc's RPO.** A daily snapshot loses up to 24 hours; the 15-minute RPO comes from continuous WAL archiving / managed PITR, which the doc's open item assigns to `wal-g` or a managed offering. Nothing here implements it. SQLite makes this *easier* than Postgres did - the `-wal` file can be archived - but nothing here does it. |
| **The restore drill** | The doc's quarterly rehearsal, with the ledger-reconciliation gate, is `tools/drill/drill.sh`. This tool proves an artifact is *readable and intact*, not that it *restores*; the drill is what proves that. |

## Verified status

Verified on 2026-09-26 against scratch SQLite databases built from
`server/migrations/20260925000000_initial_schema.sql` (fixtures under
`.agents/sqlite-port/`). Every exit code below was produced for real; no output
is fabricated.

| # | Scenario | Result |
| - | -------- | ------ |
| a | Real backup of the scratch database, `OFFSITE_CMD` unset | **exit 1**; 237,568-byte copy -> 237,600-byte artifact, `integrity_check=ok`, artifact written and verified |
| a2 | Same, with an offsite hook | **exit 0**; the hook received the artifact, the offsite copy exists |
| b | `BACKUP_ENCRYPTION_KEY` unset | **exit 6**; **zero files** written to the destination |
| c | `DATABASE_URL` = a leftover `postgres://` URL | **exit 2**, refused by name |
| c2 | `DATABASE_URL` = `mysql://...` | **exit 2** |
| c3 | `DATABASE_URL` = `sqlite::memory:` | **exit 2** |
| d | `DATABASE_URL` naming a missing file | **exit 4** |
| e | `sqlite3` off `PATH` | **exit 3** |
| f | `BACKUP_DIR` cannot be created (its parent is a file) | **exit 9** |
| g | `BACKUP_RETENTION_DAYS=abc` | warned, defaulted to 30, backup still completed |
| h | Two 40-day-old artifacts with `BACKUP_RETENTION_DAYS=30` | both pruned; the fresh artifact and an unrelated `.txt` untouched |
| i | A 90-day-old artifact with `BACKUP_RETENTION_DAYS=1` | pruned (a fresh one was written), and the fresh one survived |

### Mutation-checked

Each guard was removed and the bad behaviour reproduced, to prove the guard - not
something incidental - is what stops it. Every mutant ran as a **copy under
`.agents/`**; the shipped `backup.sh` sha256 was
`81476b16d9d6b10270046869ba0a805570fea5198fbe494dfbd67604292a27fc` **before and
after**, so "restored" is verified, not claimed.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| The verify-decrypt step uses a **different key** from the encrypt step | exit 0, artifact published | **exit 7**, `bad decrypt` from OpenSSL, **0 artifacts published**. This is what the decrypt-back verification catches: without it the run would exit 0 with an artifact the real key cannot open - a silently useless backup. |
| The copy is replaced by `head -c 4000` of the database (a truncated file) | exit 0, `integrity_check=ok` | **exit 5**, `integrity_check` = `<no output>`, **0 artifacts published** |

### Not verified

- **Any real offsite provider.** The hook was exercised with `cp` only. No
  credentials exist here, and none were invented.
- **A restore of the artifact.** `tools/drill/drill.sh` is the tool that proves
  that; this README's rows are about producing and verifying an artifact.
- **Encryption against a hostile adversary.** No key-rotation or tamper case
  beyond a wrong passphrase (which fails closed at exit 7).
- **`.backup` against a *concurrent writer* under load.** The WAL behaviour is
  the documented property of the backup API and was exercised against a WAL-mode
  database, but not under sustained concurrent write load.
- **A schedule.** Nothing invokes this automatically.

## Notes for whoever wires this up

- **There is no container fallback any more, and one would be dead code.**
  `docker compose config --services` returns `nginx` and `scheduler` - there is no
  database service to `exec` into, so a `docker compose exec postgres ...` path
  could only ever fail at runtime. The script needs the `sqlite3` CLI on `PATH`
  and the database file readable. A missing `sqlite3` is exit 3, loudly.
- `docs/backup-and-restore.md` is the contract and was **not edited**. Its open
  item "Offsite storage provider and encryption key custody" should be ticked only
  when a provider is chosen and the key has a custodian - not because this script
  exists.
