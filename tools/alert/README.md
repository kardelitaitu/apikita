# Alert delivery (the piece `docs/observability.md` specifies but nothing implements)

[`docs/observability.md`](../../docs/observability.md) defines **what to alert on**
(its alert table, lines 97-108) and states its own design rule twice:

> **Alert on things that cost money or lose data. Ignore everything else.** (line 13)
>
> Every alert below has a defined action; **anything without an action is a
> dashboard, not an alert.** (lines 15-16)

That document's open items (line 201) still carry **"Alert delivery channel
(Telegram is already in the stack)"** unchecked, and until this directory existed
there was **zero alerting code anywhere in the repo**. Verified by grep over every
`*.sh`/`*.rs`/`*.js`/`*.ts`/`*.yml` in the tree: the only non-documentation hits for
`alert` are `server/src/bin/hold-sweep.rs` and `server/src/ip_tracking.rs`, neither
of which delivers anything. `telegram/` contains a README and nothing else.

This directory is the delivery half. It is deliberately **not** a log shipper and
not a metrics backend: it takes ONE alert that some other check has already decided
is real, and gets it in front of a human with the action attached.

## Files

- `alerts.tsv` - the alert definitions: id, title, condition, threshold, action,
  coverage, source. **The single place a threshold or an action lives**, so they
  cannot drift from the doc's table and can be asserted (`alert.sh --list`).
- `alert.sh` - the transport. Delivers ONE alert over a configured channel, with a
  cooldown. Refuses to run with no channel.
- `check-alerts.sh` - runs the checks the doc's table allows with `psql` alone, and
  prints the ones it cannot run. Fires `alert.sh` on a breach.

## How to run

```sh
# a check that found something
WEBHOOK_URL='https://hooks.example/apikita' sh tools/alert/alert.sh \
    --alert ledger_drift --observed "1 drifting account: c8c2859c 60000 vs 50000"

# the psql-only checks (drift, balance-negative, stranded holds)
WEBHOOK_URL='https://hooks.example/apikita' sh tools/alert/check-alerts.sh

# every definition and its coverage
sh tools/alert/alert.sh --list

# local use, no credentials at all
ALERT_SINK_STDOUT=1 sh tools/alert/alert.sh --alert db_disk --observed "84%"
```

### Channels

The first configured one wins, and the output names it, so an operator always knows
where an alert went.

| Channel | Configured by | What it does |
| ------- | ------------- | ------------ |
| Telegram | `TELEGRAM_BOT_TOKEN` **and** `TELEGRAM_CHAT_ID` | Real HTTP `POST` to the Bot API `sendMessage` (form-encoded, `chat_id` + `text`). HTTP 200 means delivered; anything else is exit 3. |
| Webhook | `WEBHOOK_URL` | `POST` with `Content-Type: application/json` and body `{"text": "..."}`. Any `2xx` means delivered. |
| File | `ALERT_SINK_FILE=<path>` | Appends the payload. For tests and for an audit trail on a host with no network. |
| Stdout | `ALERT_SINK_STDOUT=1` | Prints the payload. Local use only. |

### Environment

| Variable | Default | Meaning |
| -------- | ------- | ------- |
| `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` | - | Telegram channel. **Both** required. |
| `WEBHOOK_URL` | - | Webhook channel. |
| `ALERT_SINK_FILE` | - | File channel. |
| `ALERT_SINK_STDOUT` | - | Set to `1` for the stdout channel. |
| `ALERT_COOLDOWN_SECONDS` | `900` | Suppress the same alert key for this long. `0` disables the cooldown. |
| `ALERT_STATE_DIR` | `${TMPDIR:-/tmp}/apikita-alerts` | Where the cooldown state files live. **Must persist across runs** (see below). |
| `ALERT_ENV` | - | Optional label prefixed to the subject, e.g. `production`. |
| `ALERT_HOST` | `hostname` | The host named in the message. |
| `ALERT_HTTP_TIMEOUT` | `10` | Connect/read timeout in seconds for both HTTP channels. |
| `ALERT_DEFINITIONS` | `<this dir>/alerts.tsv` | Override the definition table. |
| `TELEGRAM_API_BASE` | `https://api.telegram.org` | **Test seam only.** Redirects the Bot API host so the `sendMessage` request shape can be tested against a local sink. Not a production knob. |

### Secrets

