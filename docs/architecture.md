# Architecture

The system, end to end. This document **supersedes** the stack choices in
[website/01-architecture.md](website/01-architecture.md); that file covers the
frontend's internal structure and is still useful, but where it names
PocketBase or Pages Functions, this document wins.

**Changing the code?** [testing.md](testing.md) is the one to read before you
add a rule, a guard or a test — it records what this suite does check, the one
defect class that produces most of the real findings (a duplicated rule drifting
toward the weaker reading), and three ideas for an automatic guard that were
built here, measured, and rejected.

## Stack

| Component | Runs on | Language | Purpose |
| --- | --- | --- | --- |
| **Website** | Cloudflare Pages | **Astro** + islands | Marketing, signup, dashboard |
| **Edge relay** | Cheap Linux VPS (2 vCPU / 4 GB) | nginx + Docker | TLS, filtering, flood absorption |
| **API + proxy** | Northflank | **Rust** | Wallet, keys, LLM proxy, limits, webhooks |
| **Database** | Northflank, inside the API process | **SQLite** | All persistent state except identity |
| **Auth** | Northflank | **PocketBase** | Google + password, verify, reset. Auth ONLY. |

There is no database server to run, reach or secure. SQLite is a library: the API
opens one file on its own persistent volume. That removes a container and a network
hop, and it makes the single-writer property an architectural fact rather than a
tuning detail — every write in the process serialises behind one writer, which is
ample for this workload and is the reason `max_connections` buys read concurrency,
not write throughput.

Deployment: **pushing to `main` deploys both** Cloudflare Pages and Northflank.

## Topology

```
                     Cloudflare (free)
            DNS + TLS at edge + volumetric DDoS
                          |
          +---------------+----------------+
          |                                |
          v                                v
   VPS Relay  (primary)          Northflank  (fallback origin)
   nginx + Docker                Rust API + proxy + SQLite + PocketBase
          |                                ^
          +--------------------------------+
             normal path: relay -> Northflank

   Website (Cloudflare Pages) -> same public hostname
```

**The relay is the normal path; Northflank is the fallback.** If the relay is
unavailable, traffic reaches Northflank directly and the system keeps serving.

**Therefore the relay is a cost and load boundary, not a security boundary.** A
reachable backend can be reached without the relay. Both endpoints carry their own
defences. Full reasoning and the failure matrix: [`topology.md`](topology.md).

**One backend process does two jobs.** It serves the customer API (auth, wallet,
keys) *and* proxies LLM requests. They can be split later; they are one service now
because they share the database, the key lookup, and the usage accounting.

## Why the Rust server is the whole backend

The earlier design put a BFF in Cloudflare Pages Functions and kept the Rust
server as a pure proxy. That is no longer the shape:

- **Auth needs a database and a session story.** Pages Functions are stateless
  and short-lived; password hashing, verification, and reset are server work.
- **Money needs one authority.** Wallet mutations must be atomic with top-up
  status changes — that is a database transaction, not an edge function.
- **Realtime needs a long-lived connection.** Pages Functions cannot hold one.

So: **Pages serves the frontend and nothing else.** Every privileged operation
goes to the Rust server.

## Deployment

Both targets deploy on **`main`**.

### Cloudflare Pages

- Connects to the repo; builds and deploys the frontend on push to `main`.
- **Build-time env only.** Anything prefixed `PUBLIC_*` is **inlined into
  browser JavaScript and is public**. No secrets here — see
  [`.env.example`](../.env.example).

### Northflank

- Builds the Rust server. There is no database service to build alongside it.
- **Runtime env vars** carry the real secrets (provider keys, Midtrans server
  key, database URL).
- The database **file** needs a **persistent volume** and **backups** — the wallet
  ledger lives there. A volume with no backup is a single point of failure for
  every customer's balance; see [`backup-and-restore.md`](backup-and-restore.md).

### The deployment coupling to plan for

Two platforms, one push. They do **not** deploy atomically, so there is always a
window where one is new and the other is old.

| Change | Risk | Mitigation |
| --- | --- | --- |
| API adds a required field | Old frontend breaks | Additive first; make it required later |
| DB migration | Old server against new schema | **Migrations must be backward compatible** |
| Frontend needs a new endpoint | 404 until server deploys | Deploy server first, or tolerate the gap |

**Full procedure — pipeline order, expand/contract migrations, rollback, and
failure responses — is in [`deployment.md`](deployment.md).**

**Rules:** migrations are additive and backward compatible; the server tolerates
the previous frontend version for at least one release; the frontend never
assumes an endpoint exists without handling its absence.

## Authentication — PocketBase for auth ONLY

**Decision:** PocketBase handles authentication. SQLite holds everything else.

