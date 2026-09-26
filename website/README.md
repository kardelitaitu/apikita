# website

The customer-facing web surface for the platform.

**Responsibility:** landing/marketing page, account signup and login, API key
issuance, prepaid wallet top-up, usage and billing dashboards.

**Not this folder:** the request-routing proxy itself (see `server/`) and the
operator chat interface (see `telegram/`).

## Auth

Two login methods:

1. **Google sign-in**
2. **Email + password**, with email verification and password reset

Both are handled by **PocketBase**. The website is where the user signs in, but it
is not the identity store — PocketBase is. The **account** owns the wallet and the
API keys; the Telegram bot links to that account rather than holding its own
identity.

Design, including the account-takeover rules PocketBase already handles:
[`docs/architecture/identity.md`](../docs/architecture/identity.md).

## Stack

**Astro** + islands, deployed on **Cloudflare Pages**. Static marketing pages with
interactive islands for the dashboard.

The backend is the **Rust API on Northflank** — this folder talks to it and holds
no secrets. Endpoints: [`docs/server/api-spec.md`](../docs/server/api-spec.md).

> `PUBLIC_*` variables are inlined into browser JavaScript and are **not secret**.

## Live updates

The dashboard subscribes to the backend's SSE stream so balance and usage update
without a refresh, falling back to polling if the stream drops.

## Status

**Built and building.** Measured: `npm run build` emits 15 pages and `npm test`
passes 52 tests (`node --test "tests/**/*.test.ts"` reports `# tests 52`, `# pass 52`).
What exists today:

- **Public pages** — `/` (landing + pricing), `/login`, `/signup`, `/verify`,
  `/reset`, `/reset/confirm`, `/docs` and `/docs/quickstart`.
- **Dashboard** — `/dashboard` plus `/dashboard/{keys,usage,wallet}`, and the
  `/dashboard/keys/new` and `/dashboard/settings` pages.
- **Islands** — `islands/keys/KeyManagement.astro`, `islands/usage/UsageAnalytics.astro`,
  `islands/wallet/TopUpForm.astro`, each mounted by its page.
- **Shared layer** — `lib/{api,errors,format,live,pocketbase,retry-wait,auth-flow}.ts`.

`/dashboard/keys/new` and `/dashboard/settings` are specified
(`docs/website/03-functional-spec.md` lines 17, 20) and both have a page
(`src/pages/dashboard/keys/new.astro`, `src/pages/dashboard/settings.astro`).

Design and the authoritative built-vs-planned inventory:
[`docs/website/README.md`](../docs/website/README.md).

> **Note on the identity description above.** `docs/decisions.md` settles identity as
> **Rust-owned** (SQLite, no external auth service), but that migration is *decided, not
> yet in the code* — see the register's "Migration in flight" marker. The running server
> still calls PocketBase, so this folder still uses the PocketBase client.
