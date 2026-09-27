# Backup & Restore Drill

The procedure for backing up, and **proving** the backup works. Referenced in
[`deployment.md`](deployment.md) and [`website/02-data-model.md`](website/02-data-model.md); this is the
procedure itself.

**An untested backup is a belief.** The only backup that counts is one that has been
restored and verified.

## What must be backed up

| Data | Where | Loss impact |
| --- | --- | --- |
| **The SQLite database file** (ledger, wallets, keys, usage) | Northflank persistent volume | **Total loss of funds data — unrecoverable** |
| **PocketBase** (identity, passwords, emails) | Northflank | Loss of logins; customers cannot sign in |
| Relay nginx config | VPS | Rebuildable by hand, but annoying |
| Config (routing/pricing) | Git | Low — versioned already |

**Ranked by consequence: the database file first.** Losing PocketBase breaks logins;
losing the database file destroys the record of what customers are owed. They are
not equivalent.

## Backup strategy

| Layer | Method | Frequency | Retention |
| --- | --- | --- | --- |
| **Continuous** | SQLite WAL archiving / PITR (unchosen — see Open items) | continuous | 7-30 days |
| **Daily** | Full copy of the database file (the `sqlite3 .backup` / `VACUUM INTO` form, or an encrypted archive of it) | daily | 30 days |
| **Pre-migration** | Manual snapshot | before every schema change | until migration verified |
| **Weekly** | Offsite copy | weekly | 90 days |

**PITR is worth the cost.** A daily snapshot means losing a day of transactions — a
day of top-ups and billing. Point-in-time recovery reduces that to minutes, and the
ledger is the business.

> **The port changed the mechanism, not the requirement.** The database is a single
> file now, so a backup is a file copy rather than a `pg_dump`/PITR pair. Two
> consequences worth naming: (1) the database is on a **persistent volume**, so a
> backup that does not leave that volume is not a backup; and (2) copying a live
> SQLite file without `VACUUM INTO`/`.backup` can capture a torn WAL — use the
> online-backup form. The shipped tooling does exactly this — `.backup` (the online
> backup API), header + `integrity_check` verification, then the reconcile gate.

**The pre-migration snapshot is not optional.** It is the cheapest rollback that
exists; see [`deployment.md`](deployment.md).

## Offsite is not a detail

**A backup on the same host as the database is not a backup.** It protects against
an accidental `DROP TABLE` and nothing else — not a lost volume, a platform
incident, or a provider failure.

Store copies in a different provider from the one running the database. Object
storage is cheap and sufficient.

## Encryption

**Encrypt backups at rest.** A database copy contains emails and every balance. An
unencrypted dump in object storage is a breach waiting for a misconfiguration.

- Encrypt before upload, not relying solely on provider-side encryption.
- Store the key **separately** from the backup. A key alongside the data encrypts
  nothing.
- Document who holds the key, because losing it makes the backups useless.

## The restore drill

**Run this before launch, then quarterly.** It is a rehearsal, not a formality.

### Procedure

```bash
# 1. Restore the backup to a SCRATCH FILE - never over the live database file.
#    Unpack the encrypted archive; you are copying one file.
cp backup/server.db scratch/restore.db

# 2. Verify the numbers that matter. The shipped tooling does this with
#    tools/drill/drill.sh, which restores into a scratch database and runs
#    tools/reconcile/reconcile.sql as its verdict.
sqlite3 scratch/restore.db "
  SELECT count(*) AS accounts FROM accounts;
  SELECT sum(balance_idr) AS total_owed FROM wallets;
  SELECT sum(delta_idr)  AS ledger_sum FROM ledger;"

# 3. THE check: wallets must equal the ledger (tools/reconcile/reconcile.sql)
sqlite3 scratch/restore.db "
  SELECT COALESCE(w.account_id, l.account_id) AS account_id,
         COALESCE(w.balance_idr, 'NO WALLET ROW') AS balance_idr,
         COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
  FROM wallets w
  FULL OUTER JOIN ledger l ON l.account_id = w.account_id
  GROUP BY w.account_id, w.balance_idr
  HAVING COALESCE(w.balance_idr, -1) <> COALESCE(SUM(l.delta_idr), 0);"

# 4. Spot-check a known account against the live system
# 5. Verify row counts are plausible vs the live database
# 6. Delete the scratch file
```

### Pass criteria

| Check | Passes when |
| --- | --- |
| Restore completes | No errors |
| **Ledger reconciliation** | **Returns zero rows** |
| Row counts | Within expected range of live |
| Spot-check | A known account's balance matches |
| Keys present | `key_hash` rows exist (auth would still work) |

**Zero rows from the reconciliation query is the gate.** If it returns rows, either
the restore is corrupt or the live data has a bug — and both need resolving before
you could trust a recovery.

### Record the drill

