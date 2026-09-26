# Maintenance scheduler

The service that finally runs the jobs this repo kept promising to run.

## Why this exists

Three maintenance jobs were written and then **nothing ever ran them**. That is
verifiable, not assumed: a grep across every CI file, compose file, Dockerfile and
env file returned **zero references** to `benchmark`, `ip-purge` or `hold-sweep`.
The docs admitted the gap in writing:

- `docs/observability.md:161` - "neither has a scheduler behind it yet"
- `docs/ip-tracking.md:203` - "[ ] The purge is a binary with no scheduler behind
  it yet; it needs to be added to whatever runs the nightly backup and
  reconciliation jobs."

A retention promise with no job behind it is a sentence in a document, not a
behaviour in production. This service is the job behind the sentence.

## Cadence

**Nightly at `SCHEDULE_HOUR_UTC` (default `3`, i.e. 03:00 UTC).** Set the env var
to move it. The entrypoint computes the next occurrence at startup, sleeps until
then, runs the wired jobs, and repeats. A failed night does **not** kill the
scheduler - it logs the failure and retries at the next scheduled time, because a
one-off failure must not silently become "the job never runs again".

## The database is a FILE, not a service

The stack no longer runs PostgreSQL. The money database is **embedded SQLite** - a
file the API opens, named by `DATABASE_URL` in the form `sqlite://data/server.db`
(or `sqlite:///abs/path.db`). There is no host, no port and no DSN, and
`docker compose config --services` returns **`nginx`, `scheduler`** - the
`postgres` service and the `pgdata` volume are gone.

Every job here therefore uses the **`sqlite3` CLI** against that file. A
`docker compose exec postgres psql` path would be dead code that fails at runtime,
and there is none.

## What genuinely RUNS here

| Job | What it protects | How it runs |
| --- | --- | --- |
| **retention** | The `docs/ip-tracking.md` retention promise: `key_ip_seen` hashes live **7 days**, `key_ip_daily` counts live **90**. Compliance, not nicety - the surviving hashes are only "unlinkable after the salt is gone" if they are also *gone*. | Real SQL through the `sqlite3` CLI: `DELETE FROM key_ip_seen WHERE day <= date('now','-7 days')` and `DELETE FROM key_ip_daily WHERE day <= date('now','-90 days')`. |
| **reconcile** | The Gate 2 money-correctness invariant `wallets.balance_idr = SUM(ledger.delta_idr)`. `tools/reconcile/reconcile.sh` is the single best guard against silently wrong money. | `sh tools/reconcile/reconcile.sh` against the database file. Exit code preserved verbatim. |

### Why the retention job is SQL and not the binary

`server/src/bin/ip-purge.rs` is a thin wrapper: it opens a sqlx pool, calls
`ip_tracking::purge_expired`, and logs two counts. The entire body of that
function is the two `DELETE` statements above (`server/src/ip_tracking.rs:298-316`,
against `SEEN_RETENTION_DAYS = 7` / `DAILY_RETENTION_DAYS = 90` in the same file).
The job here applies **exactly that SQL** and reports **the same two counts** the
binary logs.

The cutoff is `<=`, not `<`, matching the Rust comment at
`server/src/ip_tracking.rs:281-288`: 7 and 90 are days **retained**, so the days
kept are `today - (N-1) ..= today` and every day at or before `today - N` is
deleted. `<` would quietly retain 8 days of hashes against a document that says 7
- a broken promise, not a rounding detail.

