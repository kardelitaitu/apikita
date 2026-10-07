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

**Built, and RUNNING.** The system expires credit, per deposit, as of
`topups.credit_expires_at`, which is stamped at settlement *in the same statement*
that records the settlement — so a deposit's date and its expiry instant cannot be
made to disagree by a crash. `db::expire_credit` retires each aged deposit (oldest
first, capped by that deposit's own amount and by what the wallet holds) as a negative
`usage`-reasoned ledger row plus a wallet decrement in a single transaction;
`credit_retired_at` makes a second run a no-op rather than a second debit.
`credit_expiry_months = 0` disables the window and writes NULL, which the sweep reads as
"nothing to retire" rather than as long overdue.

**The caller is now shipped and scheduled.** `db::expire_credit` is called from
`server/src/bin/usage-purge.rs`; that binary is built and copied into the server image
(`server/Dockerfile`), and `.docker/maintenance/entrypoint.sh` runs it nightly as the
`credit-expiry` job, which refuses loudly and exits non-zero if the binary is missing.
It is the only maintenance job that is **not** inline SQL, deliberately: the sweep is a
guarded debit plus a ledger row carrying `balance_after` — the ledger invariant — and
re-expressing that in shell would be a second copy of the one thing the money model
rests on. Its other sweep (`purge_expired_usage`) repeats five DELETEs the retention job
already performs inline; they are idempotent, so that is waste rather than harm.

**This paragraph previously read "Built, and NOT RUNNING", and the correction is the
point.** Until this job existed the sweep had seventeen tests, a stamped window and no
runner in production, so the term was disclosed and not enforced — a distinction this
repository draws elsewhere between a tool that exists and a tool that runs. Anything
that read the old wording is now out of date in the direction that mattered.

Three things this section does **not** claim:

- **Nothing refuses a spend against aged credit between sweeps.** Expiry is applied by
  a periodic sweep (the `credit-expiry` job), not at the point of use: a deposit that
  aged out after the last run is still spendable until the next one. The window is
  honoured to within the sweep's cadence, not to the instant.
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

> 🚫 **NOT PUBLISHABLE AS IT STANDS — the bracketed tokens below are placeholders, not
> values.** This section is now written as a clause, but it cannot be published until
> every `[[...]]` token in it is replaced with a real value. A reader or a release
> script should treat any surviving `[[...]]` token in this file as a **launch blocker**,
> not as typography. See "Before publishing this section" below.

**10.1 Who you are contracting with.** These terms are between you and
**[[OWNER_LEGAL_NAME]]**, operating the apikita service. Our contact details are set out in
10.2.

**10.2 How to contact us.** You may contact us at **[[ABUSE_EMAIL]]**. This is the single
monitored address for the service: use it for abuse reports, content complaints, billing
complaints and security reports alike. Label the subject line with the category of your
report where you can, so it is routed correctly.

**10.3 We will acknowledge and respond.** For an abuse or content report, we respond within
the periods below. They are the response times in
[`abuse-runbook.md`](abuse-runbook.md) §"Severity and response time" and are not restated
here as a separate or different commitment.

| Your report | Our response |
| --- | --- |
| An upstream provider notifies us of prohibited use | **Immediately** — hours |
| A clear spam or fraud pattern at volume | Same day |
| Suspected key sharing or limit circumvention | Within a few days |
| A consumer complaint about content | Within a week |

**Those four are the runbook's four.** There is no fifth row here on purpose: the runbook
classifies abuse and content reports, and it does not classify billing complaints or data
requests. **10.6 and 10.7 therefore state no response time**, and an earlier draft of this
section gave them one — "within a few days", borrowed from the third row above. That would
have been a service level invented here for the convenience of having a number in every
cell, in a document whose own launch table warns about "a policy claiming something the
code does not do". If a response time for billing or data requests is wanted, it belongs in
the runbook first and this table second.

**10.4 A response is not an outcome.** What we will do about a report is governed by
[`abuse-runbook.md`](abuse-runbook.md) §"Response procedure" and §"What we will NOT do",
and the consequences of an acceptable-use violation by §4 above. In particular: we
**suspend before we terminate**, because suspension is reversible; we do **not** read your
prompts to investigate a report, because not storing them is a promise this document makes
in §5; and where a finding is uncertain we restrict and monitor rather than terminate. A
report from you does not by itself entitle you to a particular action against another
account.

**10.5 Content complaints.** If content produced through an account breaches §4
(Acceptable use), report it to the address in 10.2 with the account or key involved, the
time, and what was produced. **Report content, not prompts** — we hold no prompt log to
check a claim against, so a report is assessed from behavioural records (volume, key and
IP patterns, usage aggregates), not from the text. Do not send us a third party's personal
data that the report does not require. Where a report is upheld, the action taken is one of
those in 10.4. A complaint about content that the **upstream provider** generated is in
part outside our control: §1 (The service) and §5 (Data and privacy) explain that we route,
and the provider's own handling applies.

**10.6 Billing complaints.** Send billing complaints to the same address, quoting the order
id, the top-up date and — if you can — the ledger entry. We will check the ledger against
the payment rail and correct anything that is genuinely wrong on our side: a settled
top-up that did not credit, a charge for tokens the provider did not generate, or a
misapplied limit. **Three outcomes are settled policy and will not change on complaint**:

- **Deposits and unused credit are non-refundable during operation, with no exception**
  (§3). A billing complaint can correct a wrong balance; it cannot convert credit back into
  money while we are operating.
- **Credit expires 2 years after the deposit that created it, per deposit** (§3), and the
  expiry sweep sends no notice beforehand.
- **If we close the service, every balance above USD 2.00 is paid out**, the rail you paid
  on decides the payout method, and balances at or below USD 2.00 are discharged on request
  with us covering the fee (§7). That is the one thing that overrides the non-refundable
  clause above.

**10.7 Requests about your data.** Requests to see, export, correct or delete your data go
to the address in 10.2 and are handled under [`data-retention.md`](data-retention.md)
§"Access and deletion requests". You do not have to use email to see or export your own
data — your dashboard and the bot already show your profile, balance, usage and keys, and
`GET /api/export` downloads your own records as JSON — but a request sent to 10.2 will be
actioned. **One limit is stated up front rather than explained afterwards: the ledger
cannot be deleted on request.** It is immutable by design, and it is the record both we and
you rely on in a billing complaint. Account deletion anonymises where it can and leaves
the ledger intact.

**10.8 A report does not suspend anything automatically.** Sending a complaint does not
pause billing, hold a balance or stop an account. If you need usage stopped, revoke the API
key from your dashboard first — that takes effect immediately — and then tell us.

### Before publishing this section

Nothing here is a value yet. Substitute all three, then delete this checklist and the
warning block above:

| Placeholder | Replace with | Where it also appears |
| --- | --- | --- |
| `[[ABUSE_EMAIL]]` | A mailbox that is **actually monitored** at the response times in 10.3 | `abuse-runbook.md` §Open items records the same gate, and `launch-checklist.md` "Publish the Terms of Service" |
| `[[OWNER_LEGAL_NAME]]` | The contracting entity: the owner's legal name, or the PT if the entity decision goes that way | `launch-checklist.md` §Gate 0 "Decide the contracting entity" |
| `[[RESPONSE_HOURS]]` | Only if the response times get a numeric form; the canonical table is in `abuse-runbook.md` §"Severity and response time" | `abuse-runbook.md` §"Severity and response time" |

**Three places describe this one gate, and they were reconciled when this section was written.**
`launch-checklist.md` "Publish the Terms of Service" and `abuse-runbook.md` §Open items both
used to say this section was "a requirements list, not a clause" — true when written, and false
from the moment it became one. Both were corrected in that change, and both kept their item
**unticked**, because the half of the gate that moved was the wording and the half that did not
is whether a monitored mailbox exists.

**So the remaining step is the substitution above, not another reconciliation pass.** Filling in
`[[ABUSE_EMAIL]]` with a real address is the whole of what publishes this section; if you find
yourself editing prose in the other two documents to match, something has drifted and the drift
is the thing to look at.

**Why the placeholders are there and not an address.** The address must be one that is
actually monitored, and no mailbox exists — `abuse-runbook.md` tracks the same item as
open, and no `abuse` address exists anywhere in the tree (checked). Writing a
plausible-looking address into a legal document would be worse than the gap, because the
Terms would then make a promise no one is keeping — the same failure mode this document's
own warning about "a policy claiming something the code does not do" describes.

**The dependency is a decision plus a mailbox**, not code. Until both exist, the launch
table row for the abuse contact below is **unmet on the Terms half as well as the
operational half**. The clause above is what that half will publish; the tokens are what
still stops it.

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
      last clause — and the last clause is deliberate — AND NOW WIRED.**
      `topups.credit_expires_at` is
      stamped at settlement in the same statement that records it; `db::expire_credit`
      retires aged deposits oldest-first. There is
      **no spend-time refusal**: expiry is honoured to the sweep's cadence, so credit
      that ages out after the last run stays spendable until the next one. That is a
      decision, not an omission — `try_debit` checking expiry too would be a SECOND
      definition of when credit dies, and two definitions is how a rule stops being
      trusted. See `decisions.md`, "Credit expiry: implementation".
      > **THIS LINE WAS UNTICKED FOR THREE ROUNDS, AND THE REASON IT WAS UNTICKED IS THE
      > POINT.** It once read "Built, except the last clause", which was about the CODE,
      > and the code was built and tested. But an item in a register headed "Open items"
      > asks whether the promise is DISCHARGED, not whether the code exists — and it was
      > not: `db::expire_credit` had exactly one caller,
      > `server/src/bin/usage-purge.rs`, and that binary shipped in no image and ran on no
      > schedule, so `run_retention` had no counterpart for it and **no credit expired
      > while this page published a term saying it would**. That is what the tick now
      > records: `server/Dockerfile` builds `--bin usage-purge`, and
      > `.docker/maintenance/entrypoint.sh` runs it nightly as the `credit-expiry` job,
      > which **exits non-zero** rather than reporting a sweep it could not perform.
      > `tools/backup-check/check.sh` holds both halves, so unwiring either one fails a
      > gate rather than a document.
      > **A correction note is not exempt from going stale, and this one did.** The note
      > above read "So no credit expires" and "`usage-purge` is not wired at all" — both
      > true when written and both invalidated by the commit that wired it, which updated
      > `decisions.md` and missed this page. The document that publishes the TERM is the
      > last place a stale sentence should be allowed to sit, which is why the tick and
      > the note moved in the same edit rather than a round apart.
- [ ] Whether an entity (PT) exists to contract with, or a personal account — see
      [`business/05-risk.md`](business/05-risk.md) R1.
- [ ] Complaint handling process, and who owns it.
- [ ] Notice period for price and term changes.
