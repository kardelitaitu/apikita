# Identity & Accounts

How a person becomes an account, and how that account is reached from the website
and the Telegram bot.

> **Stack:** identity lives in **this crate's own embedded SQLite**, alongside
> money. PocketBase is gone — there is one store, not two, and no service to run,
> reach or back up for auth. See [`docs/architecture.md`](../architecture.md). This
> document covers the identity side; the money schema is in the migrations under
> `server/migrations/`.

## The shape of the thing

An **account** owns the wallet. Identities link to it. A person can have several
identities and still have one balance.

```
                        SQLite (embedded)

   identities ------>  +----------------------+  <--- wallets
   (google|password)   | accounts             |  <--- api_keys
   identity_tokens --> |   id (the only key)  |  <--- sessions
   auth_attempts ----> |   status             |  <--- telegram_links
                       +----------------------+
```

`identities.account_id` references `accounts.id`. The account is the thing with a
balance, a key set and a session; an identity is one way of proving you are that
person.

**Keys and wallet hang off the account, never off an identity.** That is what
makes revocation account-wide by construction.

## Login methods

| Method | Handled by | Routes |
| --- | --- | --- |
| **Google sign-in** (OIDC ID token, verified against Google's JWKS) | this crate — `identity::google` | `POST /auth/google` |
| **Email + password** (Argon2id), with verification and reset | this crate — `identity::password`, `identity::tokens`, `identity::email` | `POST /auth/signup`, `/auth/login`, `/auth/verify-email`, `/auth/verification/resend`, `/auth/password-reset/request`, `/auth/password-reset/confirm` |

The browser obtains a Google **ID token** through Google Identity Services and
posts that one token; the server verifies the JWT and does not run an OAuth
authorization-code flow, hold a client secret, or exchange a code. Both methods end
in the same place: an opaque session cookie whose value is a row in the SQLite
`sessions` table, so the identity provider is never on the request hot path after
sign-in.

Endpoints in full: [`docs/server/api-spec.md`](../server/api-spec.md) §Auth. The
authoritative route list is `pub const ROUTES` in `server/src/routes/mod.rs`.

## Table ownership

| Table | Store | Purpose |
| --- | --- | --- |
| `accounts` | SQLite | `id` is the only key; `status`, `is_operator` |
| `identities` | SQLite | one row per way of proving who you are |
| `identity_tokens` | SQLite | single-use verification and reset links, stored only as a hash |
| `auth_attempts` | SQLite | the counters the `[limits]` caps are counted from |
| `sessions` | SQLite | server-side sessions |
| `telegram_links` | SQLite | `telegram_id` → account |
| `link_codes` | SQLite | Telegram binding codes |

**`identities` is the identity store.** It is populated on every signup and every
Google sign-in — it is not a placeholder, and there is no second source of truth
about who someone is.

## Invariants

1. **One account, many identities.** A person may sign in with Google *and* a
   password; both resolve to the same wallet.
2. **`api_keys.account_id` and `wallets.account_id` reference the account**, never
   an identity directly.
3. **SQLite is authoritative about who a user is.** `accounts.id` is the only key;
   a person is identified by the `identities` rows that reference their account.
4. **Never hard-delete an account.** The wallet, ledger and top-ups reference
   `accounts.id` with `ON DELETE RESTRICT`, so money outlives the login. Closure is
   `status = 'closed'`.
5. **`link_codes` are single-use and expiring.** Binding happens on redemption
   only.
6. **Unlinking Telegram removes one row.** It must never delete the account or
   the wallet.

## How the citations stay true

Config fields carry doc comments that say where each one is consumed — the answer to
"is this field used, and by what?". Those comments are cross-references, and a
cross-reference is only worth having if the thing it names exists.

It did not, for seven of them. `server/src/config.rs` named a `from_config`
constructor on `EmailSender` at four sites and `EmailSender::send` at three more;
the constructor is `new`, `send` takes an already-built message and reads no config
at all, and no such thing as `from_config` was ever defined. A reader following one
of those names finds nothing and draws the natural conclusion — **that the field is
unused** — which is the exact opposite of what the comment was placed there to say.

The guard is
`doc_claims::tests::every_method_a_doc_comment_cites_is_a_method_this_crate_defines`.
It walks every doc comment in `server/src`, extracts each type-member pair, and
requires the member to be either a `fn` defined somewhere in the crate or a field
declared on a type the crate defines. Two rules keep it from being noise, and both
were arrived at by watching an earlier version fail:

- **A citation of a type this crate does not define is skipped.** `Duration::from_secs`
  and `SqliteConnectOptions::from_str` are correct prose; failing on them would make
  the check fire on true statements until somebody disabled it.
- **A field named through its type is accepted.** `EmailConfig::smtp_host` reads as a
  method call but names a field, and the reader can find it. Roughly a dozen correct
  comments say it that way; rejecting them would have been the checker's fault, not
  theirs.

The first version of the guard rejected any type preceded by `::`, which discarded
the type segment of every *qualified* citation — `identity::email::EmailSender::new`
is the form this codebase actually writes, so it extracted nothing and reported
success over an empty set. That is worth recording because it is the failure mode
the guard exists to prevent, reproduced inside the guard. The lesson generalises:
**a mutation that survives is evidence about the check before it is evidence about
the anchor.** The sweep that caught it seeded a citation naming a method that does
not exist and found the suite still green.

## Email handling — the four pre-hijacking vectors

The four classic vectors, and the defences as they now exist. All of them are
predicates over `identities` in `server/src/identity/accounts.rs` and
`server/src/identity/google.rs`. The attacker has the victim's address but not
their mailbox.

The schema does real work here, so it is worth stating:

- `CHECK ((provider = 'password') = (password_hash IS NOT NULL))` — a password
  identity has a hash, a Google identity does not.
- `CHECK (provider <> 'google' OR email_verified = 1)` — a Google identity is
  **always** verified.
- `UNIQUE (provider, subject)` — one Google subject maps to one row.
- `CREATE UNIQUE INDEX identities_provider_email_uniq ON identities (provider, email)`
  — one address per provider. **Note the shape: `(provider, email)`, not
  `(email)`.** That is load-bearing (see below).

### 1. A password identity registered first, then a Google sign-in

The attacker registers the victim's address with a password of their own; the
victim then signs in with Google. If that sign-in adopted the attacker's row, the
attacker's password would open the victim's account.

**Rule: a Google sign-in may adopt an existing password identity ONLY IF that
identity's `verified_at` is set AND predates the Google identity's creation**
(`verified_at IS NOT NULL AND verified_at < now`). The comparison is against the
moment the Google identity comes into existence, so the ordering is the rule and
not merely the bit.

**Why the ordering and not just `email_verified = 1`:** without it, an attacker
could register the victim's address with a password, leave it unverified, wait for
the victim to sign in with Google (correctly refused), and then let the victim's
*own* verification retroactively authorise a link to the attacker's row. That is
why `identities.verified_at` exists as a timestamp rather than a second bit.

When the rule refuses, **a new account is created** (`GoogleSignIn::CollisionCreated`)
and `auth.email_collision_unverified` is logged at WARN with the colliding identity
id — never shown to the caller. The attacker's row is left exactly as it was.

**Why create rather than refuse:** refusing would hand an attacker a denial of
service on any address they like — register with it, never verify, and the real
owner can never use Google. Creating gives the attacker nothing (their row is inert
and unverified) and gives the victim a working account.

### 2. A verified password identity, then "Sign in with Google"

This is the legitimate case: the person signed up with a password, verified their
address, and now uses Google. The Google identity joins the existing account
(`GoogleSignIn::Linked`) — one wallet, two ways in.

The refusal branch of vector 1 is what keeps this safe: an unverified colliding row
is never adopted, so the legitimate path and the attack are told apart by whether
the address was proven first.

### 3. Google returns an unverified email — CLOSED

`verify_id_token` requires the token's `email_verified` claim to be `true` **before
touching the database**, and returns `Unauthenticated` otherwise. Without that
check the INSERT would hit the raw `CHECK` constraint and surface as a 500 for what
is the caller's problem.

The schema constraint is the backstop; the Rust check is the check. Two further
guards in the same function:

- **The algorithm is fixed to RS256**, not read from the token header. Accepting
  the algorithm a token names is the classic JWT confusion bug.
- The signature is verified against Google's published JWKS, the issuer must be one
  of Google's two real spellings, and the audience must be our client id — so a
  valid Google token minted for any other site cannot be replayed here.

### 4. Password reset on a Google-only account — DECIDED: allowed

Completing a reset already requires control of the mailbox, and control of the
mailbox is exactly what Google-ownership means. The reset therefore proves nothing
new, so it is not an escalation. Blocking it would cost support load (the
legitimate "I signed up with Google and want a password now" case) and buy nothing.

`confirm_password_reset` creates the password identity when the account has none.
Its `subject` is the normalised address, not a random id — `UNIQUE (provider,
subject)` is what stops one address having two password identities, and a random
subject would sail past it.

**The reset does not set `email_verified`.** A reset proves the mailbox was
reachable at that moment, but the verified transition is its own claim; marking it
here would let a reset launder an unverified address into a linkable one.

### The trap that disables all of it

Every defence above is gated on whether the address was proven, and when.
**`email_verified` is written in a closed set of places:**

- signup writes `0` (`create_password_account`) — nothing in that path proves the
  address,
- the Google identity is inserted with `1`, and only after `verify_id_token` has
  checked the claim,
- `mark_verified` writes `1`, and its only caller is `verify_email`, which reaches
  it by redeeming a token delivered to that address.

`set_password` — the reset path — deliberately does not write it.

**Rule: nothing else may set `email_verified` to 1** — not an admin endpoint, not a
migration backfill, not a support action, not a test fixture set for convenience.
`upsert_password_identity` takes a `verified` flag for the paths that need one; the
only production caller passes `false`.

Enforcement is specified in
[`docs/website/05-security-decisions.md`](../website/05-security-decisions.md) D3.

### The collision index is `(provider, email)`, not `(email)`

The unique index is `identities_provider_email_uniq ON identities (provider, email)`.
That shape is what allows an **unverified** password identity and a Google identity
to coexist on one address — the exact state the refusal branch relies on.
"Simplifying" it to `UNIQUE (email)` would turn that branch into a constraint
violation, which is a 500 on a user error.

Addresses are compared after `normalize_email`: **lowercased and trimmed, and
otherwise verbatim**. The residual risk is stated rather than hidden: two spellings
a human reads as one mailbox can exist as two rows. That is a support case, not a
security hole — the linking rule never infers account identity from an address that
was not proven.

## Sessions

Server-side sessions in SQLite, not JWTs. This is what makes logout real.

- Login creates a `sessions` row; the cookie carries an opaque random value and
  only its SHA-256 is stored.
- **Logout revokes the row** → immediate, on every surface.
- **Sign out everywhere** revokes all rows for the account.
- **A password reset revokes every live session for the account.** A reset is what
  a person does when they believe someone else has their credential, so leaving a
  session opened with the old password alive would defeat the transaction.
- **30 days absolute / 7 days idle** ([`decisions.md`](../decisions.md)). The
  absolute bound is seeded at login; the idle bound is applied in Rust by
  `session_is_live_at`, and resolving a session moves `last_seen_at` — only a
  credential that was actually honoured counts as activity.
- **Every place that asks "is this session live" must ask `session_is_live_at`.**
  Two of them filtered on `revoked_at IS NULL AND expires_at > ?` by hand instead —
  `POST /auth/logout-all`, which then accepted an idle cookie and let it revoke
  every other device on the account, and the auth tests' own copy of the resolver,
  which claimed in a comment to be "the same three lines" as production while
  omitting the idle half. A hand-written predicate is a second definition of the
  rule, and the two drift silently because nothing compares them. The test copy now
  calls the same function for the same reason.
- Expired and revoked rows are swept nightly, 30 days after the instant they
  stopped being usable.

## Telegram linking

> Channel structure, rooms, and the bot's behaviour:
> [`docs/telegram/README.md`](../telegram/README.md). This section covers the
> linking mechanism only.

Telegram is a custom table in SQLite, as it always was. It is not an OAuth2
provider and has no place in `identities`.

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

## Reconciliation — no longer a hazard

The old section here was about two stores drifting. Identity and money now live in
one SQLite database, so **there is no reconciliation job to schedule and no orphan
detection to build**: a row cannot exist on one side only.

The fact that survives is still worth keeping: **an auth outage does not lock out
existing users.** Sessions live in SQLite, so an existing cookie keeps working even
if Google is unreachable — only a *new* Google sign-in needs Google's JWKS, and
that is cached in memory between fills.

## Open questions

- [x] Session lifetime: **30d absolute / 7d idle** — [`decisions.md`](../decisions.md).
- [x] Accept or block a password created by reset on a Google-only account (case 4):
      **allowed** — a completed reset already required control of the mailbox. See
      §Email handling, vector 4.
- [ ] Whether email is mandatory for a Telegram-linked account. Recommended: yes,
      or losing Telegram loses the wallet.
- [ ] 2FA for accounts holding a large balance.
