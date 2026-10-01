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

These nine are CI stages rather than local scripts because each guards a failure that
produces **no error**: the thing looks like it works and does not. A local script only
helps someone who already suspects a problem.

| Stage | Guards against | Silently wrong because |
| --- | --- | --- |
| Check the edge relay streams SSE | A buffering relay | The UI answers 200 and stops updating |
| Check the compose deployment definition | A definition that parses but is wrong | `working_dir` silently misresolves the money gate's DSN |
| Check the backup contract | An offsite hook that copies nothing | The script prints "hook succeeded" and exits 0 |
| Check the reconciliation gate | A money gate that cannot fail | A drift check that never runs still looks like a green tick |
| Check the restore drill | Deletion of a live-looking target | Teardown deletes whatever it was pointed at |
| Check the rollback drill | A "rollback" that certifies a schema the old binary cannot run | The rehearsal proves nothing while printing PASS |
| Check the alert delivery contract | A failed delivery that silences its own retry | Alerts stop arriving and nothing says so |
| Check the wind-down payout report | A payout figure that is off by one IDR, or priced at a rate nobody froze | Closure is done by hand, so nothing downstream catches the mistake |
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
measured on the merged tree, `cargo test --lib` reports **636 passed / 0 failed /
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

**Identity enters through the Rust crate, not through any external service.** The
Phase 6 identity port landed: `accounts.pb_user_id` is dropped, `POST /auth/exchange`
is deleted, and `server/src/routes/auth.rs` no longer reads `POCKETBASE_URL`. A
deployed server needs no `POCKETBASE_URL` and there is no PocketBase anywhere — see
[`architecture/identity.md`](architecture/identity.md). No test needs one either,
which is why the exclusion flags could be deleted rather than named.

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
  every other file. It reached 98.3% via a loopback stub, with the ignore gone — and
  the handler has since been **deleted** along with `POST /auth/exchange` when the
  Phase 6 identity port landed.
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
reached **98.2%** via a loopback stub, with the ignore removed; the handler itself
was then deleted by the identity port, so there is no live PocketBase in the crate
to ignore a test behind.

### A percentage cannot see unreachable code — coverage can

Re-measured 2026-09-29, the question being: **can a test exist for a function no
request ever calls?** The wiring guards in `config.rs` cannot answer it, because they
see a name *referenced*, not a reference *reachable*. `max_context_tokens` read as
wired for exactly this reason — its only reader was a second
`worst_case_reservation_idr` on `UpstreamClient` that nothing outside its own tests
called, and that carried six green tests of its own.

The same run, after that function and the fields only it read were deleted:

| Metric | 2026-09-27 | 2026-09-29 (first run) | 2026-09-29 (re-measured) | 2026-09-30 (third) |
| --- | ---: | ---: | ---: | ---: |
| Region coverage | — | 97.07% | 97.07% | **97.09%** |
| Line coverage | 96.64% | 98.02% | 98.00% | **97.99%** |
| Function coverage | — | 94.36% | 94.23% | **94.06%** (109 of 1,835) |
| **Named functions never entered by any test** | not measured | 0 of 1,841 | 0 of 1,869 | **0 of 1,903** |
| Tests passing | 461 | 468 | 470 | **484** |

**Zero, still.** Every function in the crate that can be called by name is entered by
at least one test. The 109 unentered "functions" are all **closures**, which llvm-cov
records under their line number rather than a name.

The re-measurements are here because the numbers MOVE as the crate does, and a table
that reads as current when it is several changes old is a stale claim wearing a fresh
date. Line coverage moved by -0.01 and function coverage by -0.17 across two rounds of
adding tests and the guards those tests live in - not a regression, but the cost of
writing checks. The row that is supposed to be true is still true, and it is the only
row here that is a claim about the code rather than a measurement of it.

**WHY THE PERCENTAGES FALL WHILE THE SUITE GROWS.** The guards added over these
rounds - the claim checks, the schema check, the route and alert assertions - are
themselves code, and they run at test time, so they are instrumented and largely
uncovered by the tests they belong to. A guard exists to fail when something ELSE
changes, which is the opposite of being exercised. Read a falling coverage percentage
here as "more machinery was added", and read the named-function row as the one that
says whether anything is unreachable.

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

### What this repository has established it CANNOT check automatically

Three separate ideas for an automatic guard were built and **measured to death**, and
the results are here so nobody builds them again.

