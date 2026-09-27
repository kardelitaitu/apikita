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
| Account | `GET /api/me`, `GET /api/usage`, `GET /api/usage/recent`, `GET /api/topups`, `GET /api/export` | cookie |
| Keys | `GET/POST /api/keys`, `PATCH /api/keys/:id`, `POST /api/keys/:id/revoke` | cookie |
| Wallet | `POST /api/topups` | cookie |
| Telegram | `POST /api/telegram/link-code`, `DELETE /api/telegram` | cookie |
| Reviews ⚠ | `GET /api/reviews` (read), `POST /api/reviews` (**bot token only**) | cookie / bot |
| Webhooks | `POST /webhooks/midtrans` | **signature** |
| Live | `GET /events` (SSE) | cookie |
| Proxy | `POST /v1/chat/completions` | **API key** |
| **Bot** | `POST /api/bot/link` ✅, `GET /api/bot/account` ⚠, `GET /api/bot/reviews/mine` ⚠, `POST /api/bot/notify-topup` ⚠ | **bot token** |
| **Admin** | `GET /api/admin/accounts/:id`; `POST /api/admin/accounts/:id/suspend`; `POST /api/admin/accounts/:id/resume` (**alias `/restore`**) | **cookie + operator flag** |
| Ops | `GET /health` | none |

**⚠ MARKED ROUTES ARE DESIGNED, NOT BUILT.** Four endpoints in the table above have a
schema, a documented contract, and **no handler**: `GET`/`POST /api/reviews`,
`GET /api/bot/account`, `GET /api/bot/reviews/mine` and `POST /api/bot/notify-topup`.
They are not missing by accident — the whole reviews and top-up-feed flow is driven by
the Telegram **bot**, and `docs/launch-checklist.md:211` records that the bot itself is
still design-only, so their HTTP halves have nothing to exercise them. The tables they
need have existed since the initial migration (`reviews`, `review_history`,
`review_sessions`).

They are marked because this table describes the CURRENT surface, and an integrator who
reads it would call `/api/reviews`, get a 404, and conclude the **server** was broken.
The distinction is enforced by a test
(`the_spec_marks_exactly_the_designed_but_unbuilt_routes_as_designed` in
`server/src/routes/mod.rs`), so an endpoint cannot be mounted without this document
being updated with it.

**Two authentication schemes, deliberately separate:**

- **Cookie session** — the dashboard. For humans.
- **API key** — `/v1/*`. For programs. Never accept a cookie on `/v1/*`.

Keeping them distinct prevents a browser session from being usable as an API
credential, which would let a leaked cookie spend money.

**Presenting the wrong one is a bad credential, not an authorization failure:** a
cookie on `/v1/*`, or a Bearer key on a cookie endpoint, returns 401
`unauthenticated`. See [`docs/error-model.md`](../error-model.md).

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
  "status": "active",
  "is_operator": false
}
```

**This is the polling fallback** when SSE drops. Keep it cheap.

`is_operator` is a **rendering hint, not authorization**: it lets the browser
decide whether to show the admin entry points at all, instead of presenting links
that answer `403`. Every admin route independently re-checks
`accounts.is_operator` server-side (`admin.rs::require_operator`), so a forged
value here grants nothing.

### `GET /api/usage?from=&to=`

Daily buckets from `usage_daily`. **Three token classes returned separately** —
never summed. Cache-read is priced ~200x below output; a merged total cannot be
reconciled against an invoice.

Both bounds are ISO `YYYY-MM-DD`; **anything else is a `422`** naming the field in
`details.field`. **An absent or empty bound is unbounded** — it does not constrain the
window. **Both ends are inclusive**, so `from == to` returns exactly that one day.
Only when neither parameter is present is the window the last 30 buckets; a bounded
range that matches nothing returns `[]`, never that default. **No maximum span is
enforced.**

### `GET /api/usage/recent?limit=`

The last N **metered requests** for the account, newest first — the detail behind
the dashboard's "Recent requests" panel. Source is **`usage_events`**, one row per
billed request, written by the settlement transaction. `usage_daily` cannot serve
this: it is an aggregate with no model and no per-request rows.

`limit` defaults to 20 and is clamped to 1–100. Each row is
`{id, api_key_id, model, input_tokens, cache_read_tokens, output_tokens, cost_idr, created_at}`.
The three token classes are returned **separately**, never as one total, because
each is priced differently. `api_key_id` is null when the key was deleted after
the request (`ON DELETE SET NULL`). **Only this account's rows are ever returned**;
nothing exposes a prompt, a completion or a key's plaintext.

### `GET /api/export`

The account's **own** data as one JSON document — the export
[data-retention.md](../data-retention.md) specifies under "Access and deletion
requests". Scope is fixed by that document's IN/OUT table, and the rule is
**metadata, not secrets**:

- **In**: account, wallet, ledger rows, top-ups, `usage_daily`, `usage_events`,
  and API-key **metadata** (prefix, label, models, limits, expiry).
- **Out**: `key_hash`, `token_hash`, `pb_user_id`, `snap_token`, session rows,
  IP hashes and the Telegram chat id. **`admin_audit` is also out** — whether
  operator actions reach the customer is a separate open decision
  ([admin-surface.md](../admin-surface.md) Open items), which this does not
  pre-empt.

Every query is bound to the cookie-resolved account id, so a customer can only
export their own rows.

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
{ "topup_id": "uuid", "order_id": "topup_<uuid>", "snap_token": "...", "environment": "sandbox" }
```

