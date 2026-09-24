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
| Build | `cargo build` | Yes |
| Unit tests | Pure logic | Yes |
| **Integration tests** | A real Postgres, real schema, fake upstream | **Yes** |
| Schema check | Migrations apply cleanly to an empty database | Yes |
| Secret scan | No key or token committed | Yes |

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

Ordinary unit tests do not protect money. These do, and they need a real database:

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

## Real Postgres in CI, not SQLite

**Do not substitute SQLite for testing.** The schema depends on Postgres behaviour:

| Feature used | SQLite difference |
| --- | --- |
| `BIGINT` and `CHECK` constraints | Weakly enforced |
| Partial unique indexes | Different semantics |
| `FOR UPDATE` row locking | **Not equivalent** — the idempotency test would pass while production races |
| `JSONB` | Different type entirely |

**The `FOR UPDATE` row is the one that matters.** Concurrent-webhook idempotency
depends on real Postgres locking; a SQLite test would give false confidence about
the exact scenario that duplicates money.

CI runs Postgres as a service container with the same major version as production.

## Schema validation in CI

Two checks, both cheap:

1. **Migrations apply to an empty database** without error.
2. **The resulting schema matches the documented schema.** The documents in
   [`website/02-data-model.md`](website/02-data-model.md) are the source; drift means one is wrong.

**A drift check is worth the effort here** because the schema is documented before it
is implemented (see [`admin-surface.md`](admin-surface.md)) — nothing else would catch the
documents and migrations diverging.

## Migration safety gate

CI should **reject** a migration that:

| Pattern | Why |
| --- | --- |
| `DROP COLUMN` / `DROP TABLE` | Destructive in one step; see expand/contract |
| `ALTER COLUMN ... SET NOT NULL` on an existing column | Breaks the running server |
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
| `DATABASE_URL` (production) | CI secret store, **gated to the `main` branch** |
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

**CI minutes are near-zero for this project.** A Rust build with caching plus a
Postgres service container fits any free tier comfortably.

| Consideration | Note |
| --- | --- |
| `cargo` build caching | The difference between 2 minutes and 15 |
| Postgres service container | Included |
| Frontend build | Static output; seconds |

Keep the free tier in mind when choosing a provider — see
[`cost-and-sizing.md`](cost-and-sizing.md).

## Open items

- [x] CI provider: **GitHub Actions** — [`decisions.md`](decisions.md).
- [ ] Whether a staging environment exists, or CI deploys straight to production.
- [x] Migration tooling: **sqlx migrate** — [`decisions.md`](decisions.md).
- [ ] Whether the schema-drift check is implemented now or after first migration.
- [ ] Cache configuration for build times.