This is a deliberate tradeoff: PocketBase already solves password hashing, email
verification, password reset, Google OAuth2, OTP, and MFA. Rebuilding those in
Rust costs weeks and introduces new ways to get security wrong. Accepting a second
system buys that back.

**The constraint that makes this work: PocketBase owns NOTHING but identity.**

| Concern | Owner |
| --- | --- |
| Accounts, passwords, Google login, verification, reset | **PocketBase** |
| Wallet, keys, limits, top-ups, usage, ledger | **SQLite** |
| Authorization (who may spend what) | **Rust** |

### What PocketBase must NOT hold

- **No wallet balance.** Not a field, not a relation.
- **No API keys.**
- **No usage or billing data.**

The moment money lives in PocketBase, the identity system and the ledger can
diverge, and reconciling them becomes a manual job on every incident.

### The account key — the decision that matters

SQLite and PocketBase must agree on what an account *is*.

**Chosen: SQLite owns the account ID; PocketBase's user id is a linked column.**
Phase 6 drops the link — `accounts.id` becomes the only key, and the identity
tables below it become the source.

```sql
-- Schema: server/migrations/20260925000000_initial_schema.sql (source of truth)
-- accounts: id (TEXT PK), pb_user_id (UNIQUE -> PocketBase), status,
--           is_operator, created_at, updated_at
```

**Why not reuse PocketBase's id as the primary key everywhere.** It would be one
join key instead of two, which is genuinely simpler. But it makes every foreign
key in the money schema depend on PocketBase's id format and its continued
existence. Auth is the component **most likely to change** — you just changed it
once already. Do not let it own the primary key of the ledger.

The cost is one extra resolution on login: exchange a PocketBase token for
`pb_user_id`, then look up `accounts`. That happens once per session, not per
request.

### The login flow

```
1. browser -> PocketBase: authenticate (Google or email+password)
2. PocketBase returns an auth token
3. browser -> Rust: exchange that token
4. Rust verifies the token with PocketBase, extracts pb_user_id
5. Rust finds/creates accounts row, issues its OWN session cookie
6. all further API calls use the Rust session cookie
```

> **The concrete DDL lives in [`website/02-data-model.md`](website/02-data-model.md).** It is
> reproduced here only as a field summary — a second copy of the schema drifted
> once already (it was missing `is_operator` and the session audit columns).

**Rust issues its own session.** The PocketBase token is exchanged once and not
used as the API credential. That keeps PocketBase off the hot path and means the
rest of the system never has to speak PocketBase's token format.

### Sessions (in SQLite)

```sql
-- sessions: id (TEXT PK), account_id -> accounts, token_hash (UNIQUE),
--           expires_at, last_seen_at, revoked_at, user_agent, ip_hash, created_at
```