`environment` is the Midtrans environment the server used — exactly `sandbox` or
`production` — so the browser can check that the Snap environment it was built
for matches, since the two are configured on separate platforms.

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
any previous code for that account — the predecessor row is **deleted** in the same
transaction as the insert, so at most ONE code is ever live per account (two live
codes would double an attacker's chance per guess).

Capped by `limits.link_code_issuance_per_hour` (default 10) over the codes the
account has issued, so a farm cannot keep thousands in flight.

### `DELETE /api/telegram`

Unlinks Telegram. **Removes one row — never the account or the wallet.** The
handler deletes from `telegram_links` only; the wallet and the ledger are asserted
untouched by test.

### `POST /api/bot/link` — redemption (bot token)

The bot calls this to redeem a code typed in chat. Body: `{code, telegram_id}`.

**This is the highest-risk endpoint in the Telegram surface**, because a successful
guess attaches an attacker's chat to a **funded wallet**. Its safety rests on three
properties, each pinned by a test rather than by intent:

| Property | Why it matters | Test |
| --- | --- | --- |
| **Every attempt counts, including failures** | The attack IS a stream of failures, so a counter that advanced only on success would never fire | `failed_guesses_are_counted_until_the_cap_refuses_even_a_correct_code` |
| **Every refusal is byte-identical** | Distinguishing "expired" from "unknown" turns a blind 10^6 search into a walk over the few hundred codes live right now | `every_kind_of_bad_code_produces_the_same_refusal` |
| **Single-use holds under concurrency** | One conditional `UPDATE … WHERE used_at IS NULL`, so two redemptions cannot both claim the row | `a_code_is_single_use` |

Rate-limited **per IP** by `limits.link_redemption_per_hour` (default 20) over
attempts, counted in `link_redemption_attempts` by **salted IP hash, never the raw
address**. Computed, not asserted: at 20/hour a host gets ~1.7 guesses inside one
5-minute code window, so the expected time to hit a *specific* account's live code
is **~5.7 years**. The TTL is what makes the cap bite — only one code per account is
live, and it rotates — while the cap is what makes the TTL survivable. A refused attempt is not recorded, so a throttled attacker cannot extend
their own lockout or grow the table without bound. The per-account cap above does
not cover this: an attacker cycling the code space touches no account at all.

**Response: 200 with `{"status":"invalid_code"}` for every unsuccessful
redemption** — wrong, expired, used, malformed and unknown alike. A 4xx would
invite the bot's transport to retry a guess, which is the opposite of the cap's
purpose. Success returns `{"status":"linked","account_id":…}`.

The bot token is required and compared in **constant time**. **A missing
`TELEGRAM_BOT_TOKEN` refuses rather than allows** — with no configured secret
there is no way to distinguish the bot from an attacker, so the endpoint fails
closed. A cookie is not a bot credential.

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
- `order_id` is unique in SQLite — the database enforces idempotency.
- Handle `settlement`/`capture` as credit; `deny`/`cancel`/`expire` as terminal.
- **`refund`/`partial_refund` are refused, never applied** — see below.
- Respond 200 fast; slow responses get retried, compounding idempotency needs.
- Signature verification is why the Midtrans server key is server-only.

**`refund` / `partial_refund`: the platform does not do refunds.**

These two statuses are acknowledged with **HTTP 200** and the explicit body:

```json
{"status": "refund_not_supported"}
```

and logged at `error!`. The notification changes **nothing**:

- `topups.status` stays `settled` — it is not set to `refunded`.
- **No `ledger` row is written**, so `wallets.balance_idr` cannot move.
- **Nothing is published to the realtime stream** — no `balance` event is emitted,
  because no balance changed.
- `evaluate_payment_status` returns the named `PaymentAction::RefundRefused` for
  these two statuses. That variant is **deliberately distinct from
  `PaymentAction::Unrecognised`**: refusing a refund is a policy answer, not a
  status we failed to parse. The wallet-debiting refund path is removed.

200 is the right status even though we are refusing: a non-2xx makes Midtrans retry
a notification that can never succeed. The exposure this creates is a business
problem, not a code path — it is recorded in
[docs/website/04-payments.md](../website/04-payments.md).

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

- PocketBase's realtime is useless here — our data is in SQLite. This is ours.
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
delivery dispute. Let an in-flight request finish, then refuse the next one.
(This contradicts the whitepaper's Phase 2 — the whitepaper is wrong here, see
[`docs/business/05-risk.md`](../business/05-risk.md) R1.)

**The balance never goes negative — "let it finish" does NOT mean overdraft.**
`docs/decisions.md` §Money settles overdraft as **not permitted** (the
`CHECK (balance_idr >= 0)` on `wallets` is the backstop), and the stale
`allow_negative_balance_overdraft` flag that used to suggest otherwise was
**removed** — see [plans/proxy-hot-path-audit.md §5.2 F2](../plans/proxy-hot-path-audit.md).
What actually happens at settlement when the real cost exceeds what was reserved:
`settle_partial_usage` (`db.rs`) clamps the debit to the balance available
(`clamp_debit(cost_idr, available_idr)`), so the wallet is charged only what it
holds and the shortfall is logged at `error!` as a company loss. The earlier
"briefly overdraws" phrasing described a behaviour the code does not have.

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

### Implemented routes

These routes exist in `server/src/routes/mod.rs` and are the whole admin surface
today. The **operator console** at `/admin`
(`website/src/pages/admin/index.astro`) is the UI over them — browse/search,
lookup, plus suspend/resume. The UI adds no capability; it calls these routes
with the same session cookie and every guard is enforced here, server-side.

| Endpoint | Handler | Effect |
| --- | --- | --- |
| `GET /api/admin/accounts` | `admin::list_accounts` | Bounded listing. Query: `q` (matches account id / PocketBase id), `status`, `limit` (1–100, default 25), `offset`. Returns `{accounts, limit, offset}` |
| `GET /api/admin/accounts/:id` | `admin::get_account` | Read-only: `status`, `is_operator`, `created_at`, `balance_idr`, live session count, live key count. Never a credential hash |
| `GET /api/admin/accounts/:id/audit` | `admin::get_account_audit` | The account's `admin_audit` trail, newest first. Same operator + self-action guard as the read-only view. Query: `limit` (1–200, default 50) |
| `GET /api/admin/audit` | `admin::list_recent_audit` | **Cross-account** recent operator actions, newest first. Served by `admin_audit_recent_idx`. Query: `limit` (1–200, default 50) |
| `POST /api/admin/accounts/:id/suspend` | `admin::suspend_account` | `status='suspended'`; **revokes sessions and keys atomically**; one `admin_audit` row in the same transaction |
| `POST /api/admin/accounts/:id/resume` | `admin::resume_account` | `status='active'`; audits it; does not restore keys |
| `POST /api/admin/accounts/:id/restore` | `admin::resume_account` | **Alias of `/resume`** — same handler, same `action='resume'` audit row |
| `GET /api/admin/metrics` | `health::operator_metrics` | Operational counts: `server_errors`, `responses`, `error_rate` (null when nothing has been served), `unhealthy_models` (models with no usable endpoint), and `retention` (whether any age-based table holds a row past its window, naming it). Backs the `error_rate`, `all_providers_unhealthy` and `db_disk` alerts |

**`/resume` and `/restore` are two spellings of one action.** Both are mounted;
neither is deprecated. Pick either.

**Response shape.** The read-only route returns
`{account_id, status, is_operator, created_at, balance_idr, live_sessions, live_keys}`,
and the listing returns the same fields per row inside `accounts`.

**The listing exists because lookup-by-id requires knowing a UUID.** It is the
index the single-account routes assume. `q` is matched case-insensitively
against the account id and the PocketBase id — **not email**, which lives in the
identity provider and must not be duplicated here. `%` and `_` in `q` are
escaped with an explicit `ESCAPE` clause, so a literal percent sign searches for
that sign rather than matching every row. An unknown `status` value matches
nothing rather than erroring: an empty page is a normal answer, not a bad
request. `limit` is clamped to 1–100 and `offset` to ≥ 0, and both are echoed so
a client can page deterministically.
Suspend/resume return
`{account_id, status, sessions_revoked, keys_revoked}`; resume reports both counts
as `0` explicitly, so the caller can see that nothing was handed back.

### Enforcement order

Identical in every handler, and **steps 1–3 run before the target is read**:

1. Resolve the actor from the **session cookie** via `resolve_account_from_cookie`.
2. Require `accounts.is_operator = true`.
3. Refuse self-action.
4. Only then read the target.

| Case | Status | Code |
| --- | --- | --- |
| Missing, unknown, revoked, expired or **idle** cookie | 401 | `unauthenticated` |
| Authenticated, `is_operator = false` | 403 | `forbidden` |
| Operator acting on their own account (including the read-only lookup) | 403 | `forbidden` |
| Operator, target id absent | 404 | `not_found` |
| Operator, target in the wrong state | 409 | `conflict` |

Because steps 1–3 precede the target lookup, **a non-operator gets the same 403
for an existing and an absent id** — the response cannot enumerate account ids.
An operator gets an honest 404 for an absent target.

### Session lifetime: 30 days absolute, 7 days idle

Both halves are enforced ([decisions.md](../decisions.md), Gate 3):

- **Absolute** — `sessions.expires_at`, seeded at login as `now + absolute_days`
  (`config/apikita.toml`, `[sessions] absolute_days = 30`).
- **Idle** — `sessions.last_seen_at`, the last time the credential was actually
  **used**. Resolving a session cookie on any cookie-authenticated endpoint moves
  it to now; a session idle for longer than `idle_days` (7) is refused as
  unauthenticated, and a **refused** session does not move the timestamp.

Three consequences worth stating rather than discovering:

1. **The activity write is on the cookie endpoints only.** `/v1/*` authenticates
   API keys, not cookies, so it never touches `sessions` — the proxy hot path is
   unaffected.
2. **A caller cannot tell "idle" from "dead".** Both are 401 `unauthenticated`,
   the same rule already applied to revoked and expired sessions.
3. **An `idle_days` at or above `absolute_days` is inert by construction**,
   because `expires_at` is seeded from the same login instant. Only an idle bound
   *below* the absolute lifetime — the shipped 7 against 30 — changes behaviour,
   so a misconfiguration can never cut a session shorter than the register
   promises.

**403 is authorization, not authentication** — the caller is authenticated, we
know who they are, and they may not do this ([error-model.md](../error-model.md),
the 401-vs-403 table). **`forbidden` is a new stable code**, added under
error-model rule 4 ("code values are permanent; adding is fine"). It is not
`model_not_allowed` (a model allowlist) and not `wrong_credential_type` (reserved,
never emitted).

**Resume does not resurrect credentials** — deliberate, not an oversight. The
credentials were revoked because the account was abusive or compromised; restoring
the *status* says nothing about credentials already in the wild. The customer
re-authenticates and reissues a key.

**Known limitation:** `proxy::invalidate_key_cache` is **process-local**. An
instance behind the load balancer keeps honouring a revoked key for up to
`limits.key_metadata_cache_seconds` (60s default). That residual window is the
documented cost of the key-metadata cache and is **not closed by these routes**.

### Planned, not built

| Endpoint | Effect |
| --- | --- |
| `POST /api/admin/accounts/:id/adjust` | Money: ledger row, `reason='adjustment'` |
| ~~`POST /api/admin/topups/:id/refund`~~ | **Not planned.** The wallet-debiting refund machinery was removed — see [the webhook section](#post-webhooksmidtrans). A refund happens at the rail, never against the wallet |
| `POST /api/admin/keys/:id/revoke` | `revoked_at` (the customer route `POST /api/keys/:id/revoke` exists today) |
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

Rollout: **read-only and suspend/restore are built, API and UI; the admin
key-revoke route is still planned.** Money actions arrive with the first revenue,
not on day one.

## Cross-cutting rules

| Rule | Why |
| --- | --- |
| Never accept a cookie on `/v1/*` | A leaked cookie must not be able to spend |
| Never accept an amount from the client for a credit | Only the webhook creates money |
| Return only needed fields | Prevents `key_hash` or internal ids leaking |
| Key metadata cached ≤60s | The DB must not be on every token's hot path |
| Rate-limit: login, key creation, link-code redemption, top-up creation | All are abuse targets |
| Money is `INTEGER` IDR end to end | No floats, ever. `BIGINT` was the Postgres type; `STRICT` SQLite tables admit only `INTEGER` |
| Log every wallet mutation with actor and reason | The `ledger` table is that record |
| Admin endpoints require the operator flag | Not a separate credential, and never a shared secret |
| Admin endpoints return 403 `forbidden`, not 401, for a non-operator | The caller is authenticated; this is authorization, not authentication |
| Admin revocation is process-local | `invalidate_key_cache` cannot reach other instances; the cache TTL is the residual window |

## Open items

- [x] `402` for a spend-limit breach — see [`decisions.md`](../decisions.md).
- [x] SSE auth: **session cookie on a same-site subdomain** — see [`decisions.md`](../decisions.md).
- [x] Key metadata cache TTL: **60s** — see [`decisions.md`](../decisions.md).
- [ ] Internal bot endpoint for link-code redemption — auth scheme.
- [ ] Whether the proxy splits into a separate service later.