| Proposed check | Measured | Verdict |
| --- | --- | --- |
| Every `pub fn` has a production call site | 137 examined, **37 have none, all false positives** | axum route handlers, which the ROUTER calls by path. Would need a 37-entry list that is really a list of every endpoint. |
| Every operational document cites code by name, not line | shipped, and fires | the one that works |
| A comment's quoted text must sit near the line it cites | 4 candidates crate-wide, **0 misplaced** | measured, nothing left to catch, **not built** |
| A production function name appears in only one place | 251 scanned, **13 collisions, 0 real** | `new` ×8, `main` ×6, `into_response`/`fmt`/`drop` trait impls, per-type methods. All legitimate. |

**The common finding is that name- and shape-based checks cannot see the defect this
repository actually has.** The dangerous copies had *different bodies*: three session
resolvers that were each a little weaker than the original, and a rule copied into two
places so two copies could drift. A check for duplicate bodies would have caught only
the hash copies. A check for duplicate names catches none of the four. What separates
the real duplication from the 13 false positives is **whether the copies are supposed to
be the same rule**, and that is a judgement no source-level test can make — which is
why the four real findings took reading, and why the one that mattered most took a test
written at the *handler* rather than at the shared function.

So the honest summary of automated checking here: it is excellent at pinning a stated
rule to a stated instant, and it is blind to two rules quietly disagreeing. The first
kind has produced the money and session invariants in this crate. The second is a
review activity, and this section exists to say so rather than to imply otherwise.

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

> **Both figures below are AS OF the commit that wrote them, and both have since fallen —
> because this is the work that lowers them.** Re-measured at HEAD: `.md:N` citations in
> `server/src` are **130**, not 172, and `error-model.md:N` is **17**, not ~70. Nothing
> regressed; each converted citation leaves the population being counted.
> > That makes these the one kind of measurement in this repository that is **expected to go
> > stale**, and a bare present-tense number here is wrong in a way a reader cannot see. The
> > count is stated with its revision for that reason. **To reproduce:** count `.md:N`
> > occurrences across `server/src` at the revision named — summing per-file matches, since
> > one line can carry two citations and a line-counting tool under-reports (measured:
> > `git grep -c` gives **128** at BOTH revisions, so it cannot see the change at all).


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

**THE NEXT CLUSTER WAS WORSE: FOURTEEN OF NINETEEN.** The Retry-After citations —
nineteen comments pointing into `docs/error-model.md` between lines 93 and 112 — were read
next, and fourteen were wrong. Seven cited `:112` for the "floor at 1 second" rule
while `:112` is the *formula*; the rule is the `429 — rate limited` section, twenty lines
away. Four cited `:96`, which is a **blank line**, for the rule that a `Retry-After`
header appears only on 429 and 503. Two cited the 402/429 status table for the rounding
rule. A citation pointing at a blank line is the clearest possible statement that
nobody has opened the file.

Two ranges (`:99-110`, `:99-112`) were left alone: they are imprecise rather than wrong,
and precision is a preference while pointing at nothing is a defect.

So the rate is not one in six. Across thirty-one citations read so far, **eighteen were
wrong** — and the errors are not cosmetic. A comment citing the formula when it means
the floor rule will send a reader to write a client that retries instantly.

**THE THIRD CLUSTER WAS WORSE AGAIN, AND IT HAD A SINGLE CAUSE.** Thirteen citations
into `docs/error-model.md` around the streaming and upstream sections: **eleven wrong**.
Five pointed at `:159`, a blank line. Two at `:164`, the section HEADING. One at `:165`,
the heading's blank line. One at `:171`, a line inside a fenced code block. Two at prose
about unrelated subjects.

Almost all of them mean the same thing, and none cited it: **the numbered list under
`## Rules` at the end of the document.** The comments already say "rule 1", "rule 4",
"always include `request_id`" — they were paraphrasing the rules and citing whatever
line number was near. All eleven now cite the rule they name, which reads better than
the number it replaces and cannot drift.

**Across forty-four citations read, twenty-nine were wrong.** The remaining 128 are
untouched, and on this ratio the expected yield is roughly ninety more. That is the
honest size of the remaining job, and it is a reading job.

**AND THEN A CLUSTER CAME BACK CLEAN, WHICH CORRECTS THAT ESTIMATE.** Thirteen
`docs/failover.md` citations in the mid-stream and wallet-ordering sections: **none are
wrong.** Eleven are imprecise — `:138-144` stops one line short of the word it is
citing, `:162-165` includes a code fence and ends before "most expensive endpoint" —
and those now name their sections. Two are correct as they stand.

