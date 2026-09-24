# telegram

The Telegram bot service.

**Responsibility:** operator and/or customer control surface over chat —
balance and usage lookups, top-up notifications, low-balance warnings,
upstream endpoint health alerts, and circuit-breaker events.

**Not this folder:** the proxy (`server/`) and the web dashboard (`website/`).

Full specification: [`docs/telegram/README.md`](../docs/telegram/README.md) — rooms,
the top-up feed, and the review flow.

## How it fits

**The bot is a control surface, not an identity provider.** A Telegram account
links to an existing web account via a short-lived `/link` code, so both surfaces
see the same wallet. See
[`docs/architecture/identity.md`](../docs/architecture/identity.md).

The bot talks to the **Rust API on Northflank** — it does not touch the database
directly, and holds only its own bot token.

## Stack

Undecided. The bot is a thin client over
[`docs/server/api-spec.md`](../docs/server/api-spec.md), so the choice is low-risk
and can be made last.

## Status

Empty scaffolding. Nothing implemented.
