# 05 — Security Decisions

> **Superseded on stack — read [`docs/architecture.md`](../architecture.md) first.**
>
> The system is: Cloudflare Pages (frontend), **edge relay**, Rust on Northflank
> (API + proxy), **embedded SQLite** (money), **PocketBase** (identity only).
>
> | # | Status | Implement from |
> | --- | --- | --- |
> | D1 — wallet not client-writable | **Applies** | This document, and the schema |
> | D2 — token revocation | **Background only** | [`architecture.md`](../architecture.md) §Sessions — **not this section** |
> | D3 — pre-hijacking / verification | **Applies** | This document |
>
> **D2 describes PocketBase's mechanism, which we do not use.** Read it to
> understand why the earlier claim was wrong, not to implement.

Resolutions for three structural security issues raised during design.

> **Correction notice.** An earlier draft claimed PocketBase "has no token
> revocation". **That is wrong** — it has native per-record revocation. Our stack
> does not rely on it (SQLite sessions are used instead), but the claim was
> incorrect and is corrected here.

### How the threats map to the new stack

| Threat | Was handled by | Now handled by |
| --- | --- | --- |
| User sets their own balance | PocketBase API rules | **Rust authorization** + SQLite `CHECK` |
| Stolen session token | PocketBase `tokenKey` rotation | **SQLite `sessions`** row revocation |
| Account pre-hijacking | PocketBase's unverified-record logic | Unchanged — still PocketBase, still gated on `!Verified()` |
---

## D1 — Wallet balance must not be client-writable

### Problem (historical — this was the PocketBase design)

When money was going to live in PocketBase, its API rules were the obstacle: they
are **row-level, not field-level**, so a rule letting a user update their own auth
record let them write **every field on it** — including `balance_idr`.

**This is no longer the design.** Money is in SQLite. The section is kept because
the reasoning explains why the `wallets` table exists separately and why the
`CHECK` constraint is load-bearing.

### Solution — money is unreachable from the client

Money lives in **embedded SQLite** (a file the API owns), and the client has no
database credential at all.

| Store | Client access | Mutated by |
| --- | --- | --- |
| PocketBase (identity) | own profile fields | PocketBase flows |
| **SQLite (money)** | **none — no client DB access** | **Rust only** |

Every wallet read and write goes through the Rust API, which:

1. Resolves the session cookie to an account.
2. Authorizes the operation against **that** account.
3. Selects only the fields the response needs — never `SELECT *` into a DTO.

The database backstop:

```sql
balance_idr BIGINT NOT NULL DEFAULT 0 CHECK (balance_idr >= 0)
```

### Why the current design is stronger

The PocketBase design would have needed a workaround: a user who could edit their
profile could edit their balance. SQLite has no such limitation — the client has
**no database credential at all**, so the problem does not exist.

The remaining risk is a Rust authorization bug, which is why the `CHECK`
constraint stays: it is the last line that refuses a negative balance even if the
code is wrong.

### Residual risk

The Rust API is the only path to money. It must therefore:

- Never accept an amount from the client for a credit. Credits come from the
  verified webhook only ([04-payments.md](04-payments.md)).
- Rate-limit wallet mutations.
- Log every mutation with actor, reason, and resulting balance.

---

## D2 — Token revocation (BACKGROUND ONLY — do not implement)

> **Not our mechanism.** This section explains how PocketBase revokes tokens. Our
> stack does **not** use PocketBase tokens as API credentials — Rust issues opaque
> **server-side sessions** stored in SQLite, so logout deletes a row and revokes
> immediately, with no TTL window.
>
> **Implement from [`docs/architecture.md`](../architecture.md) §Sessions, not from here.**
> The material below is retained only to explain why the earlier claim was wrong
> and what the alternative would have been.

### Why it is kept

### Correcting the earlier claim

PocketBase tokens are stateless JWTs, and there are no server-side sessions — so
"logout" is discarding the token client-side, which is true. But **tokens are not
un-revocable.**

### How it actually works

From `core/record_tokens.go`:

```go
// line 56
key := (m.TokenKey() + m.Collection().AuthToken.Secret)
```

The signing key is the **collection secret plus a per-record `tokenKey`**.
Every auth record carries its own random `tokenKey` field, mixed into the
signature of its tokens.

From `core/record_model.go` (lines 1451-1456), on every save:

```go
// ensure that the token key is regenerated on password change or email change
if lastSavedRecord.TokenKey() == e.Record.TokenKey() &&
    (lastSavedRecord.Get(FieldNamePassword) != e.Record.Get(FieldNamePassword) ||
        lastSavedRecord.Email() != e.Record.Email()) {
    e.Record.RefreshTokenKey()
}
```

**Changing a password or email rotates `tokenKey`, which changes that user's
signing key, which instantly invalidates every token previously issued to them** —
on every device. Other users are unaffected.

### The three levels PocketBase offers (not used)

| Scope | Mechanism | Available |
| --- | --- | --- |
| **One user, all devices** | rotate `tokenKey` (automatic on password/email change) | Built in |
| **One user, explicit** | call `record.RefreshTokenKey()` from a hook and save | Needs a hook |
| **All users** | rotate the collection's `AuthToken.Secret` | Built in (dashboard) |

### What the PocketBase approach would have required (not used)

```go
// "Sign out everywhere", on a compromised account, or after suspicious activity.
record.RefreshTokenKey()
app.Save(record)
```

`RefreshTokenKey()` is exported (`core/record_model_auth.go`) and safe to
call from a PocketBase hook. Expose it as a **BFF endpoint**, not a client action.

Events that should trigger it:

- Password change (already automatic — do not duplicate).
- User-initiated "sign out of all devices".
- Password reset completion.
- Admin action on a suspected compromised account.
- Telegram unlink, if the account is considered at risk.

