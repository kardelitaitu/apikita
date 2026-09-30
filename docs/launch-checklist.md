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

- [x] Read the resale terms of the intended upstream provider, in full.
      **Done by direct conversation with the reseller, not by reading a posted
      document.** The provider is a reseller with no published resale-terms page, so
      the terms were settled in the negotiation itself. The outcome is recorded in
      [`config/provider1.md`](../config/provider1.md): resale is permitted, and the
      account carries **10x the concurrency limit of an ordinary account** to start.
- [x] Record the outcome in [`config/provider1.md`](../config/provider1.md).
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
- [x] Identity served by the Rust server itself, with no separate identity
      instance running. **Phase 6 has landed.** The code half is done and is the
      shipped state rather than an end state to reach: `accounts.pb_user_id` is
      dropped, the PocketBase HTTP client is gone from `server/`, and `accounts` +
      `identities` are served natively with Argon2id. See
      [`architecture/identity.md`](architecture/identity.md).
      The tick is for the CODE half only. The operational half — *no separate
      identity instance is running in the deployment* — is verified by deploying
      and observing, which is Gate 1's remaining work; there is nothing left in
      this repository that could make it true or false.
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
- [x] **Alerting on that refusal** — previously the ONE outstanding code-shaped item
      in this group. `server/src/routes/webhooks.rs` now emits `event = "refund.refused"`,
      a DISTINCT name from `topup.rejected` because they mean opposite things (a
      rejection is a failed payment that may owe money; a refusal is a refund declined
      by policy, i.e. the system working — sharing the name would page on routine
      enforcement and hide a refusal spike inside a rejection count). `alerts.tsv` carries
      the entry and `probe.sh --check refund_refusal` scans for it with its OWN
      line-offset marker, so it and `topup.rejected` cannot consume each other's events.
      Why alert on correct behaviour: a refusal is the one webhook outcome where NOTHING
      moves, which is also what a status-mapping regression routing real events into this
      arm would look like. Tested
      (`a_refund_refusal_is_logged_under_its_own_documented_event`, mutation-verified
      against an event rename), and the probe's marker behaviour was exercised end to end:
      alerts once, reports 0 on the next scan, and leaves the rejection check still seeing
      its own event.

### Ledger and balance

- [x] **No client-reachable path can write `balance_idr`.**
- [x] `ledger` is append-only; no UPDATE or DELETE exists in the codebase. **Enforced** by
  `money::tests::ledger_is_append_only::no_source_statement_mutates_the_ledger`, which scans
  every `.rs` under `server/src` and fails on either statement. It was true by inspection
  before that test existed, and the scan asserts it read the tree so it cannot pass
  vacuously.
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
> | Signature verified | `verify_midtrans_signature` in `money.rs` compares with `ct_eq` over SHA-512, pinned by known-vector tests |
> | Stored-amount validation | the `find_by_order_id` lookup in `db.rs` matches `order_id AND status AND amount_idr`, and `handle_midtrans_webhook` handles `AmountMismatch` |
> | Idempotent crediting | the conditional UPDATE above is the claim; replay test asserts exactly one credit |
> | Atomic credit | one `BEGIN IMMEDIATE` covers the status change, the wallet and the ledger row |
> | Refunds refused | `evaluate_payment_status` in `money.rs` returns `PaymentAction::RefundRefused`; two live tests, incl. an inflated amount |
> | No route writes `balance_idr` | all 7 UPDATE sites are in `db.rs` (transactional) or `bin/hold-sweep.rs` (operator tool) |
> | `CHECK (balance_idr >= 0)` | `migrations/20260925000000_initial_schema.sql:66` |
> | Cache-read not counted as input | `parse_usage_from_sse` in `upstream/client.rs` clamps with `cached.min(prompt)` so a malformed report cannot push the split negative; `a_cached_token_is_never_also_billed_as_an_input_token` asserts the arithmetic |
>
> **The refund-alerting box is explicitly OUTSIDE this group and left unticked**,
> because it is the one part that is genuinely outstanding: the event is logged but
> nothing watches it, and it is an ops task rather than a code defect.

## Gate 3 — Access control

- [x] Cookie sessions are **rejected** on `/v1/*`; API keys are rejected on
      dashboard endpoints.
