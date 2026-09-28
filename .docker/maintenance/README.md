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
| **`ip-purge`** | **BINARY NOT WIRED; WORK IS** | `server/src/bin/ip-purge.rs` is a **Rust binary**. A server image **does** exist (`server/Dockerfile`), but it ships only `apikita-server` and `migrate`, so no image in *this* compose file contains it. The **retention window IS enforced** — `run_retention` applies the same two `DELETE`s through sqlite3. It is the *binary* that does not run here. |
| **`usage-purge`** | **BINARY NOT WIRED; WORK IS** | `server/src/bin/usage-purge.rs`, same shape. `run_retention` applies all three of its sweeps — `usage_events` (90d), `usage_daily` (730d), expired/revoked `sessions` (30d) — so `docs/data-retention.md` is enforced here. |
| **`hold-sweep`** | **WIRED — REPORT-ONLY** | `server/src/bin/hold-sweep.rs` still is not shipped, but `run_hold_sweep` applies its **detector** inline through `sqlite3`: the same predicate as the binary, the same 900s bound. It counts, names the accounts and refs, and exits non-zero. It **never moves money** — the binary's `--release` is the deliberate operator action. This matters most because a stranded hold is **invisible money**: the ledger still balances and reconciliation returns *nothing*. |
| **`benchmark`** | **NOT WIRED** | `server/src/bin/benchmark.rs`. Not a maintenance promise; it is a measurement tool and has no business running on a timer. |
| **`alerts`** | **WIRED - DATABASE CHECKS** | `run_alert_checks` runs `tools/alert/check-alerts.sh` nightly, so the three SQL-answerable alerts (`ledger_drift`, `balance_negative`, `stranded_hold`) are evaluated on a schedule rather than only existing. Its exit code is preserved (1=fired 2=config 3=no sqlite3 4=failed 5=undelivered 6=unknown): a check that did not run is not a check that passed. **A breach with no channel configured still exits non-zero**, because an alert nobody receives is worse than no alerting: it is believed. |
| **`alert-probes`** | **WIRED - HTTP CHECKS** | `run_alert_probes` runs `tools/alert/probe.sh`, so `relay_down` is evaluated nightly against the relay **by service name**. `api_down` runs only when `PROBE_API_URL` is set: probe.sh defaults to `127.0.0.1:8080` (probe.sh:49), which **inside this container is its own loopback**, so an unconfigured run fired a **false api_down** - measured, and it is why the checks are now SELECTED explicitly rather than left to probe.sh's defaults. The other four need `PROBE_LOG_FILE` or `PROBE_OPERATOR_COOKIE` and are named as skipped. |

### The interim answer for the Rust jobs: run them on the host

The server image ships only `apikita-server` and `migrate`, so run these on the
**host**, on the same nightly cadence:

```sh
DATABASE_URL='sqlite://data/server.db' \
  cargo run --manifest-path server/Cargo.toml --bin ip-purge

DATABASE_URL='sqlite://data/server.db' \
  cargo run --manifest-path server/Cargo.toml --bin usage-purge

# Detection now runs in-container too (run_hold_sweep, report-only). This host
# form is what actually CREDITS the holds back, which stays a deliberate action:
DATABASE_URL='sqlite://data/server.db' \
  cargo run --manifest-path server/Cargo.toml --bin hold-sweep -- --release
```

**All three are now covered in-container.** `ip-purge` and `usage-purge` encode SQL
the entrypoint applies itself, so their *retention promises* are kept here even
though the binaries do not run. `hold-sweep` is covered too, as its **report-only**
half: `run_hold_sweep` detects, names and exits non-zero. What stays on the host is
the **money-moving** half (`--release`), and that is deliberate — silently crediting
a hold is the same invisible-money anti-pattern the sweep exists to catch.

The honest future change is to ship the three binaries in the server image and add
a service that runs them - then delete the `NOT WIRED` lines from the banner in
the **same commit**, so the log never claims a wiring the compose file lacks.

## The compose service that runs this script - PORTED

The `scheduler` service in `docker-compose.yml` **is ported to SQLite** and its
jobs run for real. It was not always: it ran `image: postgres:16` for its `psql`
client, carried two `postgres://` DSNs, and mounted neither a `sqlite3` binary nor
the API's data directory - so it could never succeed at either job, on any host.
What it is now:

```yaml
scheduler:
  build: { context: ., dockerfile: .docker/maintenance/Dockerfile }  # sqlite3, no psql
  working_dir: /srv/apikita/server        # so reconcile.sh resolves the relative DSN
  environment:
    DATABASE_URL: sqlite://data/server.db
    RECONCILE_DATABASE_URL: sqlite://data/server.db   # separate on purpose, see above
    APP_DIR: /srv/apikita                 # pinned, not left to the entrypoint default
    SCHEDULE_HOUR_UTC: ${SCHEDULE_HOUR_UTC:-3}
  volumes:
    - ./.docker/maintenance/entrypoint.sh:/usr/local/bin/maintenance-entrypoint.sh:ro
    - ./tools/reconcile:/usr/local/share/reconcile:ro
    - ./server/data:/srv/apikita/server/data          # the database file
```

