# Admin & Operations Surface

Operator capabilities. Referenced as a gap in
[`abuse-runbook.md`](abuse-runbook.md) — suspending an account is a required action with
no specified mechanism.

> Model: [`architecture/identity.md`](architecture/identity.md) · Schema:
> [`website/02-data-model.md`](website/02-data-model.md)

## The governing rule

**Every admin action goes through the same API and the same ledger as normal
traffic. There is no back door.**

| Anti-pattern | Why it breaks |
| --- | --- |
| `UPDATE wallets SET balance_idr = ...` by hand | The ledger no longer sums to the balance. Reconciliation fails forever |
| Editing PocketBase records directly for money | Identity system holding money state; the two stores diverge |
| Deleting rows to "fix" data | Destroys the audit trail that makes disputes resolvable |

**An adjustment is a ledger row, not an edit.** The `ledger.reason` enum has
`adjustment` for exactly this. It also carries a `refund` value that **nothing
writes** — the platform does not refund (see
[Money actions](#money-actions--the-ones-that-need-care)).

## What exists today

**Five admin routes are implemented** (`server/src/routes/admin.rs`, mounted in
`server/src/routes/mod.rs`). Everything else in this document is **planned, not
built** — the status of each item is marked where it appears.

| Route | Handler | Effect |
| --- | --- | --- |
| `GET /api/admin/accounts` | `admin::list_accounts` | Bounded listing with `q`/`status` filters. Same fields as the single view, per row. Never a credential |
| `GET /api/admin/accounts/:id` | `admin::get_account` | Read-only: `status`, `is_operator`, `created_at`, `balance_idr`, live session count, live key count |
| `GET /api/admin/accounts/:id/audit` | `admin::get_account_audit` | The account's `admin_audit` trail, newest first. **The read side of a table that was write-only** — the trail the console now shows |
| `POST /api/admin/accounts/:id/suspend` | `admin::suspend_account` | `status='suspended'` + revoke every live session and API key, in one transaction |
| `POST /api/admin/accounts/:id/resume` | `admin::resume_account` | `status='active'`; does not restore credentials |
| `POST /api/admin/accounts/:id/restore` | `admin::resume_account` | **Alias of `/resume`** — same handler, same behaviour, two spellings |

**`/resume` and `/restore` are the same action.** Both are mounted
(`routes/mod.rs`), both call `resume_account`, and both write the same
`admin_audit` row with `action='resume'`. Pick either; there is no difference to
reason about.

**The admin UI now exists** at `website/src/pages/admin/index.astro` — the
operator console (`/admin`). It is the front end for the routes above: browse and
filter the account list, look up an account by id, read its state, and suspend or
resume it. The launch checklist's "Read-only + suspend" is satisfied end to end,
API and UI.

**Why the listing was added.** The single-account route requires the caller to
already know a UUID, so before the listing an operator had no way to *find* an
account — the console worked only for ids captured elsewhere. The listing is the
index the lookup route assumes. It deliberately does **not** search by email:
email lives in the identity provider, and the admin surface must not become a
second identity store. The self-action rule does **not** apply to reading the
list (reading is not acting, and hiding the operator's own row would miscount
their inventory); it still applies to every action.

The UI adds **no capability**: it calls the same routes with the same session
cookie, and every rule below (operator flag, self-action refusal, 401/403/409)
is enforced server-side. The console is *gated* on the `is_operator` flag that
`GET /api/me` now returns, but that gate is cosmetic — a non-operator who
reaches it anyway gets a 403 from the server, and the console never renders a
control the server would reject.

### Auth and status codes

The guard is identical in every handler and runs in this order **before the
target is read**:

1. **Resolve the actor from the session cookie** via
   `resolve_account_from_cookie` — the same parser and resolver every cookie
   endpoint uses. No admin password, no admin header, no query parameter.
2. **Require `accounts.is_operator = true`.**
3. **Refuse self-action.**
4. Only then read the target.

| Case | Status | Code |
| --- | --- | --- |
| Missing, unknown, revoked or expired session cookie | **401** | `unauthenticated` |
| Authenticated, but `is_operator = false` | **403** | `forbidden` |
| Operator acting on their own account | **403** | `forbidden` |
| Operator, target id absent | **404** | `not_found` |
| Operator, target in the wrong state (suspend a non-`active`, resume a non-`suspended`) | **409** | `conflict` |

**403, not 401 and not 404.** The caller *is* authenticated — the cookie resolved
to a real account, so this is an authorization failure, not an authentication one
([`error-model.md`](error-model.md) §"401 vs 403"). A 401 would tell an operator
with a perfectly good session to re-login for a permissions problem; a 404 would
lie about a resource this surface exists to administer.

**`forbidden` is a new code.** The crate's `AppError` has no 403 that is honest
for this case — `model_not_allowed` is about a model allowlist, and
`wrong_credential_type` is documented as reserved and never emitted
([`error-model.md`](error-model.md) rule 4: adding a code is fine, changing one's
meaning is not).

**Steps 1–3 precede the target lookup, so a non-operator gets the same 403 for an
existing and an absent id** — the response cannot be used to enumerate account
ids. An operator is already trusted with every account and gets an honest 404.

**The self-action refusal applies to the read-only lookup too** — one rule, no
special case. An operator's own account is at `GET /api/me`.

### Suspend, precisely

One transaction does all three things, and the audit row is written inside it:

1. `accounts.status = 'suspended'`;
2. revoke **every** live session for the account;
3. revoke **every** live API key for the account.

- **Exactly one `admin_audit` row**: `action='suspend'`, `target_type='account'`,
  `target_id=<account uuid>`, `detail` = JSONB counts
  (`sessions_revoked`, `keys_revoked`, `status_from`, `status_to`).
- **A failed suspend writes no audit row** — the row is in the same transaction as
  the effect, so a rollback cannot leave a trail for an action that did not happen.
- The target row is locked `FOR UPDATE` first, so two concurrent suspends
  serialise and the second sees `suspended` and returns 409 rather than both
  reporting a successful revocation.
- **After suspend, a new login is refused anyway** — the login path rejects a
  non-`active` account outright, so it cannot mint a session, and a session is the
  only way to mint a key.

### Resume, precisely

Sets `status='active'` and writes one `admin_audit` row (`action='resume'`).

**It does not resurrect revoked credentials, and that is deliberate.** The
response reports `sessions_revoked: 0` and `keys_revoked: 0` explicitly, so the
caller can see that nothing was handed back. The reasoning:

- The credentials were revoked because the account was abusive or compromised.
  Restoring the *status* is a statement about the account's standing, not about
  the trustworthiness of credentials already in the wild at suspension time.
- "Unrevoke" would mean resurrecting a session token the customer may no longer
  hold, and re-enabling a key whose plaintext existed only once at creation — so
  the account could not be made whole by it anyway.
- The customer re-authenticates (fresh session) and issues a fresh key: one
  deliberate step, clean audit trail.

### Known limitation: revocation is not global

`proxy::invalidate_key_cache` is **process-local**. An instance behind the load
balancer keeps its cached copy of a revoked key until its TTL expires, so that
instance will keep honouring the key for up to `limits.key_metadata_cache_seconds`
(60s by default). This is the documented cost of the key-metadata cache
(`proxy.rs`), **not something the admin routes close**. Setting the config value
to 0 is the only way to make revocation immediate everywhere.

The revocation itself *is* durable in `api_keys.revoked_at`, which is what the
request path reads on a cache miss.

## Required capabilities

Inferred from the abuse runbook, ToS, and support scenarios:

| Action | Kind | Needed by |
| --- | --- | --- |
| Suspend / restore account | state | Abuse responses, ToS §7 |
| Revoke a user's key | state | Abuse response (revoke first, then assess) |
| View account, ledger, usage | read | Every billing question |
| View top-up and webhook history | read | Payment disputes |
| View abuse signals per account | read | Abuse detection |
| **Credit an adjustment** | **money** | Reconciliation fixes, goodwill |
| Force-logout (revoke sessions) | security | Account takeover |
| Cancel a pending link code | security | Linking abuse |
| Moderate a review | content | Review room |

**Refunding a top-up is not on this list and is not a capability.** The platform
does not refund — there is no endpoint to call and no operator action that moves
money back to a payer. A refund request is answered by the Terms of Service, not
by an operator. See [Money actions](#money-actions--the-ones-that-need-care).

## Access model

**Start with a single operator role.** Multi-role RBAC before you have employees is
speculative complexity.

| Role | Can |
| --- | --- |
| `user` | Their own account only |
| `operator` | Everything above, across accounts |

### Authentication

The admin surface is **the same API**, distinguished by an account flag — not a
separate service with its own auth.

- `accounts.is_operator BOOLEAN NOT NULL DEFAULT false` (excluded from all
  customer-facing responses).
- Same session mechanism as customers.
- **Not a separate admin password**, which becomes a shared secret.

**A separate admin app is a later decision.** The endpoints are the same either way;
a dedicated UI is a convenience, not an architecture.

### Why a flag and not a separate app

One code path for authorization means one place to audit. A separate admin service
duplicates auth, session handling, and revocation — and the duplication is where
the holes appear.

## Money actions — the ones that need care

> **Status: NOT IMPLEMENTED.** `/adjust` does not exist in
> `server/src/routes/mod.rs`. This is design for the "with revenue" phase, kept
> because the rules are the hard part. Until it exists, money actions are SQL by
> the owner, documented — see [Rollout](#rollout).

**One action moves money outside the Midtrans webhook: the adjustment.** It is
necessary, and it is dangerous.

**A refund is not a second one.** The platform does not refund: an inbound
Midtrans `refund` or `partial_refund` notification is acknowledged and **refused**
(200 with `{"status":"refund_not_supported"}`, logged at `error!`), and nothing is
written — the topup stays `settled`, no ledger row is appended, the wallet cannot
move. The refund code path is gone, so there is no implementation to call and no
operator action to reach for.

### Adjustment

Used for: reconciliation discrepancies, goodwill, correcting an operator mistake.

```json
POST /api/admin/accounts/:id/adjust
{ "delta_idr": -5000, "reason": "reconciliation fix", "note": "..." }
```

Rules:

1. **Writes a ledger row** with `reason='adjustment'`, delta, and `balance_after`.
2. **Atomic with the wallet update** — same transaction.
3. **Requires a note.** An unexplained adjustment is indistinguishable from theft
   during an audit.
4. **Logged with the operator's account id**, not just the affected account.

### Refund — not offered

**The platform does not refund, and no refund endpoint is planned.** There is no
`POST /api/admin/topups/:id/refund` to design against, because the refund code
path has been deleted.

An inbound Midtrans `refund` or `partial_refund` notification is **acknowledged
and refused**, never applied:

1. **200** with body `{"status":"refund_not_supported"}` — acknowledged so
   Midtrans does not retry a notification that can never succeed.
2. Logged at **`error!`**, so the refusal is visible.
3. **Nothing written.** The topup stays `settled`, no ledger row is appended, and
   the wallet cannot move.

**An unalerted refusal is silent**, which is indistinguishable from a refund bug —
so alert on that log line. It is the only signal that a refund was ever asked for.

The terms are non-refundable during operation, so a customer asking for money back
is answered by the Terms of Service — there is **no refund endpoint and no operator
refund action**. Do not confuse this with **wind-down**: if we close the service, an
operator *does* pay balances out, by the documented manual procedure in
[`wind-down.md`](wind-down.md). That is platform-initiated, not a refund on request. Note the
exposure that remains: a chargeback returns the money **at the rail** regardless of
the terms, and the platform's records will still show the credit. See
[`business/05-risk.md`](business/05-risk.md) and
[`website/04-payments.md`](website/04-payments.md).

## Non-money actions

**Implemented today: the first two rows only** (plus the read-only lookup). Every
other row is planned, not built.

| Endpoint | Status | Effect | Notes |
| --- | --- | --- | --- |
| `GET /api/admin/accounts/:id` | **built** | Read-only account view | Counts only; never a credential hash. Refuses self-action |
| `POST /api/admin/accounts/:id/suspend` | **built** | `status='suspended'` | **Revokes sessions and keys**, in one transaction, with one audit row |
| `POST /api/admin/accounts/:id/resume` | **built** | `status='active'` | **Alias: `/restore`** calls the same handler. Does **not** restore keys; the customer reissues |
| `POST /api/admin/keys/:id/revoke` | planned | `revoked_at` | Same as the user's own revoke — the customer route `POST /api/keys/:id/revoke` exists today |
| `POST /api/admin/accounts/:id/logout-all` | planned | Revoke all sessions | Takeover response — suspend already revokes sessions as a side effect |
| `DELETE /api/admin/link-codes/:id` | planned | Invalidate a pending code | |
| `POST /api/admin/reviews/:id/hide` | planned | Sets a hidden flag | **Never deletes**; see the review rules |

**Suspension must revoke sessions and keys.** Setting a status flag alone leaves a
live session and working keys — the account keeps working while appearing suspended.
This is the single easiest way to get the suspension wrong.

## Audit trail

**Every admin action writes an audit row.** Not optional, and not derived from logs.

**Definition and indexes: [`docs/website/02-data-model.md`](website/02-data-model.md)** (single
source). `admin_audit` holds: `id`, `operator_id` -> accounts (RESTRICT), `action`,
`target_type`, `target_id`, `detail` (JSONB), `created_at`.

- **`ON DELETE RESTRICT` on operator** — the trail outlives the operator's account.
- Written **in the same transaction as the effect**. An audit row for an action that
  rolled back is as bad as no audit row.
- Money actions store the amount and the resulting balance in `detail`.

## Safety rails

| Rail | Why |
| --- | --- |
| **Operator accounts cannot act on themselves** | Prevents self-crediting |
| **Money actions require a note** | Forces an explanation at the moment of intent |
| **Adjustments above a threshold require a second operator** | Cheap two-person rule; specify the threshold |
| **Suspension revokes sessions and keys atomically** | Otherwise it does not suspend |
| **No hard deletes, ever** | Closure is a status; withdrawal is a flag |
| **Admin endpoints excluded from customer API docs** | Reduces surface for probing |

**The two-person rule needs a decision:** below what amount is one operator enough?
A reasonable starting point is any adjustment above a few hundred thousand IDR.

## What the admin surface must NOT do

| Not | Why |
| --- | --- |
| Read customer prompts | We never store them; see [`data-retention.md`](data-retention.md) |
| View API keys in plaintext | Only hashes exist |
| Bypass limits or model allowlists | An operator's own key is just a key |
| Set a balance directly | Ledger only |
| Delete an account | Closure only; the ledger is retained |

## Observability for admin actions

| Signal | Why |
| --- | --- |
| Every action in `admin_audit` | The record |
| **Alert on any money action** | Rare by design; each one deserves attention |
| Alert on admin login from a new IP | Operator account compromise is total compromise |
| Daily summary of actions | Passive review catches drift |

See [`observability.md`](observability.md).

## Rollout

| Phase | Surface | Status |
| --- | --- | --- |
| **Launch** | Read-only + suspend/restore. Money actions via SQL by the owner, documented | **Routes and UI built** — the operator console at `/admin`; `/keys/:id/revoke` on the admin path is still planned (the customer route exists) |
| **With revenue** | Adjustments as an endpoint, with notes and audit. **Not refunds** — the platform does not refund | Not built |
| **Later** | Second-operator threshold, more roles, broader operator dashboards | Not built |

**Starting read-only is deliberate.** The dangerous actions are the money ones, and
they should be implemented once there is revenue to misfile — not on day one when
nobody but the owner is operating and a mis-keyed adjustment is easy to spot.

## Open items

- [x] Second-operator threshold **500,000 IDR**.
- [ ] Whether operators authenticate from a restricted IP range.
- [x] `is_operator` lives on `accounts` — a column, not a table.
- [ ] Abuse-signal dashboard: which metrics, where computed.
- [ ] Whether admin actions are exposed to the customer in their own audit view.