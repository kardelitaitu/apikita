# Decisions Register

**The single source of truth for settled decisions.** If a document lists something
here as open, this register wins.

This exists because the same decision was drifting: *session lifetime* was listed as
open in four documents, and *402 vs 429* was resolved in one while still open in
another. Scattered open items become contradictions, so settled decisions live here.

**Convention:** a decision is settled when it is here. Anything not here is genuinely
open. Do not re-open a settled decision in a document — change it here instead.

## Settled

### Stack

| Decision | Value | Rationale |
| --- | --- | --- |
| Frontend | **Astro + islands** | Native Pages fit, zero JS on marketing pages, reversible |
| Backend | **Rust on Northflank** | I/O-bound proxy; one service does API + proxy |
| Money store | **PostgreSQL** | Transactions, constraints, the ledger |
| Identity store | **PocketBase** | Auth only — never money |
| Payments | **Midtrans, QRIS only** | Card excluded: a flat fee is ~20% of a small top-up |
| Relay | **nginx on a VPS, L7** | TLS + filtering; absorbs load before the backend |
| CDN / edge | **Cloudflare** | Free tier; also where Pages lives |
| Realtime | **SSE from the Rust API** | One-directional; browser handles reconnect |

### Architecture

| Decision | Value | Rationale |
| --- | --- | --- |
| Relay role | **Optimisation, not a security boundary** | Failover requires a reachable backend, so the relay cannot be a hard boundary |
| Failover model | **Relay primary, backend fallback** | Relay down degrades, does not stop |
| Second relay | **No** | The backend is already the fallback; a second relay is cost without benefit |
| Session storage | **Server-side rows in Postgres** | Makes logout revoke immediately |
| SSE auth | **Session cookie, same-site subdomain** | A token in a query string lands in logs and history |
| Account key | **Postgres owns the id; PocketBase id is a linked column** | Auth is the component most likely to change |
| Proxy/API split | **One service for now** | They share the database, key lookup, and usage accounting |

### Money

| Decision | Value | Rationale |
| --- | --- | --- |
| Margin basis | **Uniform multiplier on all token classes** | Uniform markup yields a uniform margin rate |
| **Margin value** | **1.5 per model — no global default** | With zero fixed server overhead (Cloudflare & Northflank free tiers), M = 1.50 delivers competitive pricing in IDR while maintaining positive contribution |
| Margin location | **Per model, never global** | Pro-tier output costs ~3.4x flash; one global rate cannot fit both |
| Missing model price | **Startup error, not a default** | A silent default would bill at the wrong margin |
| Billing periods | **Peak + off-peak, billed at peak** | Off-peak is exactly half price; pricing at peak never loses |
| Credit source | **Midtrans webhook only** | Never the client callback, never the payload amount |
| Idempotency | **Unique `order_id`** | Enforced by the database, not by application logic |
| Money type | **`BIGINT` IDR** | Never floating point |
| Ledger | **Append-only; balance derivable** | Corrections are new rows, never edits |
| Balance floor | **`CHECK (balance_idr >= 0)`** | The database refuses a negative balance |
| Overdraft | **Not permitted — no flag** | The balance is non-negative by decision. `server/migrations/20260925000000_initial_schema.sql:18` is the authoritative backstop; the pre-flight in `server/src/routes/proxy.rs` always rejects when the reservation exceeds the balance. Gate 2 of [`launch-checklist.md`](launch-checklist.md) requires the constraint present and exercised |
| Refund policy | **Non-refundable, with a non-delivery exception** | The exception is what makes the clause defensible |

### API behaviour