**Read from the environment only.** The token is never echoed, never written to the
state file, never placed in the payload, and never committed - `.env` is gitignored
and no credential is stored in this directory. The Telegram channel passes the token
in the request URL, which is how the Bot API works; `alert.sh` prints the channel
name, never the URL. `check-alerts.sh` redacts the password out of `DATABASE_URL`
before printing the DSN.

## The cooldown, and why it is not optional

The doc's stated margin concern is **alert fatigue** (line 15: "A small business
with thin margins cannot afford alert fatigue"). A check that runs hourly against a
condition that is still true pages 24 times for one incident.

So the same **alert key** inside `ALERT_COOLDOWN_SECONDS` (default 900) is
suppressed: **one page per incident, not one page per run of the check.** The key
defaults to the alert id; pass `--key <something>` to scope it finer - e.g.
`--key ledger_drift/<account_id>` pages once per drifting account, while
`--key ledger_drift` pages once for the whole class of drift.

Two details that matter:

- **A throttled alert is exit 1, not exit 0.** "Suppressed on purpose" and
  "delivered" are different facts and must not be conflated, so an exit 0 always
  means a channel accepted the message.
- **The cooldown is recorded only AFTER the channel accepted the alert.** Recording
  it first would let a failed delivery suppress its own retry for the whole window -
  the worst of both: nobody was told, and nothing will try again for 15 minutes.

State is one file per key, `<state dir>/<key>.last`, containing the epoch seconds of
the last **successful** delivery. `ALERT_STATE_DIR` defaults to `${TMPDIR:-/tmp}`,
which on many systems is cleared on reboot and on some is per-process; **for a
scheduled check, point it somewhere durable** (e.g. `/var/lib/apikita-alerts`). A
cooldown that silently resets pages repeatedly - the failure it exists to prevent -
so this is called out rather than assumed.

## Exit codes

### `alert.sh`

| Code | Meaning |
| ---- | ------- |
| `0`  | **DELIVERED** - a channel accepted the alert. |
| `1`  | **THROTTLED** - the same key fired inside `ALERT_COOLDOWN_SECONDS`; nothing was sent, deliberately. |
| `2`  | **NO CHANNEL CONFIGURED** - refused, nothing delivered, and the undelivered alert is printed to stderr. |
| `3`  | **DELIVERY FAILED** - a channel was configured and did not accept the alert (connection error, non-2xx, or a Telegram non-200). |
| `4`  | Usage error: unknown alert id, missing argument, or a non-integer cooldown. |

Code `2` is the point of the tool. The failure mode that matters for an alert
system is not "the alert was wrong" - it is **"the alert was silently dropped"**,
because a dropped alert is believed. That is the same reasoning as
`tools/backup/backup.sh` refusing to write a plaintext dump (exit 6 there): the
dangerous outcome is the quiet downgrade, not the loud failure.

### `check-alerts.sh`

| Code | Meaning |
| ---- | ------- |
| `0`  | **CLEAN** - every runnable check ran and no threshold was breached. |
| `1`  | **FIRED** - at least one alert fired and was delivered (or throttled). The tool worked; something real is wrong. Mirrors `tools/backup/backup.sh`'s 1. |
| `2`  | **CONFIG** - `DATABASE_URL` is set but is not a `postgres://`/`postgresql://` DSN. |
| `3`  | **MISSING** - no usable `psql`: not on PATH and no `docker compose` fallback. |
| `4`  | **FAILED** - `psql` or `reconcile.sh` ran and failed (connection, permissions, SQL). |
| `5`  | **UNDELIVERED** - an alert fired and could **not** be delivered. Not a pass. |
| `6`  | **UNKNOWN** - a check could not run (reconcile could not run, its `HOLD SWEEP` line was unparseable, or `psql` returned something that is not a count). Not a pass. |

Codes `0`-`4` keep `tools/reconcile/reconcile.sh`'s meanings so an operator learns
one table; `5` and `6` are additive. The numbering is `reconcile.sh`'s on purpose:
`2` is a config error, `3` a missing client, `4` a failed run. `1` is `FIRED` rather
than a failure for the same reason `tools/backup/backup.sh` uses `1` for "ran, but
the news is not all good".

**Precedence, because more than one can be true at once: `5` (fired and nobody was
told) beats everything; then `2`/`3`/`4`/`6` (a check that could not run); then `1`
(fired and delivered).** "Something is wrong" is only good news if you heard about
it, and a check that did not run is not a check that passed.

## What `check-alerts.sh` actually checks

Three checks, no metrics backend needed.

