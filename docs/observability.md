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
| **Ledger drift** | `ledger_balance_drift_idr != 0` | Money is wrong. Freeze changes, investigate |
| **API down** | `/health` failing 2 min | Restart/investigate; auth is down |
| **Relay 5xx** | relay returns 502/504 | The relay is up but the backend is not |
| **Relay down** | external check fails | Total outage — single point of failure |
| **All providers unhealthy** | circuit open on every endpoint | Requests failing; check upstream |
| **Balance negative** | `balance_idr < 0` | Should be impossible (CHECK constraint). A bug |
| **DB disk >80%** | volume usage | Usage rows growing; check retention |
| **Error rate >5%** | 5 min window | Investigate |

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
- [ ] Log retention period.
- [ ] Alert delivery channel (Telegram is already in the stack).
- [ ] Whether to expose a public status page.
- [ ] Confirm prompts/completions are never logged, in code review.