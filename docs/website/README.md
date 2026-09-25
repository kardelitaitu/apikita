# Website Documentation

Design for the customer-facing web surface. **Parts of it are built** — the
Status section below marks what exists and what is still design.

**Stack lives in [`docs/architecture.md`](../architecture.md)** — Pages + Rust (Northflank) + Postgres (money) + PocketBase (identity). That document is authoritative; the docs below cover design detail.

**The frontend's contract with the backend is
[`docs/server/api-spec.md`](../server/api-spec.md)** — every endpoint the dashboard
calls.

| Doc | Covers |
| --- | --- |
| [01-architecture.md](01-architecture.md) | Frontend internals. **Superseded on stack** — see `docs/architecture.md` |
| [02-data-model.md](02-data-model.md) | PostgreSQL schema: tables, constraints, transactions, backups |
| [03-functional-spec.md](03-functional-spec.md) | Every page, flow, and state |
| [04-payments.md](04-payments.md) | Midtrans QRIS top-up, webhook, settlement, reconciliation |
| [05-security-decisions.md](05-security-decisions.md) | Wallet immutability, session revocation, verification integrity |
| [06-api-keys-and-limits.md](06-api-keys-and-limits.md) | Key creation, model access, spend/token/rate limits, proxy enforcement |

Related: [`docs/architecture/identity.md`](../architecture/identity.md) — login,
linking, and account-takeover rules.

## Scope

The website is the **system of record**: it owns accounts, wallets, and API keys.
The Telegram bot links to these accounts rather than holding its own.

It does **not** proxy LLM requests. That is [`server/`](../../server/README.md).

## Status

**The website is partly built and builds green.** Measured for this revision:
`cd website && npm run build` emits **12 static pages** and `npm test` passes
**18 tests** (`node --test "tests/**/*.test.ts"`). What follows separates what
exists from what is designed-but-unbuilt; unbuilt items are marked, never deleted.

### What exists today

Every route below has a file under `website/src/pages/` and is emitted by the build.

| Route | File | What it does |
| --- | --- | --- |
| `/` | `src/pages/index.astro` | Landing: pricing table, how-it-works, FAQ. Static, no JS |
| `/login` | `src/pages/login.astro` | Google button + email/password form |
| `/signup` | `src/pages/signup.astro` | Both signup paths, spec §Signup |
| `/verify` | `src/pages/verify.astro` | Verification-link landing |
| `/reset` | `src/pages/reset.astro` | Reset request, one neutral reply |
| `/reset/confirm` | `src/pages/reset/confirm.astro` | Set a new password |
| `/dashboard` | `src/pages/dashboard.astro` | Balance + today's usage; SSE with polling fallback |
| `/dashboard/keys` | `src/pages/dashboard/keys.astro` | Mounts the `KeyManagement` island |
| `/dashboard/usage` | `src/pages/dashboard/usage.astro` | Mounts the `UsageAnalytics` island |
| `/dashboard/wallet` | `src/pages/dashboard/wallet.astro` | Mounts the `TopUpForm` island |
| `/docs` | `src/pages/docs/index.astro` | Index over the docs pages |
| `/docs/quickstart` | `src/pages/docs/quickstart.astro` | The integration guide itself |

**Three islands exist, each mounted by exactly one page** (grep for
`src/islands/` under `src/pages/` returns three hits):

| Island | Lines | Mounted by |
| --- | --- | --- |
| `src/islands/keys/KeyManagement.astro` | 369 | `/dashboard/keys` |
| `src/islands/wallet/TopUpForm.astro` | 351 | `/dashboard/wallet` |
| `src/islands/usage/UsageAnalytics.astro` | 170 | `/dashboard/usage` |

They are plain `<script>` islands, not UI-framework components —
`astro.config` has no integration and `package.json` has no framework dependency.

**The frontend↔backend glue is written.** `website/src/lib/` holds `api.ts` (fetch
client; base URL `PUBLIC_API_BASE_URL ?? http://localhost:8080`), `pocketbase.ts`
(`POST /auth/exchange`), `live.ts` (`GET /events` SSE with polling fallback),
plus `errors.ts`, `auth-flow.ts`, `format.ts`, `retry-wait.ts`. The auth pages and
the islands call these; `website/tests/auth-flow.test.ts` and
`rate-limit.test.ts` cover the auth and error paths.

**The routes those pages call exist too.** `server/src/routes/mod.rs` mounts
`/auth/exchange`, `/auth/logout`, `/auth/logout-all`, `/api/me`, `/api/usage`,
`/api/topups`, `/api/keys`, `/api/keys/{id}`, `/api/keys/{id}/revoke`, `/events`,
`/webhooks/midtrans`, `/v1/chat/completions`, and four `/api/admin/*` routes
(see [admin-surface.md](../admin-surface.md)).

**Specified but not built.** `docs/website/03-functional-spec.md` lists two routes
that have no file under `src/pages/`:

> **Status: NOT IMPLEMENTED.** `/dashboard/keys/new` — key creation happens
> inside the `KeyManagement` island on `/dashboard/keys` instead.
> `/dashboard/settings` — profile, password, linked accounts.

> **Status: NOT IMPLEMENTED (no UI).** Logout and "sign out everywhere" have server
> routes (`/auth/logout`, `/auth/logout-all`) but no control found in the dashboard
> shell.

**One decision has overtaken the code.** `src/lib/pocketbase.ts`, `login.astro`,
`signup.astro` and `verify.astro` all speak to PocketBase, while
[`docs/decisions.md`](../decisions.md) settles identity as **Rust-owned** and records
PocketBase as going. The pages work against the older split and will need rework
when the port lands — flagged here, not resolved here.

**Not verified by this document:** that the islands' request and response shapes
match `server/src/routes/` field-for-field. Both sides exist; the contract between
them was not exercised end to end for this revision.

### Requirement coverage

Every feature in scope has a specification. The third column is the implementation
state, checked against the tree rather than assumed:

| Requirement | Where | Built? |
| --- | --- | --- |
| Top up via dynamic QRIS (Midtrans) | [04-payments.md](04-payments.md) | **Page + island built** (`/dashboard/wallet`, `TopUpForm`). A real Midtrans round-trip was not verified |
| Check balance (live, no refresh) | [01-architecture.md](01-architecture.md), [02-data-model.md](02-data-model.md) | **Built** — `lib/live.ts` SSE + polling fallback, `GET /events` mounted |
| Create API keys, all models | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) | **Built** — `KeyManagement` island over `/api/keys` |
| Usage limits (spend / token / rate / expiry) | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) | **UI built** (limit fields in the island). Server-side enforcement in `server/src/routes/proxy.rs` not verified here |
| Google + email/password login | [03-functional-spec.md](03-functional-spec.md), [../architecture/identity.md](../architecture/identity.md) | **Pages built** (`/login`, `/signup`). The Google round-trip was not exercised |
| Logout + sign out everywhere | [03-functional-spec.md](03-functional-spec.md), [05-security-decisions.md](05-security-decisions.md) | **Routes built**, **no UI control** (see above) |
| Password reset | [03-functional-spec.md](03-functional-spec.md) | **Built** — `/reset`, `/reset/confirm`, covered by `tests/auth-flow.test.ts` |

### Next step

The page layer is largely written. What remains:

1. ~~PocketBase collections and API rules~~ ([02-data-model.md](02-data-model.md)) —
   **superseded**: [`docs/decisions.md`](../decisions.md) settles identity as
   Rust-owned with the PocketBase id column dropped, so this is no longer the work.
2. ~~Astro pages and the dashboard islands~~ — **built**; see "What exists today".
   The remaining page gaps are `/dashboard/keys/new` and `/dashboard/settings`.
3. **Proxy-side key enforcement and limit checks**
   ([06-api-keys-and-limits.md](06-api-keys-and-limits.md)) — `server/src/routes/proxy.rs`
   is mounted; whether it enforces every rule in that document was **not verified**
   for this revision.
4. The BFF endpoints (auth, wallet, keys, Midtrans webhook) are **mounted** in
   `server/src/routes/mod.rs`; treat the work as integration testing them against
   the pages, not writing them.

### Decisions still open

[`docs/decisions.md`](../decisions.md) is the register and it wins. Every item this
document used to list as open is settled there, or has been overtaken:

- ~~Frontend framework on Pages~~ — **decided: Astro + islands** (register §Stack)
- ~~Session lifetime and refresh policy~~ — **decided: 30 days absolute, 7 days idle** (§Identity)
- ~~Migration tooling for Postgres (sqlx assumed)~~ — **decided: `sqlx migrate`, forward-only** (§Operations); the store itself is now **SQLite**, not Postgres (§Stack)
- ~~First-deposit minimum (50k-100k under discussion)~~ — **decided: 50,000 IDR**; re-top-up 10,000 IDR (§Product behaviour and limits)
- ~~Whether exceeding a spend limit returns 402 or 429~~ — **decided: 402** for a spend-limit breach, **429** for a rate-limit breach (§API behaviour)
- ~~Proxy cache TTL for key metadata~~ — **decided: 60 seconds** (§API behaviour)
- ~~Reconciliation job between Postgres and PocketBase~~ — **moot**: the two-store
  split is being retired (§Stack, §"Migration in flight"). No reconciliation
  decision is recorded, because there is no longer a second store to reconcile.

Nothing in this document is still open. Genuinely open items are in the register's
§"Genuinely open"; build tasks are in [`launch-checklist.md`](../launch-checklist.md).

### Verified

The Postgres schema in [02-data-model.md](02-data-model.md) was parsed and checked
(when it was written; **not re-run for this revision**):
20 statements, 9 tables, 9 foreign keys all resolving, no identity columns in
Postgres, and no money column using a floating-point type.