| Field | Why |
| --- | --- |
| Date | Prove it happens |
| Who ran it | Accountability |
| Backup age at restore | Did you restore something current? |
| Time to restore | **Your real RTO** |
| Result | Pass, or what failed |

**Measure the restore time.** It is the answer to "how long would we be down?" and
it is always longer than expected.

## Recovery scenarios

| Scenario | Response | Expected loss |
| --- | --- | --- |
| Bad migration | Restore pre-migration snapshot | Minutes |
| Accidental data corruption | PITR to just before the change (needs the continuous layer, still unchosen) | Minutes |
| Volume lost | Restore latest backup to a new volume | Up to the backup interval |
| **Platform loss** | Restore offsite to a new host | Up to the offsite interval |
| Ransomware / compromise | **Do not restore without understanding entry** | Unknown |

**The last row matters.** Restoring an unexamined compromised backup re-introduces
the vulnerability. Fix the entry point first.

## What could not be recovered

| Lost | Consequence |
| --- | --- |
| Prompts/completions | **Nothing — we never stored them** |
| In-flight requests | Nothing; they fail and the customer retries |
| Unsent Telegram notifications | Cosmetic |
| Reviews | Recoverable from the daily backup |

**The first row is a benefit of the privacy stance.** Because prompts were never
stored, a total database loss cannot leak a single prompt.

## Monitoring

| Check | Alert if |
| --- | --- |
| Backup job succeeded | It failed |
| Backup age | Older than the expected interval |
| Backup size | Anomalously small — a truncated dump looks successful |
| Offsite copy present | Missing |

**A backup job that reports success while producing a tiny file is the classic
silent failure.** Alert on size, not just exit code.

## Open items

- [x] Backup tooling: **managed PITR**, else a full dump (see `decisions.md`).
      Implemented as [`tools/backup/backup.sh`](../tools/backup/README.md): a full dump, verified
      before success, encrypted, retention-pruned, with an offsite hook and a documented exit-code
      contract. **This is the daily-snapshot half only** — it does **not** meet the 15-minute RPO;
      the continuous layer is still unchosen.
      > **PORTED (was the sharpest open item in this document).** `tools/backup/backup.sh` and
      > `tools/drill/drill.sh` now implement the **SQLite** procedure: a `.backup` copy, header +
      > `integrity_check` on both the artifact and the **restored** file, and the reconcile gate.
      > They refuse a `postgres://` `DATABASE_URL` by name (exit 2). Verified end-to-end against a
      > scratch database built from the migration: a balanced source restores and reconciles green; a
      > drifted source **fails** (exit 1); a truncated or zero-length artifact is caught by the header
      > check (exit 6); and a zero-length *source* is refused, because SQLite opens an empty file as a
      > fresh database and the drill would otherwise pass on a restore of nothing. Was tracked
      > in [`tools/backup/README.md`](../tools/backup/README.md) and
      > [`tools/drill/README.md`](../tools/drill/README.md).
- [ ] Offsite storage provider and encryption key custody.
      **A human decision**, deliberately not invented. The tool refuses to run without
      `BACKUP_ENCRYPTION_KEY` (a silent plaintext downgrade is the failure that matters) and takes
      an `OFFSITE_CMD` hook so any provider can be plugged in. Encryption is AES-256-CBC via
      OpenSSL, which is **not authenticated** — pair it with object versioning or a write-only bucket.
- [x] RTO **4 hours**, RPO **15 minutes** — verify against the drill.
      The drill is now executable: [`tools/drill/drill.sh`](../tools/drill/README.md) restores into a
      scratch database, reuses the reconciliation query as its verdict, spot-checks a balance,
      measures the restore time and writes a log. Measured on the local dev database:
      **~1–6 s** to restore a ~126 KB dump — orders of magnitude under the 4-hour RTO, but that is a
      **dev-sized** database, measured against **PostgreSQL** before the port; the production number
      is unmeasured until the drill runs there, and the drill itself still needs the port above.
- [x] Where the drill log lives.
      `tools/drill/drill.sh` writes `.agents/drill-logs/drill-<UTC-timestamp>-<target>.log` (that
      directory is gitignored, since a log names row counts and account ids). Each log carries all
      five "Record the drill" fields: `date_utc`, `run_by`, `backup_age`, `restore_ms`, `result`,
      plus the dump sha256/size, TOC count, the row-count table, the spot-check pair and the drift
      verdict. Override with `--log-dir`/`DRILL_LOG_DIR` when the retention answer moves.
- [ ] Whether PocketBase gets its own tested restore procedure.
      **Still open.** Neither tool covers PocketBase: `backup.sh` backs up only the money database and
      the drill restores only that, so identity data has no tested restore path today.
- [ ] **Port `tools/backup` and `tools/drill` to the SQLite file.** Until then the tooling cannot
      touch the shipped database — see the note under Backup tooling above.