Four things are load-bearing here, and each one was a real failure before it was
fixed. Do not "simplify" any of them away:

1. **The image is `.docker/maintenance/Dockerfile`, not `postgres:16`.** That file
   exists for exactly one reason: to put the `sqlite3` CLI on `PATH`. It is
   `alpine` (the same base as the `nginx` service above, and `sqlite` is an apk
   package) pinned to an explicit tag. There is **no Postgres client in it**, and
   putting `postgres:16` back would restore the bug this port fixed: `psql` is not
   the client these jobs use and cannot open a SQLite file at all.
2. **Both DSNs are `sqlite://data/server.db`.** They stay **two separate
   variables** - that is the design in the Environment table above, not an
   oversight: a bad DSN disarms the reconciliation job alone instead of taking the
   retention sweep down with it. Collapsing them into one removes that property.
3. **The third mount, `./server/data` -> `/srv/apikita/server/data`.** Without it
   the container cannot see the database at all - the one problem that made this
   service permanently unable to succeed rather than merely wrong.
4. **`working_dir: /srv/apikita/server`, and the mount above is _not_ `:ro`.**

   The entrypoint resolves a relative `sqlite://` path itself, against
   `${APP_DIR}/server/`. `reconcile.sh` does **not**: it gets the raw DSN and
   resolves a relative path against its own CWD (`reconcile.sh:93`). Run from the
   image default `/`, reconciliation looks for `/data/server.db`, reports exit
   `6` "no such database file", and the Gate 2 money check silently never runs
   against the real database while the retention sweep passes. `working_dir`
   points both jobs at the same file.

   The mount is writable because the **retention job is a `DELETE`**
   (`key_ip_seen`/`key_ip_daily`, `run_retention` below) and a `DELETE` against a
   read-only mount fails on every host - the retention promise would never be
   kept. The two repo mounts stay `:ro`: what this service must never write is the
   repo's source, not the data directory. `reconcile.sh` still opens the database
   `-readonly` by itself; the write privilege is the retention sweep's alone.

**What the entrypoint still refuses, loudly, and should.** A non-SQLite
`DATABASE_URL` is refused **by name** rather than guessed into a filename, a
database job does not run when `sqlite3` is absent, and a missing database file is
a reported failure - never a clean sheet against a database it never opened. The
banner says all of this on every start.

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

**Verified against a real run.** `docker compose up -d scheduler` reaches
`Up` (not `Restarting`) and logs its next run; `run retention` deleted an
expired row and reported all five counts; `run reconcile` reported drift as
exit 1. What a full 24h cycle still does not prove is listed under "Not verified".

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

Three, all bind mounts of the host checkout. **The first two are read-only; the
third cannot be** - see the reasoning in the ported-service section above.

- `./.docker/maintenance/entrypoint.sh` -> `/usr/local/bin/maintenance-entrypoint.sh`
- `./tools/reconcile` -> `/usr/local/share/reconcile` (`reconcile.sh` +
  `reconcile.sql`, unmodified and unowned by this job)
- `./server/data` -> `/srv/apikita/server/data` - the API's data directory, so
  `sqlite://data/server.db` resolves to the same file the API writes. Writable,
  because the retention job is a `DELETE` and a read-only mount makes that `DELETE`
  fail on every host; `reconcile.sh` opens the file `-readonly` on its own.

The directory is gitignored (`server/data/`, `.gitignore:30-34`) and may not exist
on a fresh checkout. An empty source directory is harmless and honest: the jobs
report "no such database file" rather than a clean sheet.

## No healthcheck, on purpose

This container serves nothing and listens on nothing, so any probe would be
theatre. Its liveness signal is the nightly job log line, and a failed job is a
non-zero exit - not a silent success. `restart: unless-stopped` matches every
other service in the compose file.

## Verification status

Verified by execution on 2026-09-26 against a scratch SQLite database built from
`server/migrations/20260925000000_initial_schema.sql` (fixture under
`.agents/sqlite-port/`), first running the entrypoint directly with
`DATABASE_URL`/`RECONCILE_SH` overridden, then — after the compose service was
ported — through the shipped `docker compose` service and its built image (second
table below). Real output, no fabricated results. Read **Not verified** at the end
before trusting any of it.

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

### The ported compose service, verified through `docker compose` (2026-09-26)

The rows above exercised the entrypoint **directly**. These exercise the shipped
`docker-compose.yml` service and the built image, which is what the port actually
changed. Same fixture (`server/data/server.db`, built from
`server/migrations/20260925000000_initial_schema.sql` plus a balanced
account/wallet/ledger and two seeded retention rows).

