# 01 — Architecture

> **Superseded on stack — read [`docs/architecture.md`](../architecture.md) first.**
> This document specified Cloudflare Pages Functions + PocketBase. The system now uses a Rust backend on Northflank with PostgreSQL. The frontend-internal sections below remain useful; the stack and backend sections are superseded.

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
| Money | **PostgreSQL** | Transactions, constraints, the ledger. |
| Identity | **PocketBase** | Google + password, verify, reset. Auth only. |
| Payments | **Midtrans Snap** | QRIS top-ups. |

> **This table previously named Pages Functions as a BFF and PocketBase as the
> database.** Both are gone: the Rust server is the backend, and money lives in
> Postgres. Superseded sections below are marked; the frontend internals remain
> valid.

Astro is a recommendation, not a hard requirement — see [Why Astro](#why-astro).

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
Northflank: Rust API + proxy  --->  PostgreSQL (money, sessions)
                                --->  PocketBase  (identity only)
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

**SSE from the Rust API.** PocketBase's realtime cannot help — the wallet and usage
data live in Postgres, which PocketBase does not stream. Contract:
[`docs/realtime.md`](../realtime.md).
## Auth token handling

- PocketBase issues a stateless auth token; there are **no server-side sessions
  and no logout endpoint**. "Logout" is discarding the token client-side.
- **Tokens ARE revocable**, but not by logging out. Every auth record carries a
  `tokenKey` mixed into the JWT signing key; rotating it invalidates all of that
  user's tokens instantly. PocketBase rotates it automatically on password or
  email change, and `RefreshTokenKey()` does it explicitly. See
  [05-security-decisions.md](05-security-decisions.md) D2.
- Store the token in an **HttpOnly, Secure, SameSite cookie** set by the Rust API
  rather than localStorage, so XSS cannot read it.
- Never put provider keys or the Midtrans server key in client-visible config.

## Secrets

| Secret | Where | Browser-visible? |
| --- | --- | --- |
| Midtrans server key | Pages env | **No** |
| Midtrans client key | Pages env | Yes (by design) |
| PocketBase superuser creds | Pages env | **No** |
| Upstream provider keys | Northflank (server) | **No** |
| Telegram bot token | Northflank (bot) | **No** |

Only `PUBLIC_*` variables may reach the client. See `.env.example`.

## Deployment

- **Static assets + Functions** → Cloudflare Pages, built from the repo.
- **PocketBase** → needs a persistent disk (SQLite). It cannot run on Pages.
  Host it on a VPS or a container host with a volume, and **back it up** — the
  wallet ledger lives there. See [02-data-model.md](02-data-model.md) on backups.
- **Region:** keep PocketBase geographically close to the Midtrans webhook
  receiver and to your customers. Webhooks arriving late is not a correctness
  problem, but a slow dashboard is.

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
- [ ] Decide `AuthToken.Duration` (token lifetime).
- [ ] Does the dashboard read usage from the API or subscribe to the SSE stream?  (SSE is the design — confirm the aggregate endpoint is not polled in parallel)
- [ ] Backup and restore procedure for PocketBase.
- [x] Wallet mutations: **10/min per account** (`decisions.md`).