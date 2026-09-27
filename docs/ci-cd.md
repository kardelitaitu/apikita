# CI/CD Pipeline

The automated checks between a commit and a deploy. Referenced as an open item in
[`deployment.md`](deployment.md) and [`local-development.md`](local-development.md); this is the
pipeline itself.

> Deploy order and migrations: [`deployment.md`](deployment.md)
> Local test setup: [`local-development.md`](local-development.md)

## What the pipeline must guarantee

1. **Nothing reaches production without passing tests.** The system holds customer
   money; a bad commit is a billing bug.
2. **Migrations run before the server, never after.** See `deployment.md`.
3. **A failure stops the sequence.** Deploying a frontend that expects an endpoint
   the backend never deployed is a silent outage.
4. **It is fast enough to run on every push.** A 20-minute pipeline gets bypassed.

## Stages

### On every pull request

| Stage | Purpose | Blocks merge |
| --- | --- | --- |
| Format check | `cargo fmt --check` | Yes |
| Lint | `cargo clippy -- -D warnings` | Yes |
| **Dependency audit** | Known CVEs in the lockfile | **Yes** |
| Build | `cargo build` | Yes |
| Unit tests | Pure logic | Yes |
| **Integration tests** | A real database (bundled SQLite), real schema, fake upstream | **Yes** |
| Schema check | Migrations apply cleanly to an empty database | Yes |
| **Server image build + smoke** | The deployable artifact builds AND serves | **Yes** |
| Website typecheck / tests / build | The frontend toolchain | Yes |
| Secret scan | No key or token committed | Yes |

**The dependency audit is the only stage that looks outside the repository.** Every
other check can pass unchanged while a crate this project depends on is found to
be vulnerable, because nothing in the source changes when an advisory is
published. It runs `cargo audit`, which reads
[`server/.cargo/audit.toml`](../server/.cargo/audit.toml).

**Exactly one advisory is excepted today, and the exception lives in that file
rather than in this pipeline.** `RUSTSEC-2023-0071` (`rsa` 0.9.10, the Marvin
timing attack) has **no fixed version** — it cannot be resolved by upgrading — and
the crate is not linked into the service: it enters the lockfile through
`sqlx-mysql`, a driver this project does not enable, and is absent from both the
built dependency graph and the binary. The file records that reachability
evidence, plus the conditions under which the exception must be re-opened.

**Why in the repository and not an `--ignore` flag here:** a flag in this file is
invisible to a reader looking at the code, and applies to every future advisory
with the same id — including one that *is* reachable. An exception that requires
a commit is an exception somebody has to justify.

**Why the audit is not run with `--deny warnings`:** "unmaintained" is a warning
class, not a vulnerability, and a gate that fails on it gets muted. A muted gate
is worse than no gate.

