# Abuse Response Runbook

What to actually do when someone abuses the platform. Named as a risk in
[`business/05-risk.md`](business/05-risk.md) R4 but never specified — this is the
procedure.

**The stakes are asymmetric.** A single abuse incident can trigger upstream
termination, which ends the business's supply. Responding late is far more costly
than responding over-cautiously.

> **Superseded: identity is Rust-owned.** The Phase 6 identity port has landed —
> `accounts.pb_user_id` is dropped, `POST /auth/exchange` is deleted, the
> PocketBase HTTP client is gone from `server/`, and identity is served natively
> by this crate (`accounts` + `identities`, Argon2id). Where the text below still
> says PocketBase is the current identity provider, this notice governs;
> [`architecture/identity.md`](architecture/identity.md) is the operative
> description.

## What abuse looks like here

Cheap access to capable models attracts specific uses. Expected categories:

| Category | Signal |
| --- | --- |
| Spam generation | High volume, repetitive prompts, short outputs |
| Phishing / scam content | Patterns in prompt **counts** — see the logging limit below |
| Scraped-content farms | Very high token volume, low customer value |
| Key sharing / resale | Many distinct IPs on one key; usage far above the key's pattern |
| Limit circumvention | Repeated limit hits, then new keys, repeatedly |
| Payment abuse | Chargebacks, stolen payment methods, many small top-ups then burst use |
| Credential attacks | Failed logins, link-code redemption attempts |

## The detection problem

**We do not log prompts.** That is a product promise (see
[`data-retention.md`](data-retention.md)) and it means abuse cannot be detected by
reading content.

That is a real limitation, not an oversight. Detection must come from **behaviour**, not
inspection:

| Signal available | What it reveals |
| --- | --- |
| Token volume per account | Volume abuse |
| Output/input ratio | Generation-heavy use (spam-like) |
| Cache-hit ratio | Repetitive prompt patterns |
| Distinct IPs per key | Sharing |
| Key creation rate | Circumvention attempts |
| Top-up then burn pattern | Fraud |
| Upstream error rates | Provider-side complaints about us |

**State this limitation honestly.** If abuse detection requires reading prompts, it
requires a policy change first, not a quiet log.

## Severity and response time

| Level | Example | Response |
| --- | --- | --- |
| **P1** | Upstream notifies us of prohibited use | **Immediately** — hours |
| **P2** | Clear spam/fraud pattern at volume | Same day |
| **P3** | Suspected sharing, limit circumvention | Within a few days |
| **P4** | Consumer complaint about content | Within a week |

**P1 outranks everything else in this document.** An upstream complaint is an
existential event, not a moderation task.

## Response procedure

### Step 1 — Confirm before acting

Gather evidence from **behavioural metrics only**:

1. Pull the account's usage: volume, token class mix, per-key breakdown.
2. Compare against the account's own baseline and against the population.
3. Check the key/IP pattern for sharing.
4. Write down what was observed, with numbers and timestamps.

**Never act on a single metric.** A spike can be a legitimate workload launch.

### Step 2 — Contain, reversibly

**Suspend, do not terminate.** Suspension is reversible; termination is not.

| Action | Effect | Reversible |
| --- | --- | --- |
| Revoke specific keys | Stops that vector | Yes — issue new keys |
| Suspend the account | Stops all use | Yes |
| Reduce limits | Throttles without stopping | Yes |
| Terminate | Ends the relationship | **No** |

**Order matters: revoke the key first, then assess.** Revoking a key stops the
traffic; suspending an account may tip off an attacker before you have evidence.

### Step 3 — Preserve evidence

Record **before** the data ages out:

- Usage aggregates for the window (these are retained; prompts are not)
- Key metadata: creation, revocation, labels
- Top-up history and payment status
- The decision and who made it

**This record is what you show an upstream provider** asking why the traffic
looked the way it did. Without it, you have no defence.

### Step 4 — Decide

| Finding | Outcome |
| --- | --- |
| Violation clear and severe | Terminate. The terms are non-refundable, but **forfeiture is still legally risky** — record the decision and the reason, do not simply keep the balance |
| Violation minor | Warn, restore, monitor |
| False positive | Restore immediately, and note why the signal fired |
| **Uncertain** | **Do not terminate.** Restrict, monitor, gather more |

**Uncertainty resolves toward restriction, not termination.** Terminating a paying
customer on a false positive is a reputational event, and it costs revenue.

### Step 5 — Upstream relationship

If the abuse affects the provider:

1. Proactively inform them, with the evidence record.
2. Explain the action taken.
3. **Demonstrate that the account is stopped.**

**Self-reporting is the single most effective way to keep the supply relationship
alive.** Discovering that a customer is abusing their cheap keys is what gets keys
revoked; being told by you, with evidence, is what makes you a partner.

### Step 6 — Post-incident

- Update detection thresholds if the signal was late or noisy.
- If the vector was structural (e.g. key sharing), consider a product fix.
- Log the incident and the outcome.

## Specific playbooks

### Key sharing

**Signal:** many distinct IPs on one key; usage not matching the account's pattern.

1. Check whether the IPs are plausible (mobile users roam; offices share egress).
   **`distinct_ips` is now recorded per key per day** — see
   [`ip-tracking.md`](ip-tracking.md). Read it before assuming: a mobile user can
   legitimately show 10-50 in a day, so a raw count is not evidence on its own.
