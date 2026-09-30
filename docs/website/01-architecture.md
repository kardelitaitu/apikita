# 01 — Architecture

> **Superseded on stack — read [`docs/architecture.md`](../architecture.md) first.**
> This document specified Cloudflare Pages Functions + PocketBase. The system now uses a Rust backend on Northflank with embedded SQLite. The frontend-internal sections below remain useful; the stack and backend sections are superseded.
>
> **Identity too is superseded here.** The Phase 6 identity port has landed: `accounts.pb_user_id` is dropped, `POST /auth/exchange` is deleted, the PocketBase HTTP client is gone from `server/`, and identity is served natively by this crate over the `accounts` + `identities` tables. Where the text below still says PocketBase is the identity provider, [`architecture/identity.md`](../architecture/identity.md) governs.

## Requirement that drives the design

**Balance and token usage must update live, without the user pressing refresh.**

That single requirement rules out a purely static site and constrains how the
browser reaches the data. Everything below follows from it.

## Stack

**Authoritative stack: [`docs/architecture.md`](../architecture.md).** This
document covers the frontend's internal structure.

| Layer | Choice | Why |
| --- | --- | --- |
| Frontend | **Astro** + islands | **Decided.** Static-first marketing; islands for the dashboard. |
| Hosting | **Cloudflare Pages** | Static assets at the edge. |
| Edge relay | **nginx on a VPS** | TLS, filtering, flood absorption before Northflank. See [`../edge-relay.md`](../edge-relay.md) |
| Backend | **Rust on Northflank** | Auth orchestration, wallet, keys, limits, webhook, SSE, proxy. |
| Money | **SQLite** (embedded) | Transactions, constraints, the ledger. |
| Identity | **Rust, over embedded SQLite** | Google + password, verify, reset. Auth only. |
| Payments | **Midtrans Snap** | QRIS top-ups. |

> **This table previously named Pages Functions as a BFF, PocketBase as the
> database and PocketBase as the identity provider.** All are gone: the Rust server
> is the backend, money lives in embedded SQLite, and identity is served natively
> by the same crate over the `accounts` + `identities` tables — see
> [`architecture/identity.md`](../architecture/identity.md). Superseded sections
> below are marked; the frontend internals remain valid.

**Astro is decided** — reasoning and rejected alternatives are below.

## Topology

```
browser (Astro pages + islands)
  |  HTTPS, JSON + SSE
  v
Cloudflare  (DNS, TLS at edge, DDoS)
  |
  v
Edge relay  (nginx on a VPS - TLS, rate limits, body caps)
  |
  v
Northflank: Rust API + proxy  --->  SQLite file (money, identity, sessions)
```

**The full picture, including failover: [`docs/topology.md`](../topology.md).**

### Why a server is required at all

Three things cannot live in the browser:

1. **Midtrans server key** — required to verify webhook signatures.
2. **Wallet mutations** — crediting and debiting must be server-authoritative.
   A client that can write its own balance is not a wallet.
3. **Upstream provider keys** — the server's concern, never exposed.

**This was previously satisfied by a Cloudflare Pages Functions BFF.** That is no
longer the design: Pages Functions are stateless and short-lived, so they cannot
hold a password flow, a database transaction, or a long-lived SSE connection. The
**Rust server on Northflank** performs all three roles. See
[`docs/architecture.md`](../architecture.md).

### Realtime

**SSE from the Rust API.** No external identity service is involved — the wallet,
identity and usage data all live in the one SQLite file the Rust API streams from.
Contract:
[`docs/realtime.md`](../realtime.md).
## Auth token handling