- [x] Cookie sessions are **rejected** on `/v1/*`; API keys are rejected on
      dashboard endpoints.
      *The first half is stronger than "rejected", and the difference matters before anyone
      audits it.* `server/src/routes/proxy.rs` reads the credential from the
      `Authorization: Bearer` header and **nothing else**: there is no code path in the
      proxy that consults a cookie, so a session cookie cannot be accepted because there is
      nowhere it would be read. "Rejected" describes a check that could be removed;
      "never read" describes an attack surface that does not exist. Verifying this item means
      looking for the ABSENCE of cookie handling in the proxy rather than for a 401, and a
      reader who went looking for a rejection would find the guarantee, then wonder whether
      the check enforcing it was tested. That is the question this note removes.*
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
      **Enforced by a test**, not a review: `the_salt_is_stable_within_a_day_and_replaced_across_days`
      (`server/src/ip_tracking.rs`) asserts the same day yields the same salt, the next day
      mints a new one, and the replacement is then stable for the rest of that day - so the
      property that matters is "a hash from yesterday cannot be recomputed today", and all
      three halves of it are checked.
      The module doc of `ip_tracking` states the rest — "**no raw IP is stored anywhere**. What
      is stored is an HMAC ... under a salt that changes every day and is never written
      down" — and `DailySalt` documents the salt as living in memory, from the OS RNG,
      replaced at the UTC boundary. The column names are checked as well, so a migration
      cannot quietly add a raw address beside them.
      *This item used to cite only those documents, which understated it: a reader auditing
      this checklist would conclude the rotation was inspection-only and re-verify it by
      hand — the exact work the test was written to remove.*
- [x] API keys stored as hashes only; the plaintext exists once, at creation.
      `create_key` in `server/src/routes/keys.rs` calls `hash_token` on the full key
      BEFORE the insert; only the hash is bound into the row.
- [x] Session tokens stored hashed; cookies are HttpOnly, Secure, SameSite.
      The cookie builder in `session_cookie` (`server/src/routes/auth.rs`) sets all
      three, and `session_cookie_carries_expected_attributes` asserts the serialized
      cookie CARRIES them, so the flags cannot be dropped silently.
- [x] `PUBLIC_*` variables contain nothing secret.
      No `PUBLIC_*` key is defined in `server/` or `config/`; the single mention is a
      comment in `server/src/routes/account.rs` about the browser cross-checking its
      own value. **Enforced for the case that actually leaks** by
      `website/tests/public-secrets.test.ts`: it pins the set of `PUBLIC_` variables the
      build can inline (only ones the source references are substituted) and fails if any
      of them is named like a secret. That gap was real — the CI Secret scan checks
      SOURCE files, while this mistake only exists in the BUILT output. Measured: a value
      planted in `PUBLIC_API_BASE_URL` appears verbatim in `dist/_astro/errors.*.js`
      with the build exiting 0.
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
      The `### Health` section of `docs/server/api-spec.md` states the rule and why (an upstream outage must
      not look like a dead server and trigger a restart loop). Tested, including the
      unauthenticated-body leak rules.
<!-- alert-scheduling: wired -->
- [ ] Alerts configured: webhook rejection, ledger drift, API down, circuit open.
      **Every one of these is now CHECKED by `tools/alert`** — see `alerts.tsv`: **10 of its 11
alerts are `covered`**, and the one exception is `relay_5xx`, which is `needs-metrics` because
it cannot be derived from the database or `/health` alone. (This line said "9 of 10" until it
was measured: the table has grown since, and nothing tied the sentence to the file - see
`todo.md`, which now cites the same numbers.) "Configured" also means a delivery channel and
a schedule, which are deployment decisions. **The code half is done AND THE COUPLING IS NOW IN PLACE** -
`run_wired_jobs` invokes BOTH alert jobs every night alongside retention, reconcile and
hold-sweep: `run_alert_checks` evaluates the database-backed alerts and `run_alert_probes`
the HTTP ones, with the relay addressed by service name because `127.0.0.1` inside the
container is its own loopback. **What still keeps this box unticked is the CHANNEL**: no
delivery channel is configured, so a breach is reported as UNMONITORED rather than
delivered — which is a deployment decision, and the run says so out loud rather than
reporting an unmonitored night as clean.

