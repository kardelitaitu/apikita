# ApiKita — Development Roadmap & Checklist

The living development roadmap for the ApiKita high-throughput LLM arbitrage proxy gateway.

> **Execution Directive:** Everything is developed and tested **locally first** (Web + Server + Edge Relay + Database + Fakes). **Midtrans payments are implemented last.**

---

## Phase 0: Groundwork & Specification (Complete)

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

## Phase 1: Local Stack & Testing Fakes Setup

- [x] **Docker Compose Local Environment (`docker-compose.yml`)**
  - [x] PostgreSQL 16 container on port `5432` with automatic migration runner
  - [x] PocketBase container on port `8090` (identity only)
  - [x] Nginx Edge Relay container on port `8000` (proxying to server `8080`, with `proxy_buffering off` on `/events` and `/v1/*`)
- [x] **Fake Upstream Provider Service**
  - [x] Local mock server speaking OpenAI `/v1/chat/completions` SSE streaming format
  - [x] Configurable test triggers via headers:
    - [x] Happy path streaming (canned tokens at 30 tok/sec)
    - [x] HTTP 429 rate limit injection (to test key cooldown)
    - [x] Mid-stream abrupt disconnection (to test unbilled wastage handling)
    - [x] HTTP 500 error (to test circuit breaker trip)

---

## Phase 2: Rust Server Core & Streaming Pipeline (`server/`)

- [x] **100-Key Pool Router & Load Balancer**
  - [x] Implement atomic least-loaded key selection (`min_by_key(|k| k.in_flight)`)
  - [x] Implement granular per-key 5-second cooldown upon receiving HTTP 429
  - [x] Implement client-side transparent retry across available keys (up to `max_key_attempts = 3`)
  - [x] Configure `reqwest::Client` persistent HTTP connection pooling (`pool_max_idle_per_host(100)`)
- [x] **Circuit Breaker**
  - [x] Consecutive failure counter (3 failures to trip to `Open`)
  - [x] Exponential cooldown backoff (30s $\rightarrow$ 60s $\rightarrow$ capped at 900s)
  - [x] Half-open trial request logic
- [ ] **Streaming Pipeline (`POST /v1/chat/completions`)**
  - [ ] Bearer API key validation with 60-second in-memory TTL cache
  - [x] Model allowlist verification
  - [x] Pre-flight worst-case balance reservation check (`402 Payment Required` on breach)
  - [x] Non-buffering SSE chunk forwarding (client $\leftarrow$ server $\leftarrow$ upstream)
  - [x] Parse usage summary from final SSE chunk
  - [x] Execute atomic usage settlement (`debit_usage_transaction`) on stream completion
  - [x] Ensure mid-stream errors do not retry silently
- [x] **Session Auth & Realtime Event Stream**
  - [x] Wire `POST /auth/exchange` to verify PocketBase JWTs and issue cookie
  - [x] Implement session revocation on `POST /auth/logout` and `POST /auth/logout-all`
  - [x] Realtime SSE stream (`GET /events`) with heartbeat and live balance broadcast

---

## Phase 3: Web Dashboard (`website/` Astro + Islands)

- [x] **Static Marketing Shell (Astro + Tailwind CSS)**
  - [x] Landing page with pricing table ($M = 1.50$ rates)
  - [x] Quick-start developer guide & cURL examples
  - [x] Zero JavaScript payload on marketing pages
- [x] **Interactive Client Islands**
  - [x] Auth island: Google OAuth2 & email/password via PocketBase SDK
  - [x] Dashboard layout with sticky Live Balance badge wired to `/events` SSE
  - [x] Polling fallback to `GET /api/me` with stale indicator if SSE disconnects
  - [x] API Key Management island:
    - [x] Create key modal with show-once plaintext key and copy button
    - [x] Key list with prefix, labels, 30-day spend limits, and revoke actions
  - [x] Usage Analytics island:
    - [x] 3-counter daily breakdown (Standard Input, Cache Read, Output tokens)

---

## Phase 4: Local End-to-End Integration Validation

- [ ] **Full Local Stack Test (Web $\rightarrow$ Relay $\rightarrow$ Server $\rightarrow$ Fake Upstream)**
  - [ ] Log in through local Web UI via PocketBase
  - [ ] Generate an API key through Web UI
  - [x] Execute streaming request using `curl` against local Edge Relay on port `8000`
  - [x] Confirm request routes through Server to Fake Upstream
  - [ ] Confirm balance decrements live on Web UI via SSE without browser refresh
  - [x] Verify database reconciliation query: $\sum \text{delta\_idr} \equiv \text{balance\_idr}$
  - [x] Test relay down failover: send requests directly to server port `8080`

---

## Phase 5: Midtrans Payment Integration (Done Last)

- [ ] **Fake Midtrans Webhook Harness**
  - [x] Local test runner firing synthetic signed webhooks to `POST /webhooks/midtrans`
  - [x] Verify signature verification rejection on invalid signatures
  - [x] Verify idempotency: replayed webhook with same `order_id` credits wallet exactly once
- [ ] **Midtrans Snap Client**
  - [x] Implement `POST /api/topups` calling Midtrans Snap API (`/snap/v1/transactions`)
  - [x] Enforce deposit limits (50k IDR initial, 10k IDR subsequent)
  - [x] Integrate Midtrans Snap.js popup modal in Web wallet island
- [ ] **Midtrans Sandbox Verification**
  - [ ] Run live end-to-end sandbox top-up with test QRIS code
  - [ ] Confirm webhook receipt, signature check, wallet credit, and realtime UI update

---

## Phase 6: Production Deployment & Launch Readiness

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
