# ApiKita — Development Roadmap & Checklist

The living development roadmap for the ApiKita high-throughput LLM arbitrage proxy gateway.

---

## Phase 0: Groundwork & Specification

- [x] **Documentation & Architecture**
  - [x] Settle core decisions register ([`docs/decisions.md`](docs/decisions.md))
  - [x] Design PostgreSQL schema ([`docs/website/02-data-model.md`](docs/website/02-data-model.md))
  - [x] Define HTTP API specification ([`docs/server/api-spec.md`](docs/server/api-spec.md))
  - [x] Audit cross-document consistency and patch obsolete PocketBase/Pages references
  - [x] Confirm official Midtrans SHA-512 constant-time signature formula
  - [x] Adopt competitive $M = 1.50$ pricing model (zero fixed server overhead)
- [x] **Repository & Version Control**
  - [x] Initialize Git on branch `main` and remote `kardelitaitu/apikita.git`
  - [x] Branch out to active development branch `0.0.1`
- [x] **Scaffolding & Foundations**
  - [x] Initial PostgreSQL migration script ([`server/migrations/20260925000000_initial_schema.sql`](server/migrations/20260925000000_initial_schema.sql))
  - [x] Strongly-typed config parser and validator ([`server/src/config.rs`](server/src/config.rs))
  - [x] Core money calculations & constant-time signature verification ([`server/src/money.rs`](server/src/money.rs))
  - [x] Atomic ledger transaction functions ([`server/src/db.rs`](server/src/db.rs))
  - [x] Axum server route structure & RFC 7807 error model ([`server/src/error.rs`](server/src/error.rs))
- [x] **Benchmarking & Limits Verification**
  - [x] Document benchmark specification ([`docs/benchmark.md`](docs/benchmark.md))
  - [x] Implement and execute release benchmark harness ([`server/src/bin/benchmark.rs`](server/src/bin/benchmark.rs))
  - [x] Verify capacity under 1-thread simulation (tested 1,000 concurrent streams at 63,750 tokens/sec with 34MB RAM)

---

## Phase 1: Real Upstream Proxy Streaming Engine (`server/`)

- [ ] **100-Key Pool Router & Load Balancer**
  - [ ] Implement atomic least-loaded key selection (`min_by_key(|k| k.in_flight)`)
  - [ ] Implement granular per-key 5-second cooldown upon receiving HTTP 429
  - [ ] Implement client-side transparent retry across available keys (up to `max_key_attempts = 3`)
  - [ ] Configure `reqwest::Client` persistent HTTP connection pooling (`pool_max_idle_per_host(100)`)
- [ ] **Circuit Breaker**
  - [ ] Consecutive failure counter (3 failures to trip to `Open`)
  - [ ] Exponential cooldown backoff (30s $\rightarrow$ 60s $\rightarrow$ capped at 900s)
  - [ ] Half-open trial request logic
- [ ] **Streaming Pipeline (`POST /v1/chat/completions`)**
  - [ ] Bearer API key validation with 60-second in-memory TTL cache
  - [ ] Model allowlist verification
  - [ ] Pre-flight worst-case balance reservation check (`402 Payment Required` on breach)
  - [ ] Non-buffering SSE chunk forwarding (client $\leftarrow$ server $\leftarrow$ upstream)
  - [ ] Parse usage summary from final SSE chunk
  - [ ] Execute atomic usage settlement (`debit_usage_transaction`) on stream completion
  - [ ] Ensure mid-stream errors do not retry silently

---

## Phase 2: Session Auth & Realtime SSE

- [ ] **PocketBase Auth Integration**
  - [ ] Wire `POST /auth/exchange` to verify PocketBase JWTs via internal REST call
  - [ ] Upsert `accounts(pb_user_id)` and initialize default wallet
  - [ ] Issue opaque session cookie (`session=<token>; HttpOnly; Secure; SameSite=Lax`)
  - [ ] Implement session revocation on `POST /auth/logout` and `POST /auth/logout-all`
