# Data Retention & Privacy

What personal data this system holds, for how long, and what it must never keep.

**This is not optional paperwork.** The system stores customer emails, spending
history, and reviews — and forwards customer prompts to a third-party provider in
another jurisdiction. That combination carries obligations regardless of company
size.

> Schema: [`docs/website/02-data-model.md`](website/02-data-model.md)
> Provider residency: [`docs/business/05-risk.md`](business/05-risk.md) R1

## What is stored

| Data | Where | Sensitivity |
| --- | --- | --- |
| Email | PocketBase | **Personal** |
| Password hash | PocketBase | Sensitive, but not usable if leaked (hashed) |
| Google account link | PocketBase | Personal |
| Telegram ID | Embedded SQLite (the database file) | Personal, pseudonymous |
| Wallet balance + ledger | Embedded SQLite (the database file) | **Financial** |
| Payout destination (wind-down only) | Embedded SQLite | **Deleted 30 days after payout** — see §Wind-down |
| Top-up history (amounts, dates) | Embedded SQLite (the database file) | **Financial** |
| Token usage per day | Embedded SQLite (the database file) | Behavioural |
| API keys | Embedded SQLite (the database file) | Credentials (hashed) — the plaintext is never stored |
| Reviews + edit history | Embedded SQLite (the database file) | Opinion, published aggregate only |
| Sessions | Embedded SQLite (the database file) | Contains IP hash and user agent |

> **Wording change only — no retention fact moved.** The money store used to be a
> managed PostgreSQL service; it is now **embedded SQLite**, a file the API opens.
> Every row, every retention period and every "never stored" claim above is
> unchanged. **This table is restated on the customer-facing
> [`/privacy`](../website/src/pages/privacy.astro) page**, which must be updated
> in the same change if any of it moves again.

## What is NOT stored

**The most important list in this document.**

| Not stored | Why |
| --- | --- |
| **Customer prompts** | We are a proxy, not a data processor. Logging prompts makes us one without consent |
| **Model completions** | Same |
| **Raw IP addresses** | Only a hash, for abuse correlation |
| **Card or payment credentials** | Midtrans handles payment; we never see them |
| **Plaintext API keys** | Shown once, then only the hash |
| **Midtrans server key** | Not customer data, but never logged either |

**The prompts/completions rule is a product promise.** Customers send prompts
through a proxy precisely because it should be a pipe. Storing them changes the
relationship and becomes a liability the moment a breach occurs.

**Enforce it in code review.** It is the kind of thing that gets added
"temporarily" for debugging and never removed. See
[`docs/observability.md`](observability.md).

## Retention periods

| Data | Keep | Rationale |
| --- | --- | --- |
| **Ledger** | **Forever** | Financial record; it is the authoritative audit trail |
| **Top-ups** | **Forever** | Financial; matches the ledger |
| **Usage daily** | 24 months | Billing disputes, then aggregate only |
| **Sessions (expired/revoked)** | 30 days | Tidy up, but keep recent for security review |
| **Reviews** | Until deleted by user | Published aggregate; individual text is theirs |
| **Review history** | Same as review | Needed to make an edit meaningful |
| **link_codes** | Until used or expired + 24h | Then delete |
| **Link-redemption attempts** | **7 days** | Salted IP hashes, same class as `key_ip_seen`; enough to investigate a live credential attack, then gone |
| **Logs** | 30-90 days | Debugging window; not a database |
| **Accounts (closed)** | Keep record, drop personal data | See below |

**The ledger is never deleted, even when a customer leaves.** It is the record of
money that moved. That is normal accounting, not a retention violation — but it
means anonymisation, not deletion, is the right mechanism for a departing customer.

## Account closure

There is **no hard delete of an account.** From
[`docs/website/02-data-model.md`](website/02-data-model.md): `ON DELETE RESTRICT` on
anything holding money, and PocketBase users are never hard-deleted.

Closure means:

1. `accounts.status = 'closed'` — set **only once the balance is zero** (step 6).
2. **Revoke all sessions** — the user is out.
4. **Retain the ledger and top-ups** (financial record).
5. **Anonymise what can be anonymised** — email replaced with a tombstone in
   PocketBase, Telegram link removed, review body cleared if requested.
6. **Keep the balance row at zero.** A closed account with a balance is unresolved
   money — do not close until it is zero.

**Never close an account that still holds a balance.** Non-refundable policy
covers an unwanted service; it does not let you keep funds for a service you are
refusing to provide.

### Wind-down

If **we** stop operating the service, the balance is not merely left zero — it is
**paid back**. That is a different event from a customer closing their own account:
platform-initiated, and it discharges the obligation rather than declining it. The
threshold, classification and rounding are settled in
[`decisions.md`](decisions.md) §Money; the procedure is
[`wind-down.md`](wind-down.md).

Two consequences for retention:

| Data | Retention |
| --- | --- |
| **Payout destination** (bank code, account number, holder name, or wallet address) | Collected on request and re-confirmed inside the notice window. **Deleted 30 days after the payout completes** — it is PII with a short life |
| **The payout reference** (`ledger.ref`, e.g. `closure_<run_id>`) | **Kept indefinitely** — it is a financial record, and the ledger is the business |

The **re-confirmation requirement** is not optional. Closure revokes the only contact
channel (step 2), and bank details older than the closure window are stale — merged
banks, closed accounts. Pay only to a destination confirmed inside the window, and only
to an account in the customer's own name: a bounced transfer is recoverable, a
wrong-account transfer is not.

## The cross-border question

**Every prompt is forwarded to a mainland-China provider.** That is the business
model, and it is the single largest privacy obligation in the system.

| Obligation | Status |
| --- | --- |
| Disclose forwarding in the terms | **Required, not yet written** |
| State the provider jurisdiction | **Required, not yet written** |
| State that prompts are not retained by us | Required, and true |
| Note that the provider's own retention applies | Required — out of our control |

**The last row is the uncomfortable one.** We do not store prompts, but the
upstream may. A customer cannot be told "we do not keep your data" without also
being told that someone else might. Say so plainly in the terms.

## Access and deletion requests

Users should be able to:

| Request | Mechanism |
| --- | --- |
| See their data | Dashboard + bot: profile, balance, usage, keys |
| Export it | A machine-readable export (not yet specified) |
| Correct it | Edit profile; the ledger is immutable by design |
| Delete account | Closure flow above, anonymising where possible |
| Delete a review | `/review withdraw` — the bot path |

**The ledger is the one thing that cannot be deleted on request**, and the terms
must say so before someone asks. Explaining it after the fact reads as evasive.

## Security obligations that follow

| Requirement | Where |
| --- | --- |
| Passwords hashed (bcrypt, via PocketBase) | [`docs/website/05-security-decisions.md`](website/05-security-decisions.md) |
| API keys hashed (SHA-256) | [`docs/website/02-data-model.md`](website/02-data-model.md) |
| Session tokens hashed, HttpOnly cookie | [`docs/architecture.md`](architecture.md) |
| Backups encrypted, off-host | [`docs/deployment.md`](deployment.md) |
| No secrets in logs | [`docs/observability.md`](observability.md) |

## Open items

- [x] Terms of service outline — see [`terms-of-service.md`](terms-of-service.md).
- [ ] Legal review of that outline (required before launch).
- [ ] A privacy policy stating exactly what is held and for how long.
- [ ] Data export format.
- [ ] Confirm whether Indonesian law imposes a breach-notification duty, and by
      when.
- [ ] Whether usage beyond 24 months should be kept in aggregate only.
