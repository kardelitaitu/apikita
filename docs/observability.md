# Observability

What to log, measure, and alert on. For a system holding customer money, the
purpose is not dashboards — it is **noticing a billing or balance problem before a
customer does**.

> Stack: Cloudflare -> edge relay (nginx) -> Rust on Northflank with embedded
> SQLite, plus PocketBase. See [`docs/architecture.md`](architecture.md) and
> [`docs/edge-relay.md`](edge-relay.md).

## The principle

**Alert on things that cost money or lose data. Ignore everything else.**

A small business with thin margins cannot afford alert fatigue. Every alert below
has a defined action; anything without an action is a dashboard, not an alert.

## Logging

**Structured JSON, always.** Not free text — you will need to query it.

| Field | Always present |
| --- | --- |
| `ts` | ISO-8601 UTC |
| `level` | error / warn / info |
| `event` | Stable name, e.g. `topup.settled` |
| `account_id` | When the request is authenticated |
| `request_id` | Correlates one client request end to end |

### Events worth naming

| Event | Level | Why |
| --- | --- | --- |
| `auth.login` | info | Audit trail |
| `auth.login_failed` | warn | Brute force detection |
| `key.created` / `key.revoked` | info | Audit trail |
| `topup.created` | info | Money in flight |
| `topup.settled` | info | Money landed |
| `topup.rejected` | warn | **Webhook verification failure — investigate** |
| `usage.settled` | info | Money out |
| `wallet.mutation` | info | Every balance change, with delta and result |
| `proxy.upstream_error` | warn | Provider health |
| `proxy.circuit_open` | warn | Provider marked unhealthy |
| `review.written` | info | Moderation signal |

### What must NEVER be logged

| Never | Why |
| --- | --- |
| Passwords or password hashes | Obvious, and still happens |
| API keys (plaintext) | Log the **prefix** only |
| Session cookie values | A log leak becomes account takeover |
| Midtrans server key | Verifies money |
| Full upstream responses | May contain customer prompts |
| **Customer prompts or completions** | Privacy; you are not the model provider |

**The prompt/response rule is a product promise, not just hygiene.** Customers send
prompts through a proxy; logging them turns you into a data processor in a way they
did not agree to. Log token **counts**, never content.

## Metrics

Keep it small. Three categories:

### Money (the ones that matter)

| Metric | Why |
| --- | --- |
| `topups_settled_total` | Revenue events |
| `topups_rejected_total` | **Non-zero means something is wrong** |
| `wallet_balance_sum_idr` | Total customer liability — a number you owe |
| `ledger_balance_drift_idr` | **Must be 0.** See reconciliation |

### Service health

| Metric | Why |
| --- | --- |
| `http_requests_total` by status | Baseline |
| `proxy_requests_total` by model, by outcome | Product usage |
| `proxy_upstream_latency_ms` | Distinguish our latency from theirs |
| `active_streams` | The memory driver |

### Rejection reasons

| Metric | Meaning |
| --- | --- |
| `proxy_rejections_total{reason}` | `no_key`, `revoked`, `model_denied`, `rate_limited`, `limit_exceeded`, `insufficient_balance` |

**Break rejections down by reason.** A spike in `insufficient_balance` is a
business signal (customers running dry). A spike in `rate_limited` is either growth
or abuse.

## Alerts

Each has a threshold and an action. If you would not act, do not alert.

| Alert | Condition | Action |
| --- | --- | --- |
| **Webhook rejection** | any `topup.rejected` | **Investigate immediately** — either an attack or a config error breaking payments |
| **Refund refused by policy** | any `refund.refused` | Correct behaviour, but nothing else moves on this path — a spike means a status-mapping regression or a dispute |
| **Ledger drift** | `ledger_balance_drift_idr != 0` | Money is wrong. Freeze changes, investigate |
| **API down** | `/health` failing 2 min | Restart/investigate; auth is down |
| **Relay 5xx** | relay returns 502/504 | The relay is up but the backend is not |
| **Relay down** | external check fails | Total outage — single point of failure |
| **All providers unhealthy** | circuit open on every endpoint | Requests failing; check upstream |
| **Balance negative** | `balance_idr < 0` | Should be impossible (CHECK constraint). A bug |
| **DB disk** | any age-based table holding a row past its retention window | Usage rows growing; check retention |
| **Error rate >5%** | 5 min window | Investigate |
| **Stranded reservation hold** | `reserve_*` negative ledger row with no positive row under the same ref | **Investigate the release path, then credit the account if the hold is lost.** Invisible to the drift query by construction — the hold left the wallet and no offsetting credit was written, so `balance` still equals `SUM(delta)` |

