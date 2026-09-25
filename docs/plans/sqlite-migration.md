# Plan: Retire PocketBase and PostgreSQL for Embedded SQLite

**Status:** draft for review · **Date:** 2026-09-25 · **Branch:** `0.0.1`
**Supersedes:** the untitled "Remove PocketBase and Migrate to Embedded SQLite + Custom Admin UI" draft
**Amends:** [`decisions.md`](../decisions.md) — see [§3](#3-decisions-this-forces-the-register-to-change)

**Settled in this revision:** identity strategy is **Google + email/password**
([§6](#6-phase-2--identity-the-real-cost)); low-code admin tooling assessed for the
read-only surface only ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)).

---

## 1. Verdict

The goal is sound and the timing is unusually good. The draft's *premise about the
current code is wrong in five places*, and three of its proposed mechanisms would
re-introduce defects this codebase has already found, fixed, and written regression
tests for. Details in [§2](#2-what-the-original-draft-got-wrong).

The short version:

| Claim | Reality |
| --- | --- |
| "Connects to PocketBase via HTTP REST" | True, but for **one call only** — `auth-refresh` on login. No Rust PocketBase client exists in the tree. |
| "Replace PocketBase with embedded SQLite" | PocketBase holds **no money and no tables we would port**. Removing it is an **auth re-implementation**, not a database migration. This is the single largest cost in the plan and the draft does not budget for it. |
| "Add `sqlx` with SQLite" | `sqlx` is **already a dependency** and already embedded — with the `postgres` driver. This is a driver swap plus a SQL-dialect port, not a new data layer. |
| "`balance REAL`" | A direct violation of a settled decision: *"Money type: `BIGINT` IDR — never floating point"*. `REAL` for money is a correctness bug. |
| "`UPDATE ... SET balance = MAX(0.0, balance - ?)`" | This is **precisely the defect `db.rs` was rewritten to fix.** `clamp_debit`'s comment states the rule: *"a clamp of the DEBIT, never of the balance"*. `MAX(0, …)` swallows a shortfall silently and makes `balance_idr = SUM(ledger.delta_idr)` unprovable. |

**The strongest argument for doing this now:** the repository has **no production
deployment and no production data** — every Phase 6 item in [`todo.md`](../../todo.md)
is unchecked. This migration is a code-and-docs change today. After launch it becomes
a data migration with a ledger to keep balanced. Doing it now is cheap; doing it later
is not.

**The strongest argument against:** SQLite gives up PITR, and the register's backup
decision is *"PITR plus offsite; restore drill required"* with *"RPO 15 minutes"*.
That guarantee is **not free to replace** — see [§8](#8-operations-backup-rpo-and-the-volume). Litestream
restores it at the cost of one more process in a 256 MB container.

**Where the real cost is:** not the database. The database port is mechanical and
compiler-verified. The cost is **identity**, because PocketBase is currently doing the
hard half of authentication for free — see [§6](#6-phase-2--identity-the-real-cost). The
plan therefore splits the work so the cheap, self-contained half ships first.

---

## 2. What the original draft got wrong

Each row is a change the improved plan makes, with the evidence that forced it.

### 2.1 Money as `REAL` — rejected

`decisions.md` → Money: *"Money type: `BIGINT` IDR | Never floating point."*
`server/migrations/20260925000000_initial_schema.sql:18` is
`balance_idr BIGINT NOT NULL DEFAULT 0 CHECK (balance_idr >= 0)`.

SQLite has no `BIGINT`, but `INTEGER` is a 64-bit signed type and is the correct
choice. Every money column in the plan is `INTEGER`, and the `CHECK (>= 0)` backstop
comes with it.

### 2.2 `MAX(0.0, balance - ?)` — rejected; it is the bug, not the fix

`server/src/db.rs:255-271` documents the rule and `clamp_debit` implements it:

> *"The rule is a clamp of the DEBIT, never of the balance … Forcing the full debit
> would drive the balance negative, which docs/decisions.md ratified as impossible."*

`MAX(0, balance - cost)` does the opposite: it clamps the *result* and discards the
evidence that a charge could not be collected. `UsageSettlement::Partial` exists
specifically so an undercharge is a **loud, recorded event** rather than a silent
one — `db.rs:744-753` logs it at `error!` level. The plan keeps the guarded
`UPDATE … WHERE balance_idr >= ?` predicate and `Partial` semantics unchanged.

### 2.3 A static `ADMIN_SECRET_KEY` bearer — rejected

Three settled decisions say no:

- *"Admin actions: Same API, same ledger, no back door."*
- *"Operator flag location: `accounts.is_operator` — a column, not a separate table."*
- *"Operator authentication: Same session as customers, plus the flag — **Not a
  separate credential, which becomes a shared secret**."*

`docs/admin-surface.md:59` names the anti-pattern outright: *"Not a separate admin
password, which becomes a shared secret."* The draft's `X-Admin-Key` is that shared
secret. The improved plan reuses the session cookie plus `is_operator`, which is
also less code — the session extractor already exists.

The draft's UI stores the key in `sessionStorage`, which is readable by any XSS on
the origin. Even if a static key were wanted, `sessionStorage` is the wrong place.

### 2.4 The four-table schema would delete fourteen tables

The draft proposes `users`, `api_keys`, `usage_logs`, `balance_adjustments`. The
existing schema has eighteen tables. What the draft drops, and what depends on it:

| Dropped | Consequence |
| --- | --- |
| `ledger` (append-only, with `balance_after`) | Destroys the invariant that makes reconciliation provable. **Launch Gate 2 depends on it.** |
| `topups` (`order_id UNIQUE`) | Destroys Midtrans idempotency — the mechanism that stops a replayed webhook crediting twice. |
| `sessions` | Destroys immediate logout. |
| `usage_daily` | Destroys the dashboard and the 30-day rolling spend window. |
| `admin_audit` | Destroys the operator trail; `admin-surface.md` requires it *"not derived from logs"*. |
| `key_ip_daily`, `key_ip_seen` | Destroys abuse detection (`ip_tracking.rs`, 782 lines). |
| `link_codes`, `telegram_links`, `reviews`, `review_history`, `review_sessions` | Destroys the Telegram surface. |
| `wallets` | Folded into `users.balance` — see below. |

**Splitting `wallets` out of `accounts` is deliberate** (`identity.md:28`: *"Keys and
wallet hang off the account, never off an identity"*). Folding `balance` onto a user
row is not simpler; it removes the place where the non-negative constraint lives.

**The improved plan keeps every existing table and name.** The draft's names map onto
existing ones; renaming would churn 106 placeholder sites and 41 documents for no
benefit:

| Draft name | Keep as |
| --- | --- |
| `users` | `accounts` + `wallets` |
| `api_keys` | `api_keys` |
| `usage_logs` | `usage_events` (**new**, see [§4.3](#43-usage_events--the-one-genuinely-new-table)) |
| `balance_adjustments` | `ledger` rows with `reason='adjustment'` — **not a second ledger** |

`balance_adjustments` as a parallel audit table is the specific anti-pattern
`admin-surface.md:17` warns about: two tables recording money means two things to
reconcile.

### 2.5 `prompt_tokens` / `completion_tokens` — rejected

The draft collapses billing to two token classes. The system bills **three**
independently — `input_tokens`, `cache_read_tokens`, `output_tokens`
(`db.rs:459-481`) — and `decisions.md` records that *"Cache-heavy usage is
loss-making at any markup"*, with cache reads priced separately
(`config/apikita.toml` `rates.cache_read_offpeak/peak`). Collapsing the classes
**breaks cache pricing**. The plan keeps three counters, never summed.

### 2.6 Run-on-startup migrations — rejected

`decisions.md` → Operations: *"Migrations: `sqlx migrate`, forward-only"* and
*"Migration timing: In CI, after a snapshot, before the server — **never on
application boot**."* The plan uses a dedicated `migrate` binary
([§5.3](#53-migration-mechanism)).

Note a pre-existing gap this exposes: **no `sqlx::migrate!` call exists in the
tree.** The Postgres schema is applied by `docker-entrypoint-initdb.d`
(`docker-compose.yml:26`), so the register's migration decision was never actually
implemented. The SQLite port is the right moment to implement it properly.

### 2.7 Smaller corrections

| Draft | Correction |
| --- | --- |
| `DATETIME DEFAULT CURRENT_TIMESTAMP` | Declare `TEXT`. `DATETIME` has no SQLite affinity match and falls through to NUMERIC, which is sloppy for ISO strings. |
| `mode=rwc` in the URL | Works, but `.create_if_missing(true)` in connect options is clearer and testable. |
| `DELETE /api/admin/users/{id}` | `identity.md` invariant 4 and `admin-surface.md` rail: **"No hard deletes, ever."** Closure is `status='closed'`. |
| Balance adjust with no threshold | `decisions.md`: *"Second-operator threshold: 500,000 IDR."* The endpoint must enforce it. |
| `GET /api/admin/stats` | Fine, but must read `ledger`, not a new aggregate table. |

---

## 3. Decisions this forces the register to change

[`decisions.md`](../decisions.md) is the single source of truth; its own rule is
*"Do not re-open a settled decision in a document — change it here instead."* So the
register is edited **first**, in Phase 0, and the docs follow.

| Register entry | Current | Becomes |
| --- | --- | --- |
| Money store | PostgreSQL | **SQLite (embedded, WAL)** |
| Identity store | PocketBase | **Rust-owned (`accounts` + `identities`)** |
| Account key | *"Postgres owns the id; PocketBase id is a linked column"* | **`accounts.id` is the only key; `pb_user_id` dropped** |
| Login methods | *"Google + email/password, with reset"* | **unchanged as a product decision — but Rust now owns all of it.** Settled 2026-09-25 ([§6](#6-phase-2--identity-the-real-cost)) |
| Password hashing | *"Argon2id — PocketBase owns this if it stays the auth provider — **verify which applies**"* | **Rust owns Argon2id.** The qualifier is now resolved: PocketBase is going, so it is ours. |
| Transaction mode | — | **`BEGIN IMMEDIATE` for read-then-write transactions** ([§4.2](#42-connection-setup--three-traps)) |
| Instance count | — | **exactly one.** SQLite cannot be shared across replicas |
| Migrations | `sqlx migrate`, forward-only | **unchanged**, but now actually implemented |
| Backup tooling | Managed PITR, else `pg_dump` + `wal-g` | **Litestream → Cloudflare R2** (or `VACUUM INTO` + offsite) |
| RPO 15 min / RTO 4 h | — | **unchanged target**; the mechanism changes, the target does not |
| Operator authentication | Same session + flag | **unchanged** — this is why the draft's static key is rejected |

Two further register additions:

- **Instance count: exactly one.** SQLite cannot be shared across replicas. This
  must be written down or someone will scale the service and corrupt the database.
- **The API is container-bound.** Local SQLite forecloses a future Cloudflare
  Workers deployment (Workers has no filesystem; that path would need D1).

---

## 4. Target schema

### 4.1 Dialect translation rules

Applied mechanically across all SQL. **106 `$N` placeholders** across 9 files.

| Postgres | SQLite | Notes |
| --- | --- | --- |
| `$1`, `$2` | `?` | sqlx SQLite is positional. Reused binds need `?1` form. |
| `PgPool` | `SqlitePool` | |
| `Transaction<'_, Postgres>` | `Transaction<'_, Sqlite>` | |
| `now()` | `CURRENT_TIMESTAMP` | |
| `gen_random_uuid()` | `Uuid::new_v4()` in Rust | `uuid` crate already a dependency. |
| `::bigint` cast | removed | |
| `RETURNING` | **keep** | SQLite ≥ 3.35. Preserves the guarded-UPDATE-then-read pattern. |
| `ON CONFLICT … DO UPDATE` | **keep**, `EXCLUDED` → `excluded` | SQLite ≥ 3.24. |
| `SELECT … FOR UPDATE` | **remove** | See [§4.4](#44-the-two-for-update-sites). |
| `CREATE EXTENSION pgcrypto` | removed | |
| `TIMESTAMPTZ`, `DATE` | `TEXT` | |
| `JSONB` | `TEXT` | sqlx `json` feature. |
| `UUID` | `TEXT` | sqlx `uuid` feature decodes `TEXT`. |
| `BIGSERIAL` | `INTEGER PRIMARY KEY AUTOINCREMENT` | |
| `BOOLEAN` | `INTEGER` (0/1) | |
| `SMALLINT` | `INTEGER` | |
| Partial indexes, `CHECK` | **keep** | Both supported. |

**Good news that shrinks the port:** the codebase uses **zero compile-time SQL
macros** — no `sqlx::query!`, no `.sqlx` offline cache, no `DATABASE_URL` needed at
build time. Every query is runtime-checked `sqlx::query`/`query_scalar`/`query_as`.
The port is therefore mechanical and compiler-verified, with no schema-drift build
step to maintain. This is a real advantage over most sqlx codebases.

### 4.2 Connection setup — three traps

```rust
let options = SqliteConnectOptions::from_str(&database_url)?
    .create_if_missing(true)
    .journal_mode(SqliteJournalMode::Wal)      // persistent in the file
    .synchronous(SqliteSynchronous::Normal)    // safe with WAL; avoids fsync per commit
    .busy_timeout(Duration::from_secs(5))      // replaces FOR UPDATE waiting
    .foreign_keys(true);                       // <-- see trap 1

let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
```

**Trap 1 — foreign keys are OFF by default in SQLite.** This is per-connection, not
per-database. Without `.foreign_keys(true)` every `ON DELETE CASCADE` in the schema
silently does nothing. The schema leans on cascade heavily (`api_keys`, `sessions`,
`usage_daily`, `key_ip_daily`, `telegram_links`, `link_codes`). Missing this does not
fail loudly — it leaks orphan rows and breaks reconciliation.

**Trap 2 — deferred transactions that read then write can fail unrecoverably.**
SQLite's default `BEGIN` is deferred: a transaction that `SELECT`s and later
`UPDATE`s may get `SQLITE_BUSY_SNAPSHOT` on upgrade, and that error **cannot be
resolved by retrying** — the transaction must be rolled back and restarted. Two
existing functions are exactly this shape: `credit_topup_transaction` (SELECT then
UPDATE) and `refund_topup_transaction` (SELECT then UPDATE). Both must use
`BEGIN IMMEDIATE`. sqlx's `pool.begin()` issues a deferred `BEGIN`, so add a helper
that acquires a connection and issues `BEGIN IMMEDIATE` explicitly, and route those
two call sites through it.

**Trap 3 — `NULL` in a composite primary key.** In Postgres a PK column is implicitly
`NOT NULL`. **SQLite does not enforce this** (a documented legacy behaviour), and
`NULL`s compare as distinct in unique indexes. `usage_daily`'s key is
`(account_id, api_key_id, day)` and `api_key_id` is *deliberately nullable*
(`ON DELETE SET NULL`). Under SQLite the `ON CONFLICT (account_id, api_key_id, day)`
upsert would **never match when `api_key_id IS NULL`** and would insert duplicate
rows every request — silently corrupting usage accounting. Fix: add an explicit
partial unique index plus a sentinel-free design, or declare the column `NOT NULL`
and use a sentinel. Recommended: keep it nullable but add

```sql
CREATE UNIQUE INDEX usage_daily_scope_uniq
  ON usage_daily (account_id, day, COALESCE(api_key_id, ''));
```

and target that index in the upsert. **This must have a regression test** — it fails
silently and only surfaces as wrong money on the dashboard.

### 4.3 `usage_events` — the one genuinely new table

The draft's "Recent Usage Feed" needs per-request rows with model, tokens and cost.
`usage_daily` cannot serve it (daily granularity) and `ledger` has no token counts.
So one new table is justified:

```sql
CREATE TABLE usage_events (
  id                TEXT PRIMARY KEY,
  account_id        TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_id        TEXT REFERENCES api_keys(id) ON DELETE SET NULL,
  model             TEXT NOT NULL,
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cost_idr          INTEGER NOT NULL DEFAULT 0,
  ref               TEXT,
  created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX usage_events_recent_idx ON usage_events (created_at DESC);
CREATE INDEX usage_events_account_idx ON usage_events (account_id, created_at DESC);
```

Three constraints on it:

1. **Three token counters**, never `prompt`/`completion` — see [§2.5](#25-prompt_tokens--completion_tokens--rejected).
2. **`cost_idr` is `INTEGER`**, never `REAL`.
3. **No prompt or completion text, ever.** Launch Gate 4 is *"Zero prompt/completion
   logging verified in code and logs"*. A per-request table is a retention liability;
   it gets a sweep job alongside the existing `ip-purge` binary, and an entry in
   [`data-retention.md`](../data-retention.md).

### 4.4 The two `FOR UPDATE` sites

`server/src/db.rs:32` and `server/src/db.rs:155`. SQLite has no row locks; `FOR
UPDATE` is a syntax error. Both are top-up idempotency guards, and both are better
served by a **conditional UPDATE plus `rows_affected()`** — which needs no lock at
all and is correct under any isolation level:

```sql
UPDATE topups SET status = 'settled', settled_at = CURRENT_TIMESTAMP
WHERE order_id = ? AND status = 'pending' AND amount_idr = ?;
```

If `rows_affected() == 0`, one disambiguating `SELECT` decides between
`AlreadySettled`, `NotFound` and `AmountMismatch` — the same three outcomes
`TopupCreditResult` already models. This removes the lock instead of emulating it,
and keeps the existing tests meaningful.

### 4.5 Suspension and soft delete — already the design, with one correction

The draft proposes `is_active INTEGER DEFAULT 1` and `is_deleted INTEGER DEFAULT 0`
on the user table. **Do not add them.** The schema already models both, more
precisely, and a second flag would be a second source of truth for "is this account
usable" — the drift the register exists to prevent.

| Need | Already exists | Note |
| --- | --- | --- |
| Account state | `accounts.status IN ('active','suspended','closed')` | Three states, not two. `closed` **is** the soft delete. |
| Key revocation | `api_keys.revoked_at` | A timestamp, not a boolean — it preserves *when*, which an audit needs. The indexes are already partial on `revoked_at IS NULL`. |
| Operator flag | `accounts.is_operator` | Unrelated to active/deleted; do not conflate the two. |
| Never hard-delete | `admin-surface.md:151` — *"No hard deletes, ever"*; `identity.md` invariant 4 | Closure is a status, not a delete. |

The FK behaviour the draft worries about is already deliberate and correct:
`wallets`, `ledger` and `topups` reference `accounts` with **`ON DELETE RESTRICT`**,
so a hard delete is refused by the database rather than orphaning money. `api_keys`,
`sessions`, `usage_daily`, `link_codes` and `telegram_links` use `ON DELETE CASCADE`.
Under SQLite the `RESTRICT` half only holds if `foreign_keys(true)` is set —
[§4.2](#42-connection-setup--three-traps), trap 1.

**The correction that matters: a status flag alone does not stop `/v1/*` traffic.**
The proxy's key lookup is

```sql
SELECT id, account_id, models, spend_limit_idr, token_limit, rate_limit_rpm,
       expires_at, revoked_at
FROM api_keys
WHERE key_hash = ?
```

— `server/src/routes/proxy.rs:517-523`. It does **not** join `accounts`, so the hot
path never reads `accounts.status`. That is precisely why `admin-surface.md:126`
insists *"Suspension must revoke sessions and keys"*: suspension works by setting
`api_keys.revoked_at` and revoking the `sessions` rows, atomically. Adding
`is_active` would change nothing on its own, and adding an `accounts` join to the hot
path would buy a read per request to achieve what revocation already achieves.

**Two residual windows, both already documented, and this plan narrows one:**

- `keys.rs:410` calls `invalidate_key_cache` on revoke, so revocation is immediate
  **in-process**. `proxy.rs:567-572` records the residual staleness for a *second*
  instance. Pinning to **exactly one instance** ([§3](#3-decisions-this-forces-the-register-to-change))
  removes that window entirely — a genuine side benefit of the SQLite constraint.
- `decisions.md` fixes the cache TTL at 60 s, so an *un-invalidated* change (a
  narrowed model allowlist) is honest about its window. Revocation is not in that
  class.

---

## 5. Phases

### 5.0 Phase 0 — Settle the register

Edit [`decisions.md`](../decisions.md) per [§3](#3-decisions-this-forces-the-register-to-change).
Nothing else starts until this lands, because every later document reads from it.

### 5.1 Phase 1 — Dependency and config

- `server/Cargo.toml`: swap `postgres` → `sqlite` in the `sqlx` feature list. Keep
  `uuid`, `chrono`, `json`. **Drop `migrate` unless [§5.3](#53-migration-mechanism) is adopted.**
- `DATABASE_URL=sqlite://data/server.db` (a file path, not a network URL).
- `data/` must be on the **persistent volume** ([§8](#8-operations-backup-rpo-and-the-volume)).
- Delete `POSTGRES_PASSWORD`, `POCKETBASE_URL` from `.env.example`.
- `docker-compose.yml`: delete the `postgres` and `pocketbase` services and the
  `pgdata`/`pbdata` volumes. Keep `nginx`. Delete `.docker/postgres/`.

### 5.2 Phase 2 — Schema port

Rewrite `server/migrations/20260925000000_initial_schema.sql` in SQLite dialect per
[§4.1](#41-dialect-translation-rules). **Replace in place** — there is no production
data to preserve, and a single migration is honest about that.

### 5.3 Phase 3 — Migration mechanism

The register says `sqlx migrate`, forward-only, never on boot. Implement it as
written: `server/src/bin/migrate.rs` using `sqlx::migrate!("./migrations")`, run in CI
and in the deploy pipeline **before** the server starts
(`deploy order: Migrate -> server -> health -> frontend`).

### 5.4 Phase 4 — Query port

14 files carry SQL. Port order is by blast radius, smallest first:

| Order | File | `$N` sites | Why this order |
| --- | --- | --- | --- |
| 1 | `routes/health.rs` | 0 | Trivial; proves the pool connects. |
| 2 | `routes/proxy.rs` | 1 | The hot path; smallest SQL surface, largest consequence. |
| 3 | `routes/webhooks.rs` | 2 | Money in. |
| 4 | `routes/events.rs` | 3 | SSE. |
| 5 | `routes/auth.rs` | 7 | Auth. |
| 6 | `abuse.rs` | 8 | |
| 7 | `ip_tracking.rs` | 11 | |
| 8 | `routes/account.rs` | 11 | |
| 9 | `routes/keys.rs` | 14 | |
| 10 | `db.rs` | 49 | The money core. Port last, when the dialect rules are proven. |
| 11 | `bin/ip-purge.rs`, `bin/benchmark.rs` | — | Tooling. |

**Do not port `db.rs` first.** It is the file with the invariants; port it once the
mechanical rules are already validated on cheap files.

### 5.5 Phase 5 — Tests become real tests

A genuine benefit worth stating: every money test in `db.rs` is currently
`#[ignore = "requires live Postgres"]`. With SQLite they need only a temp file, so
they can **run in CI by default**, including
`concurrent_requests_cannot_overdraw_a_one_request_balance` — the real-concurrency
proof that is currently never executed automatically.

Re-point them at `tempfile::TempDir` (dev-dependency), drop the `#[ignore]`, and add
the two new regression tests [§4.2](#42-connection-setup--three-traps) demands
(the `NULL`-`api_key_id` upsert, and the `BEGIN IMMEDIATE` read-then-write path).

### 5.6 Phase 6 — Identity

[§6](#6-phase-2--identity-the-real-cost). Sequenced after the database port because it
is the largest phase and the only one that depends on decisions outside the codebase
(open decisions 1 and 2). The strategy itself is settled — Google + email/password —
so this phase is unblocked and can run in parallel with Phase 8 if there is capacity.

### 5.7 Phase 7 — Admin surface

[§7](#7-phase-4--admin-surface). Conform to `admin-surface.md`, do not invent a
parallel model.

### 5.8 Phase 8 — Tooling, harnesses, docs

- `tools/reconcile/reconcile.sql` + `reconcile.sh` → SQLite (`sqlite3`, not `psql`).
- `.agents/` harnesses: `e2e-money/bin/psql`, `psql.sh`, `e2e-sse/psql.sh`,
  `e2e-sse/seed.sql`, `e2e-ui/*.mjs`, and every `e2e.toml` referencing Postgres.
- **Docs sweep — 33 files mention PocketBase, 30 mention Postgres.** The heavy ones,
  by mention count: `docs/architecture.md` (40 + 21), `docs/website/05-security-decisions.md`
  (30 + 12), `docs/architecture/identity.md` (23 + 14), `docs/website/02-data-model.md`,
  `docs/website/01-architecture.md`, `docs/local-development.md`,
  `docs/backup-and-restore.md`, `docs/deployment.md`, `docs/ci-cd.md`,
  `docs/data-retention.md`, `docs/cost-and-sizing.md`, `docs/observability.md`,
  `docs/server/api-spec.md`, `docs/launch-checklist.md`, `docs/abuse-runbook.md`,
  `docs/plan-audit.md`, plus `README.md` (the stack table) and `todo.md`.
- `README.md` stack table: `Money | Northflank | PostgreSQL` and
  `Identity | Northflank | PocketBase` both become one row: `Money + identity | Northflank | SQLite (embedded)`.

---

## 6. Phase 2 — Identity, the real cost

This is the part the draft omits, and it is **larger than the database port**.

PocketBase currently provides, per `identity.md:33-40`: password hashing,
email verification, password reset, Google OAuth2, OTP, and MFA — plus the four
pre-hijacking defences documented in `identity.md:77-127`. Removing it means either
re-implementing that in Rust, or changing the product.

### Decision — settled 2026-09-25

**Google + email/password, with reset.** The register's existing *product* decision is
kept; the *ownership* of it moves from PocketBase to Rust.

Options considered, recorded because this will otherwise be re-litigated:

| Option | Rust must own | Cost | Outcome |
| --- | --- | --- | --- |
| A. Google OAuth2 only | OAuth2 code flow, ID-token verification, session | Smallest | Not chosen — drops password login, a settled product decision. |
| B. Email + password only | Argon2id, verification, reset, rate limits | Medium | Not chosen — drops Google, also settled. |
| **C. Both** | **All of the above** | **Largest** | **Chosen.** |
| D. A different hosted IdP | Almost nothing | Small | Not chosen — re-introduces the login network hop this migration exists to remove. |

**C is the largest option, and the plan should say so plainly: this phase is bigger
than the database port.** It is where new security bugs will live, because every
defence `identity.md:77-127` currently inherits from PocketBase becomes ours to get
right.

### What C entails

| Work | Note |
| --- | --- |
| Argon2id hashing | The register already decides this. Tune cost parameters; never a bare SHA. |
| Email verification + reset | Needs **outbound email** — a new dependency, a new deliverability surface, and a new failure mode. The register does not currently account for an email provider. |
| Google OAuth2 code flow + ID-token verification | Verify `email_verified` explicitly, per `identity.md:100`. |
| Rate limiting on auth endpoints | Login, reset, verification. `abuse.rs` covers key creation and top-ups; auth is a new surface. |
| The four pre-hijacking defences | `identity.md:77-127` documents them as *PocketBase* behaviour. Each must be re-derived in Rust and re-tested. The rule at `identity.md:118` (*"`verified` may only be set by PocketBase's own flows"*) becomes **"only by our own flows"** — and is better enforced as a schema constraint than as a hook. |

**Two register additions this forces:**

1. **An email provider** — outbound transactional email is now a hard dependency.
2. **OTP and MFA are no longer provided.** PocketBase supplied both
   (`identity.md:37-38` counted them). If they are still wanted they are new work; if
   not, the document must stop claiming them.

### Schema

`accounts` gains an `identities` table (one account, many identities —
`identity.md` invariant 1) holding `provider`, `subject`, `email`, `email_verified`,
with `UNIQUE (provider, subject)`. `accounts.pb_user_id` is dropped.

`auth.rs` loses the entire `verify_pb_token` / `parse_pb_user_id` /
`normalize_pb_user_id` block and its three tests; `POST /auth/exchange` is replaced by
provider callbacks that mint the session directly.

---

## 7. Phase 4 — Admin surface

Conform to [`admin-surface.md`](../admin-surface.md) and `decisions.md`. The draft's
endpoints, corrected:

| Draft | Corrected | Why |
| --- | --- | --- |
| `GET /api/admin/users` | `GET /api/admin/accounts` | `accounts` is the table; `users` was the PocketBase collection. |
| `POST /api/admin/users` | `POST /api/admin/accounts` | Must create `accounts` **and** its zero-balance `wallets` row in one transaction, and write an `admin_audit` row. |
| `PATCH /api/admin/users/{id}/balance` with `amount_delta` | `POST /api/admin/accounts/:id/adjust` with `delta_idr` + **`reason`** + **`note`** | Matches `admin-surface.md:80`. Writes a `ledger` row with `reason='adjustment'` and `balance_after`, **in the same transaction as the wallet update**. Requires a note. Enforces the **500,000 IDR second-operator threshold**. |
| `PATCH /api/admin/users/{id}/status` | `POST /api/admin/accounts/:id/suspend` and `/restore` | **Suspension must revoke sessions and keys atomically** — a status flag alone does not suspend anything (`admin-surface.md:126`). |
| `POST /api/admin/users/{id}/keys` | `POST /api/admin/accounts/:id/keys` | Same as the user's own create-key path, not a parallel one. |
| `DELETE /api/admin/keys/{key_id}` | `POST /api/admin/keys/:id/revoke` | Sets `revoked_at`. `api_keys` already models revocation as a timestamp, never a delete. |
| `DELETE /api/admin/users/{id}` | **removed** | *"No hard deletes, ever."* Closure is `status='closed'`. |
| `GET /api/admin/stats` | keep | Read from `ledger` and `usage_events`; no new aggregate table. |

Plus the endpoints `admin-surface.md` requires that the draft omits:
`/logout-all`, `/topups/:id/refund`, `/link-codes/:id`, `/reviews/:id/hide`.

**Every one of these writes an `admin_audit` row in the same transaction as its
effect** — an audit row for an action that rolled back is as bad as no audit row.

**Auth:** an axum extractor that resolves the session cookie → `accounts` → asserts
`is_operator`. No static key, no `sessionStorage`. `accounts.is_operator` is excluded
from all customer-facing responses (`admin-surface.md:56`).

**Rollout:** `admin-surface.md:181-189` says start read-only plus suspend/restore/key
revoke, and add money actions *"once there is revenue to misfile"*. The plan follows
that: build the read + state endpoints now, gate `/adjust` and `/refund` behind a
config flag.

### 7.1 UI: hand-built `/admin` page vs a low-code tool

This choice is **only about the UI**. Every endpoint above must exist either way — a
low-code tool is an HTTP client, and it cannot supply the `/adjust` semantics, the
ledger row, or the `admin_audit` row. So adopting one does **not** shrink the endpoint
work, which is the larger half of Phase 7.

`admin-surface.md:62` already frames it correctly: *"A separate admin app is a later
decision. The endpoints are the same either way; a dedicated UI is a convenience, not
an architecture."*

**Assessment: adopt one for the launch-phase read-only surface; do not make it the
home for money actions.**

| | Hand-built `/admin` page | Low-code (Retool / ToolJet / Appsmith) |
| --- | --- | --- |
| Time to a working table | Hours | **Minutes** |
| Fits launch scope (read-only + suspend/restore/key revoke) | Yes | **Yes — this is the sweet spot** |
| Money actions with note, audit and the 500k threshold | Natural | Awkward — needs purpose-built forms, not editable cells |
| Credential custody | Session cookie, same origin | **A third party holds a credential that can move money** |
| Customer data transits | Our infrastructure only | **A third party's infrastructure** |
| Self-hosted on the 256 MB tier | n/a | **Not viable** — ToolJet/Appsmith need ~1–2 GB |

**Three things to settle before pointing it at the admin API.**

**1. The credential.** The draft's `X-Admin-Key` is the shared secret the register
rejects ([§2.3](#23-a-static-admin_secret_key-bearer--rejected)). But a low-code tool
cannot hold a browser session cookie, so it genuinely does need a machine credential.
The way to get one without inventing a special-case header:

> Create a real operator account (`is_operator = true`), issue it a **long-lived API
> key through the normal key flow**, and give that key the admin scope. It is then an
> ordinary credential in the existing model: revocable, visible in `api_keys`,
> attributable in `admin_audit`, and it expires like any other.

That keeps *"same API, same ledger, no back door"* intact. It is still a powerful
credential, and it still belongs in the tool's secret store — not a header field on a
shared dashboard, and never `sessionStorage`.

**2. Credential and data custody.** Retool/ToolJet/Appsmith cloud call the API from
**their** servers, so there is no CORS problem — but the credential then lives on their
infrastructure and every customer row transits it. That is a data-processor
relationship which the ToS and [`data-retention.md`](../data-retention.md) do not
currently mention. `admin-surface.md:194` already carries an open item about
restricting operator IP ranges, and cloud free tiers do not offer a stable egress IP.
Decide this deliberately rather than discovering it at launch.

**3. Editable tables are structurally wrong for this API.** A low-code editable table
pointed at an accounts resource will happily emit a direct balance write — which is
the **first** anti-pattern `admin-surface.md:17` names (*"`UPDATE wallets SET
balance_idr = …` by hand"*). The convenience feature is the hazard. Configure the tool
**read-only by default** and wire writes to explicit buttons calling the purpose-built
endpoints.

**Configuration rules, if adopted**

| Rule | Why |
| --- | --- |
| Read-only tables; no inline editing | Prevents the direct balance edit by construction |
| Writes are explicit buttons → `/adjust`, `/suspend`, `/restore`, `/keys/:id/revoke` | Each maps to an audited endpoint |
| No `DELETE` mapped to anything | *"No hard deletes, ever."* Closure is `status='closed'` |
| The credential is an operator key held in the tool's secret store | Not a header, not `sessionStorage` |
| Money actions stay out until there is revenue | `admin-surface.md:181-189` rollout |

**Net:** a low-code tool is a real saving on the read-only surface and a real hazard on
the money surface. Use it for the former; keep the latter behind audited endpoints and
a purpose-built form. This is open decision 4 in [§11](#11-open-decisions).

---

## 8. Operations: backup, RPO, and the volume

Three things the draft does not address, all of which are load-bearing.

**1. Persistent volume.** SQLite is a file. Northflank containers are ephemeral
without an attached volume, and a redeploy would silently discard the ledger. The
`data/` directory **must** be a mounted volume, and this must be verified by a
restore drill, not assumed.

**2. Backup and the 15-minute RPO.** `decisions.md` requires *"PITR plus offsite;
restore drill required"* with **RPO 15 minutes**. WAL alone does not provide this.
Two options:

| Option | RPO | Cost |
| --- | --- | --- |
| **Litestream → Cloudflare R2** (recommended) | seconds | One sidecar process, ~20-30 MB RSS. Continuous WAL shipping. Cloudflare is already in the stack. |
| Cron `VACUUM INTO` + upload | up to the interval | Simpler; needs a separate job; a 15-min interval means 15-min RPO at best. |

Budget Litestream's memory against the 256 MB limit — it is affordable, but it is not
free, and the plan's *"< 50 MB RSS"* target in the draft did not account for it.
Launch Gate 1 (`docs/launch-checklist.md`) changes from `pg_dump`/WAL archiving to
whichever is chosen, **and the restore drill must be re-run.**

**3. Exactly one instance.** Two replicas on two volumes is two divergent ledgers.
Pin replicas to 1 in the deploy config and record it in the register.

---

## 9. Verification

Ordered by what would fail silently if skipped.

| # | Check | Command / method |
| --- | --- | --- |
| 1 | Foreign keys actually on | Insert a child with a bad FK; it must **fail**. If it succeeds, `foreign_keys(true)` is missing. |
| 2 | `NULL` `api_key_id` upsert | Record usage twice with `api_key_id = NULL`; `usage_daily` must have **one** row, not two. |
| 3 | Read-then-write under contention | Concurrent `refund_topup_transaction` calls must not raise `SQLITE_BUSY_SNAPSHOT`. |
| 4 | Ledger invariant | `SELECT COUNT(*) FROM (SELECT w.account_id FROM wallets w LEFT JOIN ledger l ON l.account_id=w.account_id GROUP BY w.account_id, w.balance_idr HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr),0))` → **0**. This is Launch Gate 2. |
| 5 | Stranded holds | `unpaired_hold_rows` → **0** for every account (`db.rs:1692`). |
| 6 | Overdraw proof | `cargo test --lib` — the concurrency test now runs **without** `--ignored`. |
| 7 | WAL is actually on | `PRAGMA journal_mode;` → `wal`. Confirm it survives a reconnect. |
| 8 | Concurrent readers unblocked | Read while a write transaction is open; must not block. |
| 9 | Memory | Re-measure RSS. `docs/benchmark.md` claims 34 MB with 1,000 concurrent streams — **re-run it**; that number was measured against Postgres and is now unverified. |
| 10 | Write throughput ceiling | Single-writer serialisation is the new bottleneck. Re-run `bin/benchmark.rs` and record the write rate, not just token throughput. |
| 11 | Build | `cargo check && cargo build --release`. |
| 12 | Restore drill | Restore from Litestream/backup into a clean volume and run check 4 against the restored file. |

**Claims that must not be repeated without re-measurement:** the 63,750 tok/s and
34 MB figures in `docs/benchmark.md` and `todo.md` were measured against Postgres.
They are now unverified for SQLite and must be re-measured or marked stale.

---

## 10. What this costs

Stated plainly, because the register's own culture is to record what is given up.

**Gains**

- Removes one network round trip per authenticated query. Real, and the user's
  primary motivation.
- Removes two containers (Postgres, PocketBase) — separate services, so this is
  cost and operational surface, not the API container's memory.
- Money tests move from `#[ignore]`d to running in CI. This is the most valuable
  side effect in the plan.
- Single deployable artefact; no schema-version skew between app and database.

**Losses**

- **PITR is gone.** Replaced by Litestream at comparable RPO, but it is a
  re-implementation of a guarantee, and a guarantee that has to be re-proven.
- **Concurrent writes serialise.** One writer at a time. Settlement is detached from
  the response path (`proxy.rs`), so client latency is unaffected, but balance
  catch-up and the SSE feed have a new ceiling.
- **No horizontal scaling.** One instance, permanently, unless the storage layer
  changes again.
- **The API is container-bound.** A future Cloudflare Workers migration would need D1.
- **Auth is now ours.** Whatever PocketBase was doing right, we now have to do right.

**Not given up:** the ledger, the non-negative balance, three-class token accounting,
idempotent top-ups, server-side sessions, the audit trail, or any admin capability.
Those are the parts that matter, and none of them depend on Postgres.

---

## 11. Open decisions

| # | Decision | Blocks |
| --- | --- | --- |
| 1 | **Email provider** for verification and reset — a new hard dependency | Phase 6 |
| 2 | Whether OTP and MFA are still wanted now that PocketBase no longer supplies them | Phase 6 |
| 3 | **Backup mechanism** — Litestream vs `VACUUM INTO` cron | Phase 8, Launch Gate 1 |
| 4 | **Low-code admin tool** — adopt for the read-only surface, or hand-build ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)) | Phase 7 scope |
| 5 | **`usage_events` retention window** | `data-retention.md`, the sweep job |
| 6 | Whether admin money actions ship now or behind a flag | Phase 7 scope |
| 7 | Whether the 30-day rolling spend window stays a query over `usage_daily` or moves to `usage_events` | Phase 4 (`routes/keys.rs`) |
| 8 | Whether a low-code tool's access to customer data needs a ToS / privacy-notice amendment | Launch Gate 0 |

**Settled this round:** identity strategy — **C, Google + email/password, with reset**
(2026-09-25). See [§6](#6-phase-2--identity-the-real-cost).

---

## 12. Suggested execution order

1. Phase 0 — register ([§3](#3-decisions-this-forces-the-register-to-change)). *Blocks everything.*
2. Phases 1–5 — database port and tests. **Self-contained and independently
   shippable**: after Phase 5 the system runs on SQLite with PocketBase still
   present for identity, which is a legitimate intermediate state and de-risks the
   whole plan. **Nothing here depends on the identity decision, so it can start
   immediately** — which is why it is sequenced ahead of the larger phase.
3. Phase 6 — identity (Google + email/password). The largest phase. Unblocked, but
   open decisions 1 and 2 need answers first.
4. Phase 7 — admin surface endpoints, then the UI choice ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)).
5. Phase 8 — tooling and the docs sweep.

Splitting at step 2 is the main structural improvement over the draft: it turns one
large risky change into two smaller ones, and the first half is verifiable by the
tests that already exist.

**A note on what to do first if only one thing is done:** Phase 0. Every document in
the repository reads from the register, and the register currently says the stack is
Postgres and PocketBase. Leaving it stale while the code moves is exactly the failure
mode its own *"How to change a decision"* section warns about — *"a stale decision is
worse than none, because it is followed."*
