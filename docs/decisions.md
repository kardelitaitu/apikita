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
| Money store | **SQLite (embedded, WAL)** | Transactions, constraints, the ledger — on the same host as the API, so a ledger write is a local file write rather than a network round trip |
| SQL driver | **`sqlx` with the `sqlite` feature** | Not `rusqlite`. The port is a 106-site dialect change, not an API rewrite; `sqlx migrate` is already settled below; and the bottleneck is the single writer, not the binding. Reasoning: [`plans/proxy-hot-path-audit.md`](plans/proxy-hot-path-audit.md) §3 |
| Identity store | **Rust-owned** — the `accounts` + `identities` tables (target) | Auth only — never money. **Status: PocketBase still authenticates at runtime** — the Rust session flow resolves a PocketBase token into a local session and the `identities` table is created but empty. Migration Phase 6 drops `pb_user_id` and retires PocketBase; until it lands the external service *does* remain, so `architecture/identity.md` is the operative description |
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
| Session storage | **Server-side rows in the local SQLite database** | Makes logout revoke immediately |
| SSE auth | **Session cookie, same-site subdomain** | A token in a query string lands in logs and history |
| Account key | **`accounts.id` is the only key (target); the PocketBase id column is dropped at Phase 6** | Auth is the component most likely to change, so it must not own the identity. **Status: `accounts.pb_user_id` is still present as `TEXT NOT NULL UNIQUE`** — it links to the live PocketBase user and is dropped only when Phase 6 retires the link |
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
| Money type | **`INTEGER` IDR** | Never floating point. Was `BIGINT` while the store was Postgres; a `STRICT` table accepts only `INT`, `INTEGER`, `REAL`, `TEXT`, `BLOB`, `ANY`, and **rejects `BIGINT` outright** (measured). The name had to change for the no-floating-point rule to stay structural rather than declared |
| Ledger | **Append-only; balance derivable** | Corrections are new rows, never edits |
| Balance floor | **`CHECK (balance_idr >= 0)`** | The database refuses a negative balance |
| Overdraft | **Not permitted — no flag** | The balance is non-negative by decision. The `CHECK (balance_idr >= 0)` on `wallets` in `server/migrations/20260925000000_initial_schema.sql` is the authoritative backstop; the pre-flight in `server/src/routes/proxy.rs` always rejects when the reservation exceeds the balance. Gate 2 of [`launch-checklist.md`](launch-checklist.md) requires the constraint present and exercised |
| Overdraft: what it does to reporting | **Usage and revenue diverge for a clamped request, by design** | A request whose hold could not be fully covered is billed at the balance — the debit is clamped — while `usage_daily` still records the **full** cost, because the service was genuinely consumed and the usage is a fact. The ledger records only what was taken, so reconciliation still holds. The consequence for anyone reading a margin report is the part that needs saying out loud: **revenue from a clamped request is lower than the usage it recorded**, so a margin computed from `usage_daily.cost_idr` alone will show less profit than was actually collected, and one computed from usage multiplied by the price card will show revenue that was never billed. Neither is a bug and neither is a rounding artefact — it is the difference between what a customer consumed and what they could pay |
| Refund policy | **Non-refundable; inbound refunds are refused, never applied** | The platform does not move money back. A Midtrans `refund`/`partial_refund` notification is answered **200** `{"status":"refund_not_supported"}`, logged at `error!`, and changes nothing — no wallet debit, no ledger row, `topups.status` stays `settled` |
| Refund: why 200, not 4xx | **A 4xx would make Midtrans retry an event that can never succeed** | The refusal reports a fact already true at the rail; a distinct body is the observable signal, and a 4xx would be indistinguishable from a transient 5xx in Midtrans' dashboard and in our alerting |
| Refund classification | **Named `PaymentAction::RefundRefused`, never `Unrecognised`** | Refusing a refund is a policy answer, not a status we failed to parse. Collapsing it into `Unrecognised` would mislabel settled policy as a classification gap |
| Refund code path | **Deleted, not quarantined** | `DebitRefund`, `refund_topup_transaction`, `RefundResult` and `refund_decision` are removed. A money-moving `pub fn` with no caller is one wiring away from misuse. `topup_ledger_ref` and `TopupCreditResult::NotSettleable` are kept — the credit path still needs both |
| `status='refunded'` / ledger `reason='refund'` | **Unreachable from the webhook; reachable from the wind-down runbook** | No *automatic* writer remains — an inbound Midtrans refund is refused. The closure payout is the one deliberate writer, and it is operator-run ([`wind-down.md`](wind-down.md)). The CHECK values stay because migrations are forward-only and additive-only — removing one is a table rebuild. A test fixture also writes them, simulating an out-of-band bank action |
| Refund: scope of the policy | **Non-refundable during operation, with no non-delivery exception** — settled by the owner | There is no code path that returns money **in response to a customer request**, and no self-serve refund. The register previously carried a non-delivery carve-out; that carve-out is withdrawn, and [`terms-of-service.md`](terms-of-service.md) now states the same. **This row does not govern wind-down**: a closure payout is platform-initiated and sits in the row above. Read the two together — a customer cannot ask for their money back, but we will hand it back when we stop serving. The terms remain a **draft outline requiring legal review (Gate 0)** — the text records the decision, it does not certify it is enforceable |
| Refund: enforceability caveat (**not resolved here**) | **Recorded, deliberately not fixed** | The "Open legal risk — flagged, not resolved" callout in the Refunds section of `terms-of-service.md`, and risk item 2 in `business/05-risk.md`, warn that a non-refundable clause which overreaches is likelier to be struck down *in its entirety* than a narrow one, and that on non-delivery the payer generally prevails at the dispute stage **regardless of the stated policy**. Withdrawing the carve-out therefore may reduce, not increase, the clause's protective value. This is a lawyer's call; it is written down so it is not discovered later |
| **Wind-down payout** | **On closure, every balance above USD 2.00 is paid out** — settled by the owner | The one thing that overrides non-refundable. Trigger is platform-initiated (we decide to stop), never a customer request, which is why it does not contradict the row above: same money, opposite initiator. See [`wind-down.md`](wind-down.md) |
| Wind-down: classification | **The rail a customer paid on decides it — "anyone who used Midtrans is considered Indonesian"** | Settled by the owner. Per account, monotone: **ever settled a Midtrans top-up ⇒ Indonesian ⇒ bank transfer; otherwise ⇒ USD stablecoin.** Monotone so it can never be applied retroactively, and it errs toward the domestic rail with a real entity behind it — never paying crypto to an Indonesian. **Today it degenerates to "everyone is Indonesian"**, because Midtrans QRIS is the only implemented rail — stated here rather than pretending the split is exercised |
| Wind-down: threshold | **`balance_idr > 2 × closure_usd_idr_rate`, strictly greater, rate frozen once** | The owner said "more than \$2". The rate is captured **once at wind-down start** (Bank Indonesia JISDOR) and reused for every payout: balances are frozen at the same instant, so there is no fairness argument for varying it, and one auditable number beats a moving target. A live FX API is exactly what is unavailable when you are shutting down. The threshold is **ceiled**, so a sub-\$2 balance is never paid automatically |
| Wind-down: conversion | **Floor to the cent, platform absorbs the remainder** | IDR is integral; stablecoin has decimals. Rounding **down** means the payout can never create money. The bank-transfer path needs no conversion at all — the balance is already whole IDR |
| Wind-down: sub-\$2 balances | **Discharged unconditionally; the company absorbs the transfer fee** | Silent forfeiture would be unjust enrichment, and a customer can legitimately fall below the floor: the first-deposit minimum is 50,000 IDR and the re-top-up minimum 10,000, so spending down past the threshold is normal, not abuse. Consumer-protection exposure at [`business/05-risk.md`](business/05-risk.md) |
| Wind-down: expiry interaction | **Expiry is waived at closure — the whole balance is paid** | The schema holds one **un-aged** `wallets.balance_idr` (no per-deposit date exists), so expired and live credit are **not distinguishable today** — refunding "only unexpired credit" is not computable. And clawing back credit at the exact moment we stop serving is forfeiture dressed as policy |
| Wind-down: ledger | **Reuse `reason='refund'`; no new CHECK value** | The value exists but was reserved-unreachable; a closure payout is precisely that semantic. **Adding a CHECK value is not additive-safe on SQLite** — it is a table rebuild (`ALTER … ADD CONSTRAINT` is a syntax error) — so reuse is the only safe route. The row must be written or `wallet = SUM(delta_idr)` breaks |
| Wind-down: implementation | **A documented runbook, not code** | There is no treasury, no disbursement API, no crypto provider, no KYC, and no customers. A payout service is a large outbound-money surface for an event that may never occur — the same hazard `decisions.md` already deleted every callerless money-moving function for. Deliverable is [`wind-down.md`](wind-down.md) plus a read-only report. **Flagged as a promise the code does not keep**, exactly as the expiry implementation is |
| `docs/billing-system.md` | **Demoted: considered, not adopted — do not treat as authoritative** | It is unlinked and partly stale (Postgres/JSON blueprint, NOWPayments). Kept rather than deleted because it is the **only written record** of the global/crypto rail intent and the PPh 22 0.21% reference this policy needs. Its \$2.00 crypto deposit minimum is likewise the likely origin of the \$2 payout floor |
| **Entity** | **Operate personally for now; a PT becomes mandatory above 4.8 billion IDR of turnover in a year** — settled by the owner | 4.8b IDR is the PP 55/2022 UMKM ceiling, above which the 0.5% final regime ends and normal corporate tax and PT status follow. Break-even is ~100-200 customers at ~20M tokens/month (`cost-and-sizing.md`), orders of magnitude below the trigger, so this will not fire in normal operation. **Consequence to accept:** until it fires there is no lawyer review, no corporate bank account, and no limited liability — see the deferred rows in §"Genuinely open" |
| Legal review | **Deferred with the entity.** Triggered by the same 4.8b IDR line | The wind-down clause is a promise to customers that has not been reviewed by a lawyer. It is written to *reduce* exposure, and is the one clause that should not be weakened. Re-open the moment the trigger is in sight |
| Crypto rail | **Not built, not promised.** Launch on Midtrans QRIS alone | The stablecoin payout path and the USD route exist only because of an orphaned rough plan (`billing-system.md`). Building a crypto rail would add Bappebti/OJK registration, PMK 50/2025 PPh 22, KYC/AML on an email-only payee, and cross-border reporting — the largest compliance surface in the plan, for a rail with zero code today. **Practical effect: everyone is Indonesian and gets a bank transfer**, which is what the classification already degenerates to |
| Credit expiry | **2 years (24 months) from each deposit's own date** | Settled by the owner. Per deposit, not per account or from last activity — a wallet holds credit of several ages, and expiry-from-last-activity would silently extend old credit. |
| Credit expiry: implementation | **Not built — the terms promise something the code does not do yet** | There is no per-deposit expiry column, no sweep job, and no refusal of a spend against aged credit. Policy is settled; the code is a build task in [`launch-checklist.md`](launch-checklist.md). Flagged because a policy the code does not keep is worse than no policy |
| Credit expiry: legal caveat | **Recorded, not resolved** | An expiring balance that is never refunded is a consumer-protection concern in Indonesia. 2 years is defensible, but the interaction with the non-refundable clause is not settled — part of the Gate 0 legal review |
| Refund: accepted risk | **Negative float, visible only via the refusal alert** | A chargeback returns the customer's money at the rail while the wallet keeps the credit. Because the ledger does not move, no drift check fires; the `error!` line is the only fast signal, and the monthly Midtrans-vs-`topups` reconciliation surfaces it up to a month later. Mitigation: alert on the refusal and hold a reserve. **Unalerted, this is a silent loss.** See [`website/04-payments.md`](website/04-payments.md) |

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
| Password hashing | **Argon2id, owned by the Rust API (target)** | **Status: PocketBase hashes passwords today**; the `identities.password_hash` column is created but empty. Once Phase 6 lands, Rust owns Argon2id outright and nothing else can. Parameters and the rehash-on-login policy become ours to set |
| Login methods | **Google + email/password, with reset** | Unchanged as a product decision; **today PocketBase implements both**, and once Phase 6 moves identity into Rust the Rust API implements all of it, including the pre-hijacking defences in [`architecture/identity.md`](architecture/identity.md) |
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
| Instance count | **Exactly one** | SQLite cannot be shared across replicas. Northflank enforces it too — a Single Read/Write volume is *"limited to 1 instance"*. Left unwritten, someone scales the service and corrupts the database |
| Deploy downtime | **Accepted — every deploy is a brief outage** | A Single Read/Write volume forbids a rolling restart: the old container is terminated before the new one starts. A published window beats a discovered one |
| Transaction mode | **`BEGIN IMMEDIATE` for any read-then-write transaction** | SQLite upgrades a deferred read transaction to a write lock, and `SQLITE_BUSY_SNAPSHOT` on that upgrade is unrecoverable — the transaction cannot be retried, only restarted |
| Timestamp representation | **Uniform RFC3339 with a format `CHECK`; time is never written in SQL** | SQLite compares `TEXT` lexicographically, so a second format is a silent correctness bug. Measured: mixing `CURRENT_TIMESTAMP` with sqlx's encoding extended session life by up to ~24h |
| API deployability | **Container-bound — no Cloudflare Workers path** | Workers has no filesystem, so a local database forecloses it; that route would need D1 and a second dialect |

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
| Backup tooling | **Litestream → Cloudflare R2**; `VACUUM INTO` + offsite as the fallback | Continuous WAL shipping gives seconds-level RPO against a 15-minute requirement. A sidecar is *forced* by the Single Read/Write volume, not merely preferred — and `VACUUM INTO` cannot run inside a transaction |
| RPO | **15 minutes** | The ledger is the business |
| RTO | **4 hours** | Achievable by restoring to a new host |

