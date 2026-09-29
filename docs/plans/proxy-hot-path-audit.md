# Proxy Hot-Path Audit — the "SOTA blueprint" against the implementation

**Status:** analysis, for review · **Date:** 2026-09-25 · **Actions re-verified 2026-09-29: 7 of 9 done, 2 open — see [§8](#8-actions)** · **Branch:** `0.0.1`
**Companion to:** [`sqlite-migration.md`](sqlite-migration.md) — that plan covers the
*storage* port; this covers the *request* path.
**Input:** an external "SOTA Rust proxy blueprint" and a `rusqlite` vs `sqlx` analysis.

---

## 1. Verdict

The blueprint is a competent description of how a high-performance LLM proxy *should*
be built. **Four of its five recommendations are already implemented**, one of them
more thoroughly than the blueprint describes, and its stated "critical missing feature"
is present with tests.

It also rests on a false premise about this codebase, and two of its recommendations
would **regress** behaviour if applied literally.

| Blueprint point | Status |
| --- | --- |
| Global pooled `reqwest::Client` | **Already done** — `pool_max_idle_per_host(100)`, plus per-read timeouts |
| Zero-copy SSE streaming | **Already done** — `Body::from_stream(MeteredStream)`, no buffering |
| `stream_options: {include_usage: true}` — *"critical"* | **Already done, and better** — merged into the caller's object, with tests |
| Detached async billing | **Already done** — `tokio::spawn`, plus a reservation model the blueprint lacks |
| Graceful client-disconnect handling | **Deliberately different, for a documented reason** — see [§4.5](#45-graceful-disconnect--the-codebase-does-the-opposite-on-purpose) |

**The premise error:** the blueprint frames `rusqlite` vs `sqlx` as a live question about
existing inefficiency. **There is no `rusqlite` in this repository** — not in
`Cargo.toml`, not in `Cargo.lock`. At the time of writing the server used `sqlx` 0.8
with the `postgres` feature, so the question was a *forward-looking* choice for the
SQLite port, and [§3](#3-the-rusqlite-vs-sqlx-question) answered it on that basis.
**The port has since landed and followed this document: `sqlx` 0.8 with the `sqlite`
feature is what ships.** The reasoning in §3 is kept as the record of why.

---

## 2. What was checked, and how

Every claim below was verified against the tree. Counts and line numbers are from this
checkout, not from memory.

| Claim | Instrument | Result |
| --- | --- | --- |
| `rusqlite` present? | `grep` over `server/Cargo.toml`, `server/Cargo.lock` | **absent** |
| SQLite driver resolvable? | `Cargo.lock` | `sqlx-sqlite` and `libsqlite3-sys` **already in the lock** |
| Compile-time SQL macros? | `grep` for `sqlx::query!` etc. | **none** — all runtime-checked |
| Connection pooling | `upstream/client.rs:271` | `pool_max_idle_per_host(100)` |
| Streamed response | `routes/proxy.rs:1187` | `Body::from_stream(...)` |
| Streamed upstream | `upstream/client.rs:214` | `response.bytes_stream()` |
| Usage injection | `upstream/client.rs:473-486`, tests `888-907` | present, merges |
| Detached settlement | `routes/proxy.rs:1167` | `tokio::spawn(settle_after_stream(...))` |
| Local tokenizer | `grep` for `tiktoken` | **none** |
| `tcp_nodelay` | `grep` | **not set anywhere** |

---

## 3. The `rusqlite` vs `sqlx` question

### 3.1 The blueprint's general case is fair

Its claims about `sqlx` are the standard ones and they are not wrong in the abstract:
`sqlx` is async-first, carries a compile-time macro layer, and routes SQLite through a
thread-backed async wrapper. `rusqlite` is the leaner, more direct binding.

**And there is a real steelman worth stating plainly:** SQLite has no asynchronous API.
`sqlx`'s SQLite driver is therefore *not truly async* — it runs the same blocking
calls, on a background thread per connection, behind an async façade. So
"`sqlx` + SQLite" is approximately "`rusqlite` + a thread pool", with an abstraction
layer on top. That is a legitimate reason to prefer `rusqlite` for a pure-SQLite
service.

### 3.2 But four checkable facts make it the wrong move here

1. **The reason people call `sqlx` heavy does not exist in this codebase.** There are
   **zero compile-time SQL macros** — no `sqlx::query!`, no `.sqlx` offline cache, no
   `DATABASE_URL` needed at build time. Every query is runtime-checked
   `sqlx::query`/`query_scalar`/`query_as`. The build-time machinery is entirely absent.
2. **`sqlx-sqlite` is already in `Cargo.lock`.** Adding the feature is a one-line
   change. Switching to `rusqlite` means adding `rusqlite` *plus* a pool
   (`deadpool-sqlite` or `r2d2`) *plus* `spawn_blocking` plumbing.
3. **The port is a dialect port, already scoped.** 106 `$N` → `?` sites across 14 files,
   enumerated in [`sqlite-migration.md` §4.1](sqlite-migration.md). The same work against
   `rusqlite` is not a find-and-replace: it is a rewrite of every call site against a
   different API, plus losing `sqlx::migrate` and the register's settled
   *"Migrations: `sqlx migrate`, forward-only"* decision.
4. **The bottleneck is not the driver.** SQLite serialises writers. On this schema every
   settlement is a write transaction, so throughput is bounded by single-writer
   serialisation and fsync latency — not by whether the Rust layer is `sqlx` or
   `rusqlite`. Driver overhead is in the noise next to that.

### 3.3 Decision: keep sqlx, add the sqlite feature

Settled 2026-09-25. Reasoning, in order of weight:

1. **The cost of switching is a rewrite; the cost of staying is a feature flag.**
   `sqlx-sqlite` is already in `Cargo.lock`, and the port is 106 `$N` → `?` sites across
   14 files ([`sqlite-migration.md` §4.1](sqlite-migration.md)). Moving to `rusqlite`
   means re-writing every call site against a different API, adding a pool crate
   (`deadpool-sqlite` or `r2d2`) and `spawn_blocking` plumbing, and abandoning
   `sqlx::migrate` — which is a **settled register decision**
   (*"Migrations: `sqlx migrate`, forward-only"*).
2. **The usual reason to leave `sqlx` is absent here.** No compile-time macros, no
   offline cache, no build-time database. The build-time machinery the blueprint objects
   to does not exist in this tree.
3. **The bottleneck is not the driver.** SQLite serialises writers; every settlement is a
   write transaction. Throughput is bounded by single-writer serialisation and fsync, not
   by the Rust binding. Driver overhead is in the noise next to that.
4. **The steelman does not survive contact with the numbers.** `sqlx`'s SQLite driver is
   not truly async — SQLite has no async API — so it is approximately `rusqlite` plus a
   thread pool behind an async façade. That is a real argument in the abstract, but it
   optimises the component that is not the constraint.

**Revisit only if** [`sqlite-migration.md` §9](sqlite-migration.md) check 12 shows the
write path missing its target **while the driver is the saturated component** — which the
current evidence gives no reason to expect. The trigger is a measurement, not a
preference.

The blueprint's own framing supports this: *"SQLite is the right call if and only if
your target is a single node with limited write concurrency."* That is exactly the
migration plan's conclusion, and it is enforced by the platform — a Northflank
Single Read/Write volume *"limited to 1 instance"*
([`sqlite-migration.md` §8](sqlite-migration.md)).

---

## 4. Point-by-point audit

### 4.1 Global connection pooling — already done

`upstream/client.rs:271-282`:

```rust
let mut builder = reqwest::Client::builder().pool_max_idle_per_host(100);
if timeout_seconds > 0 {
    // A per-read timeout, not a total one: it catches an upstream that
    // stalls with no bytes (docs/failover.md, "Upstream stalls"), while
    // leaving a long but healthy SSE stream alone. A total timeout
    // would cut a legitimate 4096-token answer off at the wire.
    builder = builder.connect_timeout(timeout).read_timeout(timeout);
}
```

Two things worth noting, both in the codebase's favour:

- The client is built **once** in `UpstreamClient::new`, and the comment records why:
  *"The environment is read here and only here: a key that appears later needs a
  restart, which is preferable to reading `std::env` on the hot path."*
- The timeout choice is **per-read, not total**, with the reasoning written down. The
  blueprint does not raise this, and it is the subtler of the two decisions — a total
  timeout on an SSE endpoint truncates healthy long answers.

**Gap:** `tcp_nodelay` is not set. See [§5.1](#51-f1--tcp_nodelay-is-unset-on-both-sides).

### 4.2 Zero-copy SSE streaming — already done

- Client → upstream: `upstream/client.rs:214` — `bytes: Box::pin(response.bytes_stream())`
- Server → client: `routes/proxy.rs:1187` — `.body(Body::from_stream(MeteredStream::new(stream, settle_tx)))`
- `MeteredStream` implements `Stream` directly (`proxy.rs:697`), so chunks are forwarded
  as they arrive.
- nginx has `proxy_buffering off` on `/events` and `/v1/*` (`todo.md`, Phase 1), so the
  relay does not re-introduce buffering.

**Gap:** server-side `TCP_NODELAY`. See [§5.1](#51-f1--tcp_nodelay-is-unset-on-both-sides).

### 4.3 `stream_options: {include_usage: true}` — already done, and better than described

This is the blueprint's *"critical missing feature"*. It is present, at
`upstream/client.rs:473-486`, and the implementation is **strictly better** than the
blueprint's proposal:

```rust
// A caller may legitimately send stream_options of their own, so their
// object is merged into, never replaced: only include_usage is set and
// ...
options.insert("include_usage".to_string(), json!(true));
```

The blueprint says *"force this parameter in the upstream request body."* Forcing would
**overwrite a caller-supplied `stream_options`**. This implementation merges, and it is
covered by tests:

- `prepare_body_injects_stream_options` (`client.rs:888`) — asserts the object is
  created when absent.
- `prepare_body_merges_into_a_caller_supplied_stream_options` (`client.rs:895-907`) —
  asserts the caller's own key survives and only `include_usage` is forced.

**And the blueprint's related warning is already satisfied:** *"do not use tiktoken —
it is a CPU-bound synchronous library."* There is no tokenizer in the tree. Usage comes
from the upstream's reported figures.

**Genuine caveat:** injection is **unconditional**. A provider that rejects unknown
parameters would fail every streaming request. See [§5.4](#54-f4--stream_options-injection-is-unconditional).

### 4.4 Detached async billing — already done, and the model is stronger

`routes/proxy.rs:1165-1187` creates a `oneshot` channel, spawns the settlement, and
returns the stream:

```rust
let (settle_tx, settle_rx) = tokio::sync::oneshot::channel::<StreamEnd>();
// ...
tokio::spawn(settle_after_stream(/* ..., */ guard));   // :1167
// ...
.body(Body::from_stream(MeteredStream::new(stream, settle_tx)))  // :1187
```

So the response is returned before settlement, exactly as the blueprint recommends.

**But the codebase does not bill the way the blueprint assumes.** It is not
"count tokens, then debit". It is a **reservation** model:

1. **Pre-flight reserves the worst case** (`proxy.rs:1041-1053`):
   `max(client_requested, model.max_output_tokens)`, clamped to
   `hard_max_output_tokens`.
2. **Settlement releases the difference and charges the actual**, in one transaction.
3. **A guard releases the hold on any unclaimed exit** — `ReservationGuard::drop`
   (`proxy.rs:138-154`) spawns a fire-and-forget release, with the reasoning stated:
   *"`Drop` cannot await and the hold must come back even if this future is being torn
   down."*
4. **A sweep catches what the guard misses** — `unpaired_hold_rows` (`db.rs`), which
   reports reservations with no matching release.

The consequence: **the proxy never needs to count tokens to bill.** It needs the
upstream's reported usage, which `stream_options` provides. That is why the blueprint's
tiktoken warning and its `stream_options` recommendation are really the same point, and
why the codebase's model is more robust than "compute after the fact".

**Honest caveat, and the blueprint is right to raise it:** a detached `tokio::spawn` is
**not durable**. If the process dies between the response and the settlement commit, the
reservation is stranded until the sweep runs. The blueprint's framing — *"not a durable
task queue"* — is accurate. The mitigation exists; its **bound does not**. See
[§5.6](#56-f6--the-stranded-hold-bound-is-unstated).

### 4.5 Graceful disconnect — the codebase does the opposite, on purpose

This is the one point where the blueprint and the implementation genuinely disagree, and
the disagreement is worth understanding rather than resolving by default.

**The blueprint says:** on client disconnect, *"do NOT continue polling the upstream"*,
and bill only for the chunks already sent.

**The codebase says** (`proxy.rs:663-694`):

> *"The upstream body is HANDED OVER, not finished: it goes down the settlement channel
> so the usage the upstream still reports for the partial answer is read and billed
> instead of evaporating. … Finishing the lease HERE would drop the body unread —
> exactly the defect this is fixing."*

```rust
impl Drop for MeteredStream {
    fn drop(&mut self) {
        if self.done { return; }
        self.done = true;
        match self.settle.take() {
            Some(settle) => {
                // The settlement task owns the body now and reports the lease.
                let _ = settle.send(StreamEnd::Hangup(self.inner.take()));
            }
            None => {
                // Nobody is listening, so the lease must still be freed here
                // rather than leaked out of the key pool.
                if let Some(inner) = self.inner.take() {
                    inner.finish_status(0);
                }
            }
        }
    }
}
```

Note the details: the key lease travels with the body, and on a client hangup it is
released with **status 0** — no cooldown — because *"a client hangup says nothing about
the upstream."* That is a distinction the blueprint does not make.

**Why the codebase is right for this product:** the reservation was already taken and
the upstream tokens were already generated. Dropping the body unread would mean paying
the provider and billing nothing — for a prepaid reseller, a silent loss on every
abandoned stream. Reading the usage is the only way to charge accurately.

**But there is a real consequence the blueprint is gesturing at, and it is a business
decision, not a bug:**

> A user who disconnects mid-answer is still billed for what the upstream generated.

That is defensible — the tokens existed, the reservation was theirs — but it is exactly
the kind of behaviour that generates a support dispute, and the register's own reasoning
for rejecting mid-stream cutoffs is that *"a truncated answer on non-refundable funds is
the top dispute source"* (`decisions.md`, Balance exhaustion). **This should be stated
in the Terms of Service explicitly**, not left as emergent behaviour. It is currently
recorded in code comments and in `todo.md` as a tested scenario, but not in
[`terms-of-service.md`](../terms-of-service.md).

---

## 5. Findings

Small, concrete, and mostly one-line. None of them is architectural.

### 5.1 F1 — `tcp_nodelay` is unset on both sides

`grep` for `tcp_nodelay` across `server/src` returns nothing.

For an SSE proxy this is the one genuine latency gap the audit found. Nagle's algorithm
can hold back small TCP segments, and SSE frames are small and frequent — so this lands
directly on **time-to-first-token** and on perceived smoothness of a stream.

Two places:

| Side | Fix |
| --- | --- |
| Upstream client | `reqwest::Client::builder().tcp_nodelay(true)` — one line at `upstream/client.rs:271` |
| Server listener | `axum::serve` exposes no direct toggle; set `TcpStream::set_nodelay(true)` per accepted socket, or use a custom accept loop |

The server side matters more, because it is the leg the user actually feels. Worth
measuring before and after rather than assuming the size of the win.

### 5.2 F2 — `allow_negative_balance_overdraft` is stale config contradicting the register

| Location | Says |
| --- | --- |
| `config/apikita.toml:215` | `allow_negative_balance_overdraft = true` |
| `server/src/config.rs:89` | field still declared |
| `docs/decisions.md:185` | *"`allow_negative_balance_overdraft` was **removed** — overdraft is not permitted"* |
| `routes/proxy.rs:1089` | *"`allow_negative_balance_overdraft` is deliberately NOT consulted here"* |

So the field is dead — the code ignores it — but the config file still asserts `true`,
which is the opposite of the settled decision. This is precisely the failure the register
warns about in its own *"How to change a decision"* section: *"a stale decision is worse
than none, because it is followed."* An operator reading `apikita.toml` would conclude
overdraft is permitted.

**Fix:** delete the field from `StreamingConfig`, delete the line from `apikita.toml`,
and fix the test fixture at `upstream/client.rs:613`. It is dead code either way; leaving
it is a documented contradiction.

### 5.3 F3 — `default_max_output_tokens` is dead config whose comment is false

`config/apikita.toml:218-221` says:

> *"Hard cap on output tokens per request when the caller does not specify one. Worst-case
> cost reservation is computed against this."*

Neither half is true. `grep` shows the field is read in exactly one place:
`upstream/client.rs:615`, a **test fixture**. In production:

- The reservation is computed against **`model.max_output_tokens`** (384,000), not
  `default_max_output_tokens` (4,096) — `proxy.rs:1050-1053`.
- No default `max_tokens` is injected into the upstream body; `prepare_body` only merges
  `stream_options`, and the body is otherwise forwarded verbatim.

**Fix:** either wire it up as a real default, or delete it. Leaving a config knob that
does nothing while documenting behaviour it does not have is worse than either.

**Related, and verified acceptable:** because the reservation uses the model ceiling,
**every request holds ~4,112 IDR** regardless of what the client asks for
(384,000 × `output_peak` 10,707.12 / 1e6). Against `min_topup = 10,000` and
`min_first_deposit = 50,000`, that is a fine floor — it does not lock out small balances.
Recorded here so the number is known rather than discovered from a support ticket.

### 5.4 F4 — `stream_options` injection is unconditional

`prepare_body` injects `stream_options.include_usage` whenever the request streams. If a
provider does not accept that parameter, the request fails — and the code's own comment
acknowledges the shape risk without addressing capability:

```rust
// A non-object stream_options is not a shape the upstream would ...
```

The config already models providers as endpoints with per-endpoint attributes
(`upstream_model`, `concurrency_per_key`, `weight`, `api_key_envs`), so a
`supports_stream_options: bool` per endpoint would fit the existing shape and let a
non-conforming provider be added without patching code.

Today there is one verified provider and it works, so this is **latent, not live** — but
it is a blocker on adding the second provider that `docs/failover.md` needs.

### 5.5 F5 — `pool_idle_timeout` left at the default

With `pool_max_idle_per_host(100)` and three endpoints declared in `config/apikita.toml`,
the pool can hold up to 300 idle sockets for the 90-second default. On a 256 MB container
that is worth a deliberate number rather than a default, especially since only one
endpoint is real today (the others are placeholders with `weight = 0`).

### 5.6 F6 — the stranded-hold bound is unstated

`unpaired_hold_rows` detects a reservation with no release, and `ReservationGuard`
prevents most of them. What is missing is a **stated bound**: how long may a hold stay
unpaired before it is an incident? Without a number, the sweep is a query nobody runs on
a schedule, and a stranded hold is invisible money.

The `ip-purge` binary is the existing precedent for a scheduled maintenance job — it
enforces the 7d/90d IP retention promise but, per `todo.md`, still *"needs a
scheduler"*. Both jobs want the same thing.

### 5.7 F7 — runtime worker threads untuned (minor)

`main.rs:14` uses `#[tokio::main]`, so the multi-thread runtime with
`available_parallelism()` workers. On the target 0.2 vCPU Northflank plan that resolves
to one or two workers, so the blueprint's concern is largely moot in practice — but
pinning it (`worker_threads(1..=2)`) makes behaviour predictable across hosts rather
than dependent on what the scheduler reports. Low priority.

---

## 6. Where the blueprint is right

Worth recording, because these are the parts to keep:

1. **"SQLite is the right call if and only if your target is a single node with limited
   write concurrency."** Correct, and it is the migration plan's own conclusion — with
   the addition that Northflank *enforces* the single node
   ([`sqlite-migration.md` §8](sqlite-migration.md)).
2. **A global HTTP client is essential, and per-request clients are fatal.** Correct, and
   already done — including the subtler per-read-timeout decision.
3. **Do not run tiktoken on the hot path.** Correct, and already the case. The
   reservation model removes the need entirely.
4. **Billing must not block the response.** Correct, and already done, with a guard and a
   sweep the blueprint does not propose.
5. **The hot path is the thing that matters, not the ORM.** Correct, and it is the reason
   the migration plan leads with the timestamp-format and foreign-key traps rather than
   with schema aesthetics.

## 7. Where applying the blueprint literally would regress

| Recommendation | If applied literally |
| --- | --- |
| Switch to `rusqlite` | Rewrite 106 call sites against a new API, add a pool crate, lose `sqlx::migrate` and a settled register decision — to optimise the one component that is not the bottleneck. |
| *"Force `stream_options`"* | Overwrite a caller-supplied `stream_options` object. The current merge is correct and tested. |
| *"Bill only for the chunks you sent"* | Stop billing usage the upstream already generated and already charged us for. On a prepaid margin business this is a direct, silent loss on every abandoned stream. |
| *"Don't continue polling the upstream"* | Discards the usage report that makes accurate settlement possible — the exact defect `MeteredStream::drop` was written to fix. |

---

## 8. Actions

Ordered by value per unit of effort.

| # | Action | Effort | Where | Status |
| --- | --- | --- | --- | --- |
| 1 | Delete `allow_negative_balance_overdraft` from config, struct and fixture | minutes | F2 | **DONE** |
| 2 | Resolve `default_max_output_tokens` — wire it up or delete it, and fix the comment | minutes | F3 | **DONE** — deleted |
| 3 | Set `tcp_nodelay(true)` on the upstream client; measure | minutes | F1 | **DONE** — `upstream/client.rs`, not measured |
| 4 | Set `TCP_NODELAY` on the server's accepted sockets; measure time-to-first-token | small | F1 | **OPEN — and the effort estimate was wrong, see below** |
| 5 | State the stranded-hold bound; schedule the sweep alongside `ip-purge` | small | F6 | **DONE** — bound stated in `bin/hold-sweep.rs`; the scheduler runs it nightly |
| 6 | Add `supports_stream_options` per endpoint | small | F4 | **DONE** — on every endpoint in `config/apikita.toml` |
| 7 | Set `pool_idle_timeout` deliberately | minutes | F5 | **DONE** — 30s, with the reasoning inline |
| 8 | State the mid-stream-disconnect billing rule in the Terms of Service | small | [§4.5](#45-graceful-disconnect--the-codebase-does-the-opposite-on-purpose) | **DONE** — `docs/terms-of-service.md`, "Billing when the client disconnects mid-answer" |
| 9 | Pin `worker_threads` | minutes | F7 | **OPEN** — needs a measurement, see below |

*(The status column was added after the fact. Seven of these nine were already done
when this table was last read as a to-do list, and the table gave no sign of that: it
is the same stale-document class this repository keeps having to fix — a list that
reads as outstanding work long after the work landed, which sends a reader to redo
it. Every DONE above was re-verified against the tree before being marked, not
inferred from the absence of a complaint.)*

**ACTION 4 IS NOT THE `small` CHANGE THIS TABLE SUGGESTED.** The upstream half is one
line. The server half is not, and the reason is a platform API, not effort:
`TCP_NODELAY` has to be set on the LISTENING socket before the accept loop starts,
and the only stable-Rust route to a socket option is `std::net::TcpSocket` — which is
still `#![feature(tcp_socket)]`, unstable. `TcpListener::bind` sets no options at all,
so an accepted socket keeps the system default — and that default is **Nagle ON**.
Measured rather than assumed: binding, accepting, and reading the option back through
`into_std().nodelay()` returns `false`, which is the same statement said the other way
round. So the downstream SSE leg (provider to browser, the one the customer waits on)
has had Nagle enabled all along, while the upstream leg has had it off since action 3.

So closing it means one of: a `socket2` dependency, a nightly toolchain, or giving up
`axum::serve` for a hand-written accept loop that sets `set_nodelay(true)` per
connection. The last is the worst of the three — per-connection is a worse place to
set it, because it leaves a window on the first bytes, and it means owning shutdown.
None of that is worth doing for a win nobody has measured, which is what this audit's
own instruction below says to do first. **The blocker is the missing measurement, and
the estimate hid a real API constraint behind it.**

**Action 9 wants a number this repository does not have.** `worker_threads` is
untuned because the right value depends on the container's CPU allocation, which is
Northflank's developer tier until Phase 6 provisions it. Pinning a number now would
be pinning a guess, and the runtime's default is already `available_parallelism`.

**Actions 1 and 2 were the ones worth doing immediately**, because they are not
optimisations — they are the repository contradicting its own decision register, which
is the failure mode the register exists to prevent. Both are done.

**Actions 3 and 4 are the only performance work the audit found.** Action 3 is done
and was not measured. Action 4 is open, for the reason above. If either is taken up,
measure before and after; do not assume the size of the win.

**Nothing in this audit changes [`sqlite-migration.md`](sqlite-migration.md).** That plan
was checked against these findings: the `sqlx` choice stands
([§3.3](#33-decision-keep-sqlx-add-the-sqlite-feature)), the reservation model is unaffected by the storage port,
and the one storage-relevant item here — `tcp_nodelay` — is independent of the driver.