**These are implemented, and the definition is machine-readable.** Every row above has
an entry in [`tools/alert/alerts.tsv`](../tools/alert/alerts.tsv) — id, condition, threshold,
action and a coverage verdict — and [`tools/alert/README.md`](../tools/alert/README.md) is the
operator's view of it. Two scripts evaluate them without a metrics backend:
`check-alerts.sh` runs the SQL-answerable checks against the database file, and `probe.sh`
runs the external ones against `/health` and the relay. `alert.sh` is the transport, with
Telegram, webhook, file and stdout channels and a per-key cooldown so one incident pages once
rather than once per run.

**And they run on a schedule.** `.docker/maintenance/` invokes both nightly, so a breach
reaches an operator rather than only existing as a definition — see its README for which
jobs are wired and which are not.

**What is still a decision, not a gap:** WHICH channel to use and WHEN to run are an
operator's choices, and a breach with no channel configured is reported as unmonitored
rather than as a pass. `probe.sh` also states, in its own output, the alerts it cannot check
and why, so a skipped check is never mistaken for a clean one.

**The `db_disk` alert now measures RETENTION, not disk.** Its old name and condition
("DB disk >80% / volume usage") described a signal the backend cannot see, while its
own ACTION column asked for something it can: "check retention". Those were two
different questions and only the second is answerable here. `GET /api/admin/metrics`
reports the age of the oldest row per age-based table, present only when that row has
exceeded the table's window, so the alert fires when a retention **promise** is being
broken — which is an incident at any disk size, whereas 80% full is normal for a
working database. The row above is restated accordingly rather than silently redefined.

**The refund-refusal alert is a DISTINCT event, not folded into `topup.rejected`.**
They look similar and mean opposite things: a rejection is a payment that failed to
land and may owe someone money, while a refusal is a refund **declined by policy**,
which is the system working. Sharing the name would page on routine enforcement and
would let a refusal spike hide inside a rejection count.

It is alerted on even though the behaviour is correct, because a refusal is the one
webhook outcome where **nothing moves** — the topup stays `settled`, no ledger row is
appended, the balance is untouched. That is also exactly what a status-mapping
regression routing real events into this arm would look like, so without a distinct
marker the two are indistinguishable. `tools/alert/probe.sh` scans for it with its OWN
line-offset marker, so it and `topup.rejected` cannot consume each other's events.

**The all-providers-unhealthy alert is SERVED too.** `GET /api/admin/metrics` also
reports `unhealthy_models`: the models whose EVERY routed endpoint has an open
circuit. Note what this deliberately is NOT — one open endpoint means failover is
WORKING, so the server does not reuse its cooldown accessor for this. It counts only
ROUTED endpoints (weight > 0), so an unrouted placeholder can neither cause a false
alarm nor mask a real outage on the endpoint that is actually serving.

**The error-rate alert is SERVED.** `error.rs` counts 5xx responses and total
responses in process, and `GET /api/admin/metrics` (operator-only) reports them, so
`tools/alert/probe.sh --check error_rate` measures a real number instead of declaring
the alert unchecked. Two caveats worth knowing: the ceiling is over **handled**
requests (a 404 for an unmatched path never reaches `AppError`, so it is not counted),
and an empty window reports **`null`, not `0.0`** — the probe treats null as no-data
rather than a healthy service.

**What is deliberately NOT alerted:** individual 401s, individual 500s, high CPU
with normal latency, slow upstream (their problem, and you cannot fix it).

## The reconciliation check

**The single most valuable observability feature in this system.**

```sql
-- wallets.balance_idr must equal the sum of the ledger
SELECT w.account_id, w.balance_idr, COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
FROM wallets w
LEFT JOIN ledger l ON l.account_id = w.account_id
GROUP BY w.account_id, w.balance_idr
HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0);
```

**If this returns any row, money is wrong and you need to know before the customer
does.** Run it on a schedule (hourly or daily) and alert on any result.

The `ledger` is authoritative; `wallets` is a cache of it. A mismatch means a bug
in the credit/debit transaction, and it is the specific failure the append-only
ledger exists to detect.

### Plus the two cross-system checks

| Check | Why |
| --- | --- |
| `accounts.pb_user_id` exists in PocketBase | Orphans mean a deleted auth user with a funded wallet |
| Midtrans settlements vs `topups` where `settled` | Catches a missed webhook |

See [`docs/website/04-payments.md`](website/04-payments.md) for webhook details.

### And the check reconciliation cannot make: stranded holds

**A reservation hold that is never released is invisible to the query above, and that
is exactly why it needs its own check.**

A reservation writes a negative `-reserved` ledger row, and its release writes a
positive `+reserved` row under the **same** `ref` (`reserve_<uuid>`). When the release
never lands, nothing is left out of the sum in a way the reconciliation query can see:
the hold is gone from the wallet and no offsetting credit was written, so
`balance_idr = SUM(ledger.delta_idr)` still holds and the query returns **no row**.
Money is debited against a request that was never billed, and no alert fires.