| # | Check | How | Threshold |
| - | ----- | --- | --------- |
| 1 | **Ledger drift** | Delegates to `tools/reconcile/reconcile.sh` and takes **its** exit code | 0 accounts |
| 2 | **Balance negative** | `SELECT COUNT(*) FROM wallets WHERE balance_idr < 0` | 0 rows |
| 3 | **Stranded holds** | Also from `reconcile.sh`, which copies the predicate of `server/src/bin/hold-sweep.rs` verbatim and prints the count on every run | 0 over the bound |

**Check 1 is deliberately a delegation, not a second query.** `tools/reconcile/README.md`
says it plainly: "two definitions of 'stranded' is how a detector stops being
trusted." The same applies to drift, so this script does not re-derive it - it runs
`reconcile.sh` and reads its exit code (`0` clean, `1` drift, `2`/`3`/`4` could not
run, `5` stranded hold).

**Check 3 parses `reconcile.sh`'s `HOLD SWEEP` line, and FAILS if that line is
missing** (exit 6, UNKNOWN). `reconcile.sh` prints it on every run precisely because
a stranded hold is structurally invisible to the drift query; if the line ever
disappears, assuming "zero holds" is exactly the silent pass this directory exists
to prevent.

### Webhook rejection: read the emission site, honestly

`server/src/routes/webhooks.rs` emits webhook rejection as a **structured log
line**, not a counter:

- `warn!(order_id = ..., "Midtrans webhook rejected: invalid signature")` - line 116
- `error!(order_id = ..., "Webhook rejected: amount mismatch with stored record")` - line 188
- `warn!(order_id = ..., "Webhook rejected: order not found")` - line 178

