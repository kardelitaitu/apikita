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

Midtrans sends a `signature_key` field with each notification payload, computed as a
**SHA-512 hash of concatenated string fields including the server key**:

```
signature_key = SHA512(order_id + status_code + gross_amount + server_key)
```

**Implementation notes for signature calculation:**
- `order_id`: The exact merchant order ID string (e.g. `"topup_d7e4d049-..."`).
- `status_code`: The status code string from the notification body (e.g. `"200"` for successful settlement).
- `gross_amount`: The raw string value of `gross_amount` as serialized in the Midtrans notification JSON payload (Midtrans typically formats this with two decimal places, e.g. `"50000.00"`). Do not parse as a float before string concatenation; use the raw payload string or format as `format!("{:.2}", amount)`.
- `server_key`: The secret Midtrans Server Key from the backend environment.
- Compare signatures using **constant-time equality** (`subtle::ConstantTimeEq` in Rust) to prevent timing attacks.

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