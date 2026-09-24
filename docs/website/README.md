# Website Documentation

Design for the customer-facing web surface. Written before implementation.

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

**Planning complete.** Stack chosen (see [01-architecture.md](01-architecture.md));
no code written.

### Requirement coverage

Every feature in scope has a specification:

| Requirement | Where |
| --- | --- |
| Top up via dynamic QRIS (Midtrans) | [04-payments.md](04-payments.md) |
| Check balance (live, no refresh) | [01-architecture.md](01-architecture.md), [02-data-model.md](02-data-model.md) |
| Create API keys, all models | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) |
| Usage limits (spend / token / rate / expiry) | [06-api-keys-and-limits.md](06-api-keys-and-limits.md) |
| Google + email/password login | [03-functional-spec.md](03-functional-spec.md), [../architecture/identity.md](../architecture/identity.md) |
| Logout + sign out everywhere | [03-functional-spec.md](03-functional-spec.md), [05-security-decisions.md](05-security-decisions.md) |
| Password reset | [03-functional-spec.md](03-functional-spec.md) |

### Next step

Implementation. The unbuilt work is:

1. PocketBase collections and API rules ([02-data-model.md](02-data-model.md))
2. BFF endpoints (auth, wallet, keys, Midtrans webhook)
3. Astro pages and the dashboard islands
4. Proxy-side key enforcement and limit checks ([06-api-keys-and-limits.md](06-api-keys-and-limits.md))

### Decisions still open

Listed per document. The blocking ones before code:

- ~~Frontend framework on Pages~~ — **decided: Astro + islands**
- Session lifetime and refresh policy
- Migration tooling for Postgres (sqlx assumed)
- First-deposit minimum (50k-100k under discussion)
- Whether exceeding a spend limit returns 402 or 429
- Proxy cache TTL for key metadata
- Reconciliation job between Postgres and PocketBase

### Verified

The Postgres schema in [02-data-model.md](02-data-model.md) was parsed and checked:
20 statements, 9 tables, 9 foreign keys all resolving, no identity columns in
Postgres, and no money column using a floating-point type.