- Identity is handled natively by this crate: the browser posts a Google **ID token** to `POST /auth/google`, or an email and password to `/auth/login` / `/auth/signup`. There is no third-party auth service and no token exchange step.
- The server verifies the credential (Google's JWKS, or Argon2id for a password), resolves the account over the `accounts` + `identities` tables, and issues an **opaque server-side session cookie** (`HttpOnly, Secure, SameSite=Lax`) backed by SQLite.
- **Logout is explicit and immediate**: `POST /auth/logout` revokes the session row in SQLite; `POST /auth/logout-all` revokes all active sessions for the account.
- Never put upstream provider keys, database URLs, or the Midtrans server key in client-visible config.

## Secrets

| Secret | Where | Browser-visible? |
| --- | --- | --- |
| Midtrans server key | Northflank (Rust env) | **No** |
| Midtrans client key | Pages env (`PUBLIC_MIDTRANS_CLIENT_KEY`) | Yes (by design) |
| Google client id (`[identity] google_client_id`) | Northflank (Rust env) | No — the audience the server checks, not a secret |
| Upstream provider keys | Northflank (Rust env) | **No** |
| Telegram bot token | Northflank (Rust / bot env) | **No** |
| Database connection string | Northflank (Rust env) | **No** |

Only `PUBLIC_*` variables may reach the client. See `.env.example`.

## Deployment

- **Static assets (Astro)** → Cloudflare Pages, built from the repo.
- **API + Proxy (Rust) + embedded SQLite** → Northflank with a persistent volume for the database file. The wallet ledger **and identity** live in that one SQLite file; there is no separate identity service to deploy.
- **Region:** keep Northflank and database geographically close to the Midtrans webhook receiver and Indonesian users (e.g. Singapore region).

## Frontend choice — decided

**Astro, with islands. Decision made.**

It was previously "recommended, not chosen", which blocked the first line of
frontend code. Settling it now:

| Criterion | Why Astro wins |
| --- | --- |
| Pages fit | Static output + islands. **No adapter** between framework and host |
| Cost | Static assets are free on Cloudflare Pages |
| Performance | Marketing pages ship with **zero JS** — matters on Indonesian mobile |
| Live updates | Identical `EventSource` code in any framework |
| Scope | One complex surface (the dashboard); islands cover it |

**The live-update requirement never constrained this.** The SSE client is the same
everywhere:

```js
const es = new EventSource(API + "/events", { withCredentials: true })
es.addEventListener("balance", e => updateBalance(JSON.parse(e.data)))
```

See [`realtime.md`](../realtime.md) for the full contract.

### What this settles

| Question | Answer |
| --- | --- |
| Framework | Astro |
| Rendering | Static-first, islands for interactivity |
| JS on marketing pages | None |
| Dashboard | A single interactive island |
| SSE client | Vanilla, in an island |
| Build output | Static assets for Cloudflare Pages |

### Rejected alternatives

| Option | Why not |
| --- | --- |
| **Next.js** | Needs `@cloudflare/next-on-pages`, an adapter that is an extra failure mode and a runtime we do not need |
| **SvelteKit** | Perfectly capable, but another runtime to learn for no gain at this scope |
| **Plain HTML/JS** | No build step, but hand-rolled routing and state for the dashboard is work with no payoff |

**If the team later turns out to be fluent in Next.js, switch.** The architecture is
unchanged; only the adapter appears. This is a reversible decision, and reversibility
is why it is safe to make now rather than continue deferring.

### Constraints it imposes

1. **The dashboard is an island, not a SPA.** If it grows into many views with
   shared client state, revisit — that is the signal Astro is the wrong tool.
2. **No secrets in the frontend.** Only `PUBLIC_*` variables reach the browser.
3. **The API base URL is a build-time variable** on Pages; changing it needs a
   rebuild, not a restart.
## Open questions

- [x] Token revocation strategy — resolved, see
      [05-security-decisions.md](05-security-decisions.md) D2.
- [x] Session/token lifetime: **30d absolute / 7d idle** — [`decisions.md`](../decisions.md).
- [x] Dashboard reads usage from the SSE stream (`GET /events`); polls `GET /api/me` as fallback only when SSE drops.
- [ ] Backup and restore procedure for the SQLite database file (money **and** identity — there is no separate identity service to back up any more).
- [x] Wallet mutations: **10/min per account** (`decisions.md`).