**There is no service container and no `DATABASE_URL` in CI.** SQLite is bundled, so
the database is a file and the test suite needs no environment at all — see
[the integration section](#bundled-sqlite-in-ci-and-why-the-old-objection-is-gone).

### On merge to `main`

| Stage | Purpose |
| --- | --- |
| Everything above, again | `main` must be independently verified |
| **Build the release artifact** | The exact binary that will run |
| **Snapshot the database** | Before migration — see [`backup-and-restore.md`](backup-and-restore.md) |
| **Run migrations** | Forward-only, against production |
| **Deploy the server** | Northflank |
| **Health check** | `/health` must return OK |
| **Deploy the frontend** | Cloudflare Pages |
| Smoke test | Login and balance endpoint reachable |

**This is the order from `deployment.md`, and the order is the point.**

## The integration tests that matter

Ordinary unit tests do not protect money. These do, and they need a real database —
which, since the port, is a migrated temp **file** each test builds for itself
(`server/src/test_support.rs`), not a server somebody has to start:

| Test | Asserts |
| --- | --- |
| Top-up credits exactly once | A replayed webhook does not double-credit |
| Bad webhook signature rejected | No balance change, no status change |
| Amount mismatch rejected | Crediting uses the stored amount, not the payload |
| **Ledger sum equals wallet balance** | The reconciliation query returns zero rows |
| Concurrent webhooks, same `order_id` | Exactly one credit |
| Debit is atomic with the ledger row | A crash cannot leave them inconsistent |
| **Cache-read tokens are not billed as input** | The ~50x pricing error |
| Key spend limit blocks | 402 once exhausted |
| Model allowlist blocks | 403 for a disallowed model |
| **Mid-stream upstream failure** | Error emitted, no duplicate answer, billing correct |

**The reconciliation assertion is the highest-value test in the suite.** One query,
and it catches the entire class of balance bugs.

**Two of these need the fake upstream to fail deliberately** — see
[`local-development.md`](local-development.md). A fake that only succeeds tests nothing.

## Bundled SQLite in CI, and why the old objection is gone

> **This section used to say the opposite** — "Real Postgres in CI, not SQLite /
> **Do not substitute SQLite for testing**" — on the grounds that the schema
> depended on Postgres behaviour, and that a SQLite test of concurrent-webhook
> idempotency would "give false confidence about the exact scenario that duplicates
> money". **That premise has been superseded by design, and the suite now settles it
> empirically.** The old text is kept here as the record of what changed; the port
> itself is in [`plans/sqlite-migration.md`](plans/sqlite-migration.md).

The old argument rested on Postgres features. Each was replaced rather than faked:

| Then (Postgres) | Now (SQLite) | Consequence for the tests |
| --- | --- | --- |
| `SELECT ... FOR UPDATE` row locking | `BEGIN IMMEDIATE` + a conditional-`UPDATE` claim | The claim is a real serialization point in SQLite too, so the idempotency test exercises the production mechanism instead of approximating it |
| Partial unique indexes | unchanged — SQLite has them | No difference to test |
| `JSONB` | `TEXT` holding JSON | No difference to test |
| `BIGINT` + `CHECK` | `STRICT` tables + `CHECK` | `STRICT` makes "money is INTEGER" **enforced**, not merely declared |

**The specific objection was that the concurrency test would pass while production
raced.** That is now measured, not assumed: the ported test
`concurrent_requests_cannot_overdraw_a_one_request_balance` is the only real
concurrency proof of the overdraw fix, it used to be ignored — `#[ignore = "requires
live Postgres"]`, i.e. it never ran in CI — and it now runs by default against a
migrated temp file with the **production** pragmas (`journal_mode=wal`,
`foreign_keys=ON`, `busy_timeout`, `synchronous`), because `TestDb` hands its file
to `db::init_pool` rather than building a test-only pool.

So CI runs **bundled SQLite**, and the database is a file — no `services:`, no
`DATABASE_URL` for the tests:

```yaml
- name: Apply migrations to an empty database
  working-directory: server
  env:
    DATABASE_URL: sqlite://ci-schema-check.db
  run: |
    rm -f ci-schema-check.db ci-schema-check.db-wal ci-schema-check.db-shm
    cargo run --bin migrate
```

**There is no longer an ignored database tier.** `cargo test` is the whole suite:
measured on the merged tree, `cargo test --lib` reports **289 passed / 0 failed /
2 ignored**, where the previous arrangement reported 200 passed / 75 ignored and
needed a live Postgres to run the difference. The 2 remaining ignores are **not**
database tests — they need a *configured* PocketBase (collections, not just a
running container), which CI does not provide. CI names them and skips them, so the
exclusion is explicit rather than hidden behind a bare `#[ignore]`:

```yaml
- name: Integration tests (bundled SQLite)
  working-directory: server
  run: >-
    cargo test --lib
    -- --skip a_new_login_after_suspend_is_still_refused
    --skip live_exchange_token_returns_the_real_balance_and_stores_only_the_hash
```

**Why skipping by name is right and not a dodge:** both tests drive a real PocketBase
auth exchange and have no seam to fake, and identity is still PocketBase — migration
Phases 6 and 7 are not done. When Phase 6 lands, those two become ordinary tests and
the skip flags come out.

## Schema validation in CI

Two checks, both cheap:

1. **Migrations apply to an empty database** without error. CI deletes the file
   first, so "empty" is literal rather than assumed, then runs `cargo run --bin
   migrate`, which applies the whole `./migrations` set and exits non-zero unless
   `journal_mode` is `wal` and `foreign_keys` is on.
2. **The resulting schema matches the documented schema.** The **source of truth is
   now the migration itself** —
   [`server/migrations/20260925000000_initial_schema.sql`](../server/migrations/20260925000000_initial_schema.sql)
   — and
   [`tools/sqlite-probes/validate-migration-schema.py`](../tools/sqlite-probes/README.md)
   parses it. [`website/02-data-model.md`](website/02-data-model.md) is the
   **historical Postgres design**, kept for the reasoning, and is deliberately not
   the drift target: it is no longer the shipped DDL.

**A drift check is worth the effort here** because the schema was documented before it
was implemented (see [`admin-surface.md`](admin-surface.md)) — nothing else catches the
migrations and their documentation diverging. One such divergence has already
happened (a second copy of the DDL went stale, missing `is_operator`), which is why
there is exactly one copy now.

## Migration safety gate

CI should **reject** a migration that:

| Pattern | Why |
| --- | --- |
| `DROP COLUMN` / `DROP TABLE` | Destructive in one step; see expand/contract |
| `ALTER COLUMN ... SET NOT NULL` on an existing column | Breaks the running server (SQLite cannot do it in place at all — it needs a table rebuild) |
| `RENAME` | Breaks the running server |
| Missing backfill for a new `NOT NULL` column | Fails on a populated table |

**A mechanical check is better than discipline.** The rule is easy to state and easy
to forget at 11pm.

## Where migrations run

**Decision: migrations run in CI, after the pre-migration snapshot, before the server
deploy.**

| Option | Verdict |
| --- | --- |
| CI runs them | **Chosen** — ordered with the deploy, auditable |
| Application runs them on boot | Rejected — multiple instances race, and a failed boot leaves a half-migrated schema |
| Manual | Rejected — depends on someone remembering |

**Never let the application auto-migrate on startup.** With more than one instance
they race, and a rollback becomes ambiguous.

## Secrets

| Secret | Where |
| --- | --- |
| Deploy credentials | CI secret store |
| `DATABASE_URL` (production) | CI secret store, **gated to the `main` branch**. It is a `sqlite://<path>` **file path**, not a credential — but it names the volume the ledger lives on, so it stays out of PRs |
| Provider/API keys | **Never in CI** — runtime on Northflank only |

- **Pull requests from forks must not receive secrets.** Fork PRs run tests with no
  credentials, against throwaway infrastructure.
- **Never echo secrets** in logs; CI logs are widely readable.
- A secret scan on every push catches the mistake before it reaches history.

## What CI should NOT do

| Not | Why |
| --- | --- |
| Deploy on every branch | Only `main` deploys |
| Run migrations on PRs against production | Obviously |
| Publish artifacts publicly | The binary is not open source |
| Gate on coverage percentage | Vanity metric; gate on the money tests instead |
| Retry flaky tests silently | A flaky money test is a bug in the test or the code |

**The last row matters more than it looks.** A test that passes on retry teaches
everyone to re-run rather than investigate.
## Coverage: measured, not gated

The table above deliberately does NOT gate on a coverage percentage, and that
decision stands — a percentage is not evidence that the money paths are tested.
But "do not GATE on it" is not "do not MEASURE it": an unmeasured number cannot
tell you where the untested code is, and the one time this was measured it found
the worst-covered file in the crate was the **login handler**.

```bash
cd server && cargo llvm-cov --lib --lcov --output-path coverage/lcov.info
```

**Baseline, measured 2026-09-27 (`--lib`; branches were not captured by this run):**

| Metric | Value |
| --- | --- |
| Total line coverage | **95.20%** (14,348 / 15,071) |
| `money.rs`, `error.rs` | **100%** |
| Files below 90% | `routes/events.rs` 86.2%, `routes/admin.rs` 86.6%, `routes/proxy.rs` 89.9% |

**The lesson from the first measurement, kept because it is the argument for
measuring at all:** `routes/auth.rs` was at **58.6%**, by far the worst in the
repo, and the uncovered block was the *entire* `exchange_token` handler. Nothing
flagged it — the file looked like every other file, the suite was green, and the
only test that touched the handler was `#[ignore]`d behind a live PocketBase. It
is now at **98.2%** via a loopback stub, with the ignore removed.

**The three files still below 90% are the honest worklist**, in that order.


## Rollback

| Failure point | Response |
| --- | --- |
| Tests fail | Nothing deployed. Fix and push |
| Migration fails | Nothing deployed. Safe |
| Server deploy fails after migration | Old server + new schema. **Why migrations must be additive** |
| Health check fails | Roll back the server deploy; the schema can stay |
| Frontend deploy fails | New API + old UI; tolerable if the API is backward compatible |
| Bad migration shipped | **Restore from the snapshot**; do not hand-write a reverse migration |

See [`deployment.md`](deployment.md) and [`backup-and-restore.md`](backup-and-restore.md).

## Cost

**CI minutes are near-zero for this project.** A Rust build with caching fits any
free tier comfortably — and the port removed the last piece of infrastructure the
pipeline had to stand up.

| Consideration | Note |
| --- | --- |
| `cargo` build caching | The difference between 2 minutes and 15 |
| Database service container | **Gone** — SQLite is bundled, so CI starts no second process |
| Frontend build | Static output; seconds |

Keep the free tier in mind when choosing a provider — see
[`cost-and-sizing.md`](cost-and-sizing.md).

## Open items

- [x] CI provider: **GitHub Actions** — [`decisions.md`](decisions.md).
- [ ] Whether a staging environment exists, or CI deploys straight to production.
- [x] Migration tooling: **sqlx migrate** — [`decisions.md`](decisions.md).
- [ ] Whether the schema-drift check runs as a CI step or stays a local probe —
  `tools/sqlite-probes/validate-migration-schema.py` exists and parses the
  migration, but no workflow step invokes it yet.
- [ ] Cache configuration for build times.