### Migration in flight — the direction is chosen, and the tree has partly moved

The entries this changes — Money store, SQL driver, Identity store, Account key,
Money type, Backup tooling, and the Operations additions — are **decided**.

As of Phases 0–5 of [`plans/sqlite-migration.md`](plans/sqlite-migration.md):

- **`server/` no longer builds against Postgres.** The dependency, the config, the
  compose file, the schema, the migration binary and every SQL statement in `src/`
  are SQLite. `cargo check --all-targets` is clean.
- **`server/` still calls PocketBase** for `auth-refresh`, and `accounts.pb_user_id`
  is still present and `NOT NULL`. Identity is Phase 6, and the column survives
  deliberately: `auth.rs` creates accounts through it, so dropping it earlier would
  break login while claiming the intermediate phases shipped intact.
- **The test suite runs, and it runs by default.** Phase 5 gave every database test
  its own migrated SQLite file in a temp directory, so no test needs
  `DATABASE_URL` and not one `#[ignore]` remains: measured
  **134 passed / 0 failed / 0 ignored**. The money tests — including the real
  concurrency proof of the overdraw fix,
  `concurrent_requests_cannot_overdraw_a_one_request_balance` — had been
  `#[ignore = "requires live Postgres"]` and were therefore never executed
  automatically. They now are.
