# 05 — Risk

Ordered by how likely each is to end the business, not by how comfortable it is
to discuss. The first item is the one that matters.

## R1 — Undelivered service and consumer protection `[HIGH]` — resale terms RESOLVED

> **Status change (v2).** Upstream **resale is confirmed permitted** by the
> provider in use. The former blocking form of this risk — that resale violated
> terms and termination could strand customer funds — is closed. What remains is
> narrower but not eliminated, and is restated below.

**What the resale permission does not cover.** Resale being permitted means the
upstream will not terminate us *for reselling*. It is silent on four other things
that are now the live exposure:

1. **Per-provider scope.** The permission belongs to *that provider's* terms. It
   does not transfer to a second supplier. Every additional upstream is a fresh
   R1 check. Multi-supplier failover (whitepaper §4.3) therefore carries a
   recurring compliance cost, not a one-time one.
2. **Prepaid balance on non-resale termination.** Whether an unused balance is
   recoverable if the account is closed for any *other* reason (payment dispute,
   KYC failure, provider exit) has not been established. Assume not.
3. **Cross-border prompt forwarding.** We forward customer prompts to a mainland
   provider. Resale permission does not address data handling, and customers are
   not told this happens. This is a disclosure gap independent of ToS.
4. **Volume/licensing thresholds.** A permission to resell is not necessarily a
   permission to resell at scale. Check whether volume triggers a different
   agreement.

**The refund posture is narrower than "no refunds" implies.** Policy is
non-refundable, which is enforceable against a customer who changes their mind.
It is *not* enforceable where we take payment and deliver nothing — the upstream
goes down, an account is closed, or a top-up never credits. In that case the
customer paid and received no service.

That is not a hypothetical: **Midtrans is a domestic rail with a real merchant
entity behind it.** A payer who does not receive service can dispute through the
payment provider, and on non-delivery the payer generally prevails regardless of
our stated policy. Non-refundable scopes liability to *unwanted* service, not to
*undelivered* service.

**What must happen before launch:**

- Maintain a funded reserve sufficient to cover outstanding wallet liabilities in
  the event of service failure. Non-refundable policy reduces expected payouts;
  it does not make them zero.
- Decide entity structure (personal vs. PT). Dispute posture, volume ceiling, and
  tax treatment all follow from it. **Currently undecided.**
- Disclose cross-border prompt forwarding in the terms of service — drafted in
  [`docs/terms-of-service.md`](../terms-of-service.md) §5.
- Re-run this check for every provider added to the routing pool.

**Related:** the failed-request path. The whitepaper's Phase 2 cuts a stream
mid-sentence when the balance runs out. Combined with non-refundable funds, this
is the single most likely generator of a delivery dispute. See R3.

## R2 — Wholesale repricing `[HIGH]`

The entire model rests on wholesale rates staying far below retail. Those rates
are promotional, driven by compute overcapacity, and can move at any time.

- A 2× upstream price increase eliminates the margin unless M also rises — and M
  is capped by what retail charges.
- Cache pricing is the most fragile input. It is the cheapest line item and the
  most likely to be repriced once providers notice it is being resold.
- We have no hedging mechanism and no contractual price protection.

**Mitigation:** keep multiple suppliers live so a repricing at one is survivable,
and treat per-endpoint rates as hot-reloadable config (the whitepaper's
`Arc<RwLock<ConfigWrapper>>` design exists for exactly this). Monitor rate changes
as a business metric, not a config detail.

## R3 — Billing correctness `[HIGH]`

We bill from upstream-reported usage. We never measure tokens ourselves, so our
revenue depends on another company's accounting being accurate and honest.

- **Under-reported usage** → we undercharge, silently, at scale.
- **Cache misclassification** → raw and cached tokens differ 50× in cost but
  bill 50× differently. A ~2% error in the wrong direction exceeds the entire
  markup on the affected tokens (see [02-pricing.md](02-pricing.md)).
- **Streaming cut-offs for balance reasons** → the customer experiences a
  truncated answer with no explanation, on non-refundable funds. This is the
  most likely generator of a delivery dispute (see R1).

**Mitigation:** reconcile upstream invoices against our billed totals monthly and
treat any drift as a defect. On balance exhaustion, prefer **rejecting at
pre-flight** over cutting mid-stream: let an in-flight request finish even if it
briefly overdraws, then refuse the next one. A small overdraft costs less than a
truncated answer plus a payment dispute. If a cut is unavoidable it must carry a
terminal error the client can display.

## R4 — Difficulty and abuse `[MEDIUM]`

Lower prices on capable models attract use cases the legal retail providers
screen for: spam generation, phishing, scraped-content farms.

- We have no moderation capability and no compliance posture.
- Prepaid billing limits financial exposure but does not limit reputational or
  upstream-relationship damage.
- A single abuse incident can trigger upstream termination, which is not
  covered by the resale permission (see R1). Response procedure:
  [`docs/abuse-runbook.md`](../abuse-runbook.md).
- Indonesian payments put KYC on the payment rail, not the account, so identity
  assurance is weaker than it looks.

**Mitigation:** explicit acceptable-use terms, a working abuse report channel, and
the operational willingness to terminate an account quickly. This is unglamorous
work that must be staffed, not deferred.

## R5 — Price war `[MEDIUM]`

Incumbents have more capital and better trust. If one chooses to price-match in
Indonesia, our only remaining differentiators are local payment and support —
both of which an incumbent can also buy. Our defense is that this market is small
enough not to be worth their attention, which is a hope, not a strategy.

## R6 — Concentration `[MEDIUM]`

Early revenue will come from a handful of customers. The loss of one material
account is a visible revenue event. Expected and unavoidable at launch; the
mitigation is growth, not contracting.

## R7 — Operational single points of failure `[LOW–MEDIUM]`

- Edge nodes in HK/SG are the only ingress. Provider failure there is total.
- The wallet ledger is authoritative for customer money. Data loss is
  unrecoverable trust loss.
- One person maintaining this is a bus-factor of one.

## Kill criteria

Written down in advance so the decision is not made under pressure. We stop if:

1. **A required upstream prohibits resale.** The current provider permits it, so
   this is now a *per-provider* gate rather than a global one — but a second
   supplier that forbids it cannot be added to the failover pool. Stop adding it.
2. **Real `arpu_volume` × realized margin cannot cover per-customer support cost
   at any defensible price.** The model is dead; more customers make it worse.
3. **Wholesale repricing compresses the spread below operational cost** with no
   second supplier available.
4. **Two abuse incidents escalate to an upstream termination.** The risk is
   realized; do not restart it somewhere else.

## Risk register summary

| ID | Risk | Impact | Likelihood | Status |
| --- | --- | --- | ---: | --- |
| R1 | Undelivered service / consumer protection | High | Medium | **Resale resolved; entity + reserve open** |
| R2 | Wholesale repricing | High | Medium | Monitor |
| R3 | Billing correctness | High | Medium | Mitigated by design + reconciliation |
| R4 | Abuse and difficulty | Medium | Medium | Accepted, needs staffing |
| R5 | Price war | Medium | Low | Accepted |
| R6 | Customer concentration | Medium | High | Accepted at launch |
| R7 | Operational SPOF | Medium | Low | Mitigated by design |
