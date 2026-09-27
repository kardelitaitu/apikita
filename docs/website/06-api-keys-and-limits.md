# 06 — API Keys, Models, and Usage Limits

How a customer creates a key, what it can call, and how limits are enforced.

## Scope

A key is the customer's credential against the proxy. It:

- **authenticates** the caller
- **selects which models** the caller may use
- **caps spend, tokens, and rate** over a period
- **can expire** on a date

The website **configures and displays** these. The proxy
([`server/`](../../server/README.md)) **enforces** them.

> **The most important rule in this document:** if the proxy does not enforce a
> limit, the UI showing it is a lie. Never ship a limit field the proxy ignores.

## Model access

The proxy exposes public model names from
[`config/apikita.toml`](../../config/apikita.toml):

| Public name | Notes |
| --- | --- |
| `flash` | Primary. V4.1-Flash, vision, 1M ctx |
| `deepseek-v4-flash` | Legacy alias, same upstream cost and price |

"All models" means the key's allowlist includes **every currently enabled model**.
It is stored as an explicit list, not a boolean, so adding a model later does not
silently grant access to every existing key.

### Why a list, not an "all" flag

A boolean `allow_all` would grant new, possibly more expensive models to every
existing key the moment they are enabled. An explicit list makes that an
intentional change. The UI may offer "all models" as a **shortcut that writes the
full current list**, which reads as simple but behaves as explicit.

A key with an empty allowlist can call **nothing** — deny by default.

## Limits

All optional. A limit of `0` or `null` means "no limit of this kind".

| Field | Type | Period | Enforced by |
| --- | --- | --- | --- |
| `spend_limit_idr` | number | rolling period | proxy |
| `token_limit` | number | rolling period | proxy |
| `rate_limit_rpm` | number | per minute | proxy |
| `expires` | date | absolute | proxy |

### Why spend is the primary limit

A token limit treats 1 output token and 1 cache-read token as equal. They differ
by **~200x in cost** (see
[`docs/business/02-pricing.md`](../business/02-pricing.md)). A token limit is
therefore a poor proxy for money.

**`spend_limit_idr` is the limit that matters**; token and rate limits are
secondary. Offer spend first in the UI.

### Period semantics

Options were rolling window vs calendar month. **Use a rolling window** (e.g. the
trailing 30 days) unless a calendar period is explicitly needed:

- No month-boundary cliff where a user's budget resets and they burst.
- No timezone ambiguity (Indonesia is WIB/WITA/WIT — three zones).
- Harder to explain to a user than "this month", so **show the window explicitly**
  in the UI: "12,340 of 50,000 IDR used in the last 30 days".

## Data model

The `api_keys` table lives in SQLite (see [02-data-model.md](02-data-model.md)):

| Field | Type | Notes |
| --- | --- | --- |
| `id` | UUID | PK, generated server-side |
| `account_id` | UUID | FK → `accounts(id)` ON DELETE CASCADE |
| `key_hash` | text | **SHA-256 hash only** (UNIQUE, indexed) |
| `prefix` | text | Display, e.g. `apk_live_a1b2` |
| `label` | text | User-supplied label |
| `models` | jsonb | Array of allowed public model names |
| `spend_limit_idr` | bigint | 0 = unlimited |
| `token_limit` | bigint | 0 = unlimited |
| `rate_limit_rpm` | integer | 0 = unlimited |
| `expires_at` | timestamptz | null = never |
| `last_used_at` | timestamptz | Updated lazily by proxy |
| `revoked_at` | timestamptz | null = active |
| `created_at` | timestamptz | Audit timestamp |

### API endpoints (Rust backend)

Managed via cookie-authenticated endpoints in [`server/api-spec.md`](../server/api-spec.md):

| Operation | Endpoint | Access & Rule |
| --- | --- | --- |
| List / View | `GET /api/keys` | Session cookie — **never returns `key_hash`** |
| Create | `POST /api/keys` | Session cookie — returns plaintext key once |
| Update | `PATCH /api/keys/:id` | Session cookie — validates limits & models |
| Revoke | `POST /api/keys/:id/revoke` | Session cookie — sets `revoked_at = now()` |

`key_hash` is never returned by the Rust API handler.

## Key format

```
apk_live_<43 chars base62>
```

- `apk_live_` prefix makes keys greppable in logs and secret scanners — a leaked
  key in a public repo is recognizable.
