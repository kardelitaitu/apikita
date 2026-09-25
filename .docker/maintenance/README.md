# Maintenance scheduler

The service that finally runs the jobs this repo kept promising to run.

## Why this exists

Three maintenance jobs were written and then **nothing ever ran them**. That is
verifiable, not assumed: a grep across every CI file, compose file, Dockerfile and
env file returned **zero references** to `benchmark`, `ip-purge` or `hold-sweep`,
and `docker-compose.yml` defined exactly three services (postgres, pocketbase,
nginx). The docs admitted the gap in writing:

- `docs/observability.md:161` — "neither has a scheduler behind it yet"
- `docs/ip-tracking.md:203` — "[ ] The purge is a binary with no scheduler behind
  it yet; it needs to be added to whatever runs the nightly backup and
  reconciliation jobs."

A retention promise with no job behind it is a sentence in a document, not a
behaviour in production. This service is the job behind the sentence.

## Cadence

**Nightly at `SCHEDULE_HOUR_UTC` (default `3`, i.e. 03:00 UTC).** Set the env var
to move it. The entrypoint computes the next occurrence at startup, sleeps until
then, runs the wired jobs, and repeats. A failed night does **not** kill the
scheduler — it logs the failure and retries at the next scheduled time, because a
one-off failure must not silently become "the job never runs again".

## What genuinely RUNS here

| Job | What it protects | How it runs |
| --- | --- | --- |
| **retention** | The `docs/ip-tracking.md` retention promise: `key_ip_seen` hashes live **7 days**, `key_ip_daily` counts live **90**. Compliance, not nicety — the surviving hashes are only "unlinkable after the salt is gone" if they are also *gone*. | Real SQL over the `psql` client: `DELETE FROM key_ip_seen WHERE day <= CURRENT_DATE - 7` and `DELETE FROM key_ip_daily WHERE day <= CURRENT_DATE - 90`. |
| **reconcile** | The Gate 2 money-correctness invariant `wallets.balance_idr = SUM(ledger.delta_idr)`. `tools/reconcile/reconcile.sh` is the single best guard against silently wrong money. | `sh tools/reconcile/reconcile.sh` against the stack database. Exit code preserved verbatim. |

### Why the retention job is SQL and not the binary

`server/src/bin/ip-purge.rs` is a thin wrapper: it opens a sqlx pool, calls
`ip_tracking::purge_expired`, and logs two counts. The entire body of that
function is the two `DELETE` statements above (`server/src/ip_tracking.rs:298-316`,
against `SEEN_RETENTION_DAYS = 7` / `DAILY_RETENTION_DAYS = 90` in the same file).
This image is built `FROM postgres:16`, so it has the `psql` client those
statements need and nothing else. The job here applies **exactly that SQL** and
reports **the same two counts** the binary logs.

The cutoff is `<=`, not `<`, matching the Rust comment at
`server/src/ip_tracking.rs:281-288`: 7 and 90 are days **retained**, so the days
kept are `today - (N-1) ..= today` and every day at or before `today - N` is
deleted. `<` would quietly retain 8 days of hashes against a document that says 7
— a broken promise, not a rounding detail.

## What is deliberately NOT wired, and why

Stated **out loud in the startup log** (see `entrypoint.sh`, the `banner`
function) and repeated here. A scheduler that looks like it works and does nothing
is the same failure class as a money check that returns 0 while the money is
wrong — that is the exact bug this service exists to catch, so it would be absurd
for the service itself to commit it.

| Job | Status | Why it cannot run here |
| --- | --- | --- |
| **`ip-purge`** | **NOT WIRED** | `server/src/bin/ip-purge.rs` is a **Rust binary**. `server/` has no Dockerfile and `docker-compose.yml` has no Rust build stage, so no image in this compose file can contain it. The **retention window is still enforced** (see above); it is the *binary* that does not run here. |
| **`hold-sweep`** | **NOT WIRED** | `server/src/bin/hold-sweep.rs`, same reason. **Nothing sweeps stranded reservation holds in this topology.** This one matters most: a stranded hold is **invisible money** — the ledger still balances and reconciliation returns *nothing* — which is exactly why a detector with a 900s bound was written. The gap is loud, not silent. |
| **`benchmark`** | **NOT WIRED** | `server/src/bin/benchmark.rs`. Not a maintenance promise; it is a measurement tool and has no business running on a timer. |

### The interim answer for the two Rust jobs: run them on the host

Until a server image exists, run them on the **host**, on the same nightly cadence:

```sh
DATABASE_URL='postgres://postgres:dev@localhost:5432/apikita' \
  cargo run --manifest-path server/Cargo.toml --bin ip-purge

DATABASE_URL='postgres://postgres:dev@localhost:5432/apikita' \
  cargo run --manifest-path server/Cargo.toml --bin hold-sweep
```

When a server image does exist, the honest change is to add a second service that
runs `ip-purge` and `hold-sweep` from it — and to delete the `NOT WIRED` lines
from the banner in the same commit, so the log never claims a wiring the compose
file does not have.

## Usage

```sh
# the nightly loop (what the compose service runs)
docker compose up -d scheduler
docker logs -f apikita-scheduler

# run the wired jobs once, right now, and exit with their status
docker compose run --rm scheduler once

# one job at a time
docker compose run --rm scheduler retention
docker compose run --rm scheduler reconcile
```

The one-shot verbs exist so the nightly work is testable and so CI can gate on it
without waiting for 03:00.

## Exit codes

`once` and the individual verbs exit **non-zero if any wired job failed**, so this
service can gate CI the same way `tools/reconcile/reconcile.sh` does.

| Code | Meaning |
| --- | --- |
| `0` | Every wired job succeeded. |
| `1` | At least one wired job failed — retention SQL error, or reconciliation drift / unreachable DB. |
| `2` | Bad invocation, or `SCHEDULE_HOUR_UTC` is not an hour 0-23. |

`reconcile.sh`'s own codes are **preserved and reported verbatim**, never masked
by `|| true`: `1` = drift detected, `2` = `DATABASE_URL` not set, `3` = `psql`
missing, `4` = `psql` ran but failed (connection, permissions, SQL error). The
entrypoint translates those to its own exit `1` but prints the original code, so
an unreachable database is never reported as a pass.

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `DATABASE_URL` | `postgres://postgres:<POSTGRES_PASSWORD>@postgres:5432/apikita` | The retention job's connection. Reaches the `postgres` service over the compose network by service name. |
| `RECONCILE_DATABASE_URL` | falls back to `DATABASE_URL` | What the reconciliation job is pointed at. Separate so a bad DSN disarms reconciliation alone and does **not** take the retention sweep down with it. |
| `SCHEDULE_HOUR_UTC` | `3` | UTC hour 0-23 of the nightly run. |

## Mounts

Both are read-only; this service never writes to the repo.

- `./.docker/maintenance/entrypoint.sh` → `/usr/local/bin/maintenance-entrypoint.sh`
- `./tools/reconcile` → `/usr/local/share/reconcile` (`reconcile.sh` +
  `reconcile.sql`, unmodified and unowned by this job)

## No healthcheck, on purpose

This container serves nothing and listens on nothing, so any probe would be
theatre. Its liveness signal is the nightly job log line, and a failed job is a
non-zero exit — not a silent success. `restart: unless-stopped` matches every
other service in the compose file.