### Note on token lifetime (PocketBase-specific)

`AuthToken.Duration` defaults to a refreshable token. Set it deliberately:

- **Shorter** reduces the window a stolen token is usable.
- Refreshable tokens let the client renew without re-login, so a short duration
  does not force frequent logins.

Both the collection secret and each record's `tokenKey` are **hidden from API
responses** (the source deletes them during export), so they cannot leak through
a record read.

---

## D3 — Pre-hijacking defense depends on the record staying unverified

### Problem

PocketBase's protections against account pre-hijacking apply **only while the
record is unverified**. From `apis/record_auth_with_oauth2.go`:

```go
// prevent pre-hijacking with password auth
if !isLoggedAuthRecord && !e.Record.Verified() {
    needUpdate = true
    e.Record.SetRandomPassword()      // destroys a pre-registered password
}

// prevent pre-hijacking with different OAuth2 provider
if !e.Record.Verified() {
    txApp.DeleteAllExternalAuthsByRecord(e.Record)   // drops attacker's links
}
```

Both branches are gated on `!Verified()`. **If a record is marked verified before
its owner actually proves the address, these defenses stop applying** — and an
attacker who pre-registered the email keeps a working password on the victim's
account.

The trigger is mundane: a bulk import, a migration, a support action, or an admin
setting `verified = true` by hand.

> **Still applies.** `verified` remains a PocketBase field and is still the
> control that gates the pre-hijacking defenses. Nothing about the SQLite/Rust
> split changes this.

### Solution — treat verification as a security control, not a flag

**Rule: `verified` may only be set by PocketBase's own verification flows.**

Permitted:

- The user clicking a verification link or entering an OTP.
- PocketBase setting it on a matching OAuth2 email (`record_auth_with_oauth2.go`
  sets it when the OAuth email matches the record email).

Forbidden:

- Bulk imports setting `verified = true`.
- Admin/support toggling it to unblock a user.
- Migrations backfilling it.

### How to enforce it

**1. Block the direct write at the collection level.** Two layers, because neither
is sufficient alone:

**Layer 1 — API rules.** The strongest control is that the API cannot express the
change at all. Users may update their own record, but **`verified` must not be
client-writable**. PocketBase rules are row-level, not field-level, so the practical
options are:

- Keep `verified` out of the update rule's reachable path by routing profile edits
  through our own endpoint instead of the collection API, or
- Use the hook below to reject the specific transition.

**Layer 2 — a hook that rejects the transition.** PocketBase's record hook is
`onRecordUpdate`, bound to the collection by name:

```js
// Reject any transition from unverified -> verified outside PocketBase's own
// flows. PocketBase's verification, OTP and OAuth2 paths set `verified`
// server-side and do not go through this model-save path from a client.
onRecordUpdate((e) => {
  const wasVerified = e.record.original().verified
  const nowVerified = e.record.verified

  if (!wasVerified && nowVerified && !e.app.isInternalRequest) {
    throw new BadRequestError("verified is managed by the system")
  }

  e.next()
}, "users")   // bind to the auth collection only
```

> **Verified against PocketBase v0.40.4 docs.** The hook is `onRecordUpdate`, not
> `onRecordUpdateRequest` (which does not exist). The event carries `e.app` and
> `e.record` — **there is no `e.httpContext` in the JS hook API**, so 'did this
> arrive over the API' cannot be read from the event directly. That is why layer 1
> (making the field unreachable) matters more than the hook.

**Do not rely on the hook alone.** Because the event cannot distinguish the
caller, a hook is a guard against mistakes and bulk scripts, not a security
boundary. The boundary is that the field is not client-writable.

**2. Never import straight into the auth collection.** If migrating existing users,
import them unverified and have them re-verify — or accept that the protections
do not apply to imported accounts and record that explicitly.

**3. Audit `verified` transitions.** Log every change with actor and source
(`otp`, `oauth2`, `link`, `admin`). An unexpected actor is an incident.

### Related: the third gap

PocketBase matches an OAuth2 login on the returned email **without visibly
checking the provider's `email_verified` claim**. Google always returns verified
addresses, so this is fine for Google — but **only enable OAuth2 providers that
guarantee verified email addresses.** If a provider can return an unverified
address, an attacker could register it elsewhere and take the account.

---

## Summary

| # | Issue | Resolution in the CURRENT stack | Verified against |
| --- | --- | --- | --- |
| D1 | Client could write own balance | Money in SQLite, no client DB access, Rust authorization + `CHECK (balance_idr >= 0)` | Shipped schema: [`server/migrations/20260925000000_initial_schema.sql`](../../server/migrations/20260925000000_initial_schema.sql); design record: [`02-data-model.md`](02-data-model.md) |
| D2 | Token revocation | **Superseded** — SQLite `sessions` rows; logout revokes immediately | [`docs/architecture.md`](../architecture.md) §Sessions |
| D3 | Verification disables anti-hijacking | Unchanged — `verified` gated to PocketBase's own flows; hook rejects client writes | `apis/record_auth_with_oauth2.go:340-362` |

## Open items

- [ ] Implement the `verified`-guard hook in PocketBase.
- [x] Session lifetime: **30d absolute / 7d idle** — [`docs/decisions.md`](../decisions.md).
- [x] "Sign out all devices" — `POST /auth/logout-all` revokes all active sessions; implemented and mutation-tested (b31d574).
- [x] OAuth2: **only providers guaranteeing verified email** (`decisions.md`).
- [ ] Wallet mutation audit: the `ledger` table is the record; decide review cadence.
- [ ] Reconciliation job between `accounts.pb_user_id` and PocketBase.