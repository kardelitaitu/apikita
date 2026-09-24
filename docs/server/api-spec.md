# Server — API Specification

The Rust service on Northflank. It is the **entire backend**: auth exchange,
wallet, keys, limits, payments webhook, live updates, and the LLM proxy.

> Supersedes the scope in [`server/README.md`](../../server/README.md), which
> described a pure routing proxy. The stack is decided: see
> [`docs/architecture.md`](../architecture.md).

## Surface, in one table

| Group | Endpoints | Auth |
| --- | --- | --- |
| Auth | `POST /auth/exchange`, `POST /auth/logout`, `POST /auth/logout-all` | cookie / none |
| Account | `GET /api/me`, `GET /api/usage`, `GET /api/topups` | cookie |
| Keys | `GET/POST /api/keys`, `PATCH /api/keys/:id`, `POST /api/keys/:id/revoke` | cookie |
| Wallet | `POST /api/topups` | cookie |
| Telegram | `POST /api/telegram/link-code`, `DELETE /api/telegram` | cookie |
| Reviews | `GET /api/reviews` (read), `POST /api/reviews` (**bot token only**) | cookie / bot |
| Webhooks | `POST /webhooks/midtrans` | **signature** |
| Live | `GET /events` (SSE) | cookie |
| Proxy | `POST /v1/chat/completions` | **API key** |
| **Bot** | `POST /api/bot/link`, `GET /api/bot/account`, `GET /api/bot/reviews/mine`, `POST /api/bot/notify-topup` | **bot token** |
| **Admin** | `POST /api/admin/*` (see below) | **cookie + operator flag** |
| Ops | `GET /health` | none |

**Two authentication schemes, deliberately separate:**

- **Cookie session** — the dashboard. For humans.
- **API key** — `/v1/*`. For programs. Never accept a cookie on `/v1/*`.

Keeping them distinct prevents a browser session from being usable as an API
credential, which would let a leaked cookie spend money.

---

## Auth

### `POST /auth/exchange`

Exchanges a PocketBase auth token for a Rust session cookie. Called once after
login.

```json
// request
{ "pb_token": "<pocketbase jwt>" }

// 200 response
{ "account_id": "uuid", "balance_idr": 50000 }
// sets: Set-Cookie: session=<opaque>; HttpOnly; Secure; SameSite=Lax
```

Server verifies the token with PocketBase, resolves `pb_user_id` → `accounts`
(creating the account on first login), inserts a `sessions` row, and returns the
cookie. **The PocketBase token is not stored**; only the session is.

Errors: `401` invalid token.

### `POST /auth/logout`

Revokes the current session row. `204`. Clears the cookie.

### `POST /auth/logout-all`

Revokes **every** session for the account. `204`. This is real revocation, not
just discarding a token — other devices are logged out immediately.

---

## Account

### `GET /api/me`

Everything the dashboard needs on load.

```json
{
  "account_id": "uuid",
  "balance_idr": 50000,
  "usage_today": {
    "input_tokens": 0,
    "cache_read_tokens": 0,
    "output_tokens": 0,
    "cost_idr": 0
  },
  "telegram_linked": false,
  "status": "active"
}
```

**This is the polling fallback** when SSE drops. Keep it cheap.

### `GET /api/usage?from=&to=`

Daily buckets from `usage_daily`. **Three token classes returned separately** —
never summed. Cache-read is priced ~200x below output; a merged total cannot be
reconciled against an invoice.

### `GET /api/topups?limit=`

Top-up history: amount, status, created, settled. Never exposes Midtrans secrets.

---

## Keys

### `GET /api/keys`

List active and revoked keys. **Never returns `key_hash`.**

```json
[{
  "id": "uuid", "prefix": "apk_live_a1b2", "label": "prod",
  "models": ["flash"], "spend_limit_idr": 50000,
  "spend_used_idr": 12340, "rate_limit_rpm": 60,
  "expires_at": null, "last_used_at": "2026-01-01T00:00:00Z",
  "revoked_at": null
}]
```