- [ ] **Realtime Event Bus (`GET /events`)**
  - [ ] Create `tokio::sync::broadcast` channel for account updates
  - [ ] Emit absolute `balance` events on webhook credit and proxy usage settlement
  - [ ] Emit heartbeat `: heartbeat` every 20 seconds to prevent Cloudflare drops
  - [ ] Replay buffer for reconnecting clients (`Last-Event-ID`)

---

## Phase 3: Payments & Midtrans Sandbox

- [ ] **Midtrans Snap Client**
  - [ ] Implement `POST /api/topups` calling Midtrans Snap API (`/snap/v1/transactions`)
  - [ ] Enforce deposit limits (50k IDR initial, 10k IDR subsequent)
  - [ ] Generate unique `order_id = "topup_<uuid>"`
- [ ] **Webhook Handler (`POST /webhooks/midtrans`)**
  - [ ] Verify SHA-512 constant-time signature
  - [ ] Validate amount against stored `topups` row
  - [ ] Row-level lock (`SELECT ... FOR UPDATE`) for idempotency
  - [ ] Handle `settlement`/`capture` (credit) and `refund`/`partial_refund` (debit)
  - [ ] Integration test suite for webhook replay protection

---

## Phase 4: Astro Dashboard & Frontend (`website/`)

- [ ] **Static Shell (Astro + Tailwind CSS)**
  - [ ] Landing page with pricing table and transparent wholesale margin disclosure
  - [ ] Quick-start developer guide & cURL examples
  - [ ] Zero JavaScript payload on marketing pages
- [ ] **Interactive Client Islands**
  - [ ] Auth island: Google OAuth2 & email/password via PocketBase SDK
  - [ ] Dashboard layout with sticky Live Balance badge wired to `/events` SSE
  - [ ] Polling fallback to `GET /api/me` with stale indicator if SSE disconnects
  - [ ] API Key Management island:
    - [ ] Create key modal with show-once plaintext key and copy button
    - [ ] Key list with prefix, labels, 30-day spend limits, and revoke actions
  - [ ] Wallet island:
    - [ ] Top-up amount selector (preset buttons 50k, 100k, 250k, 500k)
    - [ ] Midtrans Snap.js popup checkout integration
    - [ ] Recent transactions and deposit history table
  - [ ] Usage Analytics island:
    - [ ] 3-counter daily breakdown (Standard Input, Cache Read, Output tokens)

---

## Phase 5: Telegram Bot & Community (`telegram/`)

- [ ] **Account Linking**
  - [ ] Website: `POST /api/telegram/link-code` (6-digit, 5-minute TTL)
  - [ ] Bot: `/link <code>` redeeming via internal `POST /api/bot/link` with bot token
  - [ ] Atomic re-attribution of pre-link reviews on account binding
- [ ] **Customer Commands & Notifications**
  - [ ] `/balance` and `/usage` commands
  - [ ] Automated low-balance DM (<10,000 IDR, max 1/day)
  - [ ] Anonymous top-up channel feed (`POST /api/bot/notify-topup` with masked emails)
- [ ] **Review Flow**
  - [ ] Interactive bot review conversation (rating 1–5, optional body $\le 1000$ chars)
  - [ ] Edit history tracking in `review_history`
  - [ ] Public aggregate rating endpoint `GET /api/reviews`

---

## Phase 6: Deployment & Production Launch Gates

- [ ] **Infrastructure Setup**
  - [ ] Deploy Astro frontend to **Cloudflare Pages**
  - [ ] Provision **PostgreSQL** instance with persistent volume
  - [ ] Deploy PocketBase auth instance on Northflank
  - [ ] Deploy Rust API container to **Northflank** (0.2 vCPU developer tier)
  - [ ] Configure custom domain and SSL certificates on Cloudflare
- [ ] **Operational Gating (Gates 0–5 from [`docs/launch-checklist.md`](docs/launch-checklist.md))**
  - [ ] Gate 0: Terms of Service published with cross-border forwarding disclosure
  - [ ] Gate 1: Backup script running offsite (`pg_dump` / WAL archiving)
  - [ ] Gate 2: Reconciliation query script passing with 0 drift on live DB
  - [ ] Gate 3: Cookie and API key auth separation verified
  - [ ] Gate 4: Zero prompt/completion logging verified in code and logs
  - [ ] Gate 5: Production health check operational; restore drill executed
