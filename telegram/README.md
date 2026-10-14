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

**This FOLDER is empty scaffolding** - it holds this README and nothing else, so there is
no bot process, no dependency manifest and no chosen stack above. That part is accurate.

**The SERVER half is BUILT, and reading only this folder would miss it.** The endpoints a
bot needs already exist and are mounted (`server/src/routes/telegram.rs`):

| Endpoint | Auth | Purpose |
| --- | --- | --- |
| `POST /api/telegram/link-code` | session cookie | issue the short-lived code a user sends the bot |
| `DELETE /api/telegram` | session cookie | unlink the Telegram account |
| `POST /api/bot/link` | **bot token** | the bot redeems the code, linking the chat to the wallet |

So "nothing implemented" would be wrong about the system: the linking flow, its credential
model and its refusal ordering are implemented and tested, including that `/api/bot/link`
checks the bot token BEFORE the code's shape so an unauthenticated caller learns nothing.
The reviews and top-up-notification routes remain design-only - `docs/server/api-spec.md`
marks them, and a route-table test asserts they 404 rather than half-working.

**What is left is the bot itself**, which is why the stack line above is still open.