`spend_used_idr` is computed over the configured window, from `usage_daily`.
Returning it here saves the UI a second call.

### `POST /api/keys`

```json
// request
{
  "label": "prod",
  "models": ["flash", "deepseek-v4-flash"],
  "spend_limit_idr": 50000,
  "token_limit": 0,
  "rate_limit_rpm": 60,
  "expires_at": null
}
```

**`models` omitted means all currently enabled models** — resolved server-side to
an explicit list, never stored as "all".

Response **includes the plaintext key, exactly once**:

```json
{ "id": "uuid", "key": "apk_live_<43 chars>", "prefix": "apk_live_a1b2" }
```

The server stores only the SHA-256 hash. This response is the only time the key
exists in plaintext anywhere.

Errors: `422` on an empty `models` list resolving to nothing; `400` on unknown
model names.

### `PATCH /api/keys/:id`

Raises or lowers limits and changes allowed models. **Raising takes effect
immediately; lowering is subject to the proxy's metadata cache TTL (≤60s).** The
response should say so, so the UI can warn honestly.

### `POST /api/keys/:id/revoke`

Sets `revoked_at`. Idempotent. `204`. Revocation is account-wide in effect — a
key issued via Telegram dies here too.

---

## Wallet

### `POST /api/topups`

Creates a top-up and returns a Midtrans Snap token.

```json
// request
{ "amount_idr": 50000 }

// 200 response
{ "topup_id": "uuid", "order_id": "topup_<uuid>", "snap_token": "..." }
```

Server validates the amount against the configured minimums (**first deposit vs
re-top-up differ** — see
[`docs/business/03-financial-model.md`](../business/03-financial-model.md)),
inserts a `topups` row with status `pending`, and calls Midtrans.

