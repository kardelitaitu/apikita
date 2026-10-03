# Website Documentation

> **Superseded: identity is Rust-owned.** The Phase 6 identity port has landed —
> `accounts.pb_user_id` is dropped, `POST /auth/exchange` is deleted, the
> PocketBase HTTP client is gone from `server/`, and identity is served natively
> by this crate (`accounts` + `identities`, Argon2id). Where the text below still
> says PocketBase is the current identity provider, this notice governs;
> [`architecture/identity.md`](../architecture/identity.md) is the operative
> description.

Design for the customer-facing web surface. **Parts of it are built** — the
Status section below marks what exists and what is still design.

**Stack lives in [`docs/architecture.md`](../architecture.md)** — Pages + Rust (Northflank) + embedded SQLite (money and identity). That document is authoritative; the docs below cover design detail.

**The frontend's contract with the backend is
[`docs/server/api-spec.md`](../server/api-spec.md)** — every endpoint the dashboard
calls.

| Doc | Covers |
| --- | --- |
| [01-architecture.md](01-architecture.md) | Frontend internals. **Superseded on stack** — see `docs/architecture.md` |
| [02-data-model.md](02-data-model.md) | Data model — **the DDL is the historical PostgreSQL design**; the shipped schema is `server/migrations/20260925000000_initial_schema.sql` |
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

**The website is built and builds green.** Measured 2026-09-30:
`cd website && npm run build` emits **18 static pages** and `npm test` passes
**196 tests** (`node --test "tests/**/*.test.ts"` reports `# tests 196`, `# pass 196`).
Count them rather than recalling them — both numbers move whenever a page or a
contract test lands.
Every route in the spec's table
(lines 9-21) now has a page. What follows separates what exists from what is
designed-but-unbuilt; unbuilt items are marked, never deleted.

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
| `/dashboard/keys/new` | `src/pages/dashboard/keys/new.astro` | Create-key page: mounts the island, explains allowlist semantics |
| `/dashboard/settings` | `src/pages/dashboard/settings.astro` | Profile, password change, linked accounts (Telegram link/unlink live) |
| `/docs` | `src/pages/docs/index.astro` | Index over the docs pages |
| `/docs/quickstart` | `src/pages/docs/quickstart.astro` | The integration guide itself |
| `/models` | `src/pages/models/index.astro` | The model catalogue and per-token rates |
| `/privacy` | `src/pages/privacy.astro` | The data-retention table, customer-facing |
| `/admin` | `src/pages/admin/index.astro` | **Operator console** — account lookup + suspend/resume. Gated on `is_operator` |
| `/404` | `src/pages/404.astro` | Not-found fallback |

**Four islands exist, each mounted by exactly one page** (grep for
`src/islands/` under `src/pages/` returns four hits):

| Island | Lines | Mounted by |
| --- | --- | --- |
| `src/islands/keys/KeyManagement.astro` | 369 | `/dashboard/keys` |
| `src/islands/wallet/TopUpForm.astro` | 351 | `/dashboard/wallet` |
| `src/islands/usage/UsageAnalytics.astro` | 170 | `/dashboard/usage` |
| `src/islands/admin/AccountAdmin.astro` | 311 | `/admin` |

They are plain `<script>` islands, not UI-framework components —
`astro.config` has no integration and `package.json` has no framework dependency.

**The frontend↔backend glue is written.** `website/src/lib/` holds `api.ts` (fetch
client; base URL `PUBLIC_API_BASE_URL ?? http://localhost:8080`), `auth-api.ts`
(the native auth verbs — `/auth/signup`, `/auth/login`, `/auth/google`), `live.ts` (`GET /events` SSE with polling fallback),
plus `errors.ts`, `auth-flow.ts`, `format.ts`, `retry-wait.ts`. The auth pages and
the islands call these; `website/tests/auth-flow.test.ts` and
`rate-limit.test.ts` cover the auth and error paths.

**The routes those pages call exist too.** `server/src/routes/mod.rs` mounts
`/auth/signup`, `/auth/login`, `/auth/google`, `/auth/verify-email`,
`/auth/password-reset/request`, `/auth/password-reset/confirm`,
`/auth/verification/resend`, `/auth/logout`, `/auth/logout-all`,
`/auth/password-change`, `/auth/providers`, `/api/me`, `/api/usage`,
`/api/topups`, `/api/keys`, `/api/keys/{id}`, `/api/keys/{id}/revoke`, `/events`,
`/webhooks/midtrans`, `/v1/chat/completions`, and four `/api/admin/*` routes
(see [admin-surface.md](../admin-surface.md)). `pub const ROUTES` in that file is
the authoritative list.

**Both remaining spec'd routes are now built** (`/dashboard/keys/new`,
`/dashboard/settings`), so the route table above is complete: every route in
`docs/website/03-functional-spec.md` lines 9-21 has a page.

`/dashboard/keys/new` does **not** duplicate key creation. It mounts the
`KeyManagement` island and opens its create modal, so the island remains the single
`POST /api/keys` call site; the page adds the allowlist-semantics explanation the
modal cannot fit.

