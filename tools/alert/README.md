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
- `probe.sh` - the three checks that need **no database at all** (`api_down`,
  `relay_down`, `webhook_rejection`). Opens no database connection by design, so it
  keeps working across the PostgreSQL -> SQLite migration. Fires `alert.sh` on a
  breach, through the same transport and cooldown as everything else.

## How to run

```sh
# a check that found something
WEBHOOK_URL='https://hooks.example/apikita' sh tools/alert/alert.sh \
    --alert ledger_drift --observed "1 drifting account: c8c2859c 60000 vs 50000"

# the psql-only checks (drift, balance-negative, stranded holds)
WEBHOOK_URL='https://hooks.example/apikita' sh tools/alert/check-alerts.sh

# the database-free checks (api_down, relay_down, webhook_rejection)
WEBHOOK_URL='https://hooks.example/apikita' sh tools/alert/probe.sh

# one of them, and what probe.sh can and cannot check
sh tools/alert/probe.sh --check api_down
sh tools/alert/probe.sh --list

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

### `probe.sh`

**The same table, deliberately - no new numbering was invented.** `0`-`4` keep
`reconcile.sh`'s meanings, `5` and `6` are the same additive codes
`check-alerts.sh` already uses, and the precedence rule is identical.

| Code | Meaning |
| ---- | ------- |
| `0`  | **CLEAN** - every selected check ran (or was **visibly skipped**) and no threshold was breached. |
| `1`  | **FIRED** - at least one alert fired and was delivered (or throttled). |
| `2`  | **CONFIG** - a knob is unusable: a non-integer `PROBE_*` window/interval/timeout, an unknown check id, or an unknown argument. |
| `3`  | **MISSING** - no usable `curl`, or `alert.sh` is not next to this script. |
| `4`  | **FAILED** - a check ran and could not reach a verdict (the log file is not readable). |
| `5`  | **UNDELIVERED** - an alert fired and could **not** be delivered. Not a pass. |
| `6`  | **UNKNOWN** - a configured source is absent, so the check cannot run (`PROBE_LOG_FILE` points at a file that does not exist). Unknown is not clean. |

Note the split between **SKIPPED** and **UNKNOWN**, because conflating them is how a
silent pass creeps back in: a check that is *not configured* is exit 0 with the skip
printed (there is nothing to run, and the operator is told); a check that *is
configured but unusable* is exit 6 (the operator believed it was running). A
skipped check that nobody can see is the exact defect this directory exists to
remove, so `--list` reports it too, as **skipped**.

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
`check-alerts.sh` states this on every run rather than silently skipping it - a
database check cannot answer it, and pretending otherwise would be the defect. It
is answered a different way by `probe.sh`: the same log lines, counted from the
server's **captured stdout** (`PROBE_LOG_FILE`), which is exactly the "log counter"
this section used to say was needed. No `server/**` change was required for that,
and none was made.

The remaining gap is honesty about the source: with no `PROBE_LOG_FILE` configured,
`probe.sh` prints **SKIPPED - no log source configured** and says the alert is NOT
being checked. A skipped check is stated, never counted as a pass.

## What `probe.sh` actually checks

Three checks, and **not one line of SQL**. This is deliberate, not incidental: the
stack is mid-migration from PostgreSQL to SQLite (a concurrent change under
`server/`), and a checker that reads the database goes blind during exactly the
window when the migration is most likely to break something. `probe.sh` reads only
HTTP and a log file, so it keeps working whichever engine is behind the API.

| # | Check | How | Threshold |
| - | ----- | --- | --------- |
| 1 | **API down** | Polls `GET $PROBE_API_URL/health` and measures the **span** of consecutive non-200/failed checks | The registry's `120s of failed /health checks` |
| 2 | **Relay down** | ONE `GET` of the relay's own origin (`$PROBE_RELAY_URL`) | `1 failed external check` - any HTTP answer at all is "up" |
| 3 | **Webhook rejection** | Counts `topup.rejected` lines in the **server's captured stdout** (`$PROBE_LOG_FILE`), from the line offset of the last scan | `any (>=1 in the window)` |

Three things about check 1 that are choices, not accidents:

- **The window is a real window.** One failed sample is not 120s of failure. The
  poll loop only returns once either `/health` answers (the failure run is broken -
  consecutive means consecutive) or the failures span the window. It therefore
  blocks only while the API is actually down, which is precisely when waiting out
  the window before paging is the right call.
- **A 502/504 is NOT `relay_down`.** Any HTTP answer means the relay is up; a bad
  status is `relay_5xx`'s alert, which is still unchecked. Filing "the backend is
  down" as "the edge is down" would be a wrong action attached to a real event.
- **The webhook scan advances only on delivery.** The line offset is recorded when
  somebody was actually told (delivered *or* throttled). A failed delivery does not
  advance it, so the event is not lost - the same rule `alert.sh` applies to its
  cooldown.

### Environment

Everything is overridable so the checks can be proven without waiting two minutes -
which is how the verification below was done.

| Variable | Default | Meaning |
| -------- | ------- | ------- |
| `PROBE_API_URL` | `http://127.0.0.1:8080` | API origin; `/health` is appended. |
| `PROBE_API_WINDOW_SECONDS` | `120` | The failure span that triggers `api_down` (the registry's documented window). |
| `PROBE_API_INTERVAL_SECONDS` | `10` | Poll interval while the API is failing. Floored at `1`. |
| `PROBE_RELAY_URL` | `http://127.0.0.1:8000` | The relay's own origin, checked externally. |
| `PROBE_LOG_FILE` | - | The server's captured stdout. **Unset = `webhook_rejection` is SKIPPED, visibly.** |
| `PROBE_STATE_DIR` | `${TMPDIR:-/tmp}/apikita-probe` | Where the webhook line offset lives. Must persist across runs, or the same lines are re-scanned. |
| `PROBE_HTTP_TIMEOUT` | `5` | Connect/read timeout in seconds for both external checks. |
| `ALERT_SINK_FILE` / `WEBHOOK_URL` / Telegram vars | - | **Not probe.sh's** - they belong to `alert.sh`, which does the delivering. See its table above. |
| `ALERT_COOLDOWN_SECONDS` | `900` | **Not probe.sh's** either. It applies to every firing, because every firing goes through `alert.sh`. |

`--check <id>` runs one check (repeatable); `--list` shows the three and marks
`webhook_rejection` as **skipped** when `PROBE_LOG_FILE` is unset. The script
honours the registry thresholds but never re-states them: the numbers in an alert's
message come from `alerts.tsv` via `alert.sh`.

## Coverage: what this can deliver today

**MEASURED, because this paragraph said "9 alerts" while the registry had grown to 11.** Three
counts, each read from a file rather than remembered — and each true of a DIFFERENT thing, which is
why one number kept being wrong:

| what | count | read from |
| --- | --- | --- |
| Alert definitions | **11** | `alerts.tsv`, the registry |
| Checks `probe.sh --list` prints | **7** | its own output, one row per check |
| Definitions `check-alerts.sh` names | **11** | the dispatch, which covers the registry |

The gap between 7 and 11 is not missing coverage and must not read as such: `probe.sh` covers the
seven that need a prober, and `check-alerts.sh` covers all eleven, including the three
(`ledger_drift`, `balance_negative`, `stranded_hold`) that need a database connection and so are
deliberately absent here. **A count that is right about one of those and silent about which is the
defect that produced this table**, so the column is the part that matters.

The registry's eleventh entry is `stranded_hold`, which the doc specifies in prose at lines
141-175 with the same force ("**Incident - investigate, then credit**").

| # | Doc alert (line) | Delivered today? | Why not, if not |
| - | ---------------- | ---------------- | --------------- |
| 1 | **Webhook rejection** (99) | **Yes** | `probe.sh` counts `topup.rejected` in the server's captured stdout (`PROBE_LOG_FILE`). **Skipped, not passed, when no log source is configured.** |
| 2 | **Ledger drift** (100) | **Yes** | `check-alerts.sh` -> `reconcile.sh` -> `alert.sh`. |
| 3 | **API down** (101) | **Yes** | `probe.sh` -> external `GET /health`, alerting once the failure span covers the window. Not a database fact, so it needs no database. |
| 4 | **Relay 5xx** (102) | **No** | nginx status counts. Needs a metrics backend (or an access-log counter). |
| 5 | **Relay down** (103) | **Yes** | `probe.sh` -> one external `GET` of the relay's origin. |
| 6 | **All providers unhealthy** (104) | **Yes, via `/api/admin/metrics`** | `probe.sh` reads the breaker state from the operator route. **Skipped, not passed, without `PROBE_OPERATOR_COOKIE`.** |
| 7 | **Balance negative** (105) | **Yes** | One `SELECT COUNT(*)`, threshold 0. |
| 8 | **DB disk >80%** (106) | **Yes, via `/api/admin/metrics`** | The retention lag is served in-process, and retention is the alert's own ACTION rather than its trigger. **Skipped without `PROBE_OPERATOR_COOKIE`.** |
| 9 | **Error rate >5%** (107) | **Yes, via `/api/admin/metrics`** | Same route and same caveat as rows 6 and 8. |
| + | **Stranded holds** (141-175) | **Yes** | Via `reconcile.sh`'s copy of the `hold-sweep` predicate. |

**MEASURED: 11 definitions, 10 of them reachable today, 1 not.** This paragraph said "**Covered: 6 of
10. Not covered: 4 of 10**" and gave three reasons that had all stopped being true — three rows gained
a `/api/admin/metrics` path and the table was never updated with them.

**Understating is not the safe direction, which is why this is corrected rather than left.** A table
saying an alert is unwired, when it is merely waiting for a cookie, sends an operator to wire
something that is already wired — and it hides that the real remaining gap is a single alert.

```
$ sh tools/alert/probe.sh --check db_disk
probe: NOT CHECKED - 1 of the doc's 10 alerts still needs a surface this cannot reach:
probe:   relay_5xx               - nginx access-log status counts (access logs are deliberately off).
```

`probe.sh` states the remainder itself on every run, and it is the authority — not this table.

The one that remains is `relay_5xx`, which needs nginx **access logs**, deliberately disabled for
privacy (`docs/edge-relay.md`, `docs/data-retention.md`). Enabling them to satisfy an alert would
trade a privacy decision for an operational one, and that is not this script's trade to make.

Wiring it is a call to `alert.sh --alert <id>` once the surface exists — no change to the transport,
and no change to `probe.sh`.

The delivery **mechanism**, though, is complete for all **11**: `alerts.tsv` carries every alert's
threshold and action, so wiring a metrics backend means calling `alert.sh --alert <id>` — no change to
the transport. `alert.sh --list` shows all eleven with their coverage, including the ones nothing
currently fires, and reads the registry rather than a hand-kept copy of it.

> **`alert.sh --list` and `probe.sh --list` are different listings, and both are correct.** The first
> prints every definition from `alerts.tsv` (**11**); the second prints the checks the prober has a code
> path for (**7**). This paragraph said "all ten" and the coverage section said "9 alerts" — one
> number, three different things it could have meant. The counts are now stated per-source.

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
| **A metrics backend** | The doc's other open item (line 199). **At most one** alert still needs one — `relay_5xx`, and it needs nginx access logs rather than a metrics store. This row said "**Four** of ten (down from seven)" and was wrong in the same way the coverage table was: three of those four gained a `/api/admin/metrics` path. A cost decision about Prometheus is not what remains. |
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
| g | `--list` | all **11** definitions with threshold and coverage (measured: the output has 11 data rows, one per `alerts.tsv` entry) |
| h | Unknown alert id | **exit 4**, with the known ids listed |
| i | No `psql` on PATH and no docker | **exit 3** |
| i2 | Real `psql` failure (`DATABASE_URL` at a nonexistent database) | **exit 4**; `reconcile: psql failed (exit 2)` |
| j | `DATABASE_URL=mysql://...` | **exit 2** |
| k | Drift present, **no channel configured** | **exit 5** UNDELIVERED - "an alert fired and nobody was told" is not a pass |
| l | `reconcile.sh` replaced by a stub printing no `HOLD SWEEP` line | **exit 6** UNKNOWN - the stranded-hold check is never assumed to have passed |
| m | Clean database, sink channel | **exit 0** CLEAN |
| n | The whole matrix re-run with **no** `psql` shim on `PATH` | identical results: `0` CLEAN, `1` delivered, `1` throttled (1 payload), `2`, `3`, `4`, `5`, `6` |

### `probe.sh` - verified 2026-09-26, with no database at all

Every row below was produced for real on this host with `ALERT_SINK_FILE` as the
channel (no credential needed) and `ALERT_COOLDOWN_SECONDS=0` **or** a fresh
`ALERT_STATE_DIR` per firing - because the live cooldown is 900s and would otherwise
suppress a repeat test firing. Row `x` shows that suppression working, not bypassed.
The API and relay checks were pointed at local servers via `PROBE_*_URL`, so no
component was stopped to run them.

| # | Scenario | Result |
| - | -------- | ------ |
| p1 | `--check api_down`, healthy `/health` (200) | **exit 0**, `OK  api_down: ... returned 200`, **0 payloads** |
| p2 | `--check api_down`, API **dead** (`PROBE_API_URL` at a port with nothing listening), window 3s | **exit 1**, `ALERT api_down: ... failing for 3s of the 3s window (1 consecutive failed check(s))`, **1 payload** with the registry's `Threshold: 120s of failed /health checks` |
| p3 | `--check api_down` again, API healthy | **exit 0**, **0 payloads** - silent again |
| p4 | `--check relay_down`, relay answering | **exit 0**, `OK  relay_down: ... answered (HTTP 200)`, **0 payloads** |
| p5 | `--check relay_down`, relay **unreachable** | **exit 1**, `ALERT relay_down: ... did not answer a single external check`, **1 payload**, `Threshold: 1 failed external check` |
| p6 | `--check relay_down` again, relay answering | **exit 0**, **0 payloads** |
| p7 | `--check webhook_rejection`, log with **no** rejection line | **exit 0**, `0 'topup.rejected' in 3 new line(s)`, **0 payloads** |
| p8 | append **one** line containing `topup.rejected` | **exit 1**, `ALERT webhook_rejection: 1 'topup.rejected' line(s) in 1 new line(s)`, **1 payload** |
| p9 | run again, no new lines | **exit 0**, `0 in 0 new line(s) ... (4 total; scanned from line 5)` - the offset advanced, so the same event does not re-page |
| p9b | append one more rejection | **exit 1** again - one page per rejection, not one per run |
| p10 | `PROBE_LOG_FILE` **unset** | **exit 0** and `SKIPPED webhook_rejection - no log source configured ... This alert is NOT being checked`; `--list` marks it `skipped`. A skip stated out loud, never a pass |
| p11 | `PROBE_LOG_FILE` set to a **missing** file | **exit 6** UNKNOWN - `'no rejections' cannot be claimed. Unknown is not clean.` |
| p12 | log **rotated** (shrank below the offset) | rescans from the start and still fires - the offset never hides an event |
| x | second firing inside the live 900s cooldown | `alert: THROTTLED ... cooldown 900s; nothing sent`, **still 1 payload** - the cooldown is genuinely in force |
| y | `--list` | the three checks with their `covered`/`skipped` state and the four still-unchecked ids |
| z | unknown argument / unknown check id | **exit 2** |

### Mutation-checked

Each guard was removed and the bad behaviour reproduced, to prove the guard - not
something incidental - is what stops it. `alert.sh`'s sha256 was identical before
and after (`6619a438...d341b31`), so "restored" is verified, not claimed.

| Mutation | Unmutated | Mutated |
| -------- | --------- | ------- |
| The no-channel refusal returns `exit 0` instead of `exit 2` | exit 2, refusal printed, nothing sent | **exit 0 with no channel and nothing sent** - a silently dropped alert reported as success |
| The cooldown is hard-coded to `0` (`ALERT_COOLDOWN_SECONDS` ignored) | 2 runs -> 1 payload | **3 runs -> 3 payloads** - the alert fatigue the doc warns about |
| `probe.sh`: the `api_down` window comparison `[ "$SPAN" -ge "$API_WINDOW" ]` is made unsatisfiable | API dead -> **exit 1**, 1 payload | **exit 0 CLEAN with the API still dead and nothing fired** - the outage reported as healthy |
| `probe.sh`: the `topup.rejected` pattern is changed to match nothing | rejection in the log -> **exit 1**, 1 payload | **exit 0 CLEAN with the rejection still in the log and nothing fired** |

Both mutants were run as **copies under `.agents/`**, with a copy of `alert.sh` beside
them (`probe.sh` resolves it from its own directory) - the shipped file was never
edited. `probe.sh`'s sha256 was `c429a642...454b1f0` before **and** after the
mutation run, and `alert.sh`'s was unchanged (`c140495d...39f69`) throughout.

### Not verified

- **A real Telegram delivery.** No bot token or chat id exists here, and none was
  invented. The `sendMessage` **request shape** was verified against a local sink
  (b2); the Bot API's own response handling was verified only for the failure path
  (an HTTP 404 from a foreign process holding the port -> exit 3).
- **A webhook receiver under load, or with retries.** The sink accepts one POST at a
  time and returns 200. No retry, backoff, or ordering behaviour is implemented or
  tested - `alert.sh` fires once and reports.
- **The four alerts that still need a surface.** By construction: no metrics backend,
  no access logs, no server-side breaker or volume surface exists (the doc's own open
  item). The three `probe.sh` covers are no longer in this list.
- **`probe.sh` against the real API and relay.** The checks were verified against
  local HTTP servers via `PROBE_API_URL` / `PROBE_RELAY_URL`; the `120s` window was
  exercised at 3s for the same reason the knobs exist. On this host the real API
  (`127.0.0.1:8080`) was **not listening**, and the relay (`127.0.0.1:8000`) answered -
  the dead-API path was therefore proved against a dead port, which is the same
  observable behaviour.
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
