# Launch Checklist

**Two different things were being called "open items": decisions and tasks.** That
made the open items meaningless — nobody could tell what actually blocked launch.

This document holds the **tasks**: things that need doing, not choosing. Settled
choices live in [`decisions.md`](decisions.md).

> Decisions: [`decisions.md`](decisions.md) · Deploy: [`deployment.md`](deployment.md) ·
> CI: [`ci-cd.md`](ci-cd.md)

## Gate 0 — Legal, before anything is built

**Cannot be skipped and cannot be deferred.** Two items here determine whether the
business can operate at all.

- [ ] Read the resale terms of the intended upstream provider, in full.
- [ ] Record the outcome in [`config/provider1.md`](../config/provider1.md).
- [ ] Decide the contracting entity (personal or PT) — affects dispute posture,
      volume ceiling, and tax.
> **Gate 0 partial — owner-deferred.** The legal review below is **deferred by decision**
> until turnover approaches 4.8 billion IDR/year (the PP 55/2022 UMKM ceiling). The owner
> is operating personally for now. **The residual risk is accepted knowingly**: no lawyer
> review, no corporate bank account, and no limited liability at launch. The crypto rail
> is parked entirely, which removes the largest compliance surface; everything else about
> the money path is settled in [`decisions.md`](decisions.md).

- [ ] Legal review of [`terms-of-service.md`](terms-of-service.md) — **including the
      wind-down clause**. Explicitly deferred until the 4.8b IDR trigger; re-open then.
      Questions for that review are listed in [`decisions.md`](decisions.md)
      §"Genuinely open": outbound transfers without a PT, PP 55/2022 on refunded
      revenue, KYC/AML on a payee identified by email, and unclaimed balances.
- [ ] Publish the Terms of Service, including the cross-border forwarding disclosure.
- [ ] Publish a privacy policy matching [`data-retention.md`](data-retention.md).

**Do not take a single deposit before this gate closes.** Wallet funds collected
against undeliverable or undisclosed service is the one mistake that is not
recoverable.

## Gate 1 — Infrastructure

- [ ] A persistent volume provisioned for the SQLite database file (no database
      instance to provision), and a backup of it tested by restore.
- [ ] PocketBase deployed and reachable.
- [ ] Edge relay deployed; nginx configured with `proxy_buffering off` on `/events`.
- [ ] Automatic certificate renewal on the relay **and** on the backend.
- [ ] Backend serves a valid certificate for the public hostname (required for
      failover — see [`topology.md`](topology.md)).
- [ ] Health checks configured: relay and backend independently.
- [ ] Failover path tested (relay down -> backend serves).
- [ ] Backups running, **offsite**, encrypted, and verified to produce a sane size.

## Gate 2 — Money correctness

The checks that protect customer funds. Each one is a test or a review, not an
opinion.

### Webhook

- [x] Signature verified on every request; mismatches rejected and logged.
- [x] Amount validated against the **stored** `topups` row, never the payload.
- [x] Crediting is idempotent by `order_id`.
- [x] Crediting is atomic with the `topups` status update and the ledger row.
- [x] `refund` and `partial_refund` statuses **refused, not handled**: a signed
      refund notification returns 200 with `{"status":"refund_not_supported"}`,
      logs at `error!`, and writes nothing — the topup stays `settled`, no ledger
      row is appended, the wallet cannot move.
- [ ] **Alerting on that refusal** — unalerted, a refusal is indistinguishable from
      a bug. The code logs the documented event; nothing schedules or alerts on it
      yet. This is the ONE part of this group that is genuinely outstanding, and it
      is an ops task rather than a code defect.

### Ledger and balance

- [x] **No client-reachable path can write `balance_idr`.**
- [x] `ledger` is append-only; no UPDATE or DELETE exists in the codebase.
- [x] The reconciliation query returns **zero rows** on production data.
- [x] `CHECK (balance_idr >= 0)` present and exercised.
- [x] Money is `INTEGER` IDR end to end; no float appears in any billing path. (Was `BIGINT` under Postgres; `STRICT` SQLite tables reject `BIGINT`, so the type is now `INTEGER` — see `decisions.md` §Money.)

### Billing accuracy

- [x] **Cache-read tokens are never counted as input tokens** — test with a payload
      containing cache hits.
- [x] Three token classes stored separately in `usage_daily`.
- [x] Peak/off-peak basis applied consistently between reservation and settlement.