| # | Command | Result |
| - | ------- | ------ |
| j | `docker compose config --services` | `nginx`, `scheduler` - still exactly two, no database service |
| k | `docker compose build scheduler` | image built from `.docker/maintenance/Dockerfile` |
| l | `docker compose run --rm --entrypoint sh scheduler -c 'command -v sqlite3; command -v psql || echo "psql absent (correct)"; sqlite3 --version'` | `/usr/bin/sqlite3`, `psql absent (correct)`, `3.48.0` |
| m | `docker compose run --rm --entrypoint sh scheduler -c 'ls -la /srv/apikita/server/data/ && test -f /srv/apikita/server/data/server.db && echo DATABASE VISIBLE'` | the file plus its `-wal`/`-shm` sidecars, then `DATABASE VISIBLE` - the third mount is real |
| n | `docker compose run --rm scheduler once`, clean database | **exit 0**; `job retention: OK - key_ip_seen deleted=1 (retain 7d), key_ip_daily deleted=1 (retain 90d)` and `job reconcile: OK - 0 drifting accounts` |
| o | `docker compose run --rm scheduler once`, drift injected (wallet 1009 vs ledger 1000) | **exit 1**; `reconcile: DRIFT DETECTED - 1 account(s)...` naming `acc-0001\|1009\|1000`, then `job reconcile: FAILED - exit 1`. Restored afterwards, and `once` returned to **exit 0** |

A check that cannot fail is not a check, which is why row `o` exists next to `n`:
the same command, same image, same mount, differing only in the data, changes the
exit code and names the drifting account.

### Mutation-checked

The mutant ran as a **copy under `.agents/`**; the shipped `entrypoint.sh` sha256
was `229cfe3cd5059eab3578c6cfdb65ba6476cd9275056ba2c05870d9a6c8096d80` **before and
after**.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| `sqlite3` removed from `PATH` | retention runs, 2 + 1 rows deleted | **exit 1** with "sqlite3 is not installed in this image"; **all 3 rows still present** - a client that is not there is a reported failure, not a sweep that "deleted 0" |
| The `DELETE` targets a non-existent column | retention runs clean | **exit 1**, "the key_ip_seen delete did not run"; `-bail` makes the SQL error non-zero instead of an empty result read as zero |

### Not verified

- **A full nightly cycle was never waited out** (a 24h wait). That is what the
  one-shot verbs are for — and it is exactly why the loop hid a fatal bug: every
  test used a verb that skips `next_run_epoch`, so a loop that could never run
  looked tested.
- **The loop was fixed and IS now exercised.** `next_run_epoch` used GNU
  `date -u -d "today 3:00"`, which BusyBox `date` rejects with
  `date: invalid date 'today 3:00'`. The nightly loop therefore exited 2 on its
  FIRST iteration and Compose restart-looped it forever — the scheduler could
  never have run a single job. It is now pure arithmetic
  (`utc_midnight` + the scheduled hour), verified in the container: it reports
  `next run in ...s` and computes the correct instant (checked for 02:00, 04:00
  and exactly 03:00, including the roll-over to the next day).
  `docker compose up -d scheduler` now reaches `Up`, not `Restarting`.
- **The three Rust binaries.** `ip-purge`, `usage-purge` and `hold-sweep` are still
  BINARIES NOT WIRED, by construction, but **all three jobs' work now runs inline**:
  the first two by `run_retention`, and `hold-sweep` by `run_hold_sweep` (report-only).
  For `hold-sweep` the only thing left on the host is the money-moving `--release`
  flag, which is an operator decision rather than a scheduled job.
- **`docker compose run scheduler once` on a host whose checkout has LF working-tree
  files.** The two script mounts (`entrypoint.sh`, `reconcile.sh`/`.sql`) are shell
  and SQL files executed **inside** the container. This machine's checkout has them
  with CRLF line endings (`git config core.autocrlf=true`, no `.gitattributes`
  pinning them), and a CRLF `/bin/sh` script fails immediately with
  `set: line 86: illegal option -` - before any job runs. That is a pre-existing
  property of this checkout, not of the port: it breaks the mount regardless of the
  image, and it is outside the files this port was fenced to. The rows in the table
  above were produced from an **LF copy** of the same committed files under
  `.agents/sqlite-port/lf/` (the project directory passed to
  `docker compose --project-directory`), with the mounted scripts byte-identical to
  the committed ones except for line endings. On a Linux host or a checkout with
  `core.autocrlf=false`/`.gitattributes` forcing LF, the literal commands run
  unmodified. The right fix is a `.gitattributes` entry forcing LF on
  `*.sh`/`*.sql`; it is not in this fence, so it is reported rather than done.
- **`reconcile.sql`'s drift query is verified here only as a whole.** The gate's
  `reconcile.sql` gap recorded in `tools/reconcile/README.md` was fixed in that
  file before this run (it now uses `CAST(... AS TEXT)`, not Postgres `::text`), and
  the drift detection in row `o` exercised it end to end. This README does not
  re-audit that file.