**The client never sends a status, and never credits a balance.** Only
[the webhook](#post-webhooksmidtrans) creates money.

---

## Telegram

### `POST /api/telegram/link-code`

Issues a 6-digit code: single-use, 5-minute TTL, bound to the account. Invalidates
any previous code for that account.

### `DELETE /api/telegram`

Unlinks Telegram. **Removes one row — never the account or the wallet.**

The bot calls a separate internal endpoint to redeem codes. **Redemption must be
rate-limited per account and per IP**; a 6-digit code is brute-forceable and a
guess attaches an attacker's Telegram to a funded wallet.

---

## Reviews

**Reviews are written from Telegram only.** The website may *display* an aggregate;
it can never create or edit a review. Attempting to post one from a browser session
must fail — reviews are the Telegram channel's contribution, and mixing writers
makes "who reviewed" ambiguous.

### `GET /api/reviews`

Public aggregate. Read-only, cookie-authenticated.

```json
{
  "average": 4.6,
  "count": 23,
  "customer_count": 18
}
```

**Returns the aggregate, never the list.** Publishing individual reviews with
usernames invites retaliation against reviewers — see
[`docs/telegram/README.md`](../telegram/README.md).

There is deliberately **no `"mine"` field** on this endpoint, and the dashboard
cannot read an individual review.

**The bot reads its user's own review through `GET /api/reviews/mine`** — a
separate bot-token endpoint, so the dashboard's aggregate view stays aggregate.

### `POST /api/reviews`

**Bot token only. Never a cookie.** The dashboard must not reach this endpoint.

```json
{ "telegram_id": "...", "rating": 5, "body": "fast and cheap" }
```

- **Upsert**, not append. One review per user; a second submission edits the first.
- `rating` required, 1-5. `body` optional, max 1000 chars.
- **Never silently truncate `body`** — reject with an error the bot can show.
- Before overwriting, copy the old values into `review_history` **in the same
  transaction**.
- `is_customer` is computed server-side from whether a settled top-up exists.
  **Never accept it from the caller.**
- The account is resolved from `telegram_id`: `account_id` if linked, else
  `telegram_id` alone.

Errors: `403` if called with a cookie instead of a bot token.

### `POST /api/reviews/withdraw`

Bot token only. Sets `withdrawn_at` — **does not delete**. A deleted row would
free the unique slot and let the user submit a second review.
## Webhooks

### `POST /webhooks/midtrans`

The only source of wallet credits.

```
1. read body
2. recompute signature: SHA512(order_id + status_code + gross_amount + server_key)
   (see [docs/website/04-payments.md](../website/04-payments.md))
3. compare in constant time; mismatch -> 401, log, stop
4. look up topups by order_id; unknown -> 404, log, stop
5. compare amount against the STORED row; mismatch -> reject, log
6. if status already settled -> 200, do nothing   (idempotent)
7. in ONE transaction:
     topups.status = settled
     wallets.balance_idr += amount
     insert ledger row
8. 200 quickly
```

**Non-negotiable:**

- The amount comes from **our stored row**, never the payload.
- `order_id` is unique in Postgres — the database enforces idempotency.
- Handle `settlement`/`capture` as credit; `deny`/`cancel`/`expire` as terminal;
  **`refund`/`partial_refund` as debit** even though the policy is
  non-refundable — disputes arrive uninvited, and an unhandled status corrupts the
  ledger.
- Respond 200 fast; slow responses get retried, compounding idempotency needs.
- Signature verification is why the Midtrans server key is server-only.

---

## Live updates

### `GET /events` (SSE)

Authenticated by session cookie. Streams balance and usage changes for that
account.

```
event: balance
data: {"balance_idr": 37500}

event: usage
data: {"input_tokens": 1200, "cache_read_tokens": 8000, "output_tokens": 400, "cost_idr": 812}

: heartbeat        (every 20-30s, keeps proxies from closing it)
```

- PocketBase's realtime is useless here — our data is in Postgres. This is ours.
- Emit on: webhook settlement, usage settlement, key changes.
- **Heartbeat is required.** Cloudflare and intermediaries close idle streams, and
  a silently dropped stream looks like "the balance stopped updating".
- **The frontend must fall back to polling `/api/me`** when the stream drops and
  show a stale indicator. Never render an old balance as current.

---

## Proxy

### `POST /v1/chat/completions`

OpenAI-compatible. Authenticated by **API key in `Authorization: Bearer`**, not by
cookie.

**Enforcement order — do not reorder:**

```
1. hash key, look up api_keys (cache, <=60s TTL)
2. missing / revoked / expired           -> 401
3. model not in key.models               -> 403
4. rate_limit_rpm exceeded               -> 429 + Retry-After
5. spend_limit_idr or token_limit hit    -> 402
6. wallet balance insufficient           -> 402
7. pre-flight reservation (worst case)   -> 402 if it exceeds the balance
8. proxy upstream, stream through
9. on completion: record usage, settle wallet, emit SSE
```

**Authenticate, then authorize (model), then throttle, then check money.** Checking
the wallet before the allowlist leaks the existence of models a key may not use.

**Streaming:** pipe upstream → client. Do not buffer whole bodies.

**On balance exhaustion, prefer rejecting at pre-flight over cutting mid-stream.**
With non-refundable funds, a truncated answer is the most likely source of a
delivery dispute. Let an in-flight request finish even if it briefly overdraws;
refuse the next one. (This contradicts the whitepaper's Phase 2 — the whitepaper
is wrong here, see [`docs/business/05-risk.md`](../business/05-risk.md) R1.)

**Usage is recorded on stream completion.** A crash mid-stream loses the record and
the customer got free tokens — so reconcile against upstream usage on a schedule as
a backstop.

---

## Ops

### `GET /health`

200 only when the process **and the database** are reachable. **Must not check
upstream LLM providers** — an upstream outage would otherwise look like a dead
server and trigger a restart loop. Unauthenticated.

---

## Bot (internal)

**Called by the Telegram bot with a bot token, never by a browser.** These exist
because the bot needs capabilities the dashboard deliberately does not have.

| Endpoint | Purpose |
| --- | --- |
| `POST /api/bot/link` | Redeem a `/link` code: bind `telegram_id` → account |
| `GET /api/bot/account` | Balance and usage for a `telegram_id` |
| `GET /api/bot/reviews/mine` | The caller's own review, for `/review show` |
| `POST /api/bot/notify-topup` | Called internally after a settled webhook, to post the feed |

### `POST /api/bot/link`

```json
{ "telegram_id": "123456", "code": "482913" }
```

- **Rate-limited per Telegram user and per IP.** A 6-digit code is
  brute-forceable and a guess attaches an attacker's Telegram to a funded wallet.
  This is the highest-risk endpoint in the system.
- Codes are single-use and expire in 5 minutes; issuing a new one invalidates the
  previous code for that account.
- **Re-attribution runs in the same transaction**: a review submitted before
  linking is moved to the account, or the same person gets two review rows.
- Errors: `404` unknown code, `409` account already linked to another Telegram,
  `422` expired or used.

### Authentication

A single bot token in `Authorization: Bearer`, compared in constant time.

**Not a user session, and not the operator flag.** If the token leaks, rotate it —
it can link accounts, so treat it as a credential of the same class as the
Midtrans server key.

## Admin

**Every admin action goes through this same API — there is no separate service or
back door.** Full design and the safety rules: [`admin-surface.md`](../admin-surface.md).

Authorization is the `accounts.is_operator` flag, plus a normal session cookie.
**Not a separate admin credential.**

| Endpoint | Effect |
| --- | --- |
| `POST /api/admin/accounts/:id/suspend` | `status='suspended'`; **revokes sessions and keys atomically** |
| `POST /api/admin/accounts/:id/restore` | `status='active'`; does not restore keys |
| `POST /api/admin/accounts/:id/adjust` | Money: ledger row, `reason='adjustment'` |
| `POST /api/admin/topups/:id/refund` | Money: `status='refunded'` + ledger debit |
| `POST /api/admin/keys/:id/revoke` | `revoked_at` |
| `POST /api/admin/accounts/:id/logout-all` | Revoke all sessions |
| `DELETE /api/admin/link-codes/:id` | Invalidate a pending code |
| `POST /api/admin/reviews/:id/hide` | Hidden flag — **never deletes** |

### Rules that are not optional

1. **An operator cannot act on their own account.** Prevents self-crediting.
2. **Money moves only via `ledger`**, never a direct balance edit. An
   adjustment is a ledger row with a reason and a note.
3. **Every action writes `admin_audit` in the same transaction as its effect.**
4. **Suspension must revoke sessions and keys.** A status flag alone leaves a working
   session and live keys — the account keeps running while appearing suspended.
5. **Money actions require a note.** An unexplained adjustment is indistinguishable
   from theft during an audit.
6. **Nothing here can read prompts or plaintext keys** — neither is stored.

Rollout: **read-only, suspend/restore, and key revoke at launch.** Money actions
arrive with the first revenue, not on day one.

## Cross-cutting rules

| Rule | Why |
| --- | --- |
| Never accept a cookie on `/v1/*` | A leaked cookie must not be able to spend |
| Never accept an amount from the client for a credit | Only the webhook creates money |
| Return only needed fields | Prevents `key_hash` or internal ids leaking |
| Key metadata cached ≤60s | The DB must not be on every token's hot path |
| Rate-limit: login, key creation, link-code redemption, top-up creation | All are abuse targets |
| Money is `BIGINT` IDR end to end | No floats, ever |
| Log every wallet mutation with actor and reason | The `ledger` table is that record |
| Admin endpoints require the operator flag | Not a separate credential, and never a shared secret |

## Open items

- [x] `402` for a spend-limit breach — see [`decisions.md`](../decisions.md).
- [x] SSE auth: **session cookie on a same-site subdomain** — see [`decisions.md`](../decisions.md).
- [x] Key metadata cache TTL: **60s** — see [`decisions.md`](../decisions.md).
- [ ] Internal bot endpoint for link-code redemption — auth scheme.
- [ ] Whether the proxy splits into a separate service later.