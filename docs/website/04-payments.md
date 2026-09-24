# 04 — Payments (Midtrans)

End-to-end top-up flow. Money is involved, so this is the most correctness-
sensitive document in the set.

## Decision summary

| Decision | Value |
| --- | --- |
| Rail | **QRIS only**, via Midtrans Snap |
| Card | **Disabled deliberately** — a flat per-transaction fee is ~20% of a 10k top-up |
| Refunds | **Non-refundable** by policy; see the caveat below |
| Crediting | **Server webhook only** |
| Idempotency | By `order_id` |
| Settlement | To a bank account, T+1/T+2 |

## Flow

```
1. user enters amount on /dashboard/wallet
2. BFF validates amount against minimums
3. BFF calls Midtrans -> creates transaction -> gets snap_token
4. BFF writes a topups row: status=pending, order_id, amount
5. browser opens Snap -> user pays via QRIS
6. Midtrans -> POST webhook to BFF
7. BFF verifies signature, matches order_id, checks amount
8. BFF credits wallet + updates topups.status=settled  (ONE transaction)
9. realtime push -> dashboard balance updates
```

**The browser is never trusted to say "I paid."** Step 5 returning success means
nothing — a client can claim anything. Only step 6 creates money.

## Webhook verification

Midtrans sends a `signature_key` field with each notification, computed as a
**SHA-512 hash of concatenated fields including the server key**.

> **UNVERIFIED — confirm against Midtrans documentation before implementing.**
0
> The formula is commonly documented as:
>
> `signature_key = SHA512(order_id + status_code + gross_amount + server_key)`
>
> This could not be verified against the official docs from this environment
> (Midtrans' documentation is a JavaScript-rendered SPA and the web search tool is
> unavailable). The **shape** is certainly right — a SHA-512 over order fields plus
> the server key — but the exact **field set and order** must be checked, because
> a wrong order produces a hash that never matches.
>
> **How to verify:** the Midtrans dashboard shows a sample notification, or send a
> test transaction in sandbox and log the raw payload. Confirm the concatenation
> order from their docs, then update this line and remove this notice.

### The rules hold regardless of the exact formula

These do not depend on the field order and are safe to implement now:

1. Recompute the signature server-side and compare. **Reject mismatches.** This
   is why the server key cannot live in the browser.
2. **Compare `gross_amount` against your own stored `topups` row.** Never credit
   the amount from the payload.
3. Match on `order_id`. Unknown → reject and log.
4. Treat `settlement` (and `capture`) as credit events. `deny`, `cancel`,
   `expire`, `refund`, `partial_refund` are terminal non-credit (or debit) events.
5. **Idempotent:** if the `topups` row is already `settled`, return 200 and do
   nothing. Midtrans retries; a double credit is real money.
6. Respond **200 quickly**. Slow webhooks get retried, which compounds the
   idempotency requirement.

## Amounts and fee handling

- Store amounts in **IDR as integers**. Never floats for money.
- Decide and document whether the customer's entered amount is gross (they pay
  it, balance grows by amount − fee) or net. **Show the fee before payment.**
- Minimums differ: a **re-top-up minimum** and a higher **first deposit
  minimum**. Configured, not hardcoded. See
  [`docs/business/03-financial-model.md`](../business/03-financial-model.md).

## The non-refundable caveat

Policy is non-refundable. That scopes liability to *unwanted* service — it does
**not** cover *undelivered* service. If payment succeeds and the service cannot be
delivered, the customer paid for nothing, and a QRIS dispute through the payment
provider generally favours the payer on non-delivery.

Practical consequences:

- Keep a reserve covering outstanding wallet liabilities. Non-refundable reduces
  expected payouts; it does not make them zero.
- Handle `refund` and `partial_refund` webhook statuses even though the policy
  says no refunds — they may arrive from a dispute, and an unhandled status
  leaves the ledger inconsistent.
- Record the policy in the terms of service the user accepts at top-up.

## Settlement lag

Wallet credits on webhook; money reaches the bank at T+1/T+2. The float is the
gap. At the scale this business targets (~100–200 customers) it is under a
million IDR and is not a material constraint — but it does mean **the balance and
the bank account differ for a day**, and that is normal, not a bug.

## Reconciling

Monthly, compare:

- Midtrans settlement report **vs** our `topups` rows (status `settled`).
- Billed usage **vs** upstream invoices.

Both must reconcile. A drifted wallet balance is a trust event, and upstream
usage reconciliation is a revenue-correctness control (see
[`docs/business/05-risk.md`](../business/05-risk.md) R3).

## Security checklist

- [ ] Server key only in server env; never shipped to the client.
- [ ] Signature verified on every webhook.
- [ ] Amount validated against the stored order, not the payload.
- [ ] Crediting idempotent by `order_id`.
- [ ] Crediting atomic with the `topups` status update.
- [ ] No client-side path can write `balance_idr`.
- [x] Top-up creation: **5/hour per account** (`decisions.md`).
- [ ] Reconciliation runs monthly and is reviewed.

## Open questions

- [x] Amount model: **gross** — the customer pays what they enter.
- [x] First-deposit minimum: **50,000 IDR**; re-top-up **10,000 IDR**.
- [ ] Who reviews the monthly reconciliation, and where the result is recorded.