*(Correction: this line once asserted that NO SCHEDULE ran the alert checks. That was
true when written and was invalidated by the two rounds that wired the jobs - the note
existed to state what remained, and it was not revisited when the remaining thing got
done. Kept as a note so the next reader knows the claim was checked, not paraphrased.)*
- [ ] **A restore drill has been run**, with the reconciliation query passing. **The TOOL
      is verified; the PRODUCTION run is not.** I exercised `tools/drill/drill.sh` end to
      end against a synthetic migrated source: a full drill PASSED (exit 0) with
      `reconcile.sh` reporting zero drifting rows and the spot-check comparing a real
      wallet across source and restored copy. I also proved it DETECTS bad backups rather
      than only ever passing — a corrupted artifact and a plausible-looking TRUNCATED one
      both FAIL with exit 6. **What remains is running it against production data**, which
      needs a deployed database and is why this box stays unticked:
      The RTO section of `docs/backup-and-restore.md` records that the only measured RTO so far is
      "**dev-sized**... the production number is unmeasured until the drill runs there."
- [ ] Drill log records the measured restore time — that is the real RTO. **The log DOES
      record it** (verified: `restore_ms 304` and `118` on my runs, alongside the result and
      the artifact SHA-256), so the mechanism works. The number to record is a PRODUCTION
      one; a dev-sized figure would be a claim we cannot stand behind.
- [x] Error responses match [`error-model.md`](error-model.md), including `request_id`.
      `error.rs` mints `req_<uuid>` per failure and puts it in the
      body; `error_object` in that file is the helper every error test reads the body
      through, and it fails unless the shape is exactly
      `{error: {code, message, request_id}}` — so the shape is asserted by every error
      test rather than by one that could be skipped.
- [x] SSE heartbeat runs; a dropped stream surfaces as a stale indicator, not a
      silently frozen balance.
      **Now enforced by tests on both halves.** Server: `HEARTBEAT_SECONDS` in `events.rs` sets a 25s
      heartbeat (docs/realtime.md asks for 20-30s) and `:550` pins that window.
      Frontend: the events store in `live.ts` marks itself `stale` on error, and
      `a dropped stream keeps the last balance, marks it stale, and never relabels
      polled data live` (`website/tests/live.test.ts`) asserts the last KNOWN balance
      survives and that polled data is never presented as live — mutation-verified in
      both directions.
- [ ] Abuse-report contact published and monitored.

## Gate 6 — Product surfaces

- [x] Signup shows the cross-border disclosure before the first request.
      The signup page (`website/src/pages/signup.astro`) carries a "Where your prompts go" block
      above the submit control, naming the mainland-China provider and the retention
      that is outside our control. **Pinned by a test**:
      `signup discloses where prompts are forwarded, and does so before the submit
      control` (`website/tests/landing-claims.test.ts`) asserts presence, the named
      jurisdiction, AND the ORDERING — because the claim is "before the first request",
      not "exists somewhere" (`privacy.astro` alone would not satisfy it).
      **Was provisional, and no longer is.** This box was ticked while the disclosure
      text and its test were already correct but the FORM around them still belonged to
      a different identity provider, so the claim "before the first request" was pinned
      only for the disclosure. The port has since closed that gap: the signup script
      calls `website/src/lib/auth-api.ts` (`signupRequest`, `googleSignIn`) and
      `website/src/lib/auth-flow.ts`, `website/src/lib/pocketbase.ts` is deleted, and
      the crate serves identity itself. No identity service is part of this deployment,
      so the box holds end to end with nothing outstanding behind it.
- [x] Top-up screen states the fee and the non-refundable policy before payment.
      The wallet page (`website/src/pages/dashboard/wallet.astro`) states the non-refundable policy
      and the 2-year expiry, and the header flags that first-deposit and top-up minimums
      differ. **Pinned by a test** (`the wallet states the non-refundable policy and the
      expiry before the top-up action`).
- [x] Deposit minimums enforced server-side (first vs re-top-up differ).
      `check_deposit_limit` in `server/src/routes/account.rs` selects
      `min_first_deposit` when `settled_topups == 0`, else `min_topup`, and returns a 422
      naming the limit. Covered by live tests in the same module.
