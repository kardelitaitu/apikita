# Terms of Service — Draft Outline

**Status: DRAFT OUTLINE, not legal advice.** This specifies what the terms must
cover, based on decisions already made in the other documents. Have a lawyer review
before publishing — particularly the cross-border and refund clauses, which carry
real exposure in Indonesia.

> Sources: [`data-retention.md`](data-retention.md) (what is held),
> [`business/05-risk.md`](business/05-risk.md) (R1, R4),
> [`website/04-payments.md`](website/04-payments.md) (payment flow),
> [`architecture.md`](architecture.md) (cross-border forwarding).

## Why this is not optional

Three things the business does that customers must be told **before** they pay:

| Fact | Why it must be disclosed |
| --- | --- |
| **Prompts are forwarded to a provider in mainland China** | Materially affects the customer's data |
| **Deposits are non-refundable** | A payment term; cannot be introduced after the fact |
| **Credit expires 2 years after deposit** | A term that extinguishes value; must be disclosed before it can be relied on |
| **Unused credit is paid out if the service closes** | A commitment on our side; stating it builds trust and reduces dispute exposure |
| **Prompts are not logged by us, but the upstream's retention applies** | Otherwise "we do not store your data" is misleading |

**The third is the one operators most want to skip, and the one that causes the
worst disputes.** Say it plainly.

## 1. The service

- What it is: an API gateway that forwards requests to third-party model providers.
- What it is not: a model provider, a data processor, or a storage service for
  customer content.
- **We are a conduit.** The customer's relationship with the model's behaviour,
  accuracy, and availability is indirect.

Draft language:

> apikita is a routing and billing gateway. Inference is performed by third-party
> providers. We do not generate, review, or store prompts or completions.

## 2. Accounts and eligibility

- Age and capacity to contract.
- Accurate registration information.
- **One account per person or organisation.**
- Account credentials are the customer's responsibility.
- We may suspend an account for violation (see section 7).

## 3. Credits, deposits, and billing

This is the most commercially important section.

| Term | Position |
| --- | --- |
| Prepaid model | Credit is bought before use |
| **Non-refundable** | **Deposits are not refundable for change of mind** |
| Billing basis | Per token, three classes priced separately |
| Rate changes | We may change prices with notice |
| Minimums | A first-deposit minimum and a re-top-up minimum apply |

### The refund clause — settled: non-refundable during operation, no exception

**Decided: deposits and unused credit are non-refundable, with no non-delivery
carve-out.** The earlier draft carried an exception returning unused credit where
service could not be provided. That exception is **withdrawn** — see
[`decisions.md`](decisions.md) §Money.

Draft language:

> All deposits and unused credit are non-refundable. No refund is provided for
> change of mind, for unused credit, or for any other reason, except as provided
> in the wind-down clause below.

The system matches this: there is **no code path that returns money in response to a
customer request**, and no self-serve refund. An inbound Midtrans
`refund`/`partial_refund` notification is refused and changes nothing (see
[`server/api-spec.md`](server/api-spec.md) §`POST /webhooks/midtrans`).

**Scope — this clause governs normal operation, not wind-down.** If *we* decide to
stop operating the service, we pay balances back. That is a different thing: the
customer did not ask, and we are not declining. See §Wind-down below.

> ⚠️ **Open legal risk — flagged, not resolved.** Withdrawing the carve-out was a
> deliberate business decision, and the legal consequence is **not settled**:
>
> - A clause that overreaches is more likely to be **struck down in its entirety**
>   than a narrow one — so this may reduce, not increase, the clause's protective
>   value.
> - `business/05-risk.md` records that on non-delivery **the payer generally
>   prevails at the dispute stage regardless of the stated policy**, because
>   Midtrans is a domestic rail with a real merchant entity behind it.
> - "Non-refundable" therefore scopes liability to *unwanted* service, not to
>   *undelivered* service. A chargeback returns the money at the rail whether or
>   not these terms permit it.
>
> This is a **lawyer's call and remains a Gate 0 item**. The text above records the
> decision; it does not certify it is enforceable.

