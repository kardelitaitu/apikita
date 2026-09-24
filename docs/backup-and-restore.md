# Backup & Restore Drill

The procedure for backing up, and **proving** the backup works. Referenced in
[`deployment.md`](deployment.md) and [`website/02-data-model.md`](website/02-data-model.md); this is the
procedure itself.

**An untested backup is a belief.** The only backup that counts is one that has been
restored and verified.

## What must be backed up

| Data | Where | Loss impact |
| --- | --- | --- |
| **PostgreSQL** (ledger, wallets, keys, usage) | Northflank | **Total loss of funds data — unrecoverable** |
| **PocketBase** (identity, passwords, emails) | Northflank | Loss of logins; customers cannot sign in |
| Relay nginx config | VPS | Rebuildable by hand, but annoying |
| Config (routing/pricing) | Git | Low — versioned already |

**Ranked by consequence: Postgres first.** Losing PocketBase breaks logins; losing
Postgres destroys the record of what customers are owed. They are not equivalent.

## Backup strategy

| Layer | Method | Frequency | Retention |
| --- | --- | --- | --- |
| **Continuous** | Postgres WAL archiving / PITR | continuous | 7-30 days |
| **Daily** | Full logical dump (`pg_dump`) | daily | 30 days |
| **Pre-migration** | Manual snapshot | before every schema change | until migration verified |
| **Weekly** | Offsite copy | weekly | 90 days |

**PITR is worth the cost.** A logical dump every 24 hours means losing a day of
transactions — a day of top-ups and billing. Point-in-time recovery reduces that to
minutes, and the ledger is the business.

**The pre-migration snapshot is not optional.** It is the cheapest rollback that
exists; see [`deployment.md`](deployment.md).

## Offsite is not a detail

**A backup on the same host as the database is not a backup.** It protects against
an accidental `DROP TABLE` and nothing else — not a lost volume, a platform
incident, or a provider failure.

Store copies in a different provider from the one running Postgres. Object storage
is cheap and sufficient.

## Encryption

**Encrypt backups at rest.** A dump contains emails and every balance. An
unencrypted dump in object storage is a breach waiting for a misconfiguration.

- Encrypt before upload, not relying solely on provider-side encryption.
- Store the key **separately** from the backup. A key alongside the data encrypts
  nothing.
- Document who holds the key, because losing it makes the backups useless.

## The restore drill

**Run this before launch, then quarterly.** It is a rehearsal, not a formality.

### Procedure

```bash
# 1. Provision a scratch Postgres (never restore over production)
# 2. Restore the most recent backup
pg_restore --clean --if-exists -d scratch backup.dump

# 3. Verify the numbers that matter
psql -d scratch -c \"
  SELECT count(*) AS accounts FROM accounts;
  SELECT sum(balance_idr) AS total_owed FROM wallets;
  SELECT sum(delta_idr)  AS ledger_sum FROM ledger;\"

# 4. THE check: wallets must equal the ledger
psql -d scratch -c \"
  SELECT w.account_id, w.balance_idr, COALESCE(SUM(l.delta_idr),0) AS ledger_sum
  FROM wallets w
  LEFT JOIN ledger l ON l.account_id = w.account_id
  GROUP BY w.account_id, w.balance_idr
  HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr),0);\"

# 5. Spot-check a known account against the live system
# 6. Verify row counts are plausible vs the live database
# 7. Tear down the scratch instance
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
| Accidental data corruption | PITR to just before the change | Minutes |
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

- [x] Backup tooling: **managed PITR**, else `pg_dump` + `wal-g` (see `decisions.md`).
- [ ] Offsite storage provider and encryption key custody.
- [x] RTO **4 hours**, RPO **15 minutes** — verify against the drill.
- [ ] Where the drill log lives.
- [ ] Whether PocketBase gets its own tested restore procedure.
