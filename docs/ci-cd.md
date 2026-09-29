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

**Every stage below blocks the merge.** The workflow is the source of truth, and a
test (`tools/ci-docs-check/`) fails if a step exists that this document does not name
— which is what keeps it true. The list was a third short before that check existed.

Four steps **prepare a toolchain and assert nothing**, so they are not stages: a
failure in one is a broken runner rather than a defect in the change. They are listed
anyway, so the check below can demand an exact match:

| Setup step | Provides |
| --- | --- |
| Install Rust (rustfmt + clippy) | The toolchain the format and lint stages need |
| Install Node | The toolchain the website stages need |
| Install cargo-audit | The dependency-audit binary |
| Install website dependencies | `node_modules` |

#### Correctness of the code

| Stage | Purpose |
| --- | --- |
| Format check | `cargo fmt --check` |
| Lint | `cargo clippy -- -D warnings` |
| **Dependency audit** | Known CVEs in the lockfile |
| Build | `cargo build` |
| Unit tests | Pure logic |
| **Integration tests** | A real database (bundled SQLite), real schema, fake upstream |

#### Correctness of the DATABASE

| Stage | Purpose |
| --- | --- |
| Apply migrations to an empty database | Migrations apply cleanly, in order, to a database that is empty *literally* (deleted first) |
| Validate the schema against the plan | The shipped schema still matches the plan's Appendix A, **and** 32 invariant probes pass against it |

#### The deployable artifacts actually work

| Stage | Purpose |
| --- | --- |
| Build the server image | The deployable artifact builds |
| Smoke the server image against a migrated database | It serves a real request against a real schema, **and** the booted binary answers its whole route table as documented — every protected route refusing anonymously, the DESIGNED-NOT-BUILT routes 404ing, the bot-token endpoint refusing **identically** for well-formed and malformed input, and **every other mounted route** answering its documented status — including the two `logout` verbs, which are *intentionally* anonymous and idempotent. It also asserts the **error SHAPE** on 404/400/401 paths, because `error-model.md:10` promises JSON with `code` + `request_id` — and the router-level 404s and body-parse failures are exactly where a framework default breaks that |
| Smoke the maintenance scheduler image | The nightly jobs run for real in the image that ships - retention, hold-sweep, **and the two alert jobs**, each given the SAME mounts `docker-compose.yml` gives them. The alert jobs are asserted to reach a **verdict** rather than crash on a missing mount, to **name the absent channel** instead of implying the stack is monitored, and to **skip `api_down` explicitly** when no API URL is set - because `probe.sh` would otherwise default to `127.0.0.1`, which inside a container is its own loopback, and poll it for two minutes on every scheduled run |
| Typecheck (website) | `tsc --noEmit` |
| Website contract tests | The frontend's own suites |
| Build website | Every page builds |

#### The contracts that fail SILENTLY

These seven are CI stages rather than local scripts because each guards a failure that
produces **no error**: the thing looks like it works and does not. A local script only
helps someone who already suspects a problem.

| Stage | Guards against | Silently wrong because |
| --- | --- | --- |
| Check the edge relay streams SSE | A buffering relay | The UI answers 200 and stops updating |
| Check the compose deployment definition | A definition that parses but is wrong | `working_dir` silently misresolves the money gate's DSN |
| Check the backup contract | An offsite hook that copies nothing | The script prints "hook succeeded" and exits 0 |
| Check the reconciliation gate | A money gate that cannot fail | A drift check that never runs still looks like a green tick |
| Check the restore drill | Deletion of a live-looking target | Teardown deletes whatever it was pointed at |
| Check the alert delivery contract | A failed delivery that silences its own retry | Alerts stop arriving and nothing says so |
| **Check the CI documentation** | This table going stale | An understated pipeline sends people around CI |
| Secret scan | A committed key, and an inlined `PUBLIC_*` secret | The value ships to every visitor |

Each has a README beside it in `tools/` explaining what it asserts and which mutations it
was tested against.


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
measured on the merged tree, `cargo test --lib` reports **444 passed / 0 failed /
0 ignored**, where the previous arrangement reported 200 passed / 75 ignored and
needed a live Postgres to run the difference.

**There is nothing left to skip.** Both former `#[ignore]`s are gone: the
live-PocketBase exchange test was replaced by a loopback stub, so the whole library
suite — including the auth path — runs by default with no service container. The CI
step is a plain `cargo test --lib`:

```yaml
- name: Integration tests (bundled SQLite)
  working-directory: server
  run: cargo test --lib
```

Identity still enters through PocketBase at runtime — migration Phases 6 and 7 are
not done, so a deployed server requires a live `POCKETBASE_URL`. But no *test* needs
one any more, which is why the exclusion flags could be deleted rather than named.

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

**Baseline, measured 2026-09-27 (`--lib`; this toolchain does not instrument
branches, so the branch columns read 0/0):**

| Metric | Value |
| --- | --- |
| Total line coverage | **96.64%** (14,926 / 15,445) |
| `money.rs`, `error.rs` | 100% |
| `ip_tracking.rs` | 99.8% |
| `routes/admin.rs` | 98.7% (was 86.6%) |
| `routes/auth.rs` | 98.3% (was 58.6%) |
| `routes/events.rs` | **98.1%** (was 86.2%) |
| `routes/proxy.rs` | 91.8% (was 89.9%) — now the LOWEST in the crate |
| **Files below 90%** | **NONE** (was 4) |
| `#[ignore]`d tests | **0** (was 2) |

**Every file is above 90%, and the lowest is 91.8%.** That is the end of the
worklist this section opened with — and it is worth recording how it got there,
because none of the four initial offenders was found by looking at a number.

The starting set was `auth.rs` 58.6%, `events.rs` 86.2%, `admin.rs` 86.6% and
`proxy.rs` 89.9%. What each measurement actually revealed:

- **`auth.rs`** — the *entire* `exchange_token` handler was uncovered, hidden behind
  an `#[ignore]`d live-PocketBase test. Nothing flagged it: the file looked like
  every other file. Now 98.3% via a loopback stub, with the ignore gone.
- **`proxy.rs`** — not scaffolding but `MeteredStream`, the wrapper that decides
  whether a customer is **billed** and whether a key is **cooled down**, including a
  client-hangup path whose own comment records that dropping the body unread "is
  exactly the defect this is fixing". That fix had no test.
- **`admin.rs`** — test scaffolding for the crate's **last `#[ignore]`**, which
  proved a suspended account cannot log back in. A security assertion that had never
  once run in CI.
- **`events.rs`** — the whole `sse_events_handler`: the connection cap, the replay
  vs snapshot choice, the stream deadline, and cross-account isolation on **both**
  the live and replay paths.

**The method, stated once so it is reusable: read the uncovered RANGES, not the
percentages.** A percentage tells you a file is worth opening; only the ranges tell
you whether the gap is in logic that matters or in test scaffolding. Every one of
the four above turned out to be the former, and every one was described in the
source comment as load-bearing.

**The lesson from the first measurement, kept because it is the argument for
measuring at all:** `routes/auth.rs` was at **58.6%**, by far the worst in the
repo, and the uncovered block was the *entire* `exchange_token` handler. Nothing
flagged it — the file looked like every other file, the suite was green, and the
only test that touched the handler was `#[ignore]`d behind a live PocketBase. It
is now at **98.2%** via a loopback stub, with the ignore removed.

### A percentage cannot see unreachable code — coverage can

Re-measured 2026-09-29, the question being: **can a test exist for a function no
request ever calls?** The wiring guards in `config.rs` cannot answer it, because they
see a name *referenced*, not a reference *reachable*. `max_context_tokens` read as
wired for exactly this reason — its only reader was a second
`worst_case_reservation_idr` on `UpstreamClient` that nothing outside its own tests
called, and that carried six green tests of its own.

The same run, after that function and the fields only it read were deleted:

| Metric | 2026-09-27 | 2026-09-29 (first run) | 2026-09-29 (re-measured) |
| --- | ---: | ---: | ---: |
| Region coverage | — | 97.07% | **97.07%** |
| Line coverage | 96.64% | 98.02% | **98.00%** |
| Function coverage | — | 94.36% | **94.23%** (104 of 1,801 not entered) |
| **Named functions never entered by any test** | not measured | 0 of 1,841 | **0 of 1,869** |
| Tests passing | 461 | 468 | **470** |

**Zero, still.** Every function in the crate that can be called by name is entered by
at least one test. The 104 unentered "functions" are all **closures**, which llvm-cov
records under their line number rather than a name.