| Decision | Value | Rationale |
| --- | --- | --- |
| Spend-limit breach | **402** | Permanent until top-up; 429 would invite a retry loop |
| Rate-limit breach | **429** | Transient; retrying later works |
| Wallet too low | **402** | Same as spend limit |
| Key metadata cache TTL | **60 seconds** | Bounds database load; revocation is honest about the window |
| Balance exhaustion | **Reject at pre-flight** | A truncated answer on non-refundable funds is the top dispute source |
| SSE events | **Absolute values, never deltas** | A lost delta leaves the UI wrong forever |
| Credentials | **Cookie for the dashboard, API key for `/v1/*`** | A leaked cookie must not be able to spend |

### Identity

| Decision | Value | Rationale |
| --- | --- | --- |
| Session lifetime | **30 days absolute, 7 days idle** | Rare re-login; bounded exposure on a stolen token |
| Password hashing | **Argon2id** | PocketBase owns this if it stays the auth provider — verify which applies |
| Login methods | **Google + email/password, with reset** | |
| Telegram | **A linked surface, not an identity provider** | One wallet, two surfaces |
| Review writes | **Telegram only** | A second writer makes "who reviewed" ambiguous |
| Review identity | **Keyed on the account, re-attributed on link** | Otherwise linking creates a second review slot |
| Review withdrawal | **A flag, never a delete** | Deleting frees the unique slot |

### Operations

| Decision | Value | Rationale |
| --- | --- | --- |
| CI provider | **GitHub Actions** | Assumed; only changes if the repo moves |
| Migrations | **`sqlx migrate`, forward-only** | Rust-native; no external runtime |
| Migration timing | **In CI, after a snapshot, before the server** | Ordered with the deploy; never on application boot |
| Migration safety | **Additive only** | Breaking changes are expand/contract across deploys |
| Deploy order | **Migrate -> server -> health -> frontend** | The order is what makes the non-atomic deploy safe |
| Admin actions | **Same API, same ledger, no back door** | An adjustment is a ledger row, not an edit |
| Operator flag location | **`accounts.is_operator`** — a column, not a separate table | One code path for authorization means one place to audit; a separate table invites a second lookup that can drift |
| Operator authentication | **Same session as customers**, plus the flag | Not a separate credential, which becomes a shared secret |
| Suspension | **Revokes sessions and keys atomically** | A status flag alone does not suspend anything |
| IP storage | **Salted hash only, salt deleted daily** | No raw IP is stored anywhere |
| Backup | **PITR plus offsite; restore drill required** | An untested backup is a belief |

### Product

| Decision | Value | Rationale |
| --- | --- | --- |
| Top-up location | **Telegram payments deferred** | Website only; the bot is a notification feed |
| Top-up feed identity | **Masked email, first and last character** | Six fixed asterisks so length does not leak |
| Review gating | **Open, but labelled** | Blocking non-customers loses real feedback |
| Review publication | **Aggregate only** | Individual reviews invite retaliation |
| Models offered | **Flash tier only** | Pro is ~3.4x the output cost and the arbitrage is unproven |
| Workload risk | **Cache-heavy usage is loss-making at any markup** | Segment by shape: ~19k IDR/month negative at 20M tokens. Do not acquire it on volume alone |

### Product behaviour and limits

| Decision | Value | Rationale |
| --- | --- | --- |
| Telegram feed timezone | **WIB (UTC+7)**, stored UTC | Indonesian audience; near-midnight dates need one zone |
| Telegram room access | **Customers only** | Masking hides identity, not spend amounts |
| Review gating | **Open, labelled `is_customer`** | Blocking non-customers loses real feedback |
| Review publication | **Aggregate only** | Naming reviewers invites retaliation |
| First-deposit minimum | **50,000 IDR** | 10k is negative against support cost |
| Re-top-up minimum | **10,000 IDR** | Nuisance filter, not a profitability filter |
| Top-up amount model | **Gross** — the customer pays what they enter | Simpler mental model; fee shown before payment |
| OAuth2 providers | **Only those guaranteeing verified email** | An unverified address is an account-takeover path |
| Thinking mode | **Exposed per request, not a separate model** | Thinking tokens bill as output; the caller decides |

### Limits and thresholds

