# Data Retention & Privacy

What personal data this system holds, for how long, and what it must never keep.

**This is not optional paperwork.** The system stores customer emails, spending
history, and reviews — and forwards customer prompts to a third-party provider in
another jurisdiction. That combination carries obligations regardless of company
size.

> Schema: [`docs/website/02-data-model.md`](website/02-data-model.md)
> Provider residency: [`docs/business/05-risk.md`](business/05-risk.md) R1

> **Superseded: identity is Rust-owned.** The Phase 6 identity port has landed —
> `accounts.pb_user_id` is dropped, `POST /auth/exchange` is deleted, the
> PocketBase HTTP client is gone from `server/`, and identity is served natively
> by this crate (`accounts` + `identities`, Argon2id). Where the text below still
> says PocketBase is the current identity provider, this notice governs;
> [`architecture/identity.md`](architecture/identity.md) is the operative
> description.

## What is stored

| Data | Where | Sensitivity |
| --- | --- | --- |
| Email | Embedded SQLite (`identities` table) | **Personal** |
| Password hash | Embedded SQLite (`identities` table) | Sensitive, but not usable if leaked (hashed) |
| Google account link | Embedded SQLite (`identities` table) | Personal |
| Telegram ID | Embedded SQLite (the database file) | Personal, pseudonymous |
| Wallet balance + ledger | Embedded SQLite (the database file) | **Financial** |
| Payout destination (wind-down only) | Embedded SQLite | **Deleted 30 days after payout** — see §Wind-down |
| Top-up history (amounts, dates) | Embedded SQLite (the database file) | **Financial** |
| Token usage per day | Embedded SQLite (the database file) | Behavioural |
| **Per-request usage** (`usage_events`: model, token counts, cost, time) | Embedded SQLite (the database file) | Behavioural. **Not prompts or completions** — see the "never stored" list. Added when the dashboard gained its recent-requests feed |
| API keys | Embedded SQLite (the database file) | Credentials (hashed) — the plaintext is never stored |
| Reviews + edit history | Embedded SQLite (the database file) | Opinion, published aggregate only |
| Sessions | Embedded SQLite (the database file) | Contains IP hash and user agent |

> **Wording change only — no retention fact moved.** Both stores are now **embedded
> SQLite**, a file the API opens: the money store used to be a managed PostgreSQL
> service, and identity used to be PocketBase. Every row, every retention period and
> every "never stored" claim above is unchanged. **This table is restated on the
> customer-facing [`/privacy`](../website/src/pages/privacy.astro) page**, which must
> be updated in the same change if any of it moves again.

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
| **Usage daily** (`usage_daily`) | 24 months | Billing disputes, then aggregate only. Swept nightly by the maintenance scheduler, in inline SQL |
| **Per-request usage** (`usage_events`) | **90 days** | Outlasts the 30-day rolling spend window plus a dispute window. Swept nightly by the maintenance scheduler in inline SQL (**not** by `cargo run --bin usage-purge`, which is not shipped); the boundary is inclusive, so the cutoff day is deleted and exactly 89 preceding days are kept |
| **Sessions (expired/revoked)** (`sessions`) | 30 days | Tidy up, but keep recent for security review. The window runs from the instant the session STOPPED being usable — `revoked_at` for an early logout, else `expires_at`. Swept nightly by the maintenance scheduler in inline SQL |
| **Reviews** | Until deleted by user | Published aggregate; individual text is theirs |
| **Review history** | Same as review | Needed to make an edit meaningful |
| **link_codes** | Until used or expired + 24h | Telegram account-binding codes. **The window was a promise with no code behind it.** `issue_link_code` deletes only the ONE code it is superseding (`DELETE FROM link_codes WHERE account_id = ?`), so a code a customer requested, never redeemed and never replaced had NO delete path at all — every such row ever issued was still on disk. The nightly sweep now calls `routes::telegram::purge_terminal`, which deletes on `COALESCE(used_at, expires_at)` plus one day (the SQL cutoff `datetime('now', '-1 days')`): the COALESCE is because a redeemed code ages from `used_at` and an unused one from `expires_at`, and comparing only one of them would either keep a redeemed code for a TTL it no longer has or never delete an expired one |
| **Link-redemption attempts** | **7 days** | Salted IP hashes, same class as `key_ip_seen`; enough to investigate a live credential attack, then gone. **The "then gone" was false until this round**: the promise was in this table from before the sweep existed, and nothing deleted a single row. The nightly sweep now covers this table too, through the same instant helper `usage_events` uses |
| **`auth_attempts`** | **7 days** | The credential-guessing counter behind the five `[limits]` `_per_hour` caps. Its IP-keyed rows are salted hashes (the same class as `key_ip_seen`); its account-keyed rows hold no IP-derived data at all — the writer stores the empty string in `ip_hash`, which the column's NOT NULL requires — but they are still a per-account record of who tried to sign in and when, so they take the same 7 days rather than a longer one. Every cap reads a window of one HOUR, so a seven-day-old row is already inert for enforcement. Swept nightly through the same instant helper `usage_events` uses |
| `key_ip_seen` | **7 days** | One salted hash per (key, day, address): enough to see one address spreading a key across many accounts. Purged nightly, and the salt is replaced at each UTC midnight so days cannot be linked |
| `key_ip_daily` | **90 days** | One count per (key, day) — a **trend, not a history**. The individual hashes are gone after 7 days; what survives is a number per day, which is what makes a 90-day view possible without keeping anything linkable |
| **Logs** | 30-90 days | Debugging window; not a database |
| **Accounts (closed)** | Keep record, drop personal data | See below |
| **`identity_tokens`** | **As soon as expired** | Verification and password-reset links. Not "kept for N days" — a link is stale the moment it expires, and the verifier already refuses a row past `expires_at`, so the sweep deletes on `expires_at <= now` with no grace period. **This row did not exist until the sweep did.** The purge function was written with a unit test and NO caller of any kind — no binary, no scheduler entry, not even the inline SQL in the maintenance entrypoint — so every expired link a customer ever asked for was still on disk while this page said otherwise |
| **`link_code_issues`** | **7 days** | The per-account record of Telegram link-code requests refused by the cap. Short, because the cap counts over a window measured in HOURS: a seven-day-old row can no longer refuse anything. **This row did not exist either**, and the table had no production sweep: `ip_tracking::purge_expired` deleted from it, and the maintenance entrypoint — which is what actually runs — did not. Both gaps were found by a check that now compares the Rust sweep's table list to the entrypoint's |