- **Link-code redemption is built, and its safety is the ATTEMPT CAP, not the
  code's secrecy.** A 6-digit code is 10^6 possibilities — nothing to a script — so
  the endpoint is defended by counting every guess: `limits.link_redemption_per_hour`
  (20) bounds attempts per **client IP**, and `link_code_issuance_per_hour` (10)
  bounds codes in flight per **account**, because they are different attacks (a host
  cycling accounts touches no account at all). **The arithmetic, computed rather
  than asserted:** at 20 guesses/hour a host gets ~1.7 guesses inside one 5-minute
  code window, so the per-window hit chance on a *targeted* account is ~1.7·10^-6
  and the expected time to hit it is **~5.7 years** — the code rotates, so the space
  does not shrink and the search cannot be amortised. Against the
  **one-live-code-per-account** rule, that is the whole defence: the TTL is what
  makes the cap bite, and the cap is what makes the TTL survivable. A real user
  mistyping hits a handful of these.
  Three properties are pinned by tests rather than intent: **failed guesses count**
  (the attack IS the failure stream, so a counter that advanced only on success
  would never fire — proven by refusing a *correct* code once the budget is spent);
  **every refusal is byte-identical** (wrong/expired/used/unknown cannot be told
  apart, or the endpoint becomes an oracle that narrows 10^6 to the few hundred
  codes live at any moment); and **single-use holds under concurrency** (one
  conditional `UPDATE … WHERE used_at IS NULL`, no read-then-write). Attempts are
  stored as a **salted IP hash, never the raw address** (Gate 4).
