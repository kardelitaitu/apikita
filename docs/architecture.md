# Architecture

The system, end to end. This document **supersedes** the stack choices in
[website/01-architecture.md](website/01-architecture.md); that file covers the
frontend's internal structure and is still useful, but where it names
PocketBase or Pages Functions, this document wins.

## Stack

| Component | Runs on | Language | Purpose |
| --- | --- | --- | --- |
| **Website** | Cloudflare Pages | _frontend, TBD_ | Marketing, signup, dashboard |
| **Edge relay** | Cheap Linux VPS (2 vCPU / 4 GB) | nginx + Docker | TLS, filtering, flood absorption |
| **API + proxy** | Northflank | **Rust** | Wallet, keys, LLM proxy, limits, webhooks |
| **Database** | Northflank | **PostgreSQL** | All persistent state except identity |
| **Auth** | Northflank | **PocketBase** | Google + password, verify, reset. Auth ONLY. |

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
   nginx + Docker                Rust API + proxy + Postgres + PocketBase
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

- Builds the Rust server and the Postgres database as one stack.
- **Runtime env vars** carry the real secrets (provider keys, Midtrans server
  key, database URL).
- Postgres needs a **persistent volume** and **backups** — the wallet ledger
  lives there.

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

**Decision:** PocketBase handles authentication. Postgres holds everything else.

This is a deliberate tradeoff: PocketBase already solves password hashing, email
verification, password reset, Google OAuth2, OTP, and MFA. Rebuilding those in
Rust costs weeks and introduces new ways to get security wrong. Accepting a second
system buys that back.

**The constraint that makes this work: PocketBase owns NOTHING but identity.**

| Concern | Owner |
| --- | --- |
| Accounts, passwords, Google login, verification, reset | **PocketBase** |
| Wallet, keys, limits, top-ups, usage, ledger | **Postgres** |
| Authorization (who may spend what) | **Rust** |

### What PocketBase must NOT hold

- **No wallet balance.** Not a field, not a relation.
- **No API keys.**
- **No usage or billing data.**

The moment money lives in PocketBase, the identity system and the ledger can
diverge, and reconciling them becomes a manual job on every incident.

### The account key — the decision that matters

Postgres and PocketBase must agree on what an account *is*.

**Chosen: Postgres owns the account ID; PocketBase's user id is a linked column.**

```sql
-- Schema: docs/website/02-data-model.md (single source of truth)
-- accounts: id (UUID PK), pb_user_id (UNIQUE -> PocketBase), status,
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

### Sessions (in Postgres)

```sql
-- sessions: id (UUID PK), account_id -> accounts, token_hash (UNIQUE),
--           expires_at, revoked_at, user_agent, ip_hash, created_at
```

- Cookie is **HttpOnly, Secure, SameSite=Lax** — opaque random value, hash stored.
- **Logout deletes the row** → immediate revocation, on every product surface.
- Sign out everywhere = delete all rows for the account.
- This is better than trusting a third-party token lifetime, and it costs one table.

### Password reset and email verification

These stay in PocketBase, but **the emails must be branded and the links must land
on your domain**, or customers receive mails that look like phishing. Configure
PocketBase's templates and custom redirect URLs.

### The risks accepted by this choice

| Risk | Mitigation |
| --- | --- |
| Two systems to run and back up | PocketBase is small; treat it as infrastructure, not app data |
| Identity and money could drift | Postgres is authoritative; reconcile `accounts.pb_user_id` against PocketBase on a schedule and alert on orphans |
| A deleted PocketBase user leaves a funded wallet | **Never hard-delete PocketBase users.** Deactivate them. The wallet outlives the login. |
| Auth outage blocks all logins | Existing sessions keep working — they live in Postgres |
| Two deploy targets for the backend | PocketBase changes rarely; it is not on the `main` deploy path for app code |

**The "deleted user leaves a funded wallet" row is the one to remember.** It is the
concrete reason never to hard-delete an auth record: the money is in the other
database, and nothing cascades across the boundary.

## Database

PostgreSQL. Schema outline; the full field list is in
[website/02-data-model.md](website/02-data-model.md), which must be **rewritten**
for Postgres (its PocketBase rules no longer apply).

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

**Not in Postgres:** passwords, email verification, Google login — those live in
PocketBase. There is no `credentials` table and no `identities` table; PocketBase
*is* the identity store. See
[Authentication](#authentication--pocketbase-for-auth-only).

### Rules

- **Money is integers.** `balance_idr` is `BIGINT`, never floating point.
- **Wallet mutations are transactional.** Credit + top-up status + ledger row in
  one transaction.
- **Never trust a client-supplied amount.** Credits come from the verified
  Midtrans webhook only.
- **`api_keys` is looked up by `key_hash` on the hot path** — index it.
- **Append-only ledger.** Balance is derivable from it; that is what makes a
  dispute resolvable.

## Live updates (no page refresh)

PocketBase offers realtime, but it is scoped to its own collections — and the
data we care about (wallet, usage) lives in **Postgres**, which PocketBase cannot
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
| Auth proven, not hand-rolled | Identity lives outside Postgres; the two can drift |
| Postgres: transactions, constraints, real reporting | One **extra lookup on login** to resolve `pb_user_id` |
| Server-side sessions: logout revokes immediately | Authorization logic in Rust (was PocketBase API rules) |
| PocketBase is not on the request hot path | Hard-deleting a PB user would orphan a funded wallet |

**The rule that keeps this sound: PocketBase owns identity, Postgres owns money.**
Cross the boundary in one direction only — Rust reads identity from PocketBase and
writes money to Postgres. Nothing in Postgres should ever be authoritative about
who a user is.

## What this invalidates

Documents that must be revised:

| Document | Status |
| --- | --- |
| [website/02-data-model.md](website/02-data-model.md) | **Rewrite for Postgres.** Money tables move to Postgres; identity stays in PocketBase. PocketBase *API rules* are replaced by Rust authorization. |
| [website/05-security-decisions.md](website/05-security-decisions.md) | **Update.** D1 (wallet immutability) still applies — enforced by Rust, not API rules. D2 (token revocation) is superseded by Postgres sessions. D3 (verification) still holds — it is now PocketBase's `verified` field being protected. |
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

## Open items

- [ ] Frontend language/framework on Pages (Astro was recommended, not chosen).
- [x] Session lifetime: **30d absolute / 7d idle** — [`decisions.md`](decisions.md).
- [x] Migration tooling: **sqlx migrate** — [`decisions.md`](decisions.md).
- [x] Backup and restore drill — see [`backup-and-restore.md`](backup-and-restore.md).
- [ ] Whether the proxy and the API split into two services later.
- [x] SSE auth — settled in [`decisions.md`](decisions.md).