The re-measurement is here because the numbers MOVE as the crate does, and a table
that reads as current when it is three changes old is a stale claim wearing a fresh
date. Line coverage fell by 0.02 points and function coverage by 0.13 across this
round's work - not a regression, just the cost of adding code and tests together. The
row that is supposed to be true is still true, and it is the only row here that is a
claim about the code rather than a measurement of it.

That last row is the one to keep, and **the trap is worth naming**: `config.rs`
reports **62.77% function coverage** and looks like the worst-covered file in the
crate by a distance — it is 35 closures, not 35 functions. Reading the per-file
function column without separating closures from named functions sends you to
investigate a file that has nothing wrong with it. The named-function figure is the
one that means something, and it is a different question from the percentage: a
percentage says how much of a file ran, and this says whether anything is
**unreachable**.

```bash
cd server
cargo llvm-cov --lib --no-clean                      # region/line/function table
cargo llvm-cov report --lcov --output-path cov.lcov   # per-function FN/FNDA records
# then: report every FNDA whose count is 0, ignoring names that are bare line numbers
```

Still not gated, and the reason is unchanged: a threshold would fail on a file of
test scaffolding and pass on a file whose untested block is a money path. What a
threshold *cannot* express is the question this section now answers by hand.

**And coverage is not sufficient either — it answers a narrower question.**
"Entered by a test" is not "reached by production": the deleted
`UpstreamClient::worst_case_reservation_idr` was entered by six of its own tests and
called by no request. Coverage narrows the field; it does not close it.

**The same class of drift is still open in SOURCE COMMENTS, and the scope is
measured rather than guessed.** The check above covers the fourteen documents an
operator acts on. It does not cover the Rust, which cites documents by line in
**172 places** — `docs/error-model.md:N` alone accounts for about seventy. Five were
found wrong in a single pass: `docs/observability.md:104` and `:106` in `health.rs`,
`proxy.rs`, `client.rs` and `db.rs` all pointed a reader at a DIFFERENT ALERT, because
that table gained a row and the citations did not move. They had drifted because an
earlier change in this same series edited the table. A sixth, in `health.rs`, cited
`error-model.md:168` for the "never echo the driver's detail" rule, which is not at 168
at all — 168 is the streaming-error section, and the rule lives twenty lines earlier.

**ALL 172 RESOLVE TO A NAMED SECTION, and the obvious move was rejected anyway.**
Finding the nearest preceding heading turns every one of them into a section name that
cannot drift, and a script did exactly that: 167 rewritten, 5 correctly skipped where
the "heading" was prose from a code block. It was reverted, for a reason that matters
more than the diff size.

**A WRONG LINE NUMBER IS AUDITABLE. A WRONG SECTION NAME IS NOT.** The
`error-model.md:168` citation above was already wrong. Converting it mechanically would
have produced `error-model.md ("Streaming errors")` — a wrong citation wearing the
authority of a heading, with nothing left that invites a reader to check it. The number
was wrong, but it was *visibly* wrong to anyone who looked, and that visibility is what
let this pass find it. A blanket conversion removes the audit trail from 172 places in
one commit, and the mistakes it bakes in are the ones nobody re-checks.

So the citations are fixed by hand, where each one can be READ and confirmed — which is
also what the fourteen-document check asks for. That is slower, and it is the point: the
check exists to keep a comment honest, and the fastest way to make a comment dishonest
is to rewrite it without reading it.

**AND THE RATE, MEASURED ON THE LARGEST CLUSTER, IS WHY.** Twenty-three source
comments cite the JSON example or the field table in `docs/error-model.md` — the single
biggest cluster in the crate. Reading all twenty-three against the document found **four
that pointed at the wrong key**: two claimed `request_id` where the line held `code`,
one claimed `code` where the line held the opening brace, one claimed `details` where
the line held `message`. Roughly **one in six is wrong**, and every one of them still
read as a plausible citation.

A script converts all twenty-three uniformly and has no way to notice that four of them
were lying. Reading them found four real mistakes in one pass, and would find roughly
twenty-five more across the remaining 149 on the same ratio. That is the whole
justification: the work is not typing, it is the reading, and the reading is the only
part a machine cannot do.

