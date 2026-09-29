# Error Model

One consistent error shape across the API, and the specific meanings of each
status. Clients code against this, so it is a contract.

> Endpoints: [`docs/server/api-spec.md`](server/api-spec.md)

## Response shape

**Every error returns the same JSON.** No bare HTML error pages, no empty bodies.

This is **enforced against the running binary** by the CI smoke step, not merely asserted
here. It was not always true: until recently an unrouted path returned an **empty body**
(axum's default, because the router had no fallback) and a malformed body on
`POST /auth/exchange` returned axum's **plain text** — on the one auth verb with no
credential guard, and therefore the one where a malformed body actually reaches the
extractor. Both now route through `AppError`, so the shape, the `code` vocabulary and the
`request_id` come from the one place that defines them.

The extractor's own detail text is deliberately **not** echoed into `message`: on
`/auth/exchange` the body being parsed is a PocketBase token, and `InvalidRequest` sends
its string to the client verbatim.

```json
{
  "error": {
    "code": "insufficient_balance",
    "message": "Balance too low for this request. Top up to continue.",
    "request_id": "req_7f3a...",
    "details": { "required_idr": 1200, "balance_idr": 400 }
  }
}
```

| Field | Purpose |
| --- | --- |
| `code` | **Stable, machine-readable.** Clients switch on this. Never changes meaning |
| `message` | Human-readable, shown to users. May change freely |
| `request_id` | Correlates with logs. Support asks for this |
| `details` | Optional, endpoint-specific context |

**`code` is the contract; `message` is not.** Translating a message or
matching on its text is a bug waiting to happen.

## Status codes and their meanings

| Status | Code | Meaning | Client action |
| --- | --- | --- | --- |
| 400 | `invalid_request` | Malformed body or params | Fix the request |
| 401 | `unauthenticated` | No or bad credential | Sign in / fix the key |
| 401 | `key_revoked` | Key exists but was revoked | Issue a new key |
| 401 | `key_expired` | Key past its expiry | Issue a new key |
| 402 | `insufficient_balance` | Wallet cannot cover it | Top up |
| 402 | `key_limit_exceeded` | Key's own spend/token limit hit | Raise the limit or new key |
| 403 | `model_not_allowed` | Model absent from the key's allowlist | Use an allowed model |
| 403 | `forbidden` | Authenticated, but not permitted — e.g. a non-operator calling an `/api/admin/*` route, or an operator acting on their own account | Do not retry; you do not have this permission |
| 403 | `wrong_credential_type` | **Reserved, never emitted** — a cookie on `/v1/*`, or a key on a cookie endpoint, returns 401 `unauthenticated` | Treat as 401 |
| 404 | `not_found` | No such resource | Check the id |
| 409 | `conflict` | Duplicate (e.g. Telegram already linked) | Reconcile state |
| 422 | `validation_failed` | Well-formed but invalid (e.g. a top-up amount below the provider minimum, or a date that is not `YYYY-MM-DD`) | Fix the value |
| 429 | `rate_limited` | Too fast | Back off; respect `Retry-After` |
| 500 | `internal_error` | Our bug | Retry once, then report |
| 503 | `no_upstream_available` | Every upstream is unhealthy | Retry after `Retry-After` |

## 401 vs 403 — the distinction that matters

| Case | Status | Why |
| --- | --- | --- |
| No credential | 401 | Who are you? |
| Bad credential | 401 | We cannot identify you |
| Wrong credential type — a cookie on `/v1/*`, or a key on a cookie endpoint | 401 | Same: it is not a credential we can use |
| Valid credential, model denied | **403** | We know you; you may not do this |

**Do not return 403 for a bad key.** A valid-but-unauthorized request and an
unauthenticated one are different, and clients handle them differently (re-login vs
change the request).

**A wrong credential type on the two API auth schemes is a bad credential, and
returns 401.** A cookie sent to `/v1/*`, or a key sent to a cookie endpoint, fails as
`unauthenticated`. `wrong_credential_type` stays defined and reserved, but nothing
emits it: a 403 here would confirm to the caller — including one holding a stolen
cookie or key — that the credential is genuine and merely misapplied, while the
caller's next step is the same either way. One 401 for every credential failure costs
nothing operationally and tells an attacker nothing.

## 402 vs 429 for limit exhaustion

**Resolved:** a **key's** spend/token limit returns **402**; the **rate** limit
returns **429**.

| Limit | Status | Because |
| --- | --- | --- |
| Key spend or token budget | **402** | It is a billing condition; retrying does not help |
| Wallet balance | **402** | Same |
| Requests per minute | **429** | Transient; retrying later *will* work |

**A client must not retry a 402.** It will fail identically until the user tops up.
Returning 429 for a spend limit invites clients to retry-loop against a permanent
condition.

## `Retry-After`

Included on **429** and **503**. **Seconds, not a date** — a date requires clock
synchronisation the client may not share.

### 429 — rate limited

**= seconds until the caller's window frees.** Computed from the limiter's state,
not guessed:

```
retry_after = window_seconds - (now - window_start)
```

Rounded **up** to the next whole second, and never below **1**. A client that
honours it exactly succeeds on the retry.

**Never send a fixed value for 429.** A constant teaches callers to ignore it: too
short and they hammer, too long and they abandon a working key.

### 503 — no upstream available

**= the shortest remaining cooldown across the endpoint pool**, i.e. the earliest
moment a retry could plausibly succeed.

```
retry_after = min over endpoints of (cooldown_until - now)
```

With one provider that is its current cooldown: 30s initially, doubling to a cap of
900s (see `config/apikita.toml` `[circuit_breaker]`). So a 503 may carry
`Retry-After: 900` — correct, and honest that the wait is long.

**Floor it at 1 second.** A zero or negative value is a malformed header.

### What this means for clients

| Header | Meaning |
| --- | --- |
| `Retry-After: 2` | Transient — retry almost immediately |
| `Retry-After: 900` | The provider is down; **back off properly** rather than looping |

**A long `Retry-After` is a feature, not a bug.** It is the honest answer to "when
will this work", and it is what stops a well-behaved client from making the outage
worse.

## Upstream errors

**Do not pass an upstream error body through verbatim.** It leaks provider
identity, may contain their internal ids, and couples the client contract to a
provider we may switch away from.

| Upstream | We return |
| --- | --- |
| 400 (bad request from client) | 400 `invalid_request`, our wording |
| 401/403 (our provider key is bad) | 500 or 503 — **this is our problem, not the client's** |
| 429 (provider throttled) | Retry another key first; only surface 503 if all fail |
| 5xx | Try another endpoint; else 503 |

**An upstream auth failure must never surface as a client 401.** The customer's key
is fine; our provider key is not. Returning 401 would send them chasing a
nonexistent problem with their own credentials.

## Streaming errors

Once a stream has started, the HTTP status is already sent and cannot change.

Emit a terminal error event in the stream:

```
event: error
data: {"error":{"code":"upstream_failed","message":"...","request_id":"..."}}
```

**Two codes travel this way, and neither has a status in the table above.** The
status line is long gone, so they exist only as this frame.

| Code | Meaning | Client action |
| --- | --- | --- |
| `upstream_failed` | The upstream stream **failed** — a transport or protocol error mid-answer | The answer is lost. Do not retry blindly: a retry appends a second answer and bills for both |
| `upstream_incomplete` | The upstream stream **ended cleanly but did not complete** — it closed before any usage block arrived, so the answer is truncated | Keep the partial text, but mark it incomplete. Never present it as a finished answer |

**These are different conditions.** `upstream_failed` means the upstream stream
died mid-answer; `upstream_incomplete` means it ended without error but stopped
before the answer completed. A client that collapses them cannot tell a cut-off
answer from a broken connection, and cannot say which one happened to support.

**Then close cleanly.** Do not silently stop — a client cannot distinguish a
finished answer from a truncated one, and will treat a partial response as
complete. See [`docs/failover.md`](failover.md) on mid-stream failure.

## Rules

1. **Never leak internals** — no stack traces, no SQL, no provider names in
   customer-facing errors.
2. **Never return 200 with an error in the body.** Use the status.
3. **Always include `request_id`.** It is the support conversation's starting
   point.
4. **`code` values are permanent.** Adding is fine; changing meaning is not.
5. **Validation errors name the field** — `details.field` — so the UI can highlight
   it without parsing prose.
6. **Distinguish caller error from provider error.** A malformed request must not
   trip a circuit breaker.

## Open items

- [ ] Whether to include a `docs_url` per code.
- [x] Retry-After derivation: **429 from the window, 503 from the shortest cooldown, floored at 1s.**
- [ ] Error message locale — currently English only.