- Cookie is **HttpOnly, Secure, SameSite=Lax** — opaque random value, hash stored.
- **Logout revokes the row** (sets `revoked_at`) → immediate revocation on every surface, preserving the audit trail.
- Sign out everywhere = revokes all active session rows for the account. Expired rows are swept periodically.
- **Every timestamp is written from Rust, never by SQL.** SQLite's own
  `CURRENT_TIMESTAMP` and `datetime('now')` emit a space-separated form that does
  not compare correctly against the RFC3339 values the code binds, and under the old
  SQL-side `now()` an expired session read as still valid. The columns carry a
  `GLOB '????-??-??T??:??:??*+00:00'` CHECK so the wrong format cannot be stored at
  all; see
  [§4.6 of the migration plan](plans/sqlite-migration.md#46-timestamps--the-hazard-that-would-have-shipped).
- This is better than trusting a third-party token lifetime, and it costs one table.

### Password reset and email verification

These stay in PocketBase, but **the emails must be branded and the links must land
on your domain**, or customers receive mails that look like phishing. Configure
PocketBase's templates and custom redirect URLs.

### The risks accepted by this choice

| Risk | Mitigation |
| --- | --- |
| Two systems to run and back up | PocketBase is small; treat it as infrastructure, not app data |
| Identity and money could drift | SQLite is authoritative; reconcile `accounts.pb_user_id` against PocketBase on a schedule and alert on orphans |
| A deleted PocketBase user leaves a funded wallet | **Never hard-delete PocketBase users.** Deactivate them. The wallet outlives the login. |
| Auth outage blocks all logins | Existing sessions keep working — they live in SQLite |
| Two deploy targets for the backend | PocketBase changes rarely; it is not on the `main` deploy path for app code |

**The "deleted user leaves a funded wallet" row is the one to remember.** It is the
concrete reason never to hard-delete an auth record: the money is in the other
database, and nothing cascades across the boundary.

## Database

SQLite, every table `STRICT`. Schema outline; the full field list is in
[website/02-data-model.md](website/02-data-model.md) and the authoritative
definition is
[`server/migrations/20260925000000_initial_schema.sql`](../server/migrations/20260925000000_initial_schema.sql).

> **Where the port stands:** the tree is SQLite end to end, and PocketBase is still
> the identity provider — it becomes Rust in Phase 6. The register's marker in
> [`decisions.md`](decisions.md) and the plan in
> [`plans/sqlite-migration.md`](plans/sqlite-migration.md) are the current boundary;
> prefer them where this document has not yet caught up.

| Table | Purpose |
| --- | --- |
| `accounts` | Wallet owner. UUID PK + `pb_user_id` link. |
| `sessions` | Server-side sessions — real revocation |
| `api_keys` | `key_hash`, prefix, model allowlist, limits |
| `wallets` | `balance_idr` |
| `topups` | Midtrans orders, `order_id` unique |
| `usage_daily` | input / cache-read / output tokens, cost |
| `link_codes` | Telegram binding, short TTL |
| `ledger` | Append-only wallet movements (audit) |

**Not in SQLite:** passwords, email verification, Google login — those live in
PocketBase. There is no `credentials` table and no `identities` table; PocketBase
*is* the identity store. See
[Authentication](#authentication--pocketbase-for-auth-only).

### Rules

- **Money is integers.** `balance_idr` is `INTEGER` in a `STRICT` table, never
  floating point. `STRICT` is what makes that enforced rather than declared — it
  requires SQLite 3.37 or newer.
- **Wallet mutations are transactional.** Credit + top-up status + ledger row in
  one transaction.
- **Never trust a client-supplied amount.** Credits come from the verified
  Midtrans webhook only.
- **`api_keys` is looked up by `key_hash` on the hot path** — index it.
- **Append-only ledger.** Balance is derivable from it; that is what makes a
  dispute resolvable.

## Live updates (no page refresh)

PocketBase offers realtime, but it is scoped to its own collections — and the
data we care about (wallet, usage) lives in **SQLite**, which PocketBase cannot
stream. So realtime is ours regardless of the auth decision.

**Use Server-Sent Events (SSE)** from the Rust server:

- `GET /events` — authenticated by session cookie, streams balance and usage
  changes for that account.
- Simpler than WebSockets, one-directional is all that is needed, and it works
  through Cloudflare.
- Frontend subscribes and updates the dashboard without a refresh.
- **Fallback:** poll `GET /api/me` every 30–60s if the stream drops, and show a
  **stale indicator** rather than displaying an old balance as current.

## LLM proxy path

The same Rust server proxies model calls:

```
client  --(api key)-->  Rust server  --(provider key)-->  upstream
                           |
                           +-- validate key, model allowlist, limits
                           +-- pre-flight wallet reservation
                           +-- stream response through
                           +-- record usage, settle wallet
```

**Every endpoint, auth scheme, and enforcement order:
[`server/api-spec.md`](server/api-spec.md).**

Behaviour spec: [website/06-api-keys-and-limits.md](website/06-api-keys-and-limits.md).
The key rules: deny by default, enforce limits **in the proxy** (a limit the UI
shows but the proxy ignores is a lie), and cache key metadata with a short TTL
because the database must not be on every token's hot path.

## Environment and secrets

| Setting | Where | Public? |
| --- | --- | --- |
| `DATABASE_URL` | Northflank | No |
| Provider API keys | Northflank | No |
| Midtrans server key | Northflank | No |
| Google OAuth client secret | Northflank | No |
| `PUBLIC_API_BASE_URL` | Cloudflare Pages | Yes |
| `PUBLIC_MIDTRANS_CLIENT_KEY` | Cloudflare Pages | Yes |

Canonical list: [`.env.example`](../.env.example). **Anything `PUBLIC_*` is shipped
to the browser** — it is not secret, and putting a real secret there leaks it.

## Consequences of this choice

Honest accounting of the PocketBase-for-auth hybrid:

| Gained | Taken on |
| --- | --- |
| Password hashing, verify, reset, OAuth2, OTP, MFA — all free | A **second system** to run, back up, and monitor |
| Auth proven, not hand-rolled | Identity lives outside SQLite; the two can drift |
| SQLite: transactions, constraints, real reporting | One **extra lookup on login** to resolve `pb_user_id` |
| Server-side sessions: logout revokes immediately | Authorization logic in Rust (was PocketBase API rules) |
| PocketBase is not on the request hot path | Hard-deleting a PB user would orphan a funded wallet |

**The rule that keeps this sound: PocketBase owns identity, SQLite owns money.**
Cross the boundary in one direction only — Rust reads identity from PocketBase and
writes money to SQLite. Nothing in SQLite should ever be authoritative about
who a user is.

## What this invalidates

Documents that must be revised:

| Document | Status |
| --- | --- |
| [website/02-data-model.md](website/02-data-model.md) | **Rewrite for SQLite.** Money tables are SQLite; identity stays in PocketBase until Phase 6. PocketBase *API rules* are replaced by Rust authorization. |
| [website/05-security-decisions.md](website/05-security-decisions.md) | **Update.** D1 (wallet immutability) still applies — enforced by Rust, not API rules. D2 (token revocation) is superseded by SQLite sessions. D3 (verification) still holds — it is now PocketBase's `verified` field being protected. |
| [website/01-architecture.md](website/01-architecture.md) | **Superseded** on stack; frontend internals still valid. |
| [website/03-functional-spec.md](website/03-functional-spec.md) | Mostly valid — it specifies behaviour, not storage. |
| [website/04-payments.md](website/04-payments.md) | Valid: the Midtrans flow is storage-agnostic. |
| [website/06-api-keys-and-limits.md](website/06-api-keys-and-limits.md) | Valid, with the cache TTL now ours to set rather than inherited. |

## Related documents

| Concern | Document |
| --- | --- |
| Deployment and migrations | [`deployment.md`](deployment.md) |
| CI pipeline and tests | [`ci-cd.md`](ci-cd.md) |
| Cost and capacity planning | [`cost-and-sizing.md`](cost-and-sizing.md) |
| Logging, metrics, alerting | [`observability.md`](observability.md) |
| Upstream failover and circuit breaking | [`failover.md`](failover.md) |
| Local development and fakes | [`local-development.md`](local-development.md) |
| Every API endpoint | [`server/api-spec.md`](server/api-spec.md) |
| Error responses | [`error-model.md`](error-model.md) |
| Realtime/SSE contract | [`realtime.md`](realtime.md) |
| Edge relay and TLS termination | [`edge-relay.md`](edge-relay.md) |
| Triangle topology and failover | [`topology.md`](topology.md) |
| Data retention and privacy | [`data-retention.md`](data-retention.md) |
| Database schema | [`website/02-data-model.md`](website/02-data-model.md) |
| Identity and sessions | [`architecture/identity.md`](architecture/identity.md) |

## Memory safety, enforced by the compiler

**The server crate contains no `unsafe` code, and that is a build-enforced fact
rather than a claim.** `server/src/lib.rs` opens with `#![forbid(unsafe_code)]`.

The distinction that matters: `deny` can be lifted by a single inner
`#[allow(unsafe_code)]`, so it documents an intention any one line can override.
`forbid` cannot be overridden at all — a future `unsafe` block does not warn, it
fails to compile until somebody deliberately edits that line and says why.

This is a service that holds wallet balances, verifies payment signatures and
serves customer credentials, so "no unsafe in the API crate" is a meaningful
security property to be able to state. It is only worth stating if the compiler
is the one enforcing it.

**Two panic-shaped lints are also denied on production code** —
`clippy::unwrap_used` and `clippy::indexing_slicing` — because a panic on the
request path is an availability incident rather than a style choice, and those two
catch the common accidental forms (indexing that can go out of bounds, and a
`Result`/deviation unwrapped where a caller could be told). Both were applied
without a single `#[allow]`: the three sites they flagged in production code were
each a genuine latent panic, and all three are now fixed rather than muted —

| Site | Was | Why it mattered |
| --- | --- | --- |
| `routes/auth.rs` `session_cookie` | `.unwrap()` on the header parse | A panic inside the login handler: a 500 on the endpoint every customer uses, reported as a crash rather than the malformed cookie it is. Now returns `Result` |
| `routes/keys.rs` key generation | `alphabet[idx]` | Provably in bounds (`% 62` over a 62-byte array), but the proof depends on two literals staying in step. Now `get()` with an unreachable fallback |
| `routes/keys.rs` key update | `serde_json::to_value(..).unwrap()` | A 500 on a money-adjacent endpoint. Now falls back, matching the create path |

**Test modules are deliberately out of scope.** `cargo clippy` (what CI runs)
covers the library and binaries; tests `unwrap` freely because that is how a test
asserts a precondition, and rewriting ~300 assertions would be churn with no
production benefit.

## Open items

- [x] Frontend language/framework on Pages: **Astro + islands** — decided, see [`website/01-architecture.md`](website/01-architecture.md).
- [x] Session lifetime: **30d absolute / 7d idle** — [`decisions.md`](decisions.md).
- [x] Migration tooling: **sqlx migrate** — [`decisions.md`](decisions.md).
- [x] Backup and restore drill — see [`backup-and-restore.md`](backup-and-restore.md).
- [ ] Whether the proxy and the API split into two services later.
- [x] SSE auth — settled in [`decisions.md`](decisions.md).