| Decision | Value | Rationale |
| --- | --- | --- |
| Wallet mutation limit | **10/min per account** | Protects the money path |
| Circuit breaker | **3 failures to trip, 30s cooldown** | Consecutive 5xx/timeouts per endpoint |
| Cooldown backoff | **Doubles, capped at 900s** | Stop hammering a dead provider; still recover unaided |
| Spend/token limit window | **30 days rolling** | No month-boundary cliff; no timezone ambiguity across WIB/WITA/WIT |
| Second-operator threshold | **500,000 IDR** | Above routine goodwill, below anything worth stealing quietly |
| SSE replay buffer | **100 events** | Survives a tab sleep; bounded memory |
| SSE connections per account | **5** | One per tab plus headroom |
| SSE stream lifetime | **30 min**, then reconnect | A planned reconnect beats a surprise drop |
| Health-check interval | **30s, 2 failures to fail over** | Fast enough to matter, slow enough not to flap |
| Key cooldown on 429 | **5s per key** | A throttled key rotates out without tripping the endpoint |
| Key pool attempts | **3** | Keep ≤ pool size, or it retries cooled-down keys |
| Pool exhausted | **503 + Retry-After** | Fail fast; do not queue and look hung |
| Request timeout | **120s** | Upstream stall is treated as a failure |
| Top-up creation | **5/hour per account** | Blocks payment-session spam |
| Key creation | **10/day per account** | Blunts limit circumvention |
| Review submissions | **3/hour per account** | Edits are legitimate; spam is not |
| Low-balance DM | **Below 10,000 IDR, max 1/day** | At the re-top-up floor; capped so it is not spam |

### Recovery objectives

| Decision | Value | Rationale |
| --- | --- | --- |
| Backup tooling | **Managed PITR**, else `pg_dump` + `wal-g` | PITR gives minutes of RPO; simplest thing that meets it |
| RPO | **15 minutes** | The ledger is the business |
| RTO | **4 hours** | Achievable by restoring to a new host |
## Genuinely open

These need information that does not exist yet, not a design decision:

| Item | Blocked on |
| --- | --- |
| Legal review of the Terms of Service | A lawyer |
| Northflank actual prices | A quote |
| A second upstream provider | Reading its resale terms |
| Support cost per customer | Real usage data |
| Whether a staging environment exists | A cost decision |
| Credit expiry policy | A legal/commercial decision |
| Abuse-report contact | Publishing the terms |
| Review moderation policy | Operating experience |

**Everything else previously listed as open is now settled above.** Where a document
still lists one of these as open, this register wins.

**Build tasks (as opposed to decisions) live in [`launch-checklist.md`](launch-checklist.md).**
The two were being mixed, which made "open items" meaningless.

## Where decided values live

Decided *values* that the running system reads are in `config/apikita.toml`:

| Config section | Holds |
| --- | --- |
| `[pricing]` | Currency (margin is per model) |
| `[wallet]` | Deposit minimums, low-balance threshold |
| `[sessions]` | Absolute and idle lifetimes |
| `[limits]` | Per-account rate caps, key metadata cache TTL, limit window, admin threshold |
| `[realtime]` | SSE replay buffer, connection cap, stream lifetime |
| `[key_pool]` | Rotation, cooldowns, attempts |
| `[circuit_breaker]` | Trip threshold, cooldown + backoff cap, upstream timeout, health checks |
| `[streaming]` | Mid-stream cutoff, max output tokens, context cap. `allow_negative_balance_overdraft` was **removed** — overdraft is not permitted |
| `[[models]]` | Per-model margin, rates, endpoints, key env names |

**A decision here without a corresponding config value cannot be enforced.** If you
add one, give it a home in the config or it will be implemented as a hardcoded
constant that nobody can tune.

## How to change a decision

1. Edit this register.
2. Update the documents that reference it.
3. **Do not leave the old value in place** — a stale decision is worse than none,
   because it is followed.