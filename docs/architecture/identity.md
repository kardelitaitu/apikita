# Identity & Accounts

How a person becomes an account, and how that account is reached from the website
and the Telegram bot.

> **Stack:** identity lives in **PocketBase** — still, until migration Phase 6
> replaces it in Rust; money lives in **embedded SQLite** (a file the API opens,
> not a service). See [`docs/architecture.md`](../architecture.md). This document
> covers the identity side; the money schema is in
> [`docs/website/02-data-model.md`](../website/02-data-model.md).

## The shape of the thing

An **account** owns the wallet. Identities link to it. A person can have several
identities and still have one balance.

```
        PocketBase                    SQLite (embedded)
   +---------------------+      +----------------------+
   | users (auth)        |      | accounts             |
   |   - google identity |<---->|   - pb_user_id       |
   |   - password identity|     |   - wallet balance   |
   +---------------------+      |   - api_keys         |
                                |   - sessions         |
   telegram_links (SQLite) -----+                      |
   +---------------------+      +----------------------+
```

**Keys and wallet hang off the account, never off an identity.** That is what
makes revocation account-wide by construction.

## Login methods

| Method | Handled by |
| --- | --- |
| **Google sign-in** (OAuth2) | PocketBase |
| **Email + password**, with verification and reset | PocketBase |

PocketBase is used deliberately: password hashing, verification, reset, OAuth2,
OTP, and MFA are solved problems, and re-implementing them in Rust would cost
weeks and add new ways to get security wrong.

**Rust issues its own session.** After PocketBase authenticates a user, the
browser exchanges the PocketBase token with the Rust API, which resolves
`pb_user_id` → `accounts` and issues an opaque session cookie (a row in
the SQLite `sessions` table). The PocketBase token is not used as the API credential, so
PocketBase stays off the request hot path.

## Table ownership

| Table | Store | Purpose |
| --- | --- | --- |
| `users` | PocketBase | Email, password hash, Google link, `verified` |
| `accounts` | SQLite | `pb_user_id` link, status |
| `sessions` | SQLite | Server-side sessions |
| `telegram_links` | SQLite | `telegram_id` → account |
| `link_codes` | SQLite | Telegram binding codes |

There is deliberately **no `credentials` table in SQLite.** PocketBase *is* the
identity store. Duplicating it would create two sources of truth about who
someone is. (The schema does carry an empty `identities` table — created for
Phase 6 and populated only when PocketBase is replaced. Until then it holds
nothing and PocketBase stays authoritative.)

## Invariants

1. **One account, many identities.** A person may sign in with Google *and* a
   password; both resolve to the same wallet.
2. **`api_keys.account_id` and `wallets.account_id` reference the account**, never
   an identity or a PocketBase id directly.
3. **SQLite is never authoritative about who a user is.** It stores a reference
   (`pb_user_id`) and nothing else about identity.
4. **Never hard-delete a PocketBase user.** The wallet is in SQLite and nothing
   cascades across the boundary. Set `status = 'closed'`.
5. **`link_codes` are single-use and expiring.** Binding happens on redemption
   only.
6. **Unlinking Telegram removes one row.** It must never delete the account or
   the wallet.

## Email handling — what PocketBase does for us

The four cases below are the classic pre-hijacking vectors. **PocketBase already
defends against them**, verified in `apis/record_auth_with_oauth2.go`
(lines ~340–362). The attacks and the defenses:

### 1. Google first, then email+password with the same address

PocketBase finds the record by email and links. If the record was **unverified**,
the OAuth link **randomises its password** — so an attacker who pre-registered the
address is evicted. Handled.

### 2. Email+password first, then "Sign in with Google"

Auto-links by email. If the record was **unverified**, it again randomises the
password **and deletes other OAuth links**, so one unverified record can hold at
most one OAuth link. Handled.

### 3. Google returns an unverified email

**This is the one residual gap.** PocketBase matches on the returned email without
visibly checking the provider's `email_verified` claim.