**Porting note:** the `day` column is RFC3339-**date** TEXT (`'YYYY-MM-DD'`, per
the schema's `GLOB '????-??-??'` check), not a Postgres `date`. The cutoff is
therefore `date('now', '-N days')` - a string comparison of ISO dates, which the
GLOB check guarantees is well-formed. `date('now')` is **UTC**, matching the Rust
binary. The deleted count comes from `SELECT changes()` in the same round trip,
replacing the Postgres data-modifying CTE.

## What is deliberately NOT wired, and why

Stated **out loud in the startup log** (see `entrypoint.sh`, the `banner`
function) and repeated here. A scheduler that looks like it works and does nothing
is the same failure class as a money check that returns 0 while the money is
wrong - that is the exact bug this service exists to catch, so it would be absurd
for the service itself to commit it.

| Job | Status | Why it cannot run here |
| --- | --- | --- |
| **`ip-purge`** | **NOT WIRED** | `server/src/bin/ip-purge.rs` is a **Rust binary**. `server/` has no Dockerfile and `docker-compose.yml` has no Rust build stage, so no image in this compose file can contain it. The **retention window is still enforced** (see above); it is the *binary* that does not run here. |
| **`hold-sweep`** | **NOT WIRED** | `server/src/bin/hold-sweep.rs`, same reason. **Nothing sweeps stranded reservation holds in this topology.** This one matters most: a stranded hold is **invisible money** - the ledger still balances and reconciliation returns *nothing* - which is exactly why a detector with a 900s bound was written. The gap is loud, not silent. |
| **`benchmark`** | **NOT WIRED** | `server/src/bin/benchmark.rs`. Not a maintenance promise; it is a measurement tool and has no business running on a timer. |

### The interim answer for the two Rust jobs: run them on the host

Until a server image exists, run them on the **host**, on the same nightly cadence:

```sh
DATABASE_URL='sqlite://data/server.db' \
  cargo run --manifest-path server/Cargo.toml --bin ip-purge

DATABASE_URL='sqlite://data/server.db' \
  cargo run --manifest-path server/Cargo.toml --bin hold-sweep
```

When a server image does exist, the honest change is to add a second service that
runs `ip-purge` and `hold-sweep` from it - and to delete the `NOT WIRED` lines
from the banner in the same commit, so the log never claims a wiring the compose
file does not have.

## NOT YET PORTED: the compose service that runs this script

**This is open work, and it is outside this directory.** The entrypoint here is
ported; the `scheduler` service in `docker-compose.yml` is **not**. It still is:

```yaml
scheduler:
  image: postgres:16                      # kept for its psql client
  environment:
    DATABASE_URL: postgres://postgres:${POSTGRES_PASSWORD:-dev}@postgres:5432/apikita
    RECONCILE_DATABASE_URL: postgres://postgres:${POSTGRES_PASSWORD:-dev}@postgres:5432/apikita
  volumes:
    - ./.docker/maintenance/entrypoint.sh:/usr/local/bin/maintenance-entrypoint.sh:ro
    - ./tools/reconcile:/usr/local/share/reconcile:ro
```

**Two things must change there, and neither is this directory's to change:**

1. **The image must stop being `postgres:16` and must gain a `sqlite3` binary.**
   There is no Postgres any more, and `psql` is not the client these jobs use.
2. **Both DSNs must become `sqlite://` URLs, and the API's data directory must be
   mounted.** The database is a file on the host; a container that cannot see it
   cannot run either job.

**Until that happens the nightly run FAILS LOUDLY, and that is the point.** The
entrypoint refuses a non-SQLite `DATABASE_URL` **by name** rather than guessing a
filename, refuses to run a database job when `sqlite3` is absent, and refuses a
missing database file - each a reported failure, never a clean sheet against a
database it never opened. The banner says all of this on every start.

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
| `1` | At least one wired job failed - retention SQL error, a missing/unusable database file, or reconciliation drift / unreachable database. |
| `2` | Bad invocation, or `SCHEDULE_HOUR_UTC` is not an hour 0-23. |

`reconcile.sh`'s own codes are **preserved and reported verbatim**, never masked
by `|| true`: `1` = drift detected, `2` = `DATABASE_URL` not set or not a SQLite
URL, `3` = `sqlite3` missing, `4` = `sqlite3` ran but failed, `5` = stranded
hold, `6` = no such database file. The entrypoint translates those to its own exit
`1` but prints the original code, so an unreachable database is never reported as
a pass.

**Changed from the PostgreSQL version:** the banner and the `job reconcile` start
line used to read `3=no psql 4=psql failed`; they now read
`3=no sqlite3 4=sqlite3 failed 5=stranded hold 6=no such database file`, which is
what `reconcile.sh` actually returns. The exit codes of this entrypoint are
unchanged (`0`/`1`/`2`).

## Environment

| Variable | Default | Purpose |
| --- | --- | --- |
| `DATABASE_URL` | *unset* | The retention job's database. A `sqlite://` URL; a relative path resolves against `${APP_DIR:-/srv/apikita}/server/`. A non-SQLite value is refused by name. |
| `RECONCILE_DATABASE_URL` | falls back to `DATABASE_URL` | What the reconciliation job is pointed at. Separate so a bad URL disarms reconciliation alone and does **not** take the retention sweep down with it. |
| `SCHEDULE_HOUR_UTC` | `3` | UTC hour 0-23 of the nightly run. |
| `APP_DIR` | `/srv/apikita` | Where the repo is mounted in the container; used to resolve a **relative** `sqlite://` path. |
| `RECONCILE_SH` | `/usr/local/share/reconcile/reconcile.sh` | Overridable so the job can be exercised outside a container. |

## Mounts

Both are read-only; this service never writes to the repo.

- `./.docker/maintenance/entrypoint.sh` -> `/usr/local/bin/maintenance-entrypoint.sh`
- `./tools/reconcile` -> `/usr/local/share/reconcile` (`reconcile.sh` +
  `reconcile.sql`, unmodified and unowned by this job)

**A third mount is required once the service is ported**: the directory holding the
SQLite database (the API's data directory). Without it the container cannot see the
file and every database job fails - loudly, by design, but it will never succeed.

## No healthcheck, on purpose

This container serves nothing and listens on nothing, so any probe would be
theatre. Its liveness signal is the nightly job log line, and a failed job is a
non-zero exit - not a silent success. `restart: unless-stopped` matches every
other service in the compose file.

## Verification status

Verified by execution on 2026-09-26 against a scratch SQLite database built from
`server/migrations/20260925000000_initial_schema.sql` (fixture under
`.agents/sqlite-port/`), running the entrypoint directly with
`DATABASE_URL`/`RECONCILE_SH` overridden. Real output, no fabricated results.

| # | Scenario | Result |
| - | -------- | ------ |
| a | `retention` against a fixture seeded with 3 `key_ip_seen` rows (2 expired) and 2 `key_ip_daily` rows (1 expired) | **exit 0**; `key_ip_seen deleted=2 (retain 7d), key_ip_daily deleted=1 (retain 90d)`; the surviving rows are the in-window ones |
| b | `reconcile` on a clean database | **exit 0**, `job reconcile: OK - 0 drifting accounts` |
| c | `reconcile` with drift injected (wallet 1009 vs ledger 1000) | **exit 1**, `job reconcile: FAILED - exit 1. This is NOT a success...` - the code is reported, never masked |
| d | `once` on a clean database | **exit 0**; both jobs OK |
| e | `once` with a stale `postgres://` `DATABASE_URL` | **exit 1**; retention refuses the URL by name, reconcile is unaffected by the retention DSN |
| f | `DATABASE_URL` unset | **exit 1**, "DATABASE_URL is not set (refusing to report a sweep that did not run)" |
| g | `DATABASE_URL` naming a missing file | **exit 1**, "no such database file ... (nothing was swept)" |
| h | `sqlite3` off `PATH` | **exit 1** and the job says so; the database is untouched |
| i | unknown verb / `SCHEDULE_HOUR_UTC=99` / `=abc` | **exit 2** |

### Mutation-checked

The mutant ran as a **copy under `.agents/`**; the shipped `entrypoint.sh` sha256
was `229cfe3cd5059eab3578c6cfdb65ba6476cd9275056ba2c05870d9a6c8096d80` **before and
after**.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| `sqlite3` removed from `PATH` | retention runs, 2 + 1 rows deleted | **exit 1** with "sqlite3 is not installed in this image"; **all 3 rows still present** - a client that is not there is a reported failure, not a sweep that "deleted 0" |
| The `DELETE` targets a non-existent column | retention runs clean | **exit 1**, "the key_ip_seen delete did not run"; `-bail` makes the SQL error non-zero instead of an empty result read as zero |

### Not verified

- **The compose service itself.** `scheduler` is still `postgres:16` with postgres
  DSNs (see the open item above). The entrypoint was exercised directly, not
  through `docker compose run`.
- **A container with `sqlite3` installed.** No image here has one yet, which is
  precisely why the missing-client path is exercised above.
- **A full nightly cycle at 03:00 UTC.** The `schedule` loop was not waited out;
  the one-shot verbs were used, which is what they exist for.
- **The two Rust binaries.** `ip-purge` and `hold-sweep` are NOT WIRED, by
  construction.
