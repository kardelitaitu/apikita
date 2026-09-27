# 03 — Functional Spec

Pages, flows, and states. Written as behaviour, not markup.

## Pages

| Route | Purpose | Auth |
| --- | --- | --- |
| `/` | Landing / marketing. Price comparison, signup CTA. | Public |
| `/login` | Google + email/password sign-in | Public |
| `/signup` | Registration | Public |
| `/verify` | Email verification landing | Public (token) |
| `/reset` | Request password reset | Public |
| `/reset/confirm` | Set a new password | Public (token) |
| `/dashboard` | Balance, usage, status | Required |
| `/dashboard/keys` | API key list, create, limits, revoke | Required |
| `/dashboard/keys/new` | Create key: models + limits | Required |
| `/dashboard/wallet` | Balance, top-up, history | Required |
| `/dashboard/usage` | Token usage over time | Required |
| `/dashboard/settings` | Profile, password, linked accounts | Required |
| `/docs` | Integration guide | Public |

## Signup

**Path A — Google.** One click. Account created on first sign-in. Email comes
back verified from Google.

**Path B — email + password.**

1. Email + password form. Password rules shown up front, not revealed on error.
2. Verification email sent. **The account cannot hold a balance until verified.**
3. User clicks the link → `/verify` → account active.

**Why verification gates the wallet:** an unverified email is not proof the
address belongs to the person. Combined with non-refundable deposits, an
unverified wallet is an invitation to fraud.

### Duplicate email

If the address already exists, **do not** create a second account and **do not**
reveal that it exists. Show: "If this email is registered, we've sent sign-in
instructions." Then send an email explaining they already have an account. This
avoids both account duplication and enumeration.

## Login

- Google button; email/password form below.
- Rate-limit failed attempts per account and per IP. Lock or delay after N
  failures; do not reveal which part was wrong.
- On success → dashboard.

## Logout

Simple, because there are no server-side sessions: "logout" is discarding the
PocketBase auth token and clearing the cookie the BFF set.

- One control in the dashboard header. No confirmation dialog — logging out is
  not destructive.
- Clears the token cookie server-side (BFF) **and** the client auth store, so a
  back-button cannot restore an authenticated view.
- **Token caveat:** the token itself remains cryptographically valid until it
  expires; discarding it does not revoke it. A token copied off the device still
  works. This is inherent to stateless tokens, not a bug.

### Sign out of all devices

A separate, deliberate action — this is real revocation.

- Calls the BFF, which rotates the record's `tokenKey` and saves.
- **Invalidates every token for that user instantly**, on all devices.
- Use after: password change (automatic), suspected compromise, or a "sign out
  everywhere" request.

See [05-security-decisions.md](05-security-decisions.md) D2 for the mechanism.

### Distinguish the two in the UI

Offer both, labelled differently:

| Action | Effect | Cost |
| --- | --- | --- |
| **Log out** | this device only | none, reversible by signing in |
| **Sign out everywhere** | every device | all sessions end; user must sign in again |

Presenting only "logout" leaves a user who lost a device with no remedy.
Presenting only "sign out everywhere" makes routine logout feel punitive.

## Password reset

1. `/reset`: enter email. Always respond with the same neutral message.
2. Email with a single-use, expiring token.
3. `/reset/confirm`: new password → saved → all sessions invalidated.

**Token rules:** single-use, short expiry, stored hashed, invalidated on use,
and **never echoed back in a URL that gets logged**.

## Dashboard

Live-updating by default (see [01-architecture.md](01-architecture.md)).

| Element | Data | Updates |
| --- | --- | --- |
| Balance | `wallets.balance_idr` | Realtime; polling fallback |
| Today's usage | `usage_daily` | Realtime |
| Token breakdown | input / cache-read / output | Same |
| Recent requests | last N metered calls | Poll or realtime |
| Status | upstream health | Poll |

### Required states

Every data view needs all four. Specifying them is not padding — a wallet that
shows a stale balance as current is a dispute.

- **Loading** — skeleton, not a spinner over a number that later changes.
- **Empty** — new account with no usage. Explain what to do next, don't show `0` alone.
- **Error** — a failed fetch must say so, and must **not** display the last known
  balance as if current.
- **Stale** — if realtime drops, show an indicator. Never present old data as live.

## API keys

Full spec, including model access and limit semantics:
[06-api-keys-and-limits.md](06-api-keys-and-limits.md).

