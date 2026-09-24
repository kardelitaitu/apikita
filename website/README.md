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

Empty scaffolding. Nothing implemented. Full design in
[`docs/website/README.md`](../docs/website/README.md).