So the yield is not a constant. It depends on whether a comment is quoting a DOCUMENT'S
RULES or its own prose. Every cluster that quoted a rule was mostly wrong, because the
writer paraphrased the rule and cited whatever line was near; every cluster that quoted
prose was fine, because the prose had not moved. The three `error-model.md` clusters
quoted rules — 18 wrong of 31. This one quoted prose — 0 wrong of 13.

That is a better guide than a flat ratio, and it changes what the remaining 128 are: the
ones worth reading first are the comments that name a rule, a code, or a status code,
because those are the ones that drifted.

**THE GUIDE HELD ON THE NEXT CLUSTER, AND THE CAUSE IS SHARPER.** Thirteen citations
into `docs/error-model.md` naming a STATUS CODE or a status-code rule: **twelve wrong**.
They cited one of four neighbouring things while meaning another. `error-model.md` has a
status table (47-63), a `401 vs 403` table (67-72), a `402 vs 429` section (86-99) and a
`Retry-After` section (101-116), all within sixty lines of each other, and a comment that
says "a key limit is a 402, not a 429" cited `:73-86` — the 401/403 discussion and the
heading of the very section it meant. Three cited `:81`, which is a sentence about
`wrong_credential_type`. Two cited `:50`, the **401** row, for a comment about 503.

So it is not only rules. It is **any citation that names a specific value** — a status
code, a row in a table, a numbered rule — because naming a value is what makes a comment
feel precise enough to deserve a line number. Comments that quote prose were fine three
clusters out of three. Comments that name a value are where the errors are.

That is now a testable predictor rather than a hunch, and it is the right order to read
the remaining 115: not by cluster size, but by whether the comment names a value.

**THE PREDICTOR WAS USED AS A FILTER, AND IT SHRANK THE JOB.** Applying it to the 115
remaining citations — a comment counts if the text around it names a three-digit HTTP
status, a numbered rule, a field of the error envelope, a config key, or an endpoint
path — selects **13, which is 11% of what is left.** Nine were wrong. Four cited a blank
line, the Telegram link-code section, an alerts paragraph, and the wrong JSON field;
one cited the status table's **401** row for a comment about 503. The four Retry-After
citations quoted a real sentence from the document that sits twenty-four lines below
where they pointed.

That last one is the most useful thing the filter found, and it is a class no ratio would
have predicted: **the comment QUOTES the document verbatim, and the citation is still
wrong.** `"the shortest remaining cooldown across the endpoint pool"` is real text — it
is at `error-model.md:123`, not the `:99-110` the comment cited. A citation that is
checkable, specific and quoted is still drift. Checking the quote exists is not the same
as checking where it is.

So the remaining 102 are, on this evidence, mostly comments that paraphrase prose. That
is not permission to skip them: the one clean cluster was a cluster, and a 3-of-3 record
on one kind of comment is a reason to order the work, not a reason to stop.

**AND THEN THE QUOTE-WORK WAS MEASURED BEFORE IT WAS BUILT, WHICH CHANGED THE ANSWER.**
Round 40 found a class with an anchor: a comment that QUOTES a document and cites a line
where the quote is not. A quote is checkable — the text itself is the lookup key — so
unlike every other check here, this one can be automated. A guard was written to do it.

**It was not committed, because it has nothing left to catch.** Across the whole crate,
four citations carry a quotable phrase. Three are correct. **Zero are misplaced** — the four
round 40 found by hand were all fixed in that round. A check with no candidates is a
check that cannot fail, and a guard that never fires reads as protection while providing
none. Measured first, therefore not built.

**What the measurement found instead is a third class, which is a new one.** A comment
may not merely point at the wrong line — it may put words in a document's mouth that are
not there. `routes/admin.rs` quoted `docs/admin-surface.md` as calling the audit trail
"the record that makes disputes resolvable"; the document says **"the audit trail that
makes disputes resolvable"**, at line 19, while the citation said 288. Fixed to the
verbatim text and the section that is actually the subject.

So there are three, and they are genuinely different: **WRONG** (points at unrelated
content — the bulk of what the review has found), **MISPLACED** (the quoted text is real
and elsewhere), and **MISQUOTED** (the quoted text does not exist). Only the middle one
has a machine-checkable anchor, and it is now empty.

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