- **`sessions.last_seen_at` now ENFORCES the 7-day idle bound as well as the
  30-day absolute one.** The idle half needed a write on the request path, and that
  design decision is now made: resolving a session cookie is the activity signal, so
  `resolve_account_from_cookie` refuses a session whose stored `last_seen_at` is
  older than `idle_days` and only then moves it to now (`routes/mod.rs`,
  `session_is_live_at`). The write lives on the session endpoints only — `/v1/*`
  authenticates API keys, not cookies, so the proxy hot path is untouched. An idle
  bound at or above the absolute lifetime is inert by construction, because a
  session's `expires_at` is seeded `login + absolute_days`; only a value BELOW the
  absolute lifetime (the shipped 7 against 30) changes any outcome.

This marker exists so the register does not lie in either direction. Here,
**settled means the direction is chosen, not that the tree matches it** — the register
is read by agents working in parallel with the port, and a value flipped ahead of the
code is the same failure as a value left behind it. The list above is the current
boundary between the two.

## Genuinely open

These need information that does not exist yet, not a design decision:

| Item | Blocked on |
| --- | --- |
| Legal review of the Terms of Service (incl. the wind-down clause) | A lawyer — **DEFERRED**, see the entity row below |
| Entity: PT vs personal | **Deferred, with a trigger: a PT and a lawyer become mandatory above 4.8 billion IDR of turnover in a year** (settled by the owner) |
| Cross-border reporting on USD-stablecoin payouts | Parked — there is no crypto rail (see the stablecoin row) |
| Refunding revenue already taxed under PP 55/2022 (0.5% PPh Final) — does the base reduce, and what of tax already paid? | An accountant — **budget as if the tax does not come back; treat any recovery as upside** |
| Stablecoin disbursement: Bappebti/OJK registration, PMK 50/2025 0.21% PPh 22 on liquidation, and whether paying crypto to a resident is itself a regulated act | **Parked with the rail.** No crypto code is built or promised; re-open only if a crypto rail is ever added |
| KYC/AML on an outbound payee whose only identity may be an email + a Google subject | A lawyer — **deferred until the entity trigger fires** |
| Consumer-protection review of expiry × non-refundable × the sub-\$2 discharge, taken together | A lawyer — **deferred; exposure reduced by discharging sub-\$2 balances unconditionally** |
| Treatment of unclaimed balances (retained liability vs write-off) | **Settled: a retained liability, never income** — the conservative answer costs nothing |
| Northflank actual prices | A quote — **assume the free tier holds; the architecture already assumes zero fixed overhead at M = 1.50** |
| A second upstream provider | **Deferred.** Every additional provider is a fresh R1 resale-terms check, so one provider until volume forces it |
| Support cost per customer | Real usage data — **placeholder in use: 25,000 IDR/customer/month** ([`business/03-financial-model.md`](business/03-financial-model.md)), so the model is usable now. Replace with measured data at the first real cohort |
| Whether a staging environment exists | **Not needed.** One instance, embedded SQLite, no production. Local plus a scratch-database runbook covers it |
| Abuse-report contact | Publishing the terms — **use a dedicated alias that is actually read, and publish it** |
| Review moderation policy | **Ship with "we moderate for abuse only."** Aggregate-only publication already limits the exposure; real policy needs operating experience |