> **These boxes were UNCHECKED while the work was already done and tested** — the
> same drift this file has been corrected for before. Each was verified against the
> code and its tests before being ticked, not inferred from the fact that the suite
> is green:
>
> | Item | Evidence |
> | --- | --- |
> | Signature verified | `money.rs:73` `ct_eq` over SHA-512, pinned by known-vector tests |
> | Stored-amount validation | `db.rs:140` matches `order_id AND status AND amount_idr`; `AmountMismatch` handled at `webhooks.rs:205` |
> | Idempotent crediting | the conditional UPDATE above is the claim; replay test asserts exactly one credit |
> | Atomic credit | one `BEGIN IMMEDIATE` covers the status change, the wallet and the ledger row |
> | Refunds refused | `money.rs:126` `PaymentAction::RefundRefused`; two live tests, incl. an inflated amount |
> | No route writes `balance_idr` | all 7 UPDATE sites are in `db.rs` (transactional) or `bin/hold-sweep.rs` (operator tool) |
> | `CHECK (balance_idr >= 0)` | `migrations/20260925000000_initial_schema.sql:66` |
> | Cache-read not counted as input | `upstream/client.rs:120` clamps with `cached.min(prompt)` so a malformed report cannot push the split negative; `money.rs:344` `a_cached_token_is_never_also_billed_as_an_input_token` asserts the arithmetic |
>
> **The refund-alerting box is explicitly OUTSIDE this group and left unticked**,
> because it is the one part that is genuinely outstanding: the event is logged but
> nothing watches it, and it is an ops task rather than a code defect.

## Gate 3 — Access control

- [x] Cookie sessions are **rejected** on `/v1/*`; API keys are rejected on
      dashboard endpoints.
- [x] Logout revokes the session row; "sign out everywhere" revokes all of them.
- [x] Suspension revokes sessions **and** keys atomically.
- [x] Admin endpoints require the operator flag; an operator cannot act on
      themselves.
- [x] Every admin action writes an `admin_audit` row in the same transaction.
- [x] Session lifetime is **30 days absolute and 7 days idle**, both enforced.
      The idle half is measured from `sessions.last_seen_at`, which moves when a
      session cookie resolves on a dashboard endpoint and is **not** touched by
      `/v1/*` (that path authenticates API keys, not cookies).
- [x] Link-code redemption is rate-limited per account and per IP.

> **The three admin items above were implemented in `server/src/routes/admin.rs`
> and covered by live tests, but the boxes stayed unchecked** — the checklist had
> drifted *behind* the code. `require_operator` runs before any target lookup (so
> a non-operator gets an identical 403 for a present and an absent id), one
> `BEGIN IMMEDIATE` transaction does the status change, the session revocations,
> the key revocations and the single `admin_audit` insert, and a failed suspend
> writes no audit row.
>
> **Link-code redemption was the one item genuinely unbuilt when this note was
> written**, and it is now built (`server/src/routes/telegram.rs`). Both caps the
> docs demand are enforced and independently tested: **per account** by
> `limits.link_code_issuance_per_hour` over issued codes, and **per IP** by
> `limits.link_redemption_per_hour` over recorded attempts. The property that
> matters is that **failed guesses count** — the attack on a 6-digit code IS the
> failure stream, so a counter that advanced only on success would never fire.
> Refusing a *correct* code once the budget is spent is what proves it. Every
> refusal (wrong, expired, used, malformed, unknown) is **byte-identical**, so the
> endpoint cannot be used as an oracle for which codes are live.

## Gate 4 — Data promises that must be true

**Each of these is a statement made to customers. If the code contradicts it, the
statement is false.**

- [x] Prompts and completions are never logged, stored, or sent to analytics.
      **Enforced by a test**, not a review: `a_customer_prompt_never_reaches_the_log`
      (`server/src/routes/proxy.rs`) drives a real request carrying a sentinel prompt
      through the handler with a capturing subscriber installed at TRACE and asserts the
      sentinel never appears, with a positive control so silence cannot pass.
- [x] No raw IP address is persisted; only a salted hash, salt deleted daily.
      `server/src/ip_tracking.rs:6-8` ("**no raw IP is stored anywhere**. What is stored
      is an HMAC ... under a salt that changes every day and is never written down"),
      `:16-17` (salt in memory, OS RNG, replaced at the UTC boundary).
- [x] API keys stored as hashes only; the plaintext exists once, at creation.
      `server/src/routes/keys.rs:388` hashes the full key BEFORE the insert; only the
      hash is bound into the row.
- [x] Session tokens stored hashed; cookies are HttpOnly, Secure, SameSite.
      `server/src/routes/auth.rs:251-253` sets all three, and `:558-560` asserts the
      serialized cookie CARRIES them, so the flags cannot be dropped silently.
- [x] `PUBLIC_*` variables contain nothing secret.
      No `PUBLIC_*` key is defined in `server/` or `config/`; the single mention
      (`server/src/routes/account.rs:60`) is a comment about the browser cross-checking
      its own value.
- [ ] Backup encryption keys are held separately from the backups. **Not ticked, and
      different in kind from the five above.** `tools/backup/backup.sh` does its half
      (refuses to write a plaintext dump, exit 6; AES-256-CBC with PBKDF2 200k
      iterations; the key passed as `env:` so it never enters the process list), but its
      own header records "the human decisions still open (offsite provider, key
      handling)". Key custody is a DEPLOYMENT decision, not a property of this code, so
      ticking it from a source read would be exactly the false customer statement this
      section warns about.

