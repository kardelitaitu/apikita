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
- [ ] Legal review of [`terms-of-service.md`](terms-of-service.md).
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

- [ ] Signature verified on every request; mismatches rejected and logged.
- [ ] Amount validated against the **stored** `topups` row, never the payload.
- [ ] Crediting is idempotent by `order_id`.
- [ ] Crediting is atomic with the `topups` status update and the ledger row.
- [ ] `refund` and `partial_refund` statuses handled (debit), even though the
      policy is non-refundable.

### Ledger and balance

- [ ] **No client-reachable path can write `balance_idr`.**
- [x] `ledger` is append-only; no UPDATE or DELETE exists in the codebase.
- [x] The reconciliation query returns **zero rows** on production data.
- [ ] `CHECK (balance_idr >= 0)` present and exercised.
- [x] Money is `BIGINT` end to end; no float appears in any billing path.

### Billing accuracy

- [ ] **Cache-read tokens are never counted as input tokens** — test with a payload
      containing cache hits.
- [x] Three token classes stored separately in `usage_daily`.
- [ ] Peak/off-peak basis applied consistently between reservation and settlement.

## Gate 3 — Access control

- [x] Cookie sessions are **rejected** on `/v1/*`; API keys are rejected on
      dashboard endpoints.
- [x] Logout revokes the session row; "sign out everywhere" revokes all of them.
- [ ] Suspension revokes sessions **and** keys atomically.
- [ ] Admin endpoints require the operator flag; an operator cannot act on
      themselves.
- [ ] Every admin action writes an `admin_audit` row in the same transaction.
- [ ] Link-code redemption is rate-limited per account and per IP.

## Gate 4 — Data promises that must be true

**Each of these is a statement made to customers. If the code contradicts it, the
statement is false.**

- [ ] Prompts and completions are never logged, stored, or sent to analytics.
- [ ] No raw IP address is persisted; only a salted hash, salt deleted daily.
- [ ] API keys stored as hashes only; the plaintext exists once, at creation.
- [ ] Session tokens stored hashed; cookies are HttpOnly, Secure, SameSite.
- [ ] `PUBLIC_*` variables contain nothing secret.
- [ ] Backup encryption keys are held separately from the backups.

## Gate 5 — Operational readiness

- [ ] `/health` checks the process and the database, **not** upstream providers.
- [ ] Alerts configured: webhook rejection, ledger drift, API down, circuit open.
- [ ] **A restore drill has been run**, with the reconciliation query passing.
- [ ] Drill log records the measured restore time — that is the real RTO.
- [ ] Error responses match [`error-model.md`](error-model.md), including `request_id`.
- [ ] SSE heartbeat runs; a dropped stream surfaces as a stale indicator, not a
      silently frozen balance.
- [ ] Abuse-report contact published and monitored.

## Gate 6 — Product surfaces

- [ ] Signup shows the cross-border disclosure before the first request.
- [ ] Top-up screen states the fee and the non-refundable policy before payment.
- [ ] Deposit minimums enforced server-side (first vs re-top-up differ).
- [ ] API key shown once, with an acknowledged warning.
- [ ] Telegram `/link` flow works end to end.
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
| An admin UI | Read-only + suspend is enough at launch |
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
| **Credit expiry policy** | Must be stated in the terms before publishing |

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
