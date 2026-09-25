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

### The refund clause must be precise

**Non-refundable is enforceable against a customer who changes their mind. It is
NOT enforceable against non-delivery.** If we take payment and provide no service —
or cannot provide it — the customer is entitled to their money back regardless of
what this document says, and a QRIS dispute will generally favour the payer.

Draft language:

> Unused credit is non-refundable except where required by law or where we are
> unable to provide the service for which the credit was purchased. Where service
> cannot be provided, unused credit will be returned.

**Do not write "non-refundable" without the exception.** A clause that overreaches
is more likely to be struck down entirely than a narrow one.

### Expiry

**Open decision:** do credits expire? If they do, it must be stated and the period
must be reasonable. An expiring balance that is never refunded is a consumer-
protection problem.

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
| Upstream terminates our access | Service ends; see the refund clause for unused credit |

**The last row is a real scenario.** The entire supply side could end without notice
(see [`business/05-risk.md`](business/05-risk.md) R1). The terms must say what happens to
unused credit in that case — and the honest answer is that it is returned, because
there would be nothing to deliver.

## 8. Changes to these terms

- Notice period for material changes.
- Continued use constitutes acceptance.
- **Price changes need clear notice**, or customers will feel ambushed.

## 9. Liability and governing law

- Limitation of liability, capped at amounts paid in a period.
- **Governing law: Indonesian.**
- Dispute resolution path stated.

## 10. Contact and complaints

- A real channel that is monitored.
- Response expectation.
- **A stated path for content or billing complaints**, since both will occur.

## What must be true at launch

| Requirement | Where enforced |
| --- | --- |
| Cross-border forwarding disclosed before first use | Signup + pre-topup notice |
| Non-refundable stated with the non-delivery exception | Terms + top-up screen |
| Acceptable use published and linked | Terms + an abuse contact |
| Prompts genuinely not stored | Code review; see [`data-retention.md`](data-retention.md) |
| Retention periods stated accurately | Matches the schema |

**The last two are the ones that rot.** A policy claiming something the code does
not do is worse than no policy, because it is a demonstrable false statement.

## Open items

- [ ] Legal review — **required before launch**.
- [ ] Credit expiry: yes or no, and for how long.
- [ ] Liability cap figure.
- [ ] Whether an entity (PT) exists to contract with, or a personal account — see
      [`business/05-risk.md`](business/05-risk.md) R1.
- [ ] Complaint handling process, and who owns it.
- [ ] Notice period for price and term changes.