**Everything else previously listed as open is now settled above.** Where a document
still lists one of these as open, this register wins.

**Build tasks (as opposed to decisions) live in [`launch-checklist.md`](launch-checklist.md).**
The two were being mixed, which made "open items" meaningless.

## Where decided values live

Decided *values* that the running system reads are in `config/apikita.toml`:

| Config section | Holds |
| --- | --- |
| `[pricing]` | Currency (margin is per model) |
| `[wallet]` | Deposit minimums. **Not enforced:** `low_balance_threshold_idr`, `low_balance_max_per_day`, `dormancy_days` (no low-balance alert is emitted and no sweep ages a wallet out), `min_monthly_tokens` (no monthly token floor is applied to anything), `reserve_settlement_cycles` (the reservation *is* sized for one cycle, by construction in `worst_case_reservation_idr` — this setting restates a hardcoded behaviour while reading as a control, and setting it to 5 would change nothing) |
| `[sessions]` | Absolute and idle lifetimes |
| `[limits]` | Per-account rate caps (top-up, key creation, link codes), key metadata cache TTL, limit window, admin threshold. **Not enforced:** `wallet_mutations_per_minute`, `review_per_hour` — a rate limit for a limiter that was never written, and for a review endpoint that does not exist. `wallet_mutations_per_minute` also **could not bind** if it were wired: `topup_per_hour` is 5 and is enforced on the only caller-controlled wallet mutation, and 5 per hour is under 10 per minute. It becomes the binding cap only if `topup_per_hour` rises above 600, which is a single config edit away with nothing enforcing the other side — see the arithmetic in `config/apikita.toml` |
| `[realtime]` | SSE replay buffer, connection cap, stream lifetime |
| `[key_pool]` | Rotation, cooldowns, attempts. **Not enforced:** `on_pool_exhausted` — only `reject_503` is implemented, and the config comment says the alternative "needs a queue", so a one-valued enum is presented as a setting |
| `[circuit_breaker]` | Trip threshold, cooldown + backoff cap, upstream timeout. **Not enforced:** `health_check_interval_seconds`, `health_check_failures` — there is no background health check; recovery is a trial request on real traffic, and `upstream/circuit_breaker.rs` says "no I/O" |
| `[streaming]` | Max output tokens. **Not enforced:** `mid_stream_cutoff` (the cut-off is always off and the flag is never consulted), `max_context_tokens` (**not a cap** — nothing refuses a request whose prompt exceeds it; the hold is sized from the actual body, so the ceiling bounds nothing. Its only production reader was a second `worst_case_reservation_idr` on `UpstreamClient` that no request ever called). `allow_negative_balance_overdraft` was **removed** — overdraft is not permitted |
| `[[models]]` | Per-model margin, peak rates, endpoints, key env names. **Not enforced:** `billing_basis` (every price is computed from the peak rates, so the offpeak class `input_offpeak` / `output_offpeak` / `cache_read_offpeak` is configured, checked, and never charged), `supports_vision`, `supports_thinking`, `concurrency_per_key`, `description` |

**A decision here without a corresponding config value cannot be enforced.** If you
add one, give it a home in the config or it will be implemented as a hardcoded
constant that nobody can tune.

## How to change a decision

1. Edit this register.
2. Update the documents that reference it.
3. **Do not leave the old value in place** — a stale decision is worse than none,
   because it is followed.