> **Age-based retention is enforced NIGHTLY, but NOT by that binary — and the
> difference matters if you go looking.** `server/src/bin/usage-purge.rs` is not
> shipped in the server image and does not run: the maintenance scheduler
> (`.docker/maintenance/entrypoint.sh`) performs the same three sweeps in inline
> SQL, and logs itself as `WIRED retention` while logging the binary as
> `NOT WIRED usage-purge`. The OUTCOME here is right — the rows are deleted — but a
> reader who checks the mechanism the way this paragraph used to describe it will
> find a binary that is never invoked, and conclude that nothing runs at all. That
> is the opposite of the truth, and it is what `website/src/lib/privacy.ts` said
> for months, to customers.
>
> It sweeps **ten** tables to the periods
> above: `usage_events` (90 days), `usage_daily` (24 months), expired/revoked
> `sessions` (30 days), `key_ip_seen` (7 days), `key_ip_daily` (90 days),
> `link_redemption_attempts` (7 days), `auth_attempts` (7 days), `link_code_issues`
> (7 days), expired `identity_tokens` (no grace period) and terminal `link_codes`
> (used or expired + 1 day). It is idempotent, and
> deliberately NOT on the request
> path — the settlement already writes a row per billed request, and a per-request
> delete would add a second write to the money path to do work that has to happen
> once a day.
>
> **That count is checked, and it is not a guess.** `tools/backup-check/check.sh`
> extracts the table list from the Rust purges (`db::purge_expired_usage`,
> `identity::tokens::purge_expired`, `ip_tracking::purge_expired`) and from this
> entrypoint's own `run_retention`, and fails if the two sets differ in EITHER
> direction. It found two real gaps on its first run, both described in the table
> above, and it is why the count here can be trusted rather than remembered.
>
> **One job, not three.** Three retention jobs would mean three places the policy
> can be forgotten, and that is not hypothetical: all three tables shipped with a
> documented period and **no** purge at all, because the policy lived in this
> document and nothing connected it to the code.
>
> It deliberately does **not** touch `ledger` or `topups` (financial records, kept
> forever), `reviews`/`review_history` (kept until the user deletes them),
> or
> `key_ip_*`/`link_redemption_attempts`/`auth_attempts` (swept by the maintenance
> scheduler in inline SQL, which owns the salted-hash retention and the salt-rotation
> contract; `bin/ip-purge.rs` states the same windows but is not shipped and does not
> run). `link_codes` USED to be on that exclusion list, with the reason "its own
> '+24h after use/expiry' rule is a different shape" — that reason stopped being true
> once `identity_tokens` brought an expires-then-delete table into the sweep, and the
> stale exception was holding the published window while nothing implemented it. The
> link-redemption table was the other gap in that sentence, and it was my error:
> an earlier round corrected a wrong claim here by replacing it with a different wrong
> claim rather than reading the entrypoint. It is now swept, and the sentence is true.
> `auth_attempts` was the second table to join it, and it arrived with no window in this
> document at all — the counter behind the five `_per_hour` caps was written on every
> failed sign-in while this page named neither the table nor a period for it.

