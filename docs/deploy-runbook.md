# Deploy & Rollback Runbook

**This is the operator's ordered procedure.** [`deployment.md`](deployment.md) explains *why* the
rules are what they are and is the authority on migrations and compatibility;
[`backup-and-restore.md`](backup-and-restore.md) is the authority on backups. This file is the one you
follow with a terminal open, and it is written so that the steps need no judgement calls — the
judgement is in the two documents above, and every step here cites the section that justifies it.

**Read this once before you need it.** A runbook you read for the first time during an incident is a
runbook that gets skipped.

## What you need before the first deploy

Fill these in once. Every command below refers to them.

| Variable | What it is | Where it must NOT be |
| --- | --- | --- |
| `$DEPLOY_HOST` | the backend host, ssh-reachable | — |
| `$DATA_DIR` | the **persistent volume** holding `server/data/server.db` | not on the container's writable layer |
| `$BACKUP_ENCRYPTION_KEY` | 32+ random bytes, base64 | **never** in the repo, never on the backup host |
| `$OFFSITE_CMD` | a command taking the artifact path as `$1` | see [offsite](#the-offsite-hook) |
| `$ALERT_WEBHOOK` | where alerts are delivered | — |

Two of these gate the launch checklist directly and cannot be produced here: the **persistent volume**
and the **alert channel**. Everything else in this runbook works without them until the step that
needs them.

There is **no staging environment** — `deployment.md`'s open items record that as an undecided
question, and this runbook does not answer it. Until it is answered, **production is the only
environment**, which is what makes the rollback section below load-bearing rather than theoretical.

---

## 1. Before you deploy anything

```bash
# The migration list, and nothing applied.
cd server && sqlx migrate info

# Local gates. These are the same ones CI runs; if one fails, stop here.
sh tools/compose-check/check.sh
sh tools/reconcile-check/check.sh
sh tools/rollback-check/check.sh
```

**Stop if any gate fails.** A deploy is not the place to discover a broken invariant.

### Migrations are forward-only

`deployment.md`'s **R1** is the rule, and this is the step where it matters: a migration must be
**additive and backward compatible**, because the old server keeps running against the new schema
until step 4 completes. Never write a destructive migration that the *previous* binary cannot tolerate.

---

## 2. Take a backup, and prove it restores

Do this **before** migrating. A backup you have not restored is a belief.

```bash
BACKUP_ENCRYPTION_KEY="$BACKUP_ENCRYPTION_KEY" \
OFFSITE_CMD="$OFFSITE_CMD" \
  sh tools/backup/backup.sh
```

The tool has named exit codes and they all mean stop: `5` the copy is corrupt, `6` no encryption key,
`7` the ciphertext did not decrypt back, `8` the offsite hook failed, `9` pruning failed. **`8` is the
one people argue with** — an unset `OFFSITE_CMD` is exit `1`, deliberately, because an offsite copy is
not optional.

Then verify the artifact actually restores, into a scratch file:

```bash
sh tools/drill/drill.sh          # restores into scratch and runs reconcile.sql as the verdict
```

**Record the measured restore time.** That number is the real RTO, and the launch checklist has a
separate box for it precisely because it is a measurement rather than a plan.

---

## 3. Run migrations

```bash
cd server && sqlx migrate run
```

Migrations run **before** the new server and **after** the backup, in their own process. That order is
`deployment.md`'s pipeline step 2, and it exists so a failed migration leaves nothing deployed.

**If this step fails: stop.** Nothing has been deployed. Fix the migration and start again from step 2.
This is the cheapest failure in the whole procedure, and it is cheap because the backup is already
taken.

---

## 4. Deploy the server

```bash
# build, push, then point the service at the new image
docker compose build scheduler
docker compose up -d
```

Then confirm it is actually serving before touching anything else:

```bash
curl -fsS "https://$DEPLOY_HOST/health"    # must be 200 and say healthy
```

### The scheduler is a deploy step, not a leftover

`docker compose up -d` starts **two** services: `nginx` and `scheduler`. The `scheduler` is what runs
`run_wired_jobs` — retention, reconcile, hold-sweep, credit-expiry, and both alert jobs. It is the
only thing that keeps the retention windows in `data-retention.md` true.

**If the scheduler is not running, none of that is happening.** Every retention claim reads as
satisfied because the jobs are written and tested; they are simply not executing. Confirm it:

```bash
docker compose ps scheduler                       # must be Up
docker compose logs --tail 50 scheduler           # the nightly loop's last run
docker compose run --rm scheduler once            # run every job once, right now
```

`once` is the operator's verb for "prove the schedule works without waiting for 02:00". The other
one-shot verbs are `retention`, `reconcile`, `hold-sweep`, `credit-expiry`, `alerts`, `alert-probes`.

---

## 5. Deploy the frontend

Server **before** frontend — `deployment.md`'s **R2**. The new frontend may call an endpoint that only
the new server serves.

```bash
cd website && PUBLIC_API_BASE_URL= npm run build
# then publish website/dist to Cloudflare Pages
```

**If this step fails:** the new API is live with the old UI, which is usually fine because the API
tolerates the previous frontend. Roll the frontend back on its own and leave the server.

---

## 6. Smoke test

```bash
curl -fsS "https://$DEPLOY_HOST/health"
curl -fsS -o /dev/null -w '%{http_code}\n' "https://$DEPLOY_HOST/api/me"   # 401 when signed out
```

Then sign in once through the browser and confirm the dashboard shows a balance. **A deploy that
returns 200 everywhere but cannot sign in is not done** — the session cookie is the one thing that
crosses both services, and it is what the relay's same-origin configuration exists for.

---

## 7. Rollback

### Decide first, then act

| Symptom | Roll back | Why |
| --- | --- | --- |
| Migration failed | nothing | nothing deployed (step 3) |
| Server unhealthy, **before** the frontend deploy | the server image | the old server runs on the new schema (R1) |
| Server unhealthy, **after** the frontend deploy | the server image, then the frontend | the old UI tolerates the old API |
| Server healthy, UI broken | the frontend only | the API is fine; leave it up |
| **Money is wrong** — reconcile reports drift | **stop, do not roll back** | see below |

### Rolling back the server

Point the service back at the previous image tag and redeploy. The schema **stays** — that is what R1
buys you, and it is why a rollback here is a one-line change rather than a data operation.

### Rolling back the frontend

Republish the previous build. `website/dist` is mounted read-only rather than baked into an image
deliberately (`docker-compose.yml`), so the previous build is the previous artifact.

### Never roll a migration back by hand

`deployment.md`: *"Write a new forward migration that undoes it. Down-migrations on live data are how
you lose rows."* If a migration must be undone, that is a **new deploy**, not a rollback.

### If reconcile reports drift: stop

```bash
sh tools/reconcile/reconcile.sh
```

Exit `1` is drift, `5` is a stranded hold. **Do not roll back to fix drift.** A rollback changes which
binary is running and does not touch the ledger; the drift will still be there, and the deploy that
followed it will now be harder to reason about. Freeze changes, and read
[`backup-and-restore.md`](backup-and-restore.md)'s recovery scenarios. This is the one case where the
correct action is **not** to return to the previous state.

---

## The steps that need a human

These are on the launch checklist and **cannot be completed by following this file**. Listed here so
the runbook does not read as complete.

| Item | What is actually needed |
| --- | --- |
| Persistent volume | a provisioned volume; the database must not live on the container layer |
| Edge relay deployed | the host, with `.docker/nginx/relay.conf` (`proxy_buffering off` on `/events` is already written and guarded) |
| Certificates | issued to the public hostname, with automatic renewal — nothing in the repo can verify this ahead of a deploy |
| Health checks | the probes are written; they need a deployed stack to run against |
| Failover | relay down → backend serves. Cannot be simulated meaningfully; it is a real network condition |
| Alerts | a delivery channel. The checks and thresholds are done; the *channel* is the deploy decision |
| Offsite backups | `$OFFSITE_CMD`, and the encryption key held **separately** from the backups |
| Restore drill | run against **production data**, not a synthetic database |
| Maintenance scheduler | `docker compose up -d scheduler` on a host with a persistent volume |
| Abuse-report contact | a monitored mailbox — the Terms cannot be published without one |

---

## What this runbook does not decide

- **Staging.** No staging environment is specified anywhere in this repository, and `deployment.md`
  records that as open. Every command here assumes production.
- **The contracting entity.** Personal or PT is an owner decision; it changes tax and dispute posture,
  not this procedure.
- **Legal review.** Deferred by decision until turnover approaches the PP 55/2022 ceiling
  (`launch-checklist.md`'s Gate 0 note).
- **Cloudflare Pages deployment.** The build command is here; the project configuration is not in this
  repository and is not reproduced here rather than guessed at.
