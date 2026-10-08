# Plan: Retire PocketBase and PostgreSQL for Embedded SQLite

**Status:** COMPLETE - PostgreSQL and PocketBase are both gone. Phase 6 (§5.6, §6) landed: identity is served natively by this crate, `accounts.pb_user_id` is dropped, and the `server/src/routes/auth.rs` PocketBase client is deleted. What remains is the plan's own historical record, which is why the phase narratives below still describe PocketBase as present - they say what was true when each phase was executed. Read them as a log, not as the current state.
**Date:** 2026-09-25 (written) · **Branch at writing:** `0.0.1` · shipped and on `0.0.2`
**Supersedes:** the untitled "Remove PocketBase and Migrate to Embedded SQLite + Custom Admin UI" draft
**Amends:** [`decisions.md`](../decisions.md) — see [§3](#3-decisions-this-forces-the-register-to-change)
**Companion:** [`proxy-hot-path-audit.md`](proxy-hot-path-audit.md) — the request path; this plan is the storage path

**Settled in this revision:** identity strategy is **Google + email/password**
([§6](#6-phase-2--identity-the-real-cost)); low-code admin tooling assessed for the
read-only surface only ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)); all
decisions consolidated in [§11](#11-decisions).

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

### 2.4 The four-table schema would delete twelve tables

The draft proposes `users`, `api_keys`, `usage_logs`, `balance_adjustments`. The
existing schema has **fifteen** tables — counted, not recalled: fifteen `CREATE TABLE`
statements in the single migration, and the same fifteen in
[`website/02-data-model.md`](../website/02-data-model.md). *(This section previously said
eighteen, which made the arithmetic look consistent while being wrong about the schema.)*
What the draft drops, and what depends on it:

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
| `usage_logs` | `usage_events` (**new**, see [§4.4](#44-usage_events--the-one-genuinely-new-table)) |
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
([§5.3](#53-phase-3--migration-mechanism)).

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
| Money type | `BIGINT` IDR | **`INTEGER` IDR** — `STRICT` accepts only `INT`/`INTEGER`/`REAL`/`TEXT`/`BLOB`/`ANY` and **rejects `BIGINT`** (measured). [§11.1](#111-decided) item 5 forces this, so the register's money-type row had to move with it |
| Identity store | PocketBase | **Rust-owned (`accounts` + `identities`)** |
| SQL driver | `sqlx` (`postgres` feature) | **`sqlx` + `sqlite` feature — not `rusqlite`.** Decided; reasoning in [`proxy-hot-path-audit.md` §3](proxy-hot-path-audit.md) |
| Account key | *"Postgres owns the id; PocketBase id is a linked column"* | **`accounts.id` is the only key; `pb_user_id` dropped** |
| Login methods | *"Google + email/password, with reset"* | **unchanged as a product decision — but Rust now owns all of it.** Settled 2026-09-25 ([§6](#6-phase-2--identity-the-real-cost)) |
| Password hashing | *"Argon2id — PocketBase owns this if it stays the auth provider — **verify which applies**"* | **Rust owns Argon2id.** The qualifier is now resolved: PocketBase is going, so it is ours. |
| Transaction mode | — | **`BEGIN IMMEDIATE` for read-then-write transactions** ([§4.3](#43-connection-setup--four-traps-all-measured)) |
| Instance count | — | **exactly one** — and Northflank enforces it: a Single Read/Write volume *"limited to 1 instance"* ([§8](#8-operations-backup-rpo-and-the-volume)) |
| Deploy downtime | — | **accepted.** A Single Read/Write volume forbids rolling restarts; every deploy is a brief outage |
| Timestamp representation | — | **uniform RFC3339 with a format `CHECK`; time is never written in SQL** ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped)) |
| Migrations | `sqlx migrate`, forward-only | **unchanged**, but now actually implemented |
| Backup tooling | Managed PITR, else `pg_dump` + `wal-g` | **Litestream → Cloudflare R2** (or `VACUUM INTO` + offsite) |
| RPO 15 min / RTO 4 h | — | **unchanged target**; the mechanism changes, the target does not |
| Operator authentication | Same session + flag | **unchanged** — this is why the draft's static key is rejected |

Two further register additions:

- **Instance count: exactly one.** SQLite cannot be shared across replicas. This
  must be written down or someone will scale the service and corrupt the database.
- **The API is container-bound.** Local SQLite forecloses a future Cloudflare
  Workers deployment (Workers has no filesystem; that path would need D1).

**The register carries a status marker.** [`decisions.md`](../decisions.md) gains a
*Migration in flight* subsection recording that these values are decided but not yet
in the tree. Without it, Phase 0 would swap one stale register for another: the
register is read by agents working in parallel with the port, and a value flipped
ahead of the code misleads exactly as a value left behind it does. *Settled* means the
direction is chosen, not that the tree matches it.

---

## 4. Target schema

### 4.1 The measured port inventory

Counted against the tree, not estimated. This is the actual work list.

| Construct | Sites | Where | Action |
| --- | --- | --- | --- |
| `$N` placeholders | **106** | 9 files | → `?` |
| SQL-side `now()` | **17** | `db.rs` 8, `auth.rs` 4, `keys.rs` 2, `events.rs` 1, `account.rs` 1, `abuse.rs` 1 | → bound from Rust ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped)) |
| `::bigint` / `::integer` casts | **18** | aggregate `SELECT`s in `abuse.rs`, `db.rs`, `account.rs`, `events.rs`, `keys.rs`, `ip_tracking.rs` | → **remove**; safe because every column is `INTEGER` ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped) note 3) |
| `SELECT … FOR UPDATE` | **2** | `db.rs:32`, `db.rs:155` | → conditional UPDATE + `rows_affected()` ([§4.5](#45-the-two-for-update-sites)) |
| `ON CONFLICT … DO UPDATE` | **2** | `db.rs:466`, `ip_tracking.rs:201` | → **keep**; measured working, including the table-qualified form |
| `ON CONFLICT … DO NOTHING` | **2** | `ip_tracking.rs:196`, `auth.rs:233` | → `auth.rs` goes with PocketBase; `ip_tracking.rs` is blocked by the CTE below |
| **Data-modifying CTE** | **1** | `ip_tracking.rs:190-205` | → **no SQLite equivalent; must be rewritten** ([§4.7](#47-the-data-modifying-cte--one-function-must-be-rewritten)) |
| `interval '2 hours'` | **1** | `abuse.rs:309` | → **bind from Rust**; `datetime('now','-2 hours')` is wrong — see correction 2 below |
| Columns that relied on a Postgres `DEFAULT` | **5 sites** | `auth.rs` 3, `keys.rs` 1, `account.rs` 1 | → bind `id` from `Uuid::new_v4()` and every timestamp from Rust. **Absent from this inventory as first written** — see correction 1 below |
| `SELECT now() - interval …` | 0 | — | not used in production SQL |
| `= ANY($1)` array bind | **0** | — | the 3 `ANY(` hits are Rust `.iter().any()` |
| `GREATEST` / `LEAST` | **0** | — | the 12 `LEAST` hits are prose ("at least") |
| `jsonb` / `->>` operators | **0** | — | `models` is bound as a `serde_json::Value`, not queried as JSON |
| `CREATE EXTENSION` | 1 | migration | → delete |
| `gen_random_uuid()` | — | schema defaults | → `Uuid::new_v4()` in Rust |

**Two corrections to this inventory, found while executing Phase 4.**

**Correction 1 — the inventory counted `now()` call sites but not the columns that
relied on a Postgres default.** Removing `DEFAULT gen_random_uuid()` and every
`DEFAULT now()` (§4.6, rule 2) is correct, but it silently invalidates every INSERT
that omitted those columns. Five production statements did:

| Site | Table | Columns now required |
| --- | --- | --- |
| `routes/auth.rs` account upsert | `accounts` | `id`, `created_at`, `updated_at` |
| `routes/auth.rs` wallet upsert | `wallets` | `updated_at` |
| `routes/auth.rs` session insert | `sessions` | `id`, `created_at`, `last_seen_at` |
| `routes/keys.rs` key creation | `api_keys` | `id`, `created_at` |
| `routes/account.rs` topup creation | `topups` | `created_at` |

Measured, not inferred: with the schema as shipped and the fixtures as they stood,
the ignored integration tests fail at the first fixture insert with
`NOT NULL constraint failed: accounts.id`. That is the rule-2 design working as
intended — a forgotten bind is a loud error rather than silent drift — but the
inventory should have listed these sites, because "17 `now()` sites" understated the
work by five statements and §5.2's warning 1 named only `sessions`.

**Correction 2 — `datetime('now','-2 hours')` cannot be the translation.** The row
above prescribed it, and it contradicts §4.6 rule 3. Measured: `datetime()` emits
`2026-09-25 05:09:19`, the space format, and the `created_at` GLOB CHECK refuses it
(`CHECK constraint failed: updated_at GLOB '????-??-??T??:??:??*+00:00'`). The
`strftime('%Y-%m-%dT%H:%M:%f','now') || '+00:00'` form §4.6 already documents IS
accepted. Since this is a test helper, the simpler and more honest fix is to bind
`Utc::now() - Duration::hours(2)` from Rust and keep "never write time in SQL"
absolute.

**Two pieces of good news that shrink the port:**

1. **Zero compile-time SQL macros.** No `sqlx::query!`, no `.sqlx` offline cache, no
   `DATABASE_URL` needed at build time. Every query is runtime-checked
   `sqlx::query`/`query_scalar`/`query_as`. The port is compiler-verified with no
   schema-drift build step to maintain.
2. **No array binds and no Postgres-only JSON operators.** Those are the two dialect
   gaps that usually force genuine rewrites. Neither is present.

### 4.2 Dialect translation rules

| Postgres | SQLite | Notes |
| --- | --- | --- |
| `$1`, `$2` | `?` | sqlx SQLite is positional. Reused binds need `?1` form. |
| `PgPool` | `SqlitePool` | |
| `Transaction<'_, Postgres>` | `Transaction<'_, Sqlite>` | |
| `now()` | **bind from Rust** | Not `CURRENT_TIMESTAMP` — see [§4.6](#46-timestamps--the-hazard-that-would-have-shipped). |
| `gen_random_uuid()` | `Uuid::new_v4()` in Rust | `uuid` crate already a dependency. |
| `::bigint`, `::integer` | removed | Syntax error otherwise: `unrecognized token: ":"` (measured). |
| `RETURNING` | **keep** | Measured working, including on an UPSERT. |
| `ON CONFLICT … DO UPDATE` | **keep** | Measured working, including the table-qualified `t.n = t.n + excluded.n` form. |
| `SELECT … FOR UPDATE` | **remove** | Syntax error (measured). See [§4.5](#45-the-two-for-update-sites). |
| `WITH x AS (INSERT … RETURNING …)` | **remove** | No SQLite equivalent (measured). See [§4.7](#47-the-data-modifying-cte--one-function-must-be-rewritten). |
| `interval '2 hours'` | `datetime('now','-2 hours')` | `interval` is parsed as a column name (measured). |
| `CREATE EXTENSION pgcrypto` | removed | |
| `TIMESTAMPTZ`, `DATE` | `TEXT` | With a format `CHECK` — [§4.6](#46-timestamps--the-hazard-that-would-have-shipped). |
| `JSONB` | `TEXT` | sqlx `json` feature. Measured: `serde_json::Value` binds and decodes. |
| `UUID` | `TEXT` | sqlx `uuid` feature decodes `TEXT`. |
| `BIGSERIAL` | `INTEGER PRIMARY KEY AUTOINCREMENT` | |
| `BOOLEAN` | `INTEGER` (0/1) | `TRUE`/`FALSE` literals are accepted (measured) and evaluate to 1/0. |
| `SMALLINT` | `INTEGER` | |
| Partial indexes, `CHECK`, `COALESCE` | **keep** | All supported. |
| `MAX(a, b)` | **keep, but note** | SQLite's `MAX` is dual-purpose: aggregate with 1 arg, scalar with 2+. |

**A note on the last row, because it matters.** The draft's
`MAX(0.0, balance - ?)` would **not** be a syntax error in SQLite — it is the valid
scalar form and evaluates to 0 for a shortfall (measured). The defect is semantic, not
syntactic: it would run, silently clamp, and corrupt the ledger invariant. That is a
worse failure mode than a crash, and it is the reason §2.2 is a rejection rather than a
preference.

### 4.3 Connection setup — four traps, all measured

Every claim below is reproducible by running
[`tools/sqlite-probes/sqlite-port-probe.py`](../../tools/sqlite-probes/sqlite-port-probe.py) (SQLite 3.53.1).
They are measured, not recalled, because three of the four fail **silently**.

```rust
let options = SqliteConnectOptions::from_str(&database_url)?
    .create_if_missing(true)
    .journal_mode(SqliteJournalMode::Wal)      // persists in the file (measured)
    .synchronous(SqliteSynchronous::Normal)    // safe with WAL; avoids fsync per commit
    .busy_timeout(Duration::from_secs(5))      // replaces FOR UPDATE waiting
    .foreign_keys(true);                       // <-- trap 1

let pool = SqlitePoolOptions::new().max_connections(8).connect_with(options).await?;
```

**Trap 1 — foreign keys are OFF by default.** Measured: `PRAGMA foreign_keys` → `0`.
It is **per-connection**, not per-database. With it off, an insert referencing a
non-existent parent is **accepted** — measured, not inferred. Every `ON DELETE
CASCADE` in the schema silently does nothing (`api_keys`, `sessions`, `usage_daily`,
`key_ip_daily`, `telegram_links`, `link_codes`), and the `ON DELETE RESTRICT` that is
supposed to make a hard delete of a funded account *impossible* stops working. This
is the single most dangerous omission in the port: it fails silently and it removes a
money backstop.

*Precision added while executing:* this is **raw** SQLite's default. sqlx already
overrides it — `SqliteConnectOptions::default()` sets `foreign_keys` to `ON` — so a
pool built from `SqliteConnectOptions` is not exposed to the trap by accident. The
option is still passed explicitly in both `db::init_pool` and `bin/migrate.rs`, because
a money backstop that depends on a dependency's default is one dependency bump away
from being gone. Measured with the options as written: a dangling reference is refused
with `FOREIGN KEY constraint failed` (code 787).

The same care applies to the other two options. Measured defaults in sqlx 0.8.6:
`busy_timeout` is already 5s and `journal_mode` is deliberately left **unset** (with a
source comment explaining that WAL is permanent and entering it needs an exclusive lock
`sqlite3_busy_timeout()` cannot wait on), so neither is a default to lean on — WAL is
set by `bin/migrate.rs` and re-asserted by `init_pool`, and `synchronous(Normal)` is a
real change from SQLite's `FULL`. Measured with the options as written:
`journal_mode=wal`, `foreign_keys=1`, `synchronous=1`, `busy_timeout=5000`.

**Trap 2 — deferred transactions that read then write can fail unrecoverably.**
SQLite's default `BEGIN` is deferred. A transaction that `SELECT`s and later `UPDATE`s
can get `SQLITE_BUSY_SNAPSHOT` on upgrade, and that error **cannot be resolved by
retrying** — the transaction must be rolled back and restarted. Two existing functions
are exactly this shape: `credit_topup_transaction` and `refund_topup_transaction`.
Both must use `BEGIN IMMEDIATE` (measured: accepted). sqlx's `pool.begin()` issues a
deferred `BEGIN`, so add a helper that acquires a connection and issues
`BEGIN IMMEDIATE` explicitly, and route both call sites through it.

**Executed, with one refinement.** `pool.begin_with("BEGIN IMMEDIATE")` exists in
sqlx 0.8.6 and is what the helper uses — it needs no manual connection handling, and
it *verifies* the statement opened a transaction, failing with `BeginFailed` otherwise
(measured: `begin_with("SELECT 1")` is refused, so a typo is loud). The helper is
`db::begin_immediate`.

The refinement: moving each guard **into** the `UPDATE` (§4.5) means both named
functions now begin with a write, so the read-then-write shape this trap describes no
longer exists in either of them — nor in the other four transactions in the module,
which all open with a write too. `BEGIN IMMEDIATE` is still used for all six, because
it makes the lock acquisition explicit rather than a consequence of statement
ordering, so a later edit that adds a read to the top of one of them cannot
reintroduce the trap.

**Measured: what contention actually does.** With `max_connections(8)` and two
connections contending, the second `BEGIN IMMEDIATE` waits the full `busy_timeout`
(measured 5.53s) and then **fails with `database is locked` (code 5)**; once the first
releases, the identical statement succeeds. So `busy_timeout` does replace `FOR UPDATE`
waiting, but it is a bounded wait, not an indefinite one — every write in the process
serializes behind a single writer, and a writer that cannot get in within 5s errors
rather than blocking. The transactions here are a handful of statements, so this is
headroom rather than a live risk, but it is the shape of the ceiling: SQLite gives one
writer at a time for the whole database, and `record_key_ip` sits on the proxy hot
path. Worth knowing before raising `max_connections` in the belief it buys write
throughput.

**Trap 3 — `NULL` in a composite primary key duplicates rows.** In Postgres a PK
column is implicitly `NOT NULL`; **SQLite does not enforce this.** Measured: a `NULL`
in a composite PK is **accepted** (Postgres would refuse the row outright).

The consequence, measured: with `PRIMARY KEY (account_id, api_key_id, day)` and
`api_key_id` nullable, three identical `ON CONFLICT (account_id, api_key_id, day)`
upserts produced **3 rows instead of 1**. The conflict target never matches, because
SQLite treats `NULL`s as distinct in unique indexes, so the upsert degrades into a
plain insert — every call adds a row.

**Severity, stated precisely: this is latent today, not live.** The production call
site passes `Some(key_id)` (`proxy.rs:1337`), so `api_key_id` is never `NULL` on the
settlement path. It becomes live the moment a key row is hard-deleted
(`ON DELETE SET NULL`), or a test or future caller passes `None` — and
`debit_usage_transaction` accepts `Option<Uuid>` by signature. It would then surface
as unbounded table growth and wrong per-key aggregates, with no error.

The fix is verified: a `COALESCE` unique index plus a matching conflict target makes
three calls produce **1 row with the correct accumulated total** (measured).

```sql
CREATE UNIQUE INDEX usage_daily_scope_uniq
  ON usage_daily (account_id, day, COALESCE(api_key_id, ''));
```

and target **that index** in the upsert. Cheap insurance for an invariant that is
otherwise unenforced.

**Trap 4 — `VACUUM INTO` cannot run inside a transaction.** Measured: it succeeds in
autocommit and is **refused** with `cannot VACUUM from within a transaction` otherwise.
This matters for [§8](#8-operations-backup-rpo-and-the-volume) — the backup path must
either run on a dedicated autocommit connection or commit first, and a backup script
that shares the application's pooled connection will fail.

### 4.4 `usage_events` — the one genuinely new table

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
  created_at        TEXT NOT NULL
                    CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
);
CREATE INDEX usage_events_recent_idx ON usage_events (created_at DESC);
CREATE INDEX usage_events_account_idx ON usage_events (account_id, created_at DESC);
```

Four constraints on it:

1. **Three token counters**, never `prompt`/`completion` — see [§2.5](#25-prompt_tokens--completion_tokens--rejected).
2. **`cost_idr` is `INTEGER`**, never `REAL`.
3. **No prompt or completion text, ever.** Launch Gate 4 is *"Zero prompt/completion
   logging verified in code and logs"*. A per-request table is a retention liability;
   it gets a sweep job alongside the existing `ip-purge` binary, and an entry in
   [`data-retention.md`](../data-retention.md).
4. **No `DEFAULT CURRENT_TIMESTAMP`**, and the format `CHECK` is mandatory — see
   [§4.6](#46-timestamps--the-hazard-that-would-have-shipped). This applies to every
   timestamp column in the schema, not just this one.

### 4.5 The two `FOR UPDATE` sites

`server/src/db.rs:32` and `server/src/db.rs:155`. SQLite has no row locks, and
`SELECT … FOR UPDATE` is a **syntax error** (measured). Both are top-up idempotency
guards, and both are better served by a **conditional UPDATE plus
`rows_affected()`** — which needs no lock at all and is correct under any isolation
level:

```sql
UPDATE topups SET status = 'settled', settled_at = ?
WHERE order_id = ? AND status = 'pending' AND amount_idr = ?;
```

(`settled_at` is bound from Rust — [§4.6](#46-timestamps--the-hazard-that-would-have-shipped).)

If `rows_affected() == 0`, one disambiguating `SELECT` decides between
`AlreadySettled`, `NotFound` and `AmountMismatch`. This removes the lock instead of
emulating it, and keeps the existing tests meaningful.

**Correction, found while executing Phase 4: there is a fourth outcome, and the
stricter predicate is not merely a port.** The claim above that these are "the same
three outcomes `TopupCreditResult` already models" is incomplete, because
`status = 'pending'` is stricter than the Rust check it replaces. That check was

```rust
if status == "settled" { return AlreadySettled; }
if amount_idr != webhook_amount_idr { return AmountMismatch; }
// otherwise: settle it
```

— it short-circuited only on `'settled'`, so a row in **any other** state fell
through to settlement. Two consequences:

1. **A money-duplication defect, now closed.** Sequence: settle (wallet `+N`,
   status `settled`) → refund (wallet `-N`, status `refunded`) → the original
   *settlement* webhook is replayed. The old check saw `'refunded'`, which is not
   `'settled'`, so it settled again: the wallet gained `N` back and the row returned
   to `settled`. The refund was silently undone and the money existed twice. The
   `status = 'pending'` predicate refuses this, and does so in the same shape
   `refund_decision` already uses, where every non-`settled` status is refused.
2. **A fourth outcome the enum could not express.** A row that exists, whose amount
   agrees, but whose status is `denied`, `expired` or `refunded` is neither a
   mismatch nor a replay of a *settlement*. Reporting it as `AlreadySettled` would
   describe a refunded order as settled. `TopupCreditResult` therefore gains
   `NotSettleable { status }`, mirroring `RefundResult::NotSettled { status }`. The
   webhook answers **200** with a distinct body rather than the refund path's 409: a
   non-2xx would make Midtrans retry a webhook that can never succeed, and unlike the
   insufficient-balance case there is no operator action that would change the
   outcome.

### 4.6 Timestamps — the hazard that would have shipped

This is the finding most likely to have caused a production incident, and it is
invisible unless you read sqlx's encoder.

**The two sides disagree on format.**

| Source | Produces |
| --- | --- |
| Rust binding a `DateTime<Utc>` | `'2026-10-25T06:27:22+00:00'` — sqlx-sqlite 0.8.6 encodes via `to_rfc3339_opts(SecondsFormat::AutoSi, false)` |
| SQLite `CURRENT_TIMESTAMP` | `'2026-09-25 06:28:31'` — space separator, no offset, no fraction |

Both land in the same `TEXT` columns. **SQLite compares `TEXT` lexicographically**, so
the formats are not interchangeable — measured, and the consequences are not cosmetic:

- **Same instant, two formats, compares unequal.** Measured: `'2026-10-25T06:27:22+00:00'
  = '2026-10-25 06:27:22'` → `0`.
- **Session expiry breaks in the unsafe direction.** The session guard is
  `expires_at > now()`. Measured with `expires_at` written by Rust and `now()` by
  SQLite, for a session that expired **7.5 hours earlier**: the predicate returns
  **1 — still valid**. `'T'` (0x54) sorts after `' '` (0x20), so on the expiry *date*
  the RFC3339 value always outranks the space-format value regardless of the time. The
  session survives until midnight instead of until its expiry instant. Up to ~24 hours
  of extra session life, silently, on a money-bearing session.

**The rule: one representation, enforced by the database.**

1. **Never write time in SQL.** All 17 `now()` sites become a Rust bind. SQLite has no
   `now()` anyway (measured: `no such function: now`), so this is forced — but the
   *reason* is the format, not the syntax.
2. **No `DEFAULT CURRENT_TIMESTAMP` anywhere in the schema.** A default that fires
   writes the other format. Every timestamp column is `NOT NULL` with no default, so
   forgetting a bind is a loud error rather than silent drift.
3. **Every timestamp column carries a format `CHECK`.** Measured against 8 inputs:

   ```sql
   CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
   ```

   accepts `…T06:27:22+00:00`, `…T06:27:22.890+00:00` and `…T06:27:22.000000+00:00`;
   refuses the space format, the `Z` form, the offset-less form, and garbage. This
   makes the invariant structural rather than a convention — the same reasoning that
   puts `CHECK (balance_idr >= 0)` in the schema as the authoritative backstop.

**Verified safe:** with a uniform RFC3339-offset format, lexicographic ordering is
correct even when the fractional part varies in length (measured across 3 boundary
cases), because the offset always follows the seconds. Fixed-millisecond precision
removes the last ambiguity and is what the DDL uses. If SQL-side time is ever needed
in a pinch, the matching form is
`strftime('%Y-%m-%dT%H:%M:%f','now') || '+00:00'` (measured).

**Decoding is fine.** sqlx-sqlite's `decode_datetime_from_text` tries RFC3339 first,
then a table of fallbacks including `%F %T%.f`. So RFC3339 round-trips, and — worth
knowing — the space format would *also* have decoded. The bug is in comparison, not
decoding, which is exactly why it would have gone unnoticed.

**Rejected alternative, recorded:** store `INTEGER` unix seconds. sqlx decodes an
`INTEGER` as unix seconds natively (`Utc.timestamp_opt(v, 0)`), comparisons are
numeric and unambiguous, and there is no format to get wrong. It is arguably the more
robust choice. It was rejected because it forces every Rust bind to `i64` and makes
`sqlite3` output unreadable during the incident you most want to read it in. If the
format `CHECK` proves annoying in practice, this is the fallback — and it is a
one-migration change.

**Why this also settles the `::bigint` removal.** Measured: `SUM()` over an `INTEGER`
column returns `typeof` `integer`, but over a `REAL` column returns `real`. The 18
`::bigint` casts exist precisely to force an integer type before decoding into `i64`
(`events.rs:666` records the panic that motivated them). With every money and token
column declared `INTEGER`, `SUM` returns an integer and the casts are unnecessary —
**and this is the concrete reason `balance REAL` in the draft would have broken
decoding**, not just violated a convention.

### 4.7 The data-modifying CTE — one function must be rewritten

`ip_tracking.rs:190-205` uses a Postgres-only construct:

```sql
WITH inserted AS (
    INSERT INTO key_ip_seen (...) VALUES (...) ON CONFLICT DO NOTHING RETURNING 1
)
INSERT INTO key_ip_daily (...)
VALUES ($1, $2, (SELECT COUNT(*) FROM inserted)::integer, 1)
ON CONFLICT (api_key_id, day) DO UPDATE ...
```

Measured: SQLite **rejects** this with `near "INSERT": syntax error`. SQLite's `WITH`
clause accepts only `SELECT` in a CTE; data-modifying CTEs do not exist. There is no
dialect translation for this — it is a rewrite.

The pattern's purpose is to answer "was this `(api_key, day, ip_hash)` pair already
seen today?" inside one statement, so `distinct_ips` cannot race. The port must
preserve that property. Two candidate shapes, to be settled in the phase:

| Approach | Shape | Trade-off |
| --- | --- | --- |
| **Two statements in one `BEGIN IMMEDIATE`** | `INSERT … ON CONFLICT DO NOTHING` → `changes()`; then the `key_ip_daily` upsert using that count | Preserves atomicity and the no-race property. Two round trips on one connection instead of one statement. **Preferred.** |
| `INSERT … RETURNING` then read | Insert and use `rows_affected()` directly | One statement, but the count must reach the second statement, so it still needs a transaction |

**Either way it is one function, not a pattern.** Worth noting explicitly because
`ip_tracking.rs` is 782 lines and only this one statement is affected — the rest of the
file is ordinary SQL.

**Executed: the first approach, as preferred.** `record_key_ip` is now two statements
inside one `BEGIN IMMEDIATE`, with `rows_affected()` on the `key_ip_seen` insert
supplying the 1-or-0. Measured: `rows_affected()` is 1 for a new `(key, day, ip_hash)`
and **0** when `ON CONFLICT DO NOTHING` fires, and the sequence h1, h1, h2 yields
`distinct_ips` 1, 1, 2 against `request_count` 1, 2, 3. Measured confirmation that the
rewrite was necessary: the original statement is refused with
`near "INSERT": syntax error`.

The conflict target is spelled out rather than left bare — `ON CONFLICT (api_key_id,
day, ip_hash)` matches `key_ip_seen`'s primary key, all three columns `NOT NULL` — so a
future second unique index cannot silently capture the insert.

**The same rewrite exposed an error in §4.3's wording about `usage_daily`.** That
section says the upsert must "target **that index**", and the schema comment says
"target it by name". SQLite has no `ON CONFLICT ON CONSTRAINT <name>` form: an index
cannot be named, only restated. Measured against the real index
(`usage_daily_scope_uniq (account_id, day, COALESCE(api_key_id, ''))`):

| Conflict target | Result |
| --- | --- |
| `ON CONFLICT (account_id, api_key_id, day)` — the Postgres shape, and what the port first carried | **refused**: `ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint` |
| `ON CONFLICT (account_id, day, COALESCE(api_key_id, ''))` | accepted; a NULL key and a real key accumulate into two separate rows |
| `ON CONFLICT` (target omitted) | also accepted, and equivalent here since the table has one unique index |

The expression form is used: it states which index is meant, where omitting the target
would depend on the table never gaining a second one. Note the contrast with
`key_ip_daily`, whose key is a real composite primary key and which therefore *can* use
the plain column-list form — the two upserts in the same file legitimately differ.

### 4.8 Suspension and soft delete — already the design, with one correction

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
[§4.3](#43-connection-setup--four-traps-all-measured), trap 1.

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
- `decisions.md` fixes the cache TTL at 60 s, so an *un-initialised* change (a
  narrowed model allowlist) is honest about its window. Revocation is not in that
  class.

### 4.9 `STRICT` tables — make the money rule structural

**Measured, and it changes the DDL: an `INTEGER` declaration in SQLite is only
*affinity*, not enforcement.**

```sql
CREATE TABLE plain (v INTEGER NOT NULL CHECK (v >= 0));
INSERT INTO plain VALUES (1.5);   -- accepted; stored as REAL, typeof = 'real'
```

A `REAL` value lands in a money column, and `CHECK (v >= 0)` does not stop it. For a
codebase whose register says *"Money type: `BIGINT` IDR — never floating point"*, that
is the same class of silent failure as the `MAX(0, …)` clamp: it does not crash, it
corrupts.

`STRICT` tables (SQLite ≥ 3.37) enforce the declared type. Measured:

| Test | Ordinary table | `STRICT` table |
| --- | --- | --- |
| `INSERT 1.5` into `INTEGER` | accepted, stored as `real` | **refused**: `cannot store REAL value in INTEGER column` |
| `INSERT 'yes'` into `INTEGER` | accepted | **refused**: `cannot store TEXT value in INTEGER column` |
| `INSERT NULL` into a composite PK column | accepted | **refused**: `NOT NULL constraint failed` |

Three consequences, all good:

1. **"Money is never floating point" becomes a database backstop**, not a convention.
   This is the same reasoning that puts `CHECK (balance_idr >= 0)` in the schema.
2. **The composite-PK `NULL` trap is fixed structurally for PK columns** — `STRICT`
   implies `NOT NULL` on them. (The `usage_daily` case still needs its `COALESCE`
   unique index, because there `api_key_id` is deliberately *outside* the key so it can
   stay nullable for `ON DELETE SET NULL`.)
3. **It is compatible with everything else in the schema** — measured against the
   `GLOB` timestamp `CHECK`, the `'[]'` JSON default, and partial indexes: all fine.

**Therefore: every table in [Appendix A](#appendix-a--target-sqlite-schema) is declared
`STRICT`.** Requires SQLite ≥ 3.37; sqlx-sqlite 0.8.6 bundles well past that, but the
build must pin it rather than assume.

This is the single highest-value change in the whole plan after the timestamp rule: it
converts the register's money rule from something reviewers enforce into something the
database enforces.

### 4.10 `ON DELETE SET NULL` collides with the `COALESCE` unique index

Found by running [Appendix A](#appendix-a--target-sqlite-schema) rather than by reading
it — which is the argument for validating a schema before porting against it.

The ported `usage_daily.api_key_id` was `ON DELETE SET NULL`, matching Postgres. Under
SQLite that combination is **broken**, and it fails only in a specific ordering:

1. A row exists for `(account, NULL, day)` — reachable because
   `debit_usage_transaction` takes `Option<Uuid>`.
2. A key with its own `(account, key, day)` row is deleted.
3. `SET NULL` fires and tries to write `NULL` into a row that now collides with the
   existing `NULL` row on `usage_daily_scope_uniq`.

Measured result: `UNIQUE constraint failed: index 'usage_daily_scope_uniq'`. The delete
**fails**, so a key deletion can be blocked by an unrelated NULL-keyed aggregate row.

**Fix: `usage_daily.api_key_id` becomes `ON DELETE RESTRICT`.** Three reasons, in order
of weight:

1. It matches the money tables. `wallets`, `ledger` and `topups` all use `RESTRICT`,
   and `admin-surface.md:151` says *"No hard deletes, ever."* A key with billing
   history should not be deletable at all — revoking is `revoked_at`.
2. It removes the failure mode entirely, rather than depending on which rows happen to
   exist.
3. `usage_daily` is a financial aggregate and is never swept. Destroying it via
   `CASCADE` would be worse than refusing the delete; orphaning it via `SET NULL` is
   what breaks.

`usage_events.api_key_id` keeps `ON DELETE SET NULL`, deliberately: that table **is**
swept for retention, so it must not be able to block a key purge, and it carries no
unique index that a NULL could collide with. The asymmetry is intentional and is
recorded here so it does not look like an oversight.

---

## 5. Phases

### 5.0 Phase 0 — Settle the register

Edit [`decisions.md`](../decisions.md) per [§3](#3-decisions-this-forces-the-register-to-change).
Nothing else starts until this lands, because every later document reads from it.

### 5.1 Phase 1 — Dependency and config

Executed 2026-09-25 on branch `sqlite-port` in a worktree, per [§12](#12-suggested-execution-order).

- `server/Cargo.toml`: `postgres` → `sqlite` in the `sqlx` feature list; keep
  `uuid`, `chrono`, `json`. **Add `migrate`** — it was *absent*, not present, so the
  draft's "drop `migrate`" instruction was inverted. [§5.3](#53-phase-3--migration-mechanism)
  is adopted, so the feature is required. `sqlx`'s `sqlite` feature is the **bundled**
  one (`sqlite = ["_sqlite", "sqlx-sqlite/bundled", …]`), which compiles SQLite from
  source and therefore needs no system library and no `pkg-config`. The alternative,
  `sqlite-unbundled`, is not wanted.
- `DATABASE_URL=sqlite://data/server.db` — a file path, not a network URL.
- `data/` must be on the **persistent volume** ([§8](#8-operations-backup-rpo-and-the-volume))
  **and must be gitignored**, or the local database becomes committable.
- Delete `POSTGRES_PASSWORD` and `REDIS_URL` from `.env.example`. **Keep
  `POCKETBASE_URL`** — see correction 1.
- `docker-compose.yml`: delete the `postgres` and `pocketbase` services, the
  `pgdata`/`pbdata` volumes, **and `nginx`'s `depends_on: postgres`**. Keep `nginx`.
  Delete `.docker/postgres/`.

**Four corrections, none of which were visible on paper:**

1. **`POCKETBASE_URL` must stay.** The draft said to delete it, but
   `routes/auth.rs:90` and `routes/account.rs:328` still read it, and PocketBase is
   present for identity until Phase 6 — which [§12](#12-suggested-execution-order)
   deliberately allows. Deleting it here would strip the documented local address
   while the code still calls it. The deletion belongs in Phase 6.
2. **`nginx`'s `depends_on: postgres` would have broken compose outright.** Removing
   the `postgres` service without removing the reference leaves `docker compose up`
   failing on an undefined service. The draft does not mention it.
3. **`REDIS_URL` was dead config.** Zero references in `server/src`, `config/` or
   `docs/` — the same class of defect as the `allow_negative_balance_overdraft` flag
   the hot-path audit found. Removed.
4. **`DATABASE_URL` must not carry `?mode=rwc`, and this changes Phase 3.**
   `sqlx-sqlite` 0.8.6 defaults `create_if_missing: false`
   (`src/options/mod.rs:198`); `mode=rwc` is what sets it true
   (`src/options/parse.rs:53`). So the server **fails loudly when the database is
   absent** rather than creating an empty, schema-less one — the correct behaviour
   given *"migrations never run on application boot"*. The consequence is a Phase 3
   requirement: **the migrate binary must set `create_if_missing(true)`**, because it
   is the component whose job is to create the database. Separately, SQLite creates
   the file but never its parent directory, so `data/` must exist before either binary
   opens the pool.

**One instruction was lost, and is re-homed rather than dropped.**
`.docker/postgres/README.md` documented how to reset the local database. That is still
needed — it only changes form, from `docker compose down -v` to deleting
`data/server.db` and its `-wal`/`-shm` sidecars. Phase 8 must move it into
`docs/local-development.md`; the compose header carries the short version meanwhile.

### 5.2 Phase 2 — Schema port

Executed 2026-09-25 on branch `sqlite-port`.

`server/migrations/20260925000000_initial_schema.sql` is **replaced in place** with the
SQLite schema — there is no production data to preserve, and a single migration is
honest about that. The file is now equivalent to
[Appendix A](#appendix-a--target-sqlite-schema) apart from comments, and a script
enforces that equivalence so the two cannot drift.

The port is not a transcription: **17 tables, up from 15.** The two additions are
`identities` (replacing PocketBase) and `usage_events` (the per-request admin feed).
Six existing definitions also change shape:

| Change | Why |
| --- | --- |
| `accounts.pb_user_id` **dropped** | `accounts.id` is the only key now ([§3](#3-decisions-this-forces-the-register-to-change)) |
| `sessions.last_seen_at` **added, `NOT NULL`** | The register specifies *"30 days absolute, 7 days idle"*, but the Postgres schema had no column for the idle bound, so only the absolute half was enforceable. See warning 1 below |
| `usage_daily` loses its `PRIMARY KEY` | A `COALESCE` expression cannot appear in a PK; the unique index *is* the key ([§4.3](#43-connection-setup--four-traps-all-measured) trap 3) |
| `usage_daily.api_key_id` → `ON DELETE RESTRICT` | `SET NULL` collides with the `COALESCE` index ([§4.10](#410-on-delete-set-null-collides-with-the-coalesce-unique-index)) |
| `BIGINT`/`BIGSERIAL` → `INTEGER`, `JSONB` → `TEXT`, `DATE` → `TEXT` | `STRICT` rejects `BIGINT` ([§11.1](#111-decided) item 13); `TEXT` for JSON; one representation for all time |
| every `DEFAULT now()` → **no default** | Time is bound from Rust and never written in SQL ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped)) |

**Verified by `tools/sqlite-probes/validate-migration-schema.py`: 32 checks, 0 failed.**
It applies both the shipped migration and Appendix A, asserts the two create identical
objects, then exercises the invariants against the shipped file.

**Two consequences the later phases must not miss:**

1. **`sessions.last_seen_at` is `NOT NULL` with no default.** Every `INSERT INTO
   sessions` must supply it, so `auth.rs` has to be ported in the same phase as this
   schema change or session creation fails **at runtime, not at compile time** — the
   compiler cannot see this one. Phase 4.
2. **`journal_mode = WAL` is not set here, deliberately.** WAL is a persistent database
   property, but `PRAGMA journal_mode` cannot run inside a transaction and `sqlx` wraps
   each migration in one. It belongs in the connection setup. Phase 4.

### 5.3 Phase 3 — Migration mechanism

The register says `sqlx migrate`, forward-only, never on boot. Implement it as
written: `server/src/bin/migrate.rs` using `sqlx::migrate!("./migrations")`, run in CI
and in the deploy pipeline **before** the server starts
(`deploy order: Migrate -> server -> health -> frontend`).

### 5.4 Phase 4 — Query port

**Executed 2026-09-25 on branch `sqlite-port`.** The mechanical rules were applied
across all 14 files, then the semantic work was done by blast radius. Final state:
`cargo check --all-targets` clean, `cargo build --release` clean, and **no SQL-side
`now()`, no `FOR UPDATE`, no `$N` placeholder and no `::bigint` cast** left anywhere in
`server/src`.

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

**Four things the port needed that §4.1's inventory did not name.** Each is recorded in
full where it belongs; listed here so the phase's real size is visible:

1. **Five INSERTs that relied on a removed Postgres default** — `accounts`, `wallets`,
   `sessions`, `api_keys`, `topups` ([§4.1](#41-the-measured-port-inventory),
   correction 1). These are the sites where "the compiler cannot see it" is literal.
2. **A fourth `TopupCreditResult` outcome**, because the prescribed
   `status = 'pending'` guard is stricter than the Rust check it replaces — and closing
   that gap also closed a money-duplication defect ([§4.5](#45-the-two-for-update-sites)).
3. **An expression conflict target for `usage_daily`**, since SQLite cannot name an
   index in `ON CONFLICT` ([§4.7](#47-the-data-modifying-cte--one-function-must-be-rewritten)).
4. **`db::init_pool` needed the full [§4.3](#43-connection-setup--four-traps-all-measured)
   option set**, which §5.2's warning 2 assigns to this phase: WAL, `synchronous(Normal)`,
   `busy_timeout(5s)`, `foreign_keys(on)`. Separately, `main.rs` still defaulted
   `DATABASE_URL` to `postgres://postgres:postgres@localhost:5432/apikita`; that is fixed
   to the SQLite path `.env.example` documents.

**Verified by execution, not by compilation.** The ignored unit tests cannot reach this
code — their fixtures fail first on `NOT NULL constraint failed: accounts.id`, which is
itself the confirmation of item 1. So the ported statements were exercised directly,
against a database produced by the real `bin/migrate`, through a harness that calls the
real `db::*` and `ip_tracking::*` functions. **33 checks, 0 failed**, including:

- `record_key_ip` on h1, h1, h2 → `distinct_ips` 1, 1, 2 against `request_count` 1, 2, 3.
- settle → replay → refund → **replayed settlement**. The last is refused as
  `NotSettleable`, the wallet stays at 0, and the ledger still reconciles. Under the
  pre-port check this same sequence re-credited the wallet and undid the refund.
- `usage_daily` accumulates a NULL-keyed and a keyed row into two separate rows with the
  right totals; the pre-port conflict target was refused outright.
- `balance_idr = SUM(ledger.delta_idr)` after every money step — settle, refund, hold,
  four charges, release, and a clamped shortfall.
- An expired session is not accepted, and the space timestamp format is refused by the
  schema's own CHECK.

The harness was a scratch instrument under `.agents/` and was deliberately not
committed. [§5.5](#55-phase-5--tests-become-real-tests) is where they became real
tests: every check above now runs in `cargo test --lib` against a temp database.

### 5.5 Phase 5 — Tests become real tests

**Executed.** The benefit stated below was real and it is now taken: every money test
in `db.rs` was `#[ignore = "requires live Postgres"]`, including
`concurrent_requests_cannot_overdraw_a_one_request_balance`. With SQLite they need
only a temp file, so they **run in CI by default**. Measured after the phase:
**134 passed, 0 failed, 0 ignored** — without the `--ignored` flag, and with
`DATABASE_URL` set to nothing.

What changed:

- `tempfile` is a dev-dependency, and `server/src/test_support.rs` (gated
  `#[cfg(test)]`) builds one migrated database per test. It creates the file and
  applies the real `sqlx::migrate!("./migrations")` on a single connection exactly as
  `bin/migrate.rs` does, then hands the result to `db::init_pool` — so the pragmas
  under test are the **production** ones, not a test-only approximation a passing
  suite could hide behind.
- The three modules that read `DATABASE_URL` (`db.rs`, `abuse.rs`, `ip_tracking.rs`)
  now call `TestDb::new()`. Not one `#[ignore]` remains in `src/`.
- The fixtures that omitted `id`, `created_at` and `updated_at` are replaced by named
  helpers. This is the defect class the compiler cannot see: the old
  `INSERT INTO accounts (pb_user_id) VALUES (?)` still type-checked and failed at
  runtime with `NOT NULL constraint failed: accounts.id`.
- Per-test databases made the teardown **unnecessary**. `delete_fixture_rows` and
  `delete_fixture` deleted rows by name in FK order; there is nothing left to clean
  up, and two tests that used to race through one shared database now cannot see each
  other's rows.

**Correction to [§4.1](#41-the-measured-port-inventory): its "5 sites" understate this.**
Those five are production code. The test fixtures are a *separate* five
(`db.rs` 3, `abuse.rs` 1, `ip_tracking.rs` 1) plus the wallet and topup inserts inside
the assertion helpers. That last group is where one insert survived a first pass — its
`.expect()` message differed by three words from the other two, so a whole-file
replacement missed it and only running the tests found it
(`NOT NULL constraint failed: wallets.updated_at`).

New tests, each of which fails against a naive mechanical port:

| Test | What it guards |
| --- | --- |
| `two_null_key_settlements_accumulate_into_one_usage_row` | [§4.3](#43-connection-setup--four-traps-all-measured) trap 3 — two `NULL`-keyed upserts must land on **one** row, and a keyed row must stay a separate scope |
| `a_settlement_replayed_after_a_refund_is_refused_and_credits_nothing` | [§4.5](#45-the-two-for-update-sites) — the money-duplication defect; the wallet must stay at 0 and the row must stay `refunded` |
| ~~`concurrent_refunds_serialize_without_losing_the_write_lock`~~ | **NEVER LANDED.** This row claimed "the real concurrency test this section owed. Five concurrent refunds: one refunds, four see it already refunded, none errors". The name appears in NO commit of this repository (`git log -S` searches it and finds nothing) and the `AlreadyRefunded` variant it describes does not exist either. `374c2fd` then removed `refund_topup_transaction` entirely - refunds are REFUSED rather than debited - so the test this section owed is no longer one that COULD be written against the current shape. The read-then-write contention property is still guarded by construction, because every transaction opens with its write under `BEGIN IMMEDIATE`; what is absent is a test asserting it, which §9 check 4 records |
| `an_expired_session_is_refused_and_a_live_one_is_accepted` | [§4.6](#46-timestamps--the-hazard-that-would-have-shipped) — the session-lifetime hazard, exercised through the real cookie path |
| `the_schema_refuses_a_timestamp_sqlite_would_have_written` | [§4.6](#46-timestamps--the-hazard-that-would-have-shipped) — the GLOB CHECK actually fires on the space-separated format |

**Measured, and the convenient assumption is wrong:** while a pooled SQLite connection
is open, `remove_dir_all` on its directory is **refused** on Windows — SQLite's win32
VFS does not request `FILE_SHARE_DELETE` for the main database or its `-wal`/`-shm`
sidecars. So `TestDb::close` *awaits* `pool.close()` instead of relying on drop order
(dropping a pool only signals the close), and a test that **panics** never reaches
`close` and leaves one small directory in the system temp. That is the accepted cost
of per-test isolation: bounded, and far cheaper than the single shared database these
tests used to race through.

### 5.6 Phase 6 — Identity

[§6](#6-phase-2--identity-the-real-cost). Sequenced after the database port because it
is the largest phase and the only one that depends on decisions outside the codebase
(§11.2 items 1 and 2). The strategy itself is settled — Google + email/password —
so this phase is unblocked and can run in parallel with Phase 8 if there is capacity.

### 5.7 Phase 7 — Admin surface

[§7](#7-phase-4--admin-surface). Conform to `admin-surface.md`, do not invent a
parallel model.

### 5.8 Phase 8 — Tooling, harnesses, docs

Partly executed.

- **DONE — `tools/reconcile/reconcile.sh`** now runs the `sqlite3` CLI instead of
  `psql`. `reconcile.sql` needed no change: the query is plain ANSI SQL and runs
  unchanged. Every exit path is verified by execution (0 clean, 1 drift, 2 bad URL,
  3 `sqlite3` missing, 4 SQL/file error, 5 file absent), and the drift path is proven
  by fault injection — a check that cannot fail is not a check. Two deliberate
  choices: it opens `-readonly` so reconciliation can never write, and it matches the
  URL prefix with a `case` rather than stripping it, so a leftover `postgres://…`
  fails loudly instead of being rewritten into a plausible filename.
- **DONE — `docs/local-development.md`** no longer describes a Postgres container or
  `psql -f schema.sql`. It documents `cargo run --bin migrate`, and owns the
  local-database-reset instructions that had been left in the `docker-compose.yml`
  header (now a pointer).

**Correction: the `.agents/` harness bullet below is stale and has been removed.** It
listed `e2e-money/bin/psql`, `psql.sh`, `e2e-sse/psql.sh`, `e2e-sse/seed.sql`,
`e2e-ui/*.mjs` and "every `e2e.toml` referencing Postgres". Measured: **none of them
exist** — `git ls-files | grep e2e` returns nothing. The bullet was written before the
tree was read. Anything that cannot be found has no port cost, and listing it makes the
phase look larger than it is.

**Correction: the file counts were low, and one of them points the wrong way.**
Re-measured against `git ls-files`: **43** tracked files mention `postgres|psql|5432`
(not 30) and **38** mention PocketBase (not 33). The first number overstates the work:
most of those hits are `server/src/*.rs` and the migration, where "replaces the
PostgreSQL …" is an intentional comment recording what the port changed, and deleting
it would destroy the very explanation a reader needs.

The PocketBase number is the one that matters and it does **not** mean 38 files to
rewrite: **PocketBase is still the identity provider until Phase 6**
([§6](#6-phase-2--identity-the-real-cost)), so the great majority of those mentions are
*correct as written* and a find-and-replace would make the docs lie. The sweep is
therefore a judgment pass, not a mechanical one — change the claim only where the
document asserts PocketBase owns something SQLite now owns, or where it pre-announces
Phase 6 as done.

Remaining: `README.md` (the stack table), `config/apikita.toml`, `server/README.md`,
`todo.md`, and the doc files named below, in descending mention count —
`docs/architecture.md`, `docs/website/05-security-decisions.md`,
`docs/architecture/identity.md`, `docs/website/02-data-model.md`,
`docs/website/01-architecture.md`, `docs/backup-and-restore.md`, `docs/deployment.md`,
`docs/ci-cd.md`, `docs/data-retention.md`, `docs/cost-and-sizing.md`,
`docs/observability.md`, `docs/server/api-spec.md`, `docs/launch-checklist.md`,
`docs/abuse-runbook.md`, `docs/plan-audit.md`.
- `README.md` stack table: `Money | Northflank | PostgreSQL` becomes
  `Money | Northflank | SQLite (embedded)`. The `Identity | Northflank | PocketBase`
  row stays until Phase 6.

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
a purpose-built form. Decided as 10 in [§11.1](#111-decided).

---

## 8. Operations: backup, RPO, and the volume

Five things the draft does not address, all load-bearing. The first four are answered
by Northflank's own documentation
([volumes](https://northflank.com/docs/v1/application/databases-and-persistence/add-a-volume),
[pricing](https://northflank.com/docs/v1/application/billing/pricing-on-northflank)) —
read, not assumed.

**1. The volume is mandatory, and it is not just a formality.** SQLite is a file.
Northflank volumes "persist data across restarts"; without one, a redeploy silently
discards the ledger. Mount the `data/` directory and verify by restore drill.

**2. Single-instance is enforced by the platform — which is better than a convention.**
Northflank's volume access modes: *Single Read/Write* is the default and means
*"Services are limited to 1 instance … Cannot scale services horizontally (replicas >
1) with the same volume attached … Cannot enable high availability."* Their docs also
state that *Multi Read/Write* is *"Not suitable for databases or applications
expecting exclusive write access."*

So attaching the volume in Single Read/Write mode makes [§3](#3-decisions-this-forces-the-register-to-change)'s
"exactly one instance" **structural rather than a convention someone can violate.**
That is a genuine safety win: the failure mode the register addition guards against
becomes impossible rather than discouraged.

**3. Deploys cause downtime, and that is a real cost.** The same documentation:
*"During restarts, the running container will always be terminated before the new one
starts (regardless of health check settings)."* With a Single Read/Write volume there
is **no rolling deploy and no zero-downtime restart** — every deploy is a hard stop
followed by a start. For a prepaid API this means a brief outage per release and it
needs a maintenance window, not a silent push. The register's deploy order
(*"Migrate → server → health → frontend"*) still holds, but it now has an outage
attached to it, and that belongs in the launch checklist.

**4. Two operational traps that will otherwise cost a debugging session.**

- **File permissions.** *"Ownership of persistent volumes will be given to the group
  specified in the Docker image, determined at build time. This may cause issues if
  your application attempts to read, write, or execute with a different user."*
  SQLite will fail to open the database with `unable to open database file` — an error
  that reads like a path bug and is actually an ownership bug. Fix with `USER` in the
  Dockerfile, or a `chown` in the entrypoint. Must be verified on the real volume, not
  locally.
- **Volumes cannot shrink.** *"Volume storage cannot be scaled down after creation."*
  Pick the size deliberately; it is a one-way decision.

**5. Backup and the 15-minute RPO.** `decisions.md` requires *"PITR plus offsite;
restore drill required"* with **RPO 15 minutes**. WAL alone does not provide this.

| Option | RPO | Cost |
| --- | --- | --- |
| **Litestream → Cloudflare R2** (recommended) | seconds | Continuous WAL shipping. Cloudflare is already in the stack. |
| Cron `VACUUM INTO` + upload | up to the interval | Simpler; a 15-min interval means 15-min RPO at best. |

**Litestream must run as a sidecar process inside the API container, not as a second
service.** This is forced, not preferred: a second service mounting the same volume is
impossible under Single Read/Write (one pod), and Multi Read/Write is documented as
unsuitable for databases. The architecture decision follows from the access mode.

Two consequences to budget:

- **Memory.** Litestream adds ~20-30 MB RSS against a 256 MB limit. The draft's
  *"< 50 MB RSS"* target did not account for it.
- **`VACUUM INTO` cannot run inside a transaction** ([§4.3](#43-connection-setup--four-traps-all-measured),
  trap 4). If the cron route is chosen, the backup connection must be in autocommit —
  a script reusing the application's pooled connection will fail.

Launch Gate 1 (`docs/launch-checklist.md`) changes from `pg_dump`/WAL archiving to
whichever is chosen, **and the restore drill must be re-run.**

**A further platform constraint worth recording.** The free Developer Sandbox plan
allows *"2 services, 2 jobs, 1 addon"*, and Northflank states it *"should not be used
for production applications"*. Two implications: the sidecar decision above keeps the
service count at one, which the 2-service limit makes worthwhile; and taking customer
money on a tier the vendor says is not for production is a real risk. `decisions.md`
already carries *"Northflank actual prices — a quote"* as genuinely open, and this
sharpens what the quote is for.

---

## 9. Verification

Ordered by what would fail silently if skipped. Checks 1–3 and 7 are already
**implemented as runnable probes** in `tools/sqlite-probes/` — they were used to write
[§4.3](#43-connection-setup--four-traps-all-measured) and
[§4.6](#46-timestamps--the-hazard-that-would-have-shipped), so they are evidence, not
proposals.

| # | Check | Command / method | Status |
| --- | --- | --- | --- |
| 1 | Foreign keys actually on | `PRAGMA foreign_keys` → `1`; and a bad FK insert must **fail** | **probe: PASS** |
| 2 | `NULL` `api_key_id` upsert | Two calls with `api_key_id = NULL` must yield **one** row | **test: PASS** — `two_null_key_settlements_accumulate_into_one_usage_row`. The probe first **reproduced the bug and the `COALESCE` index fix** |
| 3 | Timestamp format uniformity | Every column rejects the space format; mixed-format expiry comparison must not return "still valid" | **test: PASS** — `the_schema_refuses_a_timestamp_sqlite_would_have_written` and `an_expired_session_is_refused_and_a_live_one_is_accepted`. The probe first **reproduced the 7.5-hour session overrun** |
| 4 | Read-then-write under contention | Concurrent writes to the wallet must not raise `SQLITE_BUSY_SNAPSHOT` | **SUPERSEDED, and the verdict it carried was never true.** `374c2fd` removed `refund_topup_transaction` and the `AlreadyRefunded` variant it returned (776 lines deleted from `db.rs`): refunds are now REFUSED rather than debited, so the path this row tested no longer exists. The row previously read **test: PASS** and cited `concurrent_refunds_serialize_without_losing_the_write_lock` - a test that appears in NO commit of this repository (`git log -S` searches it and finds nothing), alongside an error variant that never existed either. The read-then-write property itself is still real and still guarded BY CONSTRUCTION - every transaction opens with its write under `BEGIN IMMEDIATE`, so there is no deferred read to upgrade - but **no test asserts it**, and `SQLITE_BUSY_SNAPSHOT` now appears in this tree only inside a comment in `db.rs`. Recorded as an honest gap rather than a pass |
| 5 | Ledger invariant | `SELECT COUNT(*) FROM (SELECT w.account_id FROM wallets w LEFT JOIN ledger l ON l.account_id=w.account_id GROUP BY w.account_id, w.balance_idr HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr),0))` → **0**. This is Launch Gate 2 | **test: PASS** — asserted after settle, refund, hold, four charges, release and a clamped shortfall, and now **by default** rather than behind `--ignored` |
| 6 | Stranded holds | `unpaired_hold_rows` → **0** for every account (`db.rs:1692`) | existing |
| 7 | WAL is actually on and persists | `PRAGMA journal_mode` → `wal`, and still `wal` on a fresh connection | **probe: PASS** |
| 8 | `::bigint` removal is safe | `typeof(SUM(col))` → `integer` for every money and token column | **PASS against the migrated schema** — 9/9 columns return `integer`, and a `REAL` column returns `real`, which is the trap the casts existed for |
| 9 | Overdraw proof | `cargo test --lib` — the concurrency test now runs **without** `--ignored` | **test: PASS** — measured **134 passed / 0 failed / 0 ignored**. The two concurrency tests re-run 15× with no flakes (0.30–0.44 s each) |
| 10 | Volume permissions | The container user can create, write and reopen the DB file **on the real volume** | new, deploy-time |
| 11 | Memory | Re-measure RSS, including the Litestream sidecar | re-run `docs/benchmark.md` |
| 12 | Write throughput ceiling | Single-writer serialisation is the new bottleneck. Record the write rate, not just token throughput | re-run `bin/benchmark.rs` |
| 13 | Build | `cargo check --all-targets`, `cargo build --release`, `cargo test` | **PASS** — check clean, release built in 18.85s, and `cargo test` runs **134 passed / 0 failed / 0 ignored**. One pre-existing failure was fixed on the way: an illustrative indented block in `upstream/key_pool.rs` was collected as a Rust doctest and did not compile, so a bare `cargo test` was red for a comment. Marked `text` |
| 14 | Restore drill | Restore from Litestream/backup into a clean volume and run check 5 against the restored file | Launch Gate 1 |

**Run the probes first, before writing any Rust.** They take seconds and they confirm
the dialect assumptions the whole port rests on:

```bash
python tools/sqlite-probes/sqlite-port-probe.py
python tools/sqlite-probes/sqlite-timestamp-probe.py
python tools/sqlite-probes/validate-migration-schema.py
```

**Claims that must not be repeated without re-measurement:** the 63,750 tok/s and
34 MB figures in `docs/benchmark.md` and `todo.md` were measured against Postgres.
They are unverified for SQLite and must be re-measured or marked stale.

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
- **No horizontal scaling, permanently.** And now enforced by the storage layer rather
  than by policy — a Single Read/Write volume cannot be mounted twice.
- **Every deploy is an outage.** A Single Read/Write volume forbids rolling restarts
  ([§8](#8-operations-backup-rpo-and-the-volume)). This is the loss the draft did not
  mention at all, and it is the one an operator will feel weekly.
- **A sidecar process is now mandatory** for backup, competing for the same 256 MB.
- **The API is container-bound.** A future Cloudflare Workers migration would need D1.
- **Auth is now ours.** Whatever PocketBase was doing right, we now have to do right.
- **One more format to police.** Timestamps need a schema-enforced representation
  ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped)) where Postgres enforced
  it for us.

**Not given up:** the ledger, the non-negative balance, three-class token accounting,
idempotent top-ups, server-side sessions, the audit trail, or any admin capability.
Those are the parts that matter, and none of them depend on Postgres.

---

## 11. Decisions

### 11.1 Decided

Settled 2026-09-25. Each is technical and evidence-backed; none needs an owner sign-off.

| # | Decision | Why |
| --- | --- | --- |
| 1 | **Identity: Google + email/password** | Keeps the register's product decision; ownership moves to Rust ([§6](#6-phase-2--identity-the-real-cost)) |
| 2 | **Driver: `sqlx` + `sqlite` feature, not `rusqlite`** | `sqlx-sqlite` is already in `Cargo.lock`; the port is a 106-site dialect change, not an API rewrite; `sqlx::migrate` is a settled register decision; and the bottleneck is the single writer, not the binding. Full reasoning: [`proxy-hot-path-audit.md` §3](proxy-hot-path-audit.md) |
| 3 | **Backup: Litestream sidecar → Cloudflare R2** | Seconds-level RPO against a 15-minute requirement. A sidecar is *forced* by the Single Read/Write volume, not merely preferred ([§8](#8-operations-backup-rpo-and-the-volume)) |
| 4 | **Timestamps: uniform RFC3339 + `GLOB` `CHECK`, never written in SQL** | The measured mixed-format hazard silently extended session life by up to ~24h ([§4.6](#46-timestamps--the-hazard-that-would-have-shipped)) |
| 5 | **All tables `STRICT`** | Without it `INTEGER` is only affinity and a `REAL` lands in a money column ([§4.9](#49-strict-tables--make-the-money-rule-structural)) |
| 6 | **`usage_daily.api_key_id` → `ON DELETE RESTRICT`** | `SET NULL` collides with the `COALESCE` unique index (measured); `RESTRICT` also matches the money tables ([§4.10](#410-on-delete-set-null-collides-with-the-coalesce-unique-index)) |
| 7 | **30-day rolling spend window stays a query over `usage_daily`** | It is an indexed aggregate. Moving it to `usage_events` would make the **billing** window depend on a **retention** sweep — a retention change would silently change what a customer is charged. `usage_events` is display-only |
| 8 | **`usage_events` retention: 90 days** | Must outlast the 30-day rolling window plus a dispute window. 90 days is already the codebase's precedent for `key_ip_daily` (`ip-tracking.md:69`) and the top of the *"Logs 30-90 days"* band (`data-retention.md:60`). Beyond that, `usage_daily` is the permanent record |
| 9 | **Admin money actions ship behind a config flag** | Follows `admin-surface.md:181-189`: read-only plus suspend/restore/key-revoke at launch; money actions *"once there is revenue to misfile"* |
| 10 | **Low-code tool: adopt for the read-only launch surface only** | Real saving on reads, real hazard on money ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)) |
| 11 | **Volume: 10 GB, provisioned once** | It cannot be shrunk. Estimate: `usage_events` at ~100 B/request is ~300 MB for 90 days at 1M requests/month; the rest is ledger and headroom. Generous because the decision is one-way |
| 12 | **Publish a maintenance window for deploys** | A Single Read/Write volume forbids rolling restarts, so every deploy is an outage ([§8](#8-operations-backup-rpo-and-the-volume)). An undocumented one is worse than a documented one |
| 13 | **Money type `BIGINT` → `INTEGER`** | `STRICT` accepts only `INT`, `INTEGER`, `REAL`, `TEXT`, `BLOB`, `ANY`; `BIGINT` is rejected outright (measured). Item 5 forces it, so the register's money-type row had to change with it — a contradiction between the two documents that only surfaced while writing Phase 0 |

### 11.2 Still needs you

These are not technical. They need a cost, a vendor, or a business position — deciding
them from inside the codebase would be guessing.

| # | Decision | Why it is yours | Blocks |
| --- | --- | --- | --- |
| 1 | **Email provider** for verification and reset | Vendor choice, cost, and deliverability reputation. Recommendation: Resend or Postmark — both have usable free tiers and a simple API; pick on deliverability, not price | Phase 6 |
| 2 | **OTP and MFA — still wanted?** | Product scope. PocketBase supplied both and `identity.md:37-38` counts them; nothing does now. If yes it is new work; if no, the doc must stop claiming it | Phase 6 |
| 3 | **Northflank free tier vs paid** | Business/cost. The vendor states the free Developer Sandbox *"should not be used for production applications"*, and taking money on it is a real risk. `decisions.md` already has *"Northflank actual prices — a quote"* open | Launch |
| 4 | **Low-code tool and customer data** | Privacy/legal. A cloud tool means customer rows transit a third party, which the ToS and `data-retention.md` do not mention | Launch Gate 0 |
| 5 | **Mid-stream disconnect billing in the ToS** | Business/legal. The proxy bills usage the upstream already generated; that is defensible but must be stated, not emergent (see [`proxy-hot-path-audit.md` §4.5](proxy-hot-path-audit.md)) | Launch Gate 0 |

---

## 12. Suggested execution order

1. Phase 0 — register ([§3](#3-decisions-this-forces-the-register-to-change)). *Blocks everything.*
2. Phases 1–5 — database port and tests. **Self-contained and independently
   shippable**: after Phase 5 the system runs on SQLite with PocketBase still
   present for identity, which is a legitimate intermediate state and de-risks the
   whole plan. **Nothing here depends on the identity decision, so it can start
   immediately** — which is why it is sequenced ahead of the larger phase.
3. Phase 6 — identity (Google + email/password). The largest phase. Unblocked, but
   §11.2 items 1 and 2 need answers first.
4. Phase 7 — admin surface endpoints, then the UI choice ([§7.1](#71-ui-hand-built-admin-page-vs-a-low-code-tool)).
5. Phase 8 — tooling and the docs sweep.

Splitting at step 2 is the main structural improvement over the draft: it turns one
large risky change into two smaller ones, and the first half is verifiable by the
tests that already exist.

**One caveat on that split, found while executing Phase 0.** "Independently shippable"
describes the *result* of Phases 1–5, not every point inside them. Phase 1 swaps the
`sqlx` feature in `Cargo.toml` while Phases 2–4 have not yet ported the queries, so the
tree **does not compile in between**. In a checkout shared with parallel agents that
window is a hazard: anyone else's `cargo check` fails for reasons that look like their
own. Either land Phases 1–5 as one continuous unit, or run them in a separate
`git worktree` and merge once `cargo check` is green. Do not leave the shared tree
mid-port.

**Taken: the worktree.** Phases 1–5 are being executed on branch `sqlite-port` in
`C:/dev/apikita-sqlite-port`, created from `0.0.1` at `0343c49`. `0.0.1` stays green
throughout; the merge happens once `cargo check` passes there.

**A note on what to do first if only one thing is done:** Phase 0. Every document in
the repository reads from the register, and the register currently says the stack is
Postgres and PocketBase. Leaving it stale while the code moves is exactly the failure
mode its own *"How to change a decision"* section warns about — *"a stale decision is
worse than none, because it is followed."*

---

## Appendix A — Target SQLite schema

The complete replacement for `server/migrations/20260925000000_initial_schema.sql`.
Conventions applied throughout, and why:

| Convention | Reason |
| --- | --- |
| **Every table is `STRICT`** | The single most important line in this appendix. Without it `INTEGER` is only *affinity* and a `REAL` lands in a money column silently ([§4.9](#49-strict-tables--make-the-money-rule-structural)). Requires SQLite ≥ 3.37. |
| `TEXT` ids | Postgres `UUID` → `TEXT`. sqlx's `uuid` feature decodes it. Generated by `Uuid::new_v4()` in Rust, so no `gen_random_uuid()`. |
| `INTEGER` for every money and token column | The register's *"never floating point"* rule — now **enforced**, not merely declared, because of `STRICT`. This is also what makes the `::bigint` casts removable: `SUM` over `INTEGER` returns an integer. |
| `TEXT` timestamps with a `GLOB` format `CHECK` | [§4.6](#46-timestamps--the-hazard-that-would-have-shipped). **No `DEFAULT CURRENT_TIMESTAMP` anywhere** — a default that fires writes the wrong format. |
| `TEXT` for JSON | Postgres `JSONB` → `TEXT`; sqlx's `json` feature handles the conversion. |
| `INTEGER PRIMARY KEY` (no `AUTOINCREMENT`) for append-only tables | The schema never deletes from `ledger` or `admin_audit`, so rowids cannot be reused. Plain `INTEGER PRIMARY KEY` is a rowid alias and avoids the extra `sqlite_sequence` write on **every insert** — which matters on the settlement hot path. |
| Explicit `NOT NULL` on every key column | `STRICT` now implies it for PK columns, but being explicit documents intent and survives a table created non-`STRICT` by mistake. |
| `ON DELETE RESTRICT` on money references | Makes a hard delete of a funded account **refused by the database**. Requires `foreign_keys(true)` to mean anything. |
| `COALESCE` unique index on `usage_daily` | Fixes the `NULL`-key upsert duplication (verified). Still required under `STRICT`, because `api_key_id` is deliberately outside the key so it can stay nullable. |

```sql
-- =============================================================================
-- apikita — SQLite schema (replaces the PostgreSQL initial schema)
--
-- PREREQUISITES
--   1. SQLite >= 3.37.0  — every table below is STRICT, which is what makes
--      "money is INTEGER" enforced rather than merely declared.
--   2. PRAGMA foreign_keys = ON on EVERY connection. Without it every ON DELETE
--      clause below is silently inert and the RESTRICT backstop is gone.
--   3. Every timestamp is bound from Rust. Never write time in SQL: SQLite's
--      CURRENT_TIMESTAMP emits a format that does not compare against RFC3339,
--      which silently extends session lifetimes (see the plan, section 4.6).
-- =============================================================================

-- ---------------------------------------------------------------------------
-- Accounts and identity
-- ---------------------------------------------------------------------------

CREATE TABLE accounts (
  id          TEXT PRIMARY KEY,
  -- PocketBase link. STILL PRESENT and NOT NULL, because PocketBase remains the
  -- identity provider through Phases 1-5: auth.rs creates the account with
  -- `INSERT INTO accounts (pb_user_id) ... ON CONFLICT (pb_user_id)` and
  -- account.rs reads it back. Dropping it here would break login, contradicting
  -- the plan's claim that Phases 1-5 ship with identity intact. Phase 6 drops it.
  pb_user_id  TEXT NOT NULL UNIQUE,
  status      TEXT NOT NULL DEFAULT 'active'
              CHECK (status IN ('active','suspended','closed')),
  is_operator INTEGER NOT NULL DEFAULT 0 CHECK (is_operator IN (0,1)),
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at  TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- Replaces PocketBase. One account, many identities (identity.md invariant 1).
-- Additive for now: created empty in Phase 2 and populated in Phase 6, which is
-- when `accounts.pb_user_id` is dropped and accounts.id becomes the only key.
CREATE TABLE identities (
  id             TEXT PRIMARY KEY,
  account_id     TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  provider       TEXT NOT NULL CHECK (provider IN ('google','password')),
  subject        TEXT NOT NULL,   -- provider's stable subject id
  email          TEXT NOT NULL,
  email_verified INTEGER NOT NULL DEFAULT 0 CHECK (email_verified IN (0,1)),
  password_hash  TEXT,            -- Argon2id; NULL for OAuth-only identities
  created_at     TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at     TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00'),

  UNIQUE (provider, subject),

  -- A password identity must carry a hash; an OAuth identity must not.
  CHECK ((provider = 'password') = (password_hash IS NOT NULL)),

  -- Google guarantees a verified address (identity.md:100). Encode that so an
  -- unverified Google identity cannot exist, rather than trusting the callback.
  CHECK (provider <> 'google' OR email_verified = 1)
) STRICT;

CREATE INDEX identities_account_idx ON identities (account_id);
CREATE UNIQUE INDEX identities_provider_email_uniq ON identities (provider, email);

-- ---------------------------------------------------------------------------
-- Money
-- ---------------------------------------------------------------------------

CREATE TABLE wallets (
  account_id  TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE RESTRICT,
  balance_idr INTEGER NOT NULL DEFAULT 0 CHECK (balance_idr >= 0),
  updated_at  TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- Append-only. Never UPDATE, never DELETE. Corrections are new rows.
CREATE TABLE ledger (
  id            INTEGER PRIMARY KEY,
  account_id    TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  delta_idr     INTEGER NOT NULL,
  reason        TEXT NOT NULL
                CHECK (reason IN ('topup','usage','adjustment','refund')),
  ref           TEXT,
  balance_after INTEGER NOT NULL,
  created_at    TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX ledger_account_created_idx ON ledger (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Sessions
-- ---------------------------------------------------------------------------

CREATE TABLE sessions (
  id           TEXT PRIMARY KEY,
  account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  token_hash   TEXT NOT NULL UNIQUE,
  expires_at   TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00'),
  -- NEW. The register specifies "30d absolute, 7d idle" but the Postgres schema
  -- had no column for the idle bound, so only the absolute half was enforceable
  -- (auth.rs:255 notes this). Adding it now, while the schema is being rewritten.
  last_seen_at TEXT NOT NULL CHECK (last_seen_at GLOB '????-??-??T??:??:??*+00:00'),
  revoked_at   TEXT CHECK (revoked_at IS NULL
                           OR revoked_at GLOB '????-??-??T??:??:??*+00:00'),
  user_agent   TEXT,
  ip_hash      TEXT,
  created_at   TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX sessions_account_idx ON sessions (account_id) WHERE revoked_at IS NULL;
CREATE INDEX sessions_expires_idx ON sessions (expires_at);

-- ---------------------------------------------------------------------------
-- API keys
-- ---------------------------------------------------------------------------

CREATE TABLE api_keys (
  id              TEXT PRIMARY KEY,
  account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  key_hash        TEXT NOT NULL UNIQUE,
  prefix          TEXT NOT NULL,
  label           TEXT,
  models          TEXT NOT NULL DEFAULT '[]',
  spend_limit_idr INTEGER NOT NULL DEFAULT 0,
  token_limit     INTEGER NOT NULL DEFAULT 0,
  rate_limit_rpm  INTEGER NOT NULL DEFAULT 0,
  expires_at      TEXT CHECK (expires_at IS NULL
                              OR expires_at GLOB '????-??-??T??:??:??*+00:00'),
  last_used_at    TEXT CHECK (last_used_at IS NULL
                              OR last_used_at GLOB '????-??-??T??:??:??*+00:00'),
  revoked_at      TEXT CHECK (revoked_at IS NULL
                              OR revoked_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at      TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX api_keys_hash_idx ON api_keys (key_hash) WHERE revoked_at IS NULL;
CREATE INDEX api_keys_account_idx ON api_keys (account_id);

-- ---------------------------------------------------------------------------
-- Top-ups
-- ---------------------------------------------------------------------------

CREATE TABLE topups (
  id          TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  amount_idr  INTEGER NOT NULL CHECK (amount_idr > 0),
  order_id    TEXT NOT NULL UNIQUE,     -- idempotency, enforced by the DB
  status      TEXT NOT NULL DEFAULT 'pending'
              CHECK (status IN ('pending','settled','denied','expired','refunded')),
  snap_token  TEXT,
  -- Which payment rail funded this top-up. It decides the payout method at
  -- wind-down: a Midtrans customer is paid back by bank transfer, anyone else in
  -- USD stablecoin. Deliberately NOT NULL with NO DEFAULT -- a default of
  -- 'midtrans' would silently mislabel a row written by a path that forgot to
  -- name its rail, whereas this fails loudly. The value set is FROZEN: SQLite
  -- has no ALTER TABLE ... ADD CONSTRAINT, so widening a CHECK means a 12-step
  -- table rebuild. Only the two rails that exist may appear here.
  rail        TEXT NOT NULL CHECK (rail IN ('midtrans','crypto')),
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  settled_at  TEXT CHECK (settled_at IS NULL
                          OR settled_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX topups_account_created_idx ON topups (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Usage
-- ---------------------------------------------------------------------------

-- NO PRIMARY KEY. A COALESCE expression cannot appear in a PK, and the
-- NULL-key duplication fix requires it (section 4.3, trap 3). The unique index
-- below IS the key, and the upsert must target it by name.
--
-- api_key_id is RESTRICT, not SET NULL. Measured: SET NULL collides with the
-- COALESCE unique index the moment a NULL-keyed row already exists for the same
-- (account_id, day) -- "UNIQUE constraint failed: usage_daily_scope_uniq". RESTRICT
-- also matches wallets/ledger/topups and enforces "no hard deletes" for a key that
-- has billing history. See section 4.10.
CREATE TABLE usage_daily (
  account_id        TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  api_key_id        TEXT REFERENCES api_keys(id) ON DELETE RESTRICT,
  day               TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cost_idr          INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE UNIQUE INDEX usage_daily_scope_uniq
  ON usage_daily (account_id, day, COALESCE(api_key_id, ''));
CREATE INDEX usage_daily_account_day_idx ON usage_daily (account_id, day DESC);

-- Per-request rows, for the admin "recent usage" feed. Never stores prompt or
-- completion text (Launch Gate 4). Retention: 90 days, swept (§11.1 item 8).
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
  created_at        TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX usage_events_recent_idx ON usage_events (created_at DESC);
CREATE INDEX usage_events_account_idx ON usage_events (account_id, created_at DESC);

-- ---------------------------------------------------------------------------
-- Telegram
-- ---------------------------------------------------------------------------

CREATE TABLE link_codes (
  code       TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  expires_at TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00'),
  used_at    TEXT CHECK (used_at IS NULL
                         OR used_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE TABLE telegram_links (
  telegram_id TEXT PRIMARY KEY,
  account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  linked_at   TEXT NOT NULL CHECK (linked_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX telegram_links_account_idx ON telegram_links (account_id);

-- ---------------------------------------------------------------------------
-- Reviews
-- ---------------------------------------------------------------------------

CREATE TABLE reviews (
  id           TEXT PRIMARY KEY,
  account_id   TEXT REFERENCES accounts(id) ON DELETE SET NULL,
  telegram_id  TEXT,
  rating       INTEGER NOT NULL CHECK (rating BETWEEN 1 AND 5),
  body         TEXT CHECK (length(body) <= 1000),
  is_customer  INTEGER NOT NULL DEFAULT 0 CHECK (is_customer IN (0,1)),
  withdrawn_at TEXT CHECK (withdrawn_at IS NULL
                           OR withdrawn_at GLOB '????-??-??T??:??:??*+00:00'),
  created_at   TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00'),
  updated_at   TEXT NOT NULL CHECK (updated_at GLOB '????-??-??T??:??:??*+00:00'),

  CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)
) STRICT;

CREATE UNIQUE INDEX reviews_account_uniq ON reviews (account_id)
  WHERE account_id IS NOT NULL;
CREATE UNIQUE INDEX reviews_telegram_uniq ON reviews (telegram_id)
  WHERE account_id IS NULL;

CREATE TABLE review_history (
  id          INTEGER PRIMARY KEY,
  review_id   TEXT NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
  rating      INTEGER NOT NULL,
  body        TEXT,
  replaced_at TEXT NOT NULL CHECK (replaced_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE TABLE review_sessions (
  telegram_id TEXT PRIMARY KEY,
  step        TEXT NOT NULL,
  rating      INTEGER,
  body        TEXT,
  editing     INTEGER NOT NULL DEFAULT 0 CHECK (editing IN (0,1)),
  expires_at  TEXT NOT NULL CHECK (expires_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

-- ---------------------------------------------------------------------------
-- Admin audit
-- ---------------------------------------------------------------------------

-- Append-only. Written in the SAME transaction as the effect it records.
CREATE TABLE admin_audit (
  id          INTEGER PRIMARY KEY,
  operator_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE RESTRICT,
  action      TEXT NOT NULL,
  target_type TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  detail      TEXT,
  created_at  TEXT NOT NULL CHECK (created_at GLOB '????-??-??T??:??:??*+00:00')
) STRICT;

CREATE INDEX admin_audit_operator_idx ON admin_audit (operator_id, created_at DESC);
CREATE INDEX admin_audit_target_idx   ON admin_audit (target_type, target_id);

-- ---------------------------------------------------------------------------
-- Abuse signals (salted IP hashes only; no raw IP is ever stored)
-- ---------------------------------------------------------------------------

CREATE TABLE key_ip_daily (
  api_key_id    TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day           TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  distinct_ips  INTEGER NOT NULL DEFAULT 0,
  request_count INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (api_key_id, day)
) STRICT;

CREATE TABLE key_ip_seen (
  api_key_id TEXT NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  day        TEXT NOT NULL CHECK (day GLOB '????-??-??'),
  ip_hash    TEXT NOT NULL,
  PRIMARY KEY (api_key_id, day, ip_hash)
) STRICT;
```

**Three notes on this schema that the phase must not lose:**

1. **`usage_daily` has no `PRIMARY KEY`, deliberately.** The `COALESCE` unique index
   *is* the key, and `db.rs:466`'s upsert must be retargeted to it. Leaving the
   original `ON CONFLICT (account_id, api_key_id, day)` would be a syntax-valid,
   silently-wrong statement.
2. **`identities` encodes the Google-verified rule as a constraint.** `identity.md`
   asks for a hook and an audit; a `CHECK` is stronger and free. It does not replace
   the hook for the *password* provider, where provenance genuinely cannot be expressed
   in SQL — that stays application-level.
3. **`sessions.last_seen_at` is new and required.** Without it the register's *"7 days
   idle"* half is unimplementable, which is a pre-existing gap this migration is the
   right moment to close. Adding it means `auth.rs` must touch the row on use, which
   is a write on the session path — measure that against the single-writer ceiling.