2. If it looks like distribution: revoke the key, notify the account, monitor the
   replacement.
3. **Consider whether this is worth policing.** It is a terms violation, but it may
   be a customer with a team and no awareness. A warning is usually correct first.

### Limit circumvention

**Signal:** an account repeatedly creates keys after hitting limits.

1. The limits are working as designed; this is a pricing conversation.
2. **Consider whether the limits are simply too low** — customers work around limits
   that do not fit their legitimate use.
3. Key creation is already capped per account per day
   (`limits.key_creation_per_day`, counted from `api_keys` rows so revoking one to
   mint another does not evade it). Leave the rest to pricing.

**The cap is a hard cap of 10/day; more than 3 is the suspicion threshold**
([`ip-tracking.md`](ip-tracking.md)). A customer creating 5 keys in a day after
hitting a limit is a pricing conversation. Do not treat the hard cap being hit as
proof of abuse on its own.

### Payment abuse

**Signal:** top-up succeed then rapid consumption; small repeated deposits.

1. Freeze the balance; do not let it be consumed while a payment may be reversed.
2. Coordinate with Midtrans.
3. **The non-refundable policy does not protect against a reversed payment** — a
   chargeback removes the money regardless of policy.

### Credential attack

**Signal:** failed logins, link-code redemption failures, enumeration attempts.

1. Rate limits should already contain it — **both halves are ours.**
   Link-code redemption is capped by `limits.link_redemption_per_hour`, enforced in
   `routes/telegram.rs`, and login is capped by the five `[limits]` keys read and
   enforced in `server/src/auth_attempts.rs`: `login_per_hour_per_ip` and
   `login_per_hour_per_account` cover sign-in, and `signup_per_hour_per_ip`,
   `password_reset_per_hour_per_account` and `verification_resend_per_hour` cover the
   surrounding auth surface. Nothing in auth is PocketBase's decision any more, and
   there is no `POST /auth/exchange` to limit.
   This row used to say login was **NOT** contained — that nothing in
   `server/src/routes/auth.rs` called a limiter and that any login limit was
   "configured on the PocketBase side". That was true before the identity port and is
   false now: the counters live in `auth_attempts`, the Rust server enforces them, and
   an operator working a credential attack should confirm the cap in
   `config/apikita.toml` rather than reach for a PocketBase admin UI that no longer has
   anything to configure.
2. If it is sustained, block the source at the **edge relay**, not in the backend.
3. Alert the affected account if there is evidence of a successful attempt.

## What we will NOT do

| Not | Why |
| --- | --- |
| Read customer prompts to investigate | Violates a stated promise; needs a policy change first |
| Terminate without a written record | Indefensible if challenged |
| Silently keep a suspended customer's money | The terms are non-refundable, but silently keeping it is still not defensible — record the forfeiture and tell the customer why |
| Ignore an upstream complaint because it seems minor | The relationship is the business |

## Tooling needed

| Capability | Status |
| --- | --- |
| Per-account usage with token class breakdown | In schema (`usage_daily`) |
| Distinct IPs per key | **Built** — `key_ip_daily`, salted daily hash; see [`ip-tracking.md`](ip-tracking.md) |
| Key creation rate per account | **Enforced** — `limits.key_creation_per_day`, counted from `api_keys` rows |
| Top-up rate per account | **Enforced** — `limits.topup_per_hour`, counted from `topups` rows |
| Account suspend/restore | **Built** — `POST /api/admin/accounts/:id/suspend`, `/restore` (alias `/resume`). Revokes sessions and keys in one transaction, with an audit row. See [`admin-surface.md`](admin-surface.md) §Non-money actions |
| Incident log | **Built** — the suspend/restore action trail (`admin_audit`) is readable per account and across accounts via `GET /api/admin/accounts/:id/audit` and `GET /api/admin/audit` |

**The admin path is no longer a gap.** It was: suspending an account is a required
capability, and at the time this table was written it had no endpoint at all. It now
has one — auditable, in a single transaction, refusing self-action — so an operator
responding to a live incident should use the operator console at `/admin` rather than
any external admin UI. See [`admin-surface.md`](admin-surface.md). There is no
PocketBase admin UI to reach for: that service does not exist.

## Open items

- [x] Admin endpoints with an audit trail — see [`admin-surface.md`](admin-surface.md).
- [x] Distinct-IP tracking — see [`ip-tracking.md`](ip-tracking.md).
- [ ] Thresholds for behavioural abuse signals — **starting values exist** in
      [`ip-tracking.md`](ip-tracking.md) §Abuse signals; they need tuning against real
      traffic before they are trusted.
- [ ] An abuse-report contact, published in the terms. **Two halves, both missing.**
      There is no mailbox (no `abuse@` anywhere in the tree — checked) and no page that
      publishes one: [`terms-of-service.md`](terms-of-service.md) §10 "Contact and
      complaints" is a requirements list, not a clause, and that document contains no
      address, URL or named form at all. §10 now says so explicitly rather than reading
      as a finished clause. Writing a plausible address would be worse than the gap —
      the Terms would promise a channel nobody monitors, which is the exact failure the
      doc's own warning about "a policy claiming something the code does not do" names.
      **Neither half is code**, so this cannot be closed from this repository.
- [ ] Whether abuse detection warrants a limited-retention prompt sampling policy.