**Detection is a separate query with its own invariant.** It counts `reserve_*`
refs that have a negative row and **no positive row under the same ref**. **Zero rows
is the invariant.** A non-zero count means a release failed to land — most often the
fire-and-forget `ReservationGuard::drop` (`server/src/routes/proxy.rs`) never reached
the database, or the process died between the response and the settlement commit.

**WHICH IMPLEMENTATION RUNS, because this paragraph used to name the wrong one.**
It said `unpaired_hold_rows` (`server/src/db.rs`), and that function is **called by
nothing except its own tests**. Three copies of this detector exist:

| Implementation | Runs |
| --- | --- |
| `db::unpaired_hold_rows` (`server/src/db.rs`) | **never in production** — tests only |
| `bin/hold-sweep.rs` | **never in production** — not shipped in the server image, the scheduler logs it as NOT WIRED |
| `.docker/maintenance/entrypoint.sh` (`run_hold_sweep`) | nightly, and it is **the one** the scheduler runs |
| `tools/alert/check-alerts.sh` (`HOLDS_OVER`) | nightly, fires the `stranded_hold` alert |

**There are FOUR copies, and this table previously named the wrong one.** It listed
`bin/hold-sweep.rs` as "nightly, via the maintenance scheduler". The binary is not
shipped in the server image and the scheduler says so on every run: `NOT WIRED
hold-sweep` is not what it prints — it prints `WIRED hold-sweep - REPORT-ONLY, SQL
inline in this entrypoint, using the SAME predicate as
server/src/bin/hold-sweep.rs`. What runs is the **inline SQL**, not the binary. This is
the same error this paragraph already documents for retention, where
`data-retention.md` said the sweeps were enforced by `usage-purge.rs` and the binary
that enforces them does not run either.

All three that run carry their own SQL rather than calling the Rust ones, which is the
same duplication that hid a second reservation rule until it was measured. The
`db.rs` copy is left in place because it states the invariant in the place a reader
looks for it — but it is not the detector, and an operator told to run it would find
nothing scheduled.

**The sweep now RUNS on a schedule.** The maintenance scheduler
(`.docker/maintenance/`) runs `run_hold_sweep` every night, alongside the retention
sweeps — and its CI smoke seeds a stranded hold and a HEALTHY paired reservation, then
asserts the detector fires on the first and stays silent on the second. It is
**report-only**, matching the binary's default: the scheduler detects, names the
accounts and refs, and exits non-zero, while crediting a hold back stays a deliberate
operator action (`hold-sweep --release`). Silently correcting a stranded hold is the
same invisible-money anti-pattern this whole section describes.

**The bound.** A hold may legitimately be unpaired at the moment of a sweep — the
request is still streaming, and the upstream timeout is deliberately **per read rather
than total** so a long healthy stream is left alone. So the sweep reads
`circuit_breaker.request_timeout_seconds` (`config/apikita.toml`) rather than assuming
the request has finished, and **a hold still unpaired at the next sweep is an
incident**: investigate the release path and credit the account if the hold is lost.

| Rule | Bound |
| --- | --- |
| Younger than the per-read upstream timeout | Normal — the request may still be in flight |
| Still unpaired at two consecutive sweeps | **Incident — investigate, then credit** |

## Request tracing

One `request_id` generated at ingress and carried through:

```
client -> Rust API -> upstream provider -> response
```

- **Return it to the client** in a header. A support conversation that starts with
  "what is your request id" is dramatically shorter than one that does not.
- Include it in every log line for that request, including upstream errors.

## Uptime monitoring

External check against `GET /health` from outside the platform.

**`/health` must not check upstream providers** — an upstream outage would then
look like a dead server and trigger a restart loop. It checks the process and the
database only.

## Open items

- [ ] Metrics backend (self-hosted Prometheus vs a hosted service — cost matters).
- [x] Confirm the other five never-log classes are absent — **audited**, not assumed.
      A sweep of every `info!`/`warn!`/`error!`/`debug!`/`trace!` call site in
      `server/src` for `token`, `key` and `cookie` finds only `key_id` (a uuid),
      `token_hash` and `api_key_id` — never a raw API key, a session token, or a
      cookie value. The one place a raw key exists (`auth.rs` exchanges it) hashes
      before any write. This is a point-in-time audit, so the TEST above is what
      holds the line going forward.
- [ ] Log retention period.
- [ ] Alert delivery channel (Telegram is already in the stack).
- [ ] Whether to expose a public status page.
- [x] Confirm prompts/completions are never logged — **now enforced by a test, not a
      review.** `a_customer_prompt_never_reaches_the_log` (`server/src/routes/proxy.rs`)
      drives a real request carrying a sentinel prompt through the handler with a
      capturing `tracing` subscriber installed, and asserts the sentinel NEVER
      appears. It captures at **TRACE**, not info, because the promise breaks in
      practice when someone adds a `debug!` while investigating a bug — which is
      exactly what a one-off review cannot prevent. It carries a positive control
      (the request must still be observable by model/account) so it cannot pass on a
      server that logs nothing at all.