**The mirror image is worse, because coverage cannot see it at all.** A rule can be
fully covered — every line of it executed by a test — while the code that actually
runs is a *different copy* of it. `max(requested, model cap).min(hard cap)` was
written inline in the handler and rewritten in two test helpers, because a test cannot
call an expression buried in the middle of a request. The copies under test were at
100%; the line the handler executed was at nothing. Every function is entered, every
line is covered, and the money depends on a formula no test ever ran. It is now one
method — `ModelConfig::reserved_output_tokens` — that all three sites call, which is
the only arrangement in which "this test covers it" means anything.

So coverage answers "did any test run this function", and the question that matters is
"is the function the money depends on the function under test". The first is
mechanical; the second is a review, and the first being green is precisely what makes
the second worth doing.

The obvious completion — a source-walk test asserting every `pub fn` has a
production call site — was measured before being dismissed, and it does not work:
of 137 `pub fn`s examined, **37 have no production call site and every one is a false
positive.** They are axum route handlers (`create_key`, `exchange_token`,
`sse_events_handler`, …), which the ROUTER calls by path rather than by name, plus
test helpers. A test built on that idea would carry a 37-entry exception list that is
really a list of every endpoint in the product, and it would still miss the case that
matters, because a handler reached by the router is by definition live while a helper
reached by nothing is not distinguishable from it by any name-based rule.

So the question stays manual, the way the four findings above were. What makes it
tractable is that it is now a *narrow* question — zero named functions are unentered
— rather than an open search through a 97%-covered crate.

**The second measurement made the same point again, in a different file.** Reading
the uncovered *ranges* rather than the percentages showed `routes/proxy.rs`'s
remaining gap was not scaffolding: `MeteredStream` — the wrapper that decides
whether a customer is **billed** and whether a key is **cooled down** — was
entirely uncovered, including the client-hangup path whose own comment records
that dropping the body unread "is exactly the defect this is fixing". That fix had
no test. It does now, along with the usage-tail cap, which is `91.8%` and rising.

**What replaces the worklist: branch coverage, and it is currently NOT OBTAINABLE
here — measured, not assumed.** There is no file left to point at, so the remaining
gap is of a different kind: this toolchain does not instrument BRANCHES (the lcov
branch columns read 0/0), and a well-covered line can still hide an untaken arm.

`cargo llvm-cov --branch` needs a nightly compiler (`the option Z is only accepted
on the nightly compiler`), and **nightly IS installed on this machine**, so the
measurement looked achievable. It is not:

| Attempt | Result |
| --- | --- |
| `cargo +nightly llvm-cov --branch --lib` | suite runs (all tests pass), then `failed to collect object files` |
| …with a custom `CARGO_TARGET_DIR` | same — ruled out the target-dir override |
| …with `RUSTC_WRAPPER` unset | same |
| …with a clean `CARGO_HOME` config containing **no** `rustc-wrapper` | same |
| **`cargo llvm-cov --lib` (LINE coverage), same worktree** | **SUCCEEDS** — 459 KB of lcov |

That last row is the control that makes the finding trustworthy: line coverage works
in the very same checkout, so this is specifically `--branch` failing to find its
instrumented objects under Windows + MSVC + sccache — not a broken environment.
`show-env` explains why it is at least visible: llvm-cov reports
`__CARGO_LLVM_COV_RUSTC_WRAPPER_PRE_EXISTING=sccache`, i.e. it knows a pre-existing
wrapper is present and tries to compose with it.

**So the 0/0 branch columns are a real limitation of this setup, not an artefact of
the config.** Anyone who wants branch numbers should try a Linux or macOS checkout
first, or a machine without a global `rustc-wrapper`, before spending time on it —
which is what this table is for.


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
- [x] Whether the schema-drift check runs as a CI step or stays a local probe —
  **it runs as a CI step** (`Validate the schema against the plan`, immediately after
  the migrations-applied step). The answer for a money system is yes: the checker
  compares the plan's Appendix A with the shipped migration AND runs 32 invariant
  probes (every table STRICT, no REAL/FLOAT column, the CHECK constraints actually
  refusing bad rows, FK and RESTRICT behaviour). Answering the question was worth it
  on its own: the file had been **failing**, the plan was missing `topups.rail`, and
  because the drift check exits before the invariants, those 32 probes had never run
  anywhere — a red first half was hiding a whole second half.
- [ ] Cache configuration for build times.