**The ledger is never deleted, even when a customer leaves.** It is the record of
money that moved. That is normal accounting, not a retention violation — but it
means anonymisation, not deletion, is the right mechanism for a departing customer.

## Account closure

There is **no hard delete of an account.** From
[`docs/website/02-data-model.md`](website/02-data-model.md): `ON DELETE RESTRICT` on
anything holding money, and accounts are never hard-deleted.

Closure means:

1. `accounts.status = 'closed'` — set **only once the balance is zero** (step 6).
2. **Revoke all sessions** — the user is out.
4. **Retain the ledger and top-ups** (financial record).
5. **Anonymise what can be anonymised** — email replaced with a tombstone in the
   `identities` table, Telegram link removed, review body cleared if requested.
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
| Disclose forwarding in the terms | **WRITTEN, not yet reviewed or published** |
| State the provider jurisdiction | **WRITTEN, not yet reviewed or published** |
| State that prompts are not retained by us | Required, and true |
| Note that the provider's own retention applies | Required — out of our control |

**"Written" and "in force" are different, and the two rows above are the first.** The
text exists in [`terms-of-service.md`](terms-of-service.md): "Prompts are forwarded to a
provider in mainland China" (:19), the jurisdiction requirement named (:156), and the
disclosure promised before first use (:292). What is still open is the legal REVIEW -
explicitly deferred until the 4.8b IDR trigger - and PUBLICATION; both are tracked in
[`launch-checklist.md`](launch-checklist.md):28 and :33. *(Correction: this table once marked
both rows as unwritten, which was the one status that was untrue - a reader tracking
privacy readiness was told a disclosure did not exist when it did. It was measured, not
recalled: the ToS lines cited above are the evidence.)*

**The last row is the uncomfortable one.** We do not store prompts, but the
upstream may. A customer cannot be told "we do not keep your data" without also
being told that someone else might. Say so plainly in the terms.

## Access and deletion requests

Users should be able to:

| Request | Mechanism |
| --- | --- |
| See their data | Dashboard + bot: profile, balance, usage, keys |
| Export it | `GET /api/export` — a JSON download of the customer's own data. **Specified below** |
| Correct it | Edit profile; the ledger is immutable by design |
| Delete account | Closure flow above, anonymising where possible |
| Delete a review | `/review withdraw` — the bot path |

**The ledger is the one thing that cannot be deleted on request**, and the terms
must say so before someone asks. Explaining it after the fact reads as evasive.

### The export — what is IN and what is OUT

Returned by `GET /api/export` (session cookie, the account's own data only).
**The scope is exactly the data the customer can already see or act on**, so the
export makes no new disclosure and needs no new policy decision:

| Included | Why it is the customer's to export |
| --- | --- |
| Account: id, status, created_at | Their account |
| Wallet: balance_idr | Their money |
| Ledger rows (`delta_idr`, reason, ref, balance_after, created_at) | Their transaction history |
| Top-ups: amount, status, order id, created/settled | Their payments |
| Usage: `usage_daily` and `usage_events` rows | Their consumption |
| API key **metadata**: prefix, label, models, limits, expiry, revoked/last-used | Their configuration |
| Telegram link state (linked: true/false) | Their linked surface |

| **Excluded** | **Why** |
| --- | --- |
| `key_hash`, `token_hash` | Credentials/internal ids. **Hashes are not the customer's data to hold** — handing them out is an attack surface for no user benefit |
| Session rows and IP hashes | Security records; `docs/ip-tracking.md` keeps IPs as salted hashes precisely so they are not exported |
| `admin_audit` rows | Whether these reach the customer is a **separate open decision** (see [admin-surface.md](admin-surface.md) Open items). The export does not pre-empt it |
| Password hash, Google identity (`identities` rows) | We hold these, but they are credentials: the password hash is never handed out, and the Google link is a provider subject, not the customer's data to export. See §Security obligations |

**"Metadata, not secrets" is the rule.** The export is for the customer's own
records (tax, accounting, migration); it is never a channel that reveals a
credential or a security signal that was deliberately hashed.

## Security obligations that follow

| Requirement | Where |
| --- | --- |
| Passwords hashed (Argon2id, owned by the Rust API) | [`docs/decisions.md`](decisions.md), [`docs/website/05-security-decisions.md`](website/05-security-decisions.md) |
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