**Logout and "sign out everywhere" are implemented.** Both controls render in the
dashboard shell (`layouts/DashboardLayout.astro`, the `#logout` and `#logout-all`
buttons) and are wired to `POST /auth/logout` and `POST /auth/logout-all`
respectively; each clears the local token and redirects to `/login`. "Sign out
everywhere" lives only in the header — `/dashboard/settings` links to it rather than
repeating it.

> **Status: IMPLEMENTED.** Telegram link/unlink is a live control.
> `server/src/routes/mod.rs` mounts `POST /api/telegram/link-code` (lines 199) and
> `DELETE /api/telegram` (lines 200), and `/dashboard/settings` now drives both:
> "Link Telegram" issues a single-use 5-minute code and shows the exact
> `/link <code>` command, and "Unlink Telegram" removes the link while leaving the
> account and balance untouched. The page previously described the flow read-only
> on the false premise that no routes were mounted; that premise was stale.

**Identity is Rust-owned — the PocketBase split is history.** The pages
(`login.astro`, `signup.astro`, `verify.astro`, `reset.astro`) speak to this
crate's native auth routes through `src/lib/auth-api.ts`; `src/lib/pocketbase.ts`
is deleted and the `pocketbase` npm dependency is gone from `website/package.json`.
The server side of the migration (Phase 6) has **landed**: `routes/auth.rs` no
longer reads `POCKETBASE_URL`, `accounts.pb_user_id` is dropped, and
`POST /auth/exchange` is deleted. Phase 7 (admin) has also landed as the `/admin`
operator console over the Rust admin routes. The operative description is
[`architecture/identity.md`](../architecture/identity.md).

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
| Edit a key's limits / models / label | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) | **Built** — the island's Edit modal issues `PATCH /api/keys/:id`. A limit *reduction* shows the ≤60s cache-TTL warning |
| Usage limits (spend / token / rate / expiry) | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) | **UI built** (limit fields in the island). Server-side enforcement in `server/src/routes/proxy.rs` not verified here |
| Google + email/password login | [03-functional-spec.md](03-functional-spec.md), [../architecture/identity.md](../architecture/identity.md) | **Pages built** (`/login`, `/signup`). The Google round-trip was not exercised |
| Logout + sign out everywhere | [03-functional-spec.md](03-functional-spec.md), [05-security-decisions.md](05-security-decisions.md) | **Built** — header controls in `layouts/DashboardLayout.astro` call `POST /auth/logout` and `POST /auth/logout-all` |
| Password reset | [03-functional-spec.md](03-functional-spec.md) | **Built** — `/reset`, `/reset/confirm`, covered by `tests/auth-flow.test.ts` |

### Next step

The page layer is largely written. What remains:

1. ~~PocketBase collections and API rules~~ ([02-data-model.md](02-data-model.md)) —
   **not work in this repository, and now moot.** The identity port has **landed**:
   identity is Rust-owned over the `accounts` + `identities` tables in the same
   embedded SQLite database as money, `accounts.pb_user_id` is dropped and
   `POST /auth/exchange` is deleted. There are no PocketBase collections to
   configure and no PocketBase instance to run.
2. ~~Astro pages and the dashboard islands~~ — **built**; see "What exists today".
   No page gaps remain: every route in the spec's table has a file.
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
- ~~Migration tooling for Postgres (sqlx assumed)~~ — **decided: `sqlx migrate`, forward-only** (§Operations); the store itself is now **SQLite**, not Postgres (§Stack) — applied by `DATABASE_URL=sqlite://<path> cargo run --bin migrate`
- ~~First-deposit minimum (50k-100k under discussion)~~ — **decided: 50,000 IDR**; re-top-up 10,000 IDR (§Product behaviour and limits)
- ~~Whether exceeding a spend limit returns 402 or 429~~ — **decided: 402** for a spend-limit breach, **429** for a rate-limit breach (§API behaviour)
- ~~Proxy cache TTL for key metadata~~ — **decided: 60 seconds** (§API behaviour)
- ~~Reconciliation job between Postgres and PocketBase~~ — **void**: identity and
  money now live in one embedded SQLite database, `accounts.pb_user_id` is dropped,
  so a row cannot exist on one side only. There is no reconciliation job to schedule
  and no orphan check to build — see
  [`../architecture/identity.md`](../architecture/identity.md) §Reconciliation.

Nothing this document used to list as open is still undecided. The reconciliation
item above is void rather than open: it was a consequence of two stores, and there is
now one. Genuinely open items are in the
register's §"Genuinely open"; build tasks are in
[`launch-checklist.md`](../launch-checklist.md).

### Verified

The schema in [02-data-model.md](02-data-model.md) was parsed and checked when it was
written — that was the **Postgres** design, and it is **not re-run and no longer the
shipped schema**:
20 statements, 9 tables, 9 foreign keys all resolving, no identity columns in the
money store, and no money column using a floating-point type.

The shipped schema is
[`server/migrations/20260925000000_initial_schema.sql`](../../server/migrations/20260925000000_initial_schema.sql),
every table `STRICT`, validated by
[`tools/sqlite-probes/validate-migration-schema.py`](../../tools/sqlite-probes/validate-migration-schema.py)
(see [`docs/architecture.md`](../architecture.md) §Database).