- The stored form is a **hash** of the full key. Lookup hashes the presented
  value and matches. Use a fast hash (SHA-256) here, **not** a password hash: this
  is a high-entropy random token, not a guessable password, and the lookup is on
  the hot path of every proxied request.
- `prefix` stores the first ~12 chars for display. It is not secret.

**The plaintext key is shown exactly once**, at creation. It is never stored and
never retrievable. Lost key → revoke and create a new one. The UI must say this
before the user dismisses it.

## Enforcement (in the proxy)

This is the part the website cannot do for you.

### Per-request sequence

```
1. hash presented key -> look up api_keys by key_hash
2. not found / revoked / expired    -> 401
3. model not in key.models          -> 403
4. rate_limit_rpm exceeded          -> 429 + Retry-After
5. spend_limit_idr or token_limit   -> 402
   exceeded for the window
6. wallet balance sufficient        -> else 402
7. proxy the request
8. record usage against the key and the account
```

**Order matters.** Authenticate, then authorize (model), then throttle (rate),
then check money. Checking the wallet before the model allowlist leaks that a
model exists to a key not permitted to use it.

### Caching

The proxy must **not** query the database on every request — that would put the
database on every token's hot path. SQLite made the query itself far cheaper
(there is no network hop), but it did not remove the reason: **SQLite has one
writer at a time for the whole database**, so a read on every token is still
serialising against the write lock.

- Cache key metadata (limits, model list, revoked state) with a **short TTL**
  (e.g. 30–60 seconds).
- **Consequence to accept and document:** revoking a key, or lowering a limit, is
  **not instant**. It takes effect within the TTL at the latest.
- The UI must not promise instant revocation. Say "takes effect within a minute".
- For emergency revocation where seconds matter, rotate the account or use a
  push-based invalidation. Out of scope initially; note it.

### Usage accounting

Usage must be recorded against **both** the key and the account:

- **Key** — for the key's own `spend_limit_idr` window.
- **Account** — for the wallet balance and `usage_daily`.

Because the token classes are priced differently, store **three counters**
(input, cache-read, output), not one. A single counter makes the spend limit and
the invoice impossible to reconcile. See
[02-data-model.md](02-data-model.md) on `usage_daily`.

### Failure mode to avoid

If the proxy records usage **after** responding to a stream, a crash mid-stream
loses the record and the customer got free tokens. Record on stream completion,
and reconcile from upstream usage on a schedule as a backstop.

## UI — key management

### Create key

Fields: label, models (default: all current), spend limit, token limit, rate
limit, expiry. Only label is required; the rest default to unlimited.

**Defaults matter.** A key with no limits is the common case and must be one
click. Do not force a limit on a user who does not want one.

### List keys

| Column | Notes |
| --- | --- |
| Label | |
| Prefix | `apk_live_a1b2…` |
| Models | "All" or a count |
| Spend | "12,340 / 50,000 IDR (30d)" or "Unlimited" |
| Last used | |
| Status | Active / Revoked / Expired |

### Detail / edit

Show current-window usage against each limit. Allow raising limits immediately;
**lowering** should warn that it may cut off in-flight usage within the TTL.

### Revoke

Confirm, then immediate from the UI's perspective. State the TTL caveat in the
confirmation — do not claim instant.

### Show-once modal

On creation, display the full key with:

- a copy button
- an explicit warning that it will not be shown again
- a "I've saved it" acknowledgement before dismissal

## Limits vs. wallet balance

Two different ceilings, and users will confuse them:

| | Scope | Set by |
| --- | --- | --- |
| **Wallet balance** | the whole account, all keys | top-ups and usage |
| **Key spend limit** | one key | the user |

A key limit can only be **more restrictive** than the wallet. If the wallet is
empty, every key is dead regardless of its own limit. The UI should make the
relationship visible, not present them as unrelated numbers.

## Open items

- [x] Exclude `key_hash` from list responses — enforced server-side by Rust `GET /api/keys`.
- [x] Cache TTL **60s** — see [`docs/decisions.md`](../decisions.md).
- [x] Spend limit returns **402** — see [`docs/decisions.md`](../decisions.md).
- [x] Rolling window **30 days** — `config/apikita.toml` `[limits]`.
- [x] Key creation: **10/day per account** (`decisions.md`).