- [x] API key shown once, with an acknowledged warning.
      Behaviour: `plaintextKeyOf` (`website/tests/dashboard-form.test.ts`) pins that
      only a create response can reveal a key and a missing one is never invented.
      Warning: the new-key page (`website/src/pages/dashboard/keys/new.astro`) says the key is shown
      **once** and that the server stores only its SHA-256 hash — **pinned by a test**,
      because a copy edit can delete the warning while the code stays correct.
- [x] Telegram `/link` flow works end to end **on the server side** — issue,
      redeem, unlink and re-link are implemented and covered by live tests. The
      Telegram *bot* itself is still design-only (`telegram/README.md` has no code),
      so the flow has not been exercised against the Bot API.
- [x] Review flow creates once and edits thereafter; withdrawal is a flag.
      **Closed by serving the flow to the customer's own session, not by the bot.**
      The three properties the gate asked for are properties of the `reviews`
      table, which has carried them since the initial migration: "creates once" is
      `reviews_account_uniq`, "edits thereafter" is the `review_history` copy
      written in the same transaction as the overwrite, and "withdrawal is a flag"
      is `withdrawn_at`. `server/src/routes/reviews.rs` is the writer. The gate was
      blocked on code, never on the data model — see "The Telegram bot is
      deferred" below.
- [x] Telegram top-up feed posts on settlement only, never on creation.
      **Restated, because the Telegram half of it is deferred with the bot.** What
      the gate protects is that a top-up is announced only once it SETTLES; that is
      enforced today on the surviving channel. `server/src/routes/webhooks.rs`
      publishes a live-balance event gated by `credit_balance_to_publish`, which
      returns `Some` only for `TopupCreditResult::Settled`, and the website
      consumes it over SSE (`website/src/lib/live.ts`). A Telegram feed would be a
      second channel for the same event, so building it later does not reopen this
      item — it inherits the gate rather than duplicating it.

### The Telegram bot is deferred

The two items above were the only CODE items left on this list, and both ran
through a Telegram bot that does not exist: `telegram/` holds a README and
nothing else, and the bot was never written. Rather than leave two gates hanging
on an unbuilt component, the product decision is recorded here:

- **The bot is formally deferred.** `docs/telegram/README.md` remains the
  specification for it, and `docs/server/api-spec.md` marks the three
  bot-token endpoints it would need as designed-not-built. `POST /api/bot/link`
  is built and stays built — the `/link` flow is server-side complete.
- **Reviews are served to the website**, replacing the bot-as-only-writer rule.
  This is strictly stronger on the point that mattered: the bot had already
  authenticated the chat, so a `telegram_id` in the request body was safe there;
  under cookie auth the identity can only come from the session.
- **The top-up feed keeps its existing channel** (SSE on settlement), which is
  what the gate actually asserts.

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
| **Wind-down runbook exercised** | Policy settled (balances above USD 2.00 paid out) and the runbook is written, but the payout is manual and has never been executed. It touches money and there is no treasury — so it is **not** a launch blocker on the same footing as a deployment, but it must be walked through against a scratch database before it is ever needed. **The read-only half is now a tool** (`tools/wind-down/report.sh`, CI-checked): it runs Step 3's eligibility query, splits the threshold strictly and floors the stablecoin units. **What remains unexercised is Step 5 and after** — the manual transfer, the ledger row and the verification |

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
5. **GATE 1 CANNOT BE TICKED FROM THIS REPOSITORY, and that is why it is said here once
   rather than per item.** Every Gate 1 box is the state of a deployment - a volume, a
   certificate, a proxy, a backup schedule - and none of them leaves a trace in git. A
   reader auditing this checklist will go looking for the evidence, find none, and have to
   decide whether the item is a lie or simply external. It is external. The repository
   holds the CONFIGURATION for several of them (`.docker/nginx/relay.conf`, `tools/backup/`),
   and the check tools verify that configuration: `relay-check` proves the relay is
   configured as written and `backup-check` proves the backup script runs. Neither proves
   anything is deployed. Configuration is checked; deployment is not, and no test in this
   repository can make it so.