**Mitigation:** only enable OAuth2 providers that guarantee verified addresses.
Google does. If a provider is added later that does not, this becomes an
account-takeover path.

### 4. Password reset on a Google-only account

PocketBase's reset flow can set a password on any record with an email — including
one created via Google. So a Google-only account can gain a password.

**Practical risk is low** — resetting requires control of the email, and Google
accounts have a verified one. Decide whether to accept it or block it. If blocked,
it is app-level work: check for an existing password before honouring a reset.

### The trap that disables all of it

Every defense above is gated on `!Verified()`. **If a record is marked verified
before the owner proves the address, the protections stop applying.**

**Rule: `verified` may only be set by PocketBase's own flows.**

- Permitted: the user clicking a verification link or entering an OTP; PocketBase
  setting it on a matching OAuth email.
- Forbidden: bulk imports setting it, admin/support toggling it to unblock
  someone, migrations backfilling it.

Enforcement (a hook rejecting client-sourced changes, plus auditing every
transition) is specified in
[`docs/website/05-security-decisions.md`](../website/05-security-decisions.md) D3.

## Sessions

Server-side sessions in SQLite, not JWTs. This is what makes logout real.

- Login creates a `sessions` row; the cookie carries an opaque random value and
  only its hash is stored.
- **Logout revokes the row** → immediate, on every surface.
- **Sign out everywhere** revokes all rows for the account.
- Expired rows are swept on a schedule.

Schema and indexes: [`docs/website/02-data-model.md`](../website/02-data-model.md).

## Telegram linking

> Channel structure, rooms, and the bot's behaviour:
> [`docs/telegram/README.md`](../telegram/README.md). This section covers the
> linking mechanism only.

Telegram is **not** an OAuth2 provider, so PocketBase cannot model it. It is a
custom table in SQLite.

### Flow

1. User is signed in on the website and selects **Connect Telegram**.
2. Rust issues a 6-digit code: single-use, short TTL (5 min), bound to
   `account_id`.
3. User sends `/link <code>` in the bot.
4. The bot calls Rust to redeem it: valid, unexpired, unused → binds
   `telegram_id` → account.
5. Code is marked used and invalidated.

**Why this proves both sides:** the code demonstrates control of the web account;
the chat demonstrates control of the Telegram account. An email typed into the bot
would prove neither.

### Rate limiting is mandatory

A 6-digit code is brute-forceable, and a successful guess attaches an attacker's
Telegram to a **funded wallet**.

- Cap attempts per account and per IP.
- Invalidate on use, on unlink, and on expiry.
- Issuing a new code invalidates the previous one.

This is the highest-risk endpoint in the Telegram surface.

## Top-ups

Wallet credits happen on the **server webhook only** — never the client callback,
never the amount in the payload. Idempotency is by `order_id` (unique in
SQLite). A user may legitimately top up from both surfaces in one day, so
idempotency is per-order, not per-account.

See [`docs/website/04-payments.md`](../website/04-payments.md).

## Reconciliation — the hazard of two stores

Identity and money now live in different systems, so they can drift.

- **Schedule a job** that checks every `accounts.pb_user_id` still exists in
  PocketBase, and alerts on orphans.
- **Never hard-delete a PocketBase user.** A deleted user with a funded wallet is
  money nobody can reach.
- An auth outage does **not** lock out existing users — sessions live in SQLite,
  and a PocketBase that answers **5xx or 429** is treated as an outage (500,
  retryable) rather than as a rejected token. Only a **4xx refusal** is a 401.
  Collapsing the two would log every signed-in customer out during a 30-second
  PocketBase restart.

## Open questions

- [x] Session lifetime: **30d absolute / 7d idle** — [`decisions.md`](../decisions.md).
- [ ] Accept or block a password created by reset on a Google-only account (case 4).
- [ ] Whether email is mandatory for a Telegram-linked account. Recommended: yes,
      or losing Telegram loses the wallet.
- [ ] Reconciliation job: schedule, and where alerts go.
- [ ] 2FA for accounts holding a large balance.