- **Create:** label (required), models, spend/token/rate limits, expiry — all
  optional except label. Defaults are **unlimited**, so the common case is one
  click. Key generated server-side → **shown once** with a copy button and an
  explicit "it will not be shown again" warning that requires acknowledgement.
- **List:** prefix, label, model access, spend against limit, last used, status.
  Never the full key, never `key_hash`.
- **Edit:** raise limits immediately. **Lowering** warns that the proxy applies
  changes within its cache TTL (up to a minute), so a reduction is not instant.
- **Revoke:** confirm, then immediate from the UI. The confirmation states the
  TTL caveat honestly rather than claiming instant enforcement. Revocation
  affects **all** surfaces — a key issued via Telegram dies here too.
- **Empty state:** explain what a key is for and link to `/docs`.

## Wallet / top-up

Full flow in [04-payments.md](04-payments.md).

- Enter amount. Enforce minimums (re-top-up vs first deposit differ).
- Show the fee and what lands in the balance — do not surprise the user.
- Open Midtrans Snap → QRIS.
- **After payment, the balance updates only when the server webhook confirms.**
  Show "waiting for payment confirmation", not an optimistic credit. A user who
  sees a credited balance that later reverts will not trust the product again.

## Settings

- Change password (requires current password).
- Link / unlink Telegram: shows the `/link` code flow.
- **Unlinking Telegram must warn** that the account remains and can be relinked.
  It must never delete the account or the wallet.

## Error and edge behaviour

**Every error code the API can return needs a defined UI behaviour.** The API
defines 14 machine-readable codes (see [`error-model.md`](../error-model.md)); a dashboard that
only handles five of them shows a raw error or a blank screen for the rest.

### Error codes → UI

| Status | Code | What the user sees |
| --- | --- | --- |
| 400 | `invalid_request` | "Something was wrong with that request." No retry offered |
| 401 | `unauthenticated` | **Redirect to login**, preserving the destination |
| 401 | `key_revoked` | "This key was revoked." Action: create a new one |
| 401 | `key_expired` | "This key expired on {date}." Action: create a new one |
| 402 | `insufficient_balance` | "Balance too low." Show current balance + **link to top-up** |
| 402 | `key_limit_exceeded` | "This key hit its {spend/token} limit." Action: raise it or new key |
| 403 | `model_not_allowed` | "This key cannot use {model}." Do **not** offer a retry |
| 403 | `wrong_credential_type` | **Reserved, never emitted** — a wrong-type credential arrives as 401 `unauthenticated`, handled above |
| 404 | `not_found` | "Not found." Offer a route back to the dashboard |
| 409 | `conflict` | "Already linked" / "already exists" — refresh and show current state |
| 422 | `validation_failed` | **Highlight the offending field** using `details.field`, not a generic banner |
| 429 | `rate_limited` | "Too many requests." Show the `Retry-After` wait; disable the action |
| 500 | `internal_error` | "Something went wrong on our side." Show the `request_id` |
| 503 | `no_upstream_available` | Status banner: inference is degraded. **Browsing still works** |

### Rules that follow

1. **Show `request_id` on 5xx.** It is the support conversation's starting point,
   and a user who can quote it saves everyone a round trip.
2. **Never offer a retry for 402 or 403.** Both are permanent until the user acts;
   a retry button teaches the wrong model.
3. **422 highlights the field.** Reading `details.field` is the difference between
   "invalid input" and "your rating must be 1-5".
4. **A 503 is not a login failure.** The API being degraded must not bounce the user
   to the login screen.
5. **Never render a raw error body.** Server messages are for support, not users.

### Non-API edge cases

| Case | Behaviour |
| --- | --- |
| Session expired mid-action | Redirect to login, preserve the destination |
| Webhook delayed | "Payment received, confirming" — **never credit optimistically** |
| Realtime disconnected | Poll + stale badge; keep the last value, never blank it |
| Upstream unhealthy | Status banner; say requests may fail rather than hiding it |
| Network offline | Say so. Do not present a cached balance as live |
## Accessibility

Not optional, and cheap if done from the start:

- Every input has a label.
- Errors are announced, not colour-only.
- Keyboard-navigable: login, create key, top-up.
- Contrast meets WCAG AA — the dashboard is a financial UI.
- Money is formatted consistently and with the currency stated, since
  misreading `10.000` as ten thousand vs ten is a real hazard in ID locale.

## Not in scope

- LLM request proxying → `server/`
- Telegram bot UI → `telegram/`
- Admin operations → PocketBase dashboard initially