There is **no column anywhere in the schema that records a rejected webhook**, so
`any topup.rejected` (the doc's condition, line 99) is **not answerable from SQL**.
This is stated on every `check-alerts.sh` run rather than silently skipped. It is
alertable the moment a metrics backend counts those log lines, or the moment a
`topups.status` / audit row is written for a rejection - **both are changes outside
this fence** (`server/**`), so they are reported here, not made here.

## Coverage: what this can deliver today

The doc's table has 9 alerts; this directory also defines `stranded_hold`, which the
doc specifies in prose at lines 141-175 with the same force ("**Incident -
investigate, then credit**").

| # | Doc alert (line) | Delivered today? | Why not, if not |
| - | ---------------- | ---------------- | --------------- |
| 1 | **Webhook rejection** (99) | **No** | The event exists only as a log line in `server/src/routes/webhooks.rs` (see above). Needs a metrics backend or a log counter. |
| 2 | **Ledger drift** (100) | **Yes** | `check-alerts.sh` -> `reconcile.sh` -> `alert.sh`. |
| 3 | **API down** (101) | **No** | An external `GET /health` probe on a 2-minute window. Not a database fact, and no prober exists in this repo. |
| 4 | **Relay 5xx** (102) | **No** | nginx status counts. Needs a metrics backend (or an access-log counter). |
| 5 | **Relay down** (103) | **No** | External check against the relay. |
| 6 | **All providers unhealthy** (104) | **No** | Circuit-breaker state is in-process (`server/src/upstream/circuit_breaker.rs`); it is not exposed anywhere a shell script can read. |
| 7 | **Balance negative** (105) | **Yes** | One `SELECT COUNT(*)`, threshold 0. |
| 8 | **DB disk >80%** (106) | **No** | Volume usage - not visible in SQL. |
| 9 | **Error rate >5%** (107) | **No** | HTTP counters over a 5-minute window. Needs a metrics backend. |
| + | **Stranded holds** (141-175) | **Yes** | Via `reconcile.sh`'s copy of the `hold-sweep` predicate. |

**Covered: 3 of 10. Not covered: 7 of 10, and the reasons are in the table.** The
four the task named as genuinely metrics-dependent - error rate, relay 5xx, DB
disk, all-providers-unhealthy - are four of the seven.

The delivery **mechanism**, though, is complete for all ten: `alerts.tsv` carries
every doc alert's threshold and action, so wiring a metrics backend means calling
`alert.sh --alert <id>` - no change to the transport. `alert.sh --list` shows all
ten with their coverage, including the ones nothing currently fires.

## What is deliberately NOT alerted

The doc is explicit (lines 109-110): individual 401s, individual 500s, high CPU
with normal latency, and slow upstream are **not** alerts. Nothing here invents an
alert the doc did not ask for; `alerts.tsv` contains exactly the doc's 9 rows plus
`stranded_hold`, which the doc specifies in prose.

## Still a human decision

| Decision | Why it is not this script's to make |
| -------- | ----------------------------------- |
| **Which channel** | Telegram is "already in the stack" per the doc, but the doc's open item is unchecked and `telegram/` contains only a README - **no bot exists**. A bot token and a chat id must be created by a human with a Telegram account. Nothing here invents either. |
| **The chat id** | Where alerts land is a policy choice: an operator DM, a private ops room, or a room customers can see. A wrong choice leaks balance and drift figures to the wrong audience. |
| **Bot token custody** | Where `TELEGRAM_BOT_TOKEN` lives (secret manager, host environment, compose secret) and how it is rotated if it leaks. |
| **Scheduling** | **Nothing runs `check-alerts.sh` yet** - no cron, no compose service, no CI job. `tools/reconcile/` and `tools/backup/` have the same gap. |
| **Cooldown length** | 900s matches `reconcile.sh`'s hold bound and is a starting point, not a measured value. What "one page per incident" means for your traffic is a judgement call. |
| **A metrics backend** | The doc's other open item (line 199). Seven of ten alerts need it; self-hosted Prometheus vs a hosted service is a cost decision, not a technical one. |
| **A public status page** | The doc's open item at line 202. Separate from alerting; nothing here addresses it. |

**`docs/observability.md` was NOT edited.** Its open item at line 201 - "Alert
delivery channel (Telegram is already in the stack)" - should be ticked only when a
channel is actually configured and an alert has actually been received by a human,
not because this script exists. On this host it has not been.

## Verified status

Verified on 2026-09-25 against the local dev stack (compose service `postgres`,
database `apikita`), plus a local HTTP sink. Every exit code below was produced for
real; no output is fabricated.

**This host has no `psql`** (`tools/reconcile/README.md` and
`tools/backup/README.md` document the same). `check-alerts.sh` therefore uses the
container itself: it prefers a host `psql`, and otherwise runs its own query through
`docker compose exec -T postgres psql` **and puts a generated `psql` shim on `PATH`**
so the `reconcile.sh` it delegates to can run too. That shim forwards to the same
real `psql` and streams `reconcile.sql` to its stdin, because the container cannot
see host paths. Every result below was produced on that path - the whole matrix was
re-run with **no** shim on `PATH`, so exit codes and error text are psql's own.

| # | Scenario | Result |
| - | -------- | ------ |
| a | No channel configured | **exit 2**; the undelivered alert printed to stderr; **no state file, no sink file** - nothing delivered |
| b | Delivery to a local HTTP sink (`WEBHOOK_URL=http://127.0.0.1:18901/hook`) | **exit 0**; the sink received `{"text": "ALERT: Ledger drift\nACTION: Money is wrong. Freeze changes, investigate\n\nAlert    : ledger_drift - Ledger drift\nObserved : 2 drifting account(s)...\nThreshold: 0 (must equal 0)\nCondition: ledger_balance_drift_idr != 0\n\nHost : ...\nTime : ...\nSource : docs/observability.md:100\nCoverage : covered..."}` |
| b2 | Telegram channel against a local sink (`TELEGRAM_API_BASE`) | **exit 0**; `POST /bot<token>/sendMessage`, form-encoded: `chat_id=-1001234567890&disable_web_page_preview=true&text=ALERT%3A+Webhook+rejection%0AACTION%3A+...` |
| c | Two identical alerts, same key, back to back | first **exit 0** delivered; second **exit 1** THROTTLED; **1 payload** in the sink |
| c2 | Same alert id, different `--key` | **exit 0**, delivered (2 payloads total) |
| c3 | Different alert id | **exit 0**, delivered |
| c4 | `ALERT_COOLDOWN_SECONDS=0` | **exit 0**, delivered again (cooldown genuinely disabled) |
| c5 | `ALERT_COOLDOWN_SECONDS=abc` | **exit 4** usage error |
| d | **Real injected drift**: a probe account with a `wallets` row (`5000`) and **no** ledger rows | `check-alerts.sh` **exit 1**, `reconcile.sh` named the account (`4a5444b3-...|5000|0`), `alert.sh` delivered; probe removed afterwards, **drift back to 0** |
| d2 | Same drift, run again immediately | `alert.sh` **exit 1** THROTTLED - one page for the incident, not one per run |
| d3 | Probe removed | `check-alerts.sh` **exit 0** CLEAN |
| e | `WEBHOOK_URL=http://127.0.0.1:1/...` (nothing listening) | **exit 3**; `curl: (7) Failed to connect to 127.0.0.1 port 1`; the alert reported as NOT delivered |
| e2 | Sink returns HTTP 500 | **exit 3**; `webhook delivery FAILED (HTTP 500)` |
| e3 | After one delivery and two failures, the state dir holds **only** the successful key | a failed delivery did not consume the cooldown |
| f | Stranded hold: a `reserve_probe-*` row, `-1234`, backdated 2h, wallet reduced by the same amount | **the drift query returned 0** (structurally blind, as the doc says); `check-alerts.sh` **exit 1** and fired **`stranded_hold`**, not `ledger_drift`; `hold-sweep.exe` independently reported the same hold; cleanup restored drift to 0 and left 0 probe rows |
| g | `--list` | all 10 definitions with threshold and coverage |
| h | Unknown alert id | **exit 4**, with the known ids listed |
| i | No `psql` on PATH and no docker | **exit 3** |
| i2 | Real `psql` failure (`DATABASE_URL` at a nonexistent database) | **exit 4**; `reconcile: psql failed (exit 2)` |
| j | `DATABASE_URL=mysql://...` | **exit 2** |
| k | Drift present, **no channel configured** | **exit 5** UNDELIVERED - "an alert fired and nobody was told" is not a pass |
| l | `reconcile.sh` replaced by a stub printing no `HOLD SWEEP` line | **exit 6** UNKNOWN - the stranded-hold check is never assumed to have passed |
| m | Clean database, sink channel | **exit 0** CLEAN |
| n | The whole matrix re-run with **no** `psql` shim on `PATH` | identical results: `0` CLEAN, `1` delivered, `1` throttled (1 payload), `2`, `3`, `4`, `5`, `6` |

### Mutation-checked

Each guard was removed and the bad behaviour reproduced, to prove the guard - not
something incidental - is what stops it. `alert.sh`'s sha256 was identical before
and after (`6619a438...d341b31`), so "restored" is verified, not claimed.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| The no-channel refusal returns `exit 0` instead of `exit 2` | exit 2, refusal printed, nothing sent | **exit 0 with no channel and nothing sent** - a silently dropped alert reported as success |
| The cooldown is hard-coded to `0` (`ALERT_COOLDOWN_SECONDS` ignored) | 2 runs -> 1 payload | **3 runs -> 3 payloads** - the alert fatigue the doc warns about |

### Not verified

- **A real Telegram delivery.** No bot token or chat id exists here, and none was
  invented. The `sendMessage` **request shape** was verified against a local sink
  (b2); the Bot API's own response handling was verified only for the failure path
  (an HTTP 404 from a foreign process holding the port -> exit 3).
- **A webhook receiver under load, or with retries.** The sink accepts one POST at a
  time and returns 200. No retry, backoff, or ordering behaviour is implemented or
  tested - `alert.sh` fires once and reports.
- **The seven metrics-dependent alerts.** By construction: no metrics backend exists
  (the doc's own open item).
- **A schedule.** Nothing invokes `check-alerts.sh` automatically.
- **`ALERT_STATE_DIR` on a durable path.** Verified under `.agents/`, i.e. a
  writable local filesystem. A read-only or per-process `${TMPDIR:-/tmp}` is handled
  (a warning; cooldown ineffective) but was not exercised.
- **The generated `psql` shim against a host that HAS `psql`.** The host-`psql`
  branch is the one that skips the shim; it was not exercised, because this host has
  no `psql`. The container branch - the one that runs here - was.

## Notes for whoever wires this up

- **`reconcile.sh` must stay runnable from here.** `check-alerts.sh` reads its stdout
  and exit code. If `reconcile.sh` ever stops printing the `HOLD SWEEP` line, this
  script exits 6 rather than reporting clean - by design.
- **`DATABASE_URL` defaults to the local dev DSN**, with a loud warning, exactly as
  `tools/backup/backup.sh` does and for the same reason: this runs from cron with no
  environment, and the alternative to the documented default is no check at all.
  Pointing it at production is an explicit `DATABASE_URL`. (This differs from
  `reconcile.sh`, which exits 2 - a gate must not silently pick a database; a
  scheduled alert check must not silently stop running.)
- **No secrets in this directory, ever.** If you add a channel, read its credential
  from the environment the way the Telegram and webhook channels do.