### Expiry — settled: credit expires 2 years after deposit

**Decided: unused credit expires 2 years (24 months) from the date it was
deposited.** Settled in [`decisions.md`](decisions.md) §Money; the period is stated
here because it must be disclosed before it is relied on.

Draft language:

> Credit expires 2 years (24 months) after the date of the deposit that created it.
> Expiry is calculated per deposit, not from the account's most recent activity.
> Credit that has expired is no longer usable and is not refundable — except at
> wind-down, where expiry is waived (§Wind-down).

**Per deposit, not per account.** A wallet that receives top-ups over time holds
credit of several ages; the clock runs on each deposit from its own date. The
alternative — expiry from last activity — would silently extend the life of old
credit, which is not what was decided.

> ⚠️ **Open legal risk — flagged, not resolved.** An expiring balance that is never
> refunded is a consumer-protection concern in Indonesia; 2 years is a defensible
> period but the interaction with the non-refundable clause is **not settled**. This
> remains part of the Gate 0 legal review.

**Implemented.** The system expires credit, per deposit, as of
`topups.credit_expires_at`, which is stamped at settlement *in the same statement*
that records the settlement — so a deposit's date and its expiry instant cannot be
made to disagree by a crash. `db::expire_credit` retires each aged deposit (oldest
first, capped by that deposit's own amount and by what the wallet holds) as a negative
`usage`-reasoned ledger row plus a wallet decrement in a single transaction;
`credit_retired_at` makes a second run a no-op rather than a second debit. It runs
from the `usage-purge` binary. `credit_expiry_months = 0` disables the window and
writes NULL, which the sweep reads as "nothing to retire" rather than as long overdue.

Three things this section does **not** claim:

- **Nothing refuses a spend against aged credit between sweeps.** Expiry is applied by
  a periodic sweep (`usage-purge`), not at the point of use: a deposit that aged out
  after the last run is still spendable until the next one. The window is honoured
  to within the sweep's cadence, not to the instant.
- **No notification is sent before credit expires.** The sweep retires it silently.
  That is the part a customer is most likely to experience as a surprise.
- **Expired credit is not refunded.** Which is what the open legal risk above is
  about — implementing the mechanism does not resolve it.

## 4. Acceptable use

Required by [`business/05-risk.md`](business/05-risk.md) R4 — abuse is a named risk with no
current handling.

**Prohibited:**

- Content that is illegal under Indonesian law
- Spam, phishing, malware generation
- Fraud, impersonation, or deception
- Automated abuse of the platform (scraping the gateway, key sharing beyond the
  account)
- Attempting to circumvent rate limits, spend limits, or model allowlists
- Reselling the service without an agreement

**Consequence:** suspension or termination, with unused credit forfeited **only**
where the violation is egregious and permitted by law. Forfeiture for a minor
violation invites a dispute you will lose.

## 5. Data and privacy

The disclosure this whole document hinges on.

| Statement | Accuracy requirement |
| --- | --- |
| We do not store prompts or completions | **Must remain true in code** — see [`data-retention.md`](data-retention.md) |
| We store account, billing, and usage records | True; stated with retention periods |
| **Prompts are forwarded to providers outside Indonesia** | True; must name the jurisdiction |
| The provider's own retention and handling applies | True; outside our control |
| We do not sell personal data | Must remain true |

Draft language:

> Requests are forwarded to third-party model providers, which may process them
> outside Indonesia. We do not retain prompts or completions. Providers may retain
> them under their own policies, which we do not control.

**The customer must be told this before their first request, not in a buried
policy.** A summary at signup and before the first top-up.

## 6. Availability and no warranty

- **No uptime SLA at launch.** Do not promise one you cannot measure or afford.
- Service is provided "as is"; upstream providers may fail or change without notice.
- We are not liable for indirect or consequential loss.

**A specific point worth stating:** a failed or truncated inference consumes credit
for the tokens actually produced. See [`failover.md`](failover.md) — a mid-stream
upstream failure is billed per actual usage, not per customer expectation.

### Billing when the client disconnects mid-answer

**If the client disconnects before the answer finishes, the customer is still billed
for the tokens the provider generated up to that point.**

This follows from the same basis as the point above: billing is for what was produced,
not for what was received. By the time a connection drops, the provider has already
generated tokens for that request, and it reports that usage to us. We read the usage
report and bill against it, rather than discarding it — the work was done and the
provider has charged us for it.

The charge is for tokens **actually generated**, at the rates in section 3. It is not a
charge for the full answer, and it is not a minimum. A disconnect does not create a
refund entitlement.

Draft language:

> Charges are based on the tokens the provider generates for a request. If your
> connection ends before the response is complete, the tokens generated up to that
> point are still billed.

## 7. Suspension and termination

| Trigger | Action |
| --- | --- |
| Acceptable-use violation | Suspend, investigate, then terminate or restore |
| Non-payment | Not applicable — prepaid |
| Suspected fraud or account takeover | Immediate suspension |
| Upstream terminates our access | Service ends — see §Wind-down |
| We cease operating the service | See §Wind-down |

**The last two rows are real scenarios.** The entire supply side could end without
notice (see [`business/05-risk.md`](business/05-risk.md) R1), or we may wind the
service down ourselves. Both end up in the same place, and the honest answer is that
unused credit is **paid back** — there would be nothing left to deliver. That is what
§Wind-down specifies.

### Wind-down — balances are paid out on closure

**Decided: when the service closes, every balance above USD 2.00 is paid out.**
Settled in [`decisions.md`](decisions.md) §Money; the procedure is
[`wind-down.md`](wind-down.md). This is the one thing that overrides the
non-refundable clause above.

Draft language:

> If we decide to stop operating the service, we will give at least **30 days'
> notice**. On the wind-down date every balance is frozen, and we will pay out the
> remaining balance of every account holding **more than USD 2.00**.
>
> - Customers who paid through Midtrans (QRIS) are paid by **bank transfer**, to an
>   account in the customer's own name.
> - Customers whose deposits settled in a stablecoin are paid in **USD stablecoin**.
>
> The USD/IDR rate is the Bank Indonesia **JISDOR** rate on the wind-down date,
> fixed once for all payouts, and each payout is **rounded down** to the nearest
> cent. **Every remaining balance is paid, including balances of USD 2.00 or less** —
> we cover the transfer fee. Section 3 does not limit this section.

**Three things to be precise about:**

- **Why there is no floor in practice.** USD 2.00 names the point above which a payout
  is automatic, but balances at or below it are **discharged on request, with us
  covering the transfer fee** — so no reachable balance is ever stranded. A customer
  who deposits 50,000 IDR and spends down below the threshold has done nothing wrong,
  and "we already gave you nothing" is not a defensible answer. Making this
  unconditional was a deliberate choice: it removes the consumer-protection exposure
  from stacking a payout floor on top of expiry and non-refundability.
- **Which rail decides the payout.** The rail a customer paid on. Anyone who used
  Midtrans is treated as Indonesian and paid by bank transfer. This can never be
  applied retroactively, and it never pays crypto to an Indonesian.
- **Expiry is waived.** Credit that would have expired is paid anyway. The schema
  holds one un-aged balance, so expired and live credit are not distinguishable —
  and clawing credit back at the moment we stop serving would be forfeiture wearing
  a policy's clothes.

> ⚠️ **Legal review deferred — a decision, not an oversight.** The owner has decided to
> operate personally and to defer legal review until turnover approaches **4.8 billion
> IDR/year** (the PP 55/2022 UMKM ceiling, above which a PT and normal corporate tax
> follow). Consequence accepted: at launch there is **no lawyer review, no corporate
> bank account, and no limited liability**. This clause nonetheless **reduces** exposure
> and is the one clause that should not be weakened — re-open the review the moment the
> trigger is in sight. The specific questions are listed in
> [`decisions.md`](decisions.md) §"Genuinely open".

**⚠️ Not implemented.** There is no treasury, no disbursement integration and no
payout code. Closing the service today is a **manual procedure** documented in
[`wind-down.md`](wind-down.md) — deliberately, because a payout path with no traffic
is the same hazard the codebase already removed every callerless money-moving
function for.

## 8. Changes to these terms

- Notice period for material changes.
- Continued use constitutes acceptance.
- **Price changes need clear notice**, or customers will feel ambushed.

## 9. Liability and governing law

- Limitation of liability, capped at amounts paid in a period.
- **Governing law: Indonesian.**
- Dispute resolution path stated.

## 10. Contact and complaints

**NOT YET WRITTEN — this section is a requirements list, not a clause.** It says what must
be true; it does not yet say it, and it names no channel. That distinction matters because
the launch table below asserts an abuse contact is enforced by "Terms + an abuse contact",
and today this document contains **no address, URL or named form anywhere** (checked, not
assumed: a scan for any email or http URL in this file returns zero).

Required before launch:

- A real channel that is monitored.
- Response expectation.
- **A stated path for content or billing complaints**, since both will occur.

**Why it cannot be written from this repository.** The address must be one that is actually
monitored, and no mailbox exists — `docs/abuse-runbook.md` tracks the same item as open,
and there is no `abuse@` anywhere in the tree (checked). Writing a plausible-looking
address into a legal document would be worse than the gap, because the Terms would then
make a promise no one is keeping — the same failure mode this document's own warning about
"a policy claiming something the code does not do" describes.

**The dependency is a decision plus a mailbox**, not code. Until both exist, the launch
table row for the abuse contact is **unmet on the Terms half as well as the operational
half**, and this heading is the place that says so out loud rather than reading as a
finished clause.

## What must be true at launch

| Requirement | Where enforced |
| --- | --- |
| Cross-border forwarding disclosed before first use | Signup + pre-topup notice |
| Non-refundable stated (no exception during operation) | Terms + top-up screen |
| Wind-down payout stated | Terms + top-up screen |
| Acceptable use published and linked | Terms + an abuse contact — **the abuse contact half is NOT written**; see §10 |
| Prompts genuinely not stored | Code review; see [`data-retention.md`](data-retention.md) |
| Retention periods stated accurately | Matches the schema |

**The last two are the ones that rot.** A policy claiming something the code does
not do is worse than no policy, because it is a demonstrable false statement.

## Open items

- [ ] Legal review — **required before launch**, and it must include the wind-down
      clause (see §Wind-down and `decisions.md` §"Genuinely open" for the specific
      legal and tax questions).
- [x] ~~Wind-down payout threshold, classification and rate basis.~~ **Settled:
      above USD 2.00, rail decides the payout, JISDOR rate frozen once.** The
      *runbook* is written; the code is not, and there is no disbursement rail.
- [x] ~~Credit expiry: yes or no, and for how long.~~ **Settled: 2 years from
deposit date, per deposit** (see §Expiry). The *implementation* below is still open.
- [x] ~~Credit expiry **implementation**: a schema field for per-deposit expiry, a
      sweep job, and refusal of a spend against expired credit.~~ **Built, except the
      last clause — and the last clause is deliberate.** `topups.credit_expires_at` is
      stamped at settlement in the same statement that records it; `db::expire_credit`
      retires aged deposits oldest-first and runs in the `usage-purge` binary. There is
      **no spend-time refusal**: expiry is honoured to the sweep's cadence, so credit
      that ages out after the last run stays spendable until the next one. That is a
      decision, not an omission — `try_debit` checking expiry too would be a SECOND
      definition of when credit dies, and two definitions is how a rule stops being
      trusted. See `decisions.md`, "Credit expiry: implementation".
      > **This line said "the code is not written" and was stale.** The column, the
      > sweep and the binary wiring all exist and are tested; only the spend-time
      > refusal is absent, on purpose. Corrected rather than deleted so the next reader
      > can see which half was real.
- [ ] Whether an entity (PT) exists to contract with, or a personal account — see
      [`business/05-risk.md`](business/05-risk.md) R1.
- [ ] Complaint handling process, and who owns it.
- [ ] Notice period for price and term changes.