## Gate 5 — Operational readiness

- [x] `/health` checks the process and the database, **not** upstream providers.
      `server/src/routes/health.rs` probes `SELECT 1` only; the doc-comment at
      `docs/server/api-spec.md:466-470` states the rule and why (an upstream outage must
      not look like a dead server and trigger a restart loop). Tested, including the
      unauthenticated-body leak rules.
- [ ] Alerts configured: webhook rejection, ledger drift, API down, circuit open.
      **Every one of these is now CHECKED by `tools/alert`** — see `alerts.tsv`, 9 of 10
      entries `covered` — but "configured" also means a delivery channel and a schedule,
      which are deployment decisions. The code half is done.
- [ ] **A restore drill has been run**, with the reconciliation query passing.
- [ ] Drill log records the measured restore time — that is the real RTO.
- [x] Error responses match [`error-model.md`](error-model.md), including `request_id`.
      `server/src/error.rs:233` mints `req_<uuid>` per failure and `:273` puts it in the
      body; `error.rs:607` asserts the exact documented shape
      `{error: {code, message, request_id}}`.
- [x] SSE heartbeat runs; a dropped stream surfaces as a stale indicator, not a
      silently frozen balance.
      **Now enforced by tests on both halves.** Server: `events.rs:26-28` sets a 25s
      heartbeat (docs/realtime.md asks for 20-30s) and `:550` pins that window.
      Frontend: `live.ts:195` marks the store `stale` on error, and
      `a dropped stream keeps the last balance, marks it stale, and never relabels
      polled data live` (`website/tests/live.test.ts`) asserts the last KNOWN balance
      survives and that polled data is never presented as live — mutation-verified in
      both directions.
- [ ] Abuse-report contact published and monitored.

## Gate 6 — Product surfaces

- [ ] Signup shows the cross-border disclosure before the first request.
- [ ] Top-up screen states the fee and the non-refundable policy before payment.
- [ ] Deposit minimums enforced server-side (first vs re-top-up differ).
- [ ] API key shown once, with an acknowledged warning.
- [x] Telegram `/link` flow works end to end **on the server side** — issue,
      redeem, unlink and re-link are implemented and covered by live tests. The
      Telegram *bot* itself is still design-only (`telegram/README.md` has no code),
      so the flow has not been exercised against the Bot API.
- [ ] Review flow creates once and edits thereafter; withdrawal is a flag.
- [ ] Telegram top-up feed posts on settlement only, never on creation.

## What is NOT on this list

Deliberately excluded, and why:

| Not a launch task | Why |
| --- | --- |
| A second upstream provider | Desirable, not blocking; one provider works |
| A staging environment | Costs money; production-only until revenue justifies it |
| Uptime SLA | Do not promise what you cannot measure |
| A public status page | Later |
| An admin UI | **Built** — read-only lookup + suspend/restore at `/admin`; that was the launch bar |
| 2FA | Later; Google sign-in already carries it |
| Model breadth | Flash tier only |

## The three that actually matter

If everything else slips, these three cannot:

1. **Gate 0 closes before the first deposit.** Legal and entity structure.
2. **The reconciliation query returns zero rows.** It is the single best guard
   against silently wrong money.
3. **A restore drill has been run.** An untested backup is a belief.

## Open items

**The authoritative list lives in [`decisions.md`](decisions.md), not here.** Two lists
of "the genuinely open items" drifted apart within two rounds, which is the same
failure as the scattered open items fixed earlier. **Single source.**

That said, the ones that block *launch* specifically are:

| Blocker | Gate |
| --- | --- |
| **Legal review of the Terms of Service** | Gate 0 — cannot take money without it |
| **Abuse-report contact** | Gate 5 — required to publish the terms |
| **Support cost per customer** | Commercial model; not a launch blocker but the decisive business input |
| **Credit expiry implementation** | Policy settled (2 years per deposit); the code is not written — no per-deposit expiry column, no sweep job, no refusal of a spend against aged credit |
| **Wind-down runbook exercised** | Policy settled (balances above USD 2.00 paid out) and the runbook is written, but the payout is manual and has never been executed. It touches money and there is no treasury — so it is **not** a launch blocker on the same footing as a deployment, but it must be walked through against a scratch database before it is ever needed |

The rest — Northflank prices, a second provider, a staging environment, review
moderation policy, the second-operator threshold, the bot runtime — are open but
**do not gate launch**. Full list: [`decisions.md`](decisions.md).

## How to use this checklist

1. Gates are ordered; **Gate 0 blocks everything** — no deposits before it closes.
2. An unchecked box is a task, not a decision. Decisions live in
   [`decisions.md`](decisions.md).
3. **Gate 4 items are promises made to customers.** If the code contradicts one, the
   promise is false — check them again before any change to logging or tracking.
4. Re-check Gate 2 (money correctness) after **any** change to the webhook, the
   ledger, or billing.
