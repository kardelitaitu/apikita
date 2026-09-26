# Wind-down runbook

**What this is:** the operator procedure for closing the service and paying balances
back. The policy is settled in [`decisions.md`](decisions.md) §Money; the customer-facing
wording is [`terms-of-service.md`](terms-of-service.md) §Wind-down.

**Why it is a runbook and not code.** There is no treasury, no disbursement
integration, no crypto provider, no KYC, and — at the time of writing — no customers.
A payout service would be a large outbound-money surface for an event that may never
occur. The codebase already deleted every callerless money-moving function for exactly
this reason (`decisions.md` §Money, "deleted, not quarantined"). A payout path with no
traffic is that same hazard, shipped deliberately. **Write the code when a rail exists.**

> ⚠️ **Legal and tax review is a prerequisite, not a follow-up.** See the open items in
> `decisions.md` §"Genuinely open". In particular: whether the entity can make outbound
> IDR transfers at all, PP 55/2022 on refunded revenue, and the crypto regime
> (Bappebti/OJK, PMK 50/2025 0.21% PPh 22) for stablecoin payouts.

---

## The settled rules

| Rule | Value |
| --- | --- |
| Trigger | **We decide to close.** Platform-initiated, never a customer request |
| Notice | **At least 30 days** |
| Threshold | **`balance_idr > 2 × closure_usd_idr_rate`** — strictly greater |
| Rate | **Bank Indonesia JISDOR on the wind-down date, frozen once** for all payouts |
| Classification | **Ever settled a Midtrans top-up ⇒ Indonesian ⇒ bank transfer. Otherwise ⇒ USD stablecoin** |
| Rounding | **Down to the cent** — the platform absorbs the remainder, never creating money |
| Sub-threshold | **Not paid automatically.** Claimable within **12 months**; the company covers the transfer fee |
| Unclaimed | **A retained liability** — never recognised as revenue |
| Expiry | **Waived.** The whole balance is paid |
| Ledger | **Reuse `reason='refund'`** — see why below |

### Why the rate is frozen

Balances are frozen at the same instant the rate is captured, so a per-payout rate
would value identical balances differently on the same day — arbitrary, and an
invitation to dispute. One auditable number also survives an auditor's re-check.
A live FX API is, moreover, precisely what is unavailable when you are shutting down.

### Why `reason='refund'` and not a new value

The value already exists in the `ledger.reason` CHECK but had no writer. A closure
payout is exactly that semantic. **Adding a CHECK value on SQLite is not additive-safe**
— there is no `ALTER … ADD CONSTRAINT`; it is a twelve-step table rebuild, the same
class of change as *removing* one. Reuse is not merely convenient, it is the only safe
route. A `reason='adjustment'` row would also silently inherit the 500,000 IDR
two-operator rule, which is arguably correct anyway — but the honest label is
`refund`.

---

## Step 1 — Freeze the decision

Pick the wind-down date. Capture, **in the config and in writing**:

```toml
[wallet]
closure_threshold_usd = 2.00
closure_usd_idr_rate  = <JISDOR rate on the wind-down date>
closure_rate_source   = "BI JISDOR"
closure_rate_observed = "<YYYY-MM-DD>"
```

Per `decisions.md`, **a decision with no config value cannot be enforced** — it becomes a
hardcoded constant nobody can tune.

## Step 2 — Notice, while sessions still work

**Notice must run while `accounts.status = 'active'`.** Do not suspend. Suspension
revokes every session and key atomically (`decisions.md`), which destroys the only
channel you have for reaching the customer — and `'closed'` is a table rebuild to add
to the CHECK.

Give at least 30 days. Tell customers to **confirm their payout destination**.

## Step 3 — The eligibility query

The refundable figure is `wallets.balance_idr`, **whole**. Expiry is waived, and in any
case the schema holds one un-aged integer — expired and live credit are not
distinguishable at all today.

```sql
-- Frozen rate: substitute the captured value, never a live lookup.
-- 2 * rate, because the rule is "more than USD 2.00" (strictly greater).
SELECT w.account_id,
       w.balance_idr,
       CASE WHEN EXISTS (
         SELECT 1 FROM topups t
          WHERE t.account_id = w.account_id
            AND t.status = 'settled'
            AND t.rail = 'midtrans'
       ) THEN 'bank_transfer' ELSE 'stablecoin' END AS payout_rail
  FROM wallets w
 WHERE w.balance_idr > (2 * :closure_usd_idr_rate)
 ORDER BY w.balance_idr DESC;
```

> **`topups.rail` does not exist yet.** Until it is added, **every customer is
> Indonesian** — Midtrans QRIS is the only implemented rail — and the classification
> query degenerates to `'bank_transfer'` for everyone. That is the correct answer today,
> not a stub: there has never been a crypto top-up to classify.

## Step 4 — The sub-threshold list

```sql
SELECT w.account_id, w.balance_idr
  FROM wallets w
 WHERE w.balance_idr > 0
   AND w.balance_idr <= (2 * :closure_usd_idr_rate);
```

These are **not forfeited.** Publish an address for claims, allow **12 months**, cover
the transfer fee. Keep unclaimed amounts as a **retained liability** — do not book them
as income.

## Step 5 — Pay out

**Bank transfer (Indonesian).** Pay `balance_idr` **exactly** — no conversion, IDR is
already whole. The destination must be in the **customer's own name**; a mismatch is a
stop, not a judgement call. A failed transfer bounces and is recoverable; a
wrong-account transfer is not.

**Stablecoin (everyone else).** Convert with integer arithmetic, rounding **down**:

```
units = floor(balance_idr * 1_000_000 / closure_usd_idr_rate)   -- USDC has 6 decimals
```

Rounding down means the payout can never create money. Disclose the dust.

## Step 6 — Write the ledger rows, then close

One row per paid account. **Both are mandatory** — `wallet = SUM(delta_idr)` breaks if
the ledger and the wallet disagree.

```sql
-- 1. Debit the wallet to zero.
INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at)
VALUES (:account_id, -:refundable, 'refund', 'closure_' || :run_id, 0, :now);

-- 2. Zero the balance.
UPDATE wallets SET balance_idr = 0, updated_at = :now WHERE account_id = :account_id;

-- 3. Only now is the account closable.
UPDATE accounts SET status = 'closed', updated_at = :now WHERE id = :account_id;
```

`accounts.status = 'closed'` is set **only after the balance reaches zero** — the rule
that a non-zero wallet is never closed is already recorded in
[`data-retention.md`](data-retention.md).

## Step 7 — Verify

```
tools/reconcile/reconcile.sh
```

**Zero rows.** This is the only proof the payout was complete and correct, and it must
be run against a scratch copy before any live payout — an untested recovery path is a
belief, and this is the same standard the backup drill is held to.

## Step 8 — Destroy the banking details

Payout destinations are PII with a short life. Delete them **30 days after the payout
completes**; keep the payout *reference*, which is financial and permanent. See
[`data-retention.md`](data-retention.md).

---

## What this runbook deliberately does not do

- **No HTTP endpoint.** No `POST /api/admin/.../refund`. An operator running a script
  is the correct level of ceremony for an event that happens once.
- **No automation of the transfer.** Nothing initiates a payment without a human.
- **No customer-initiated path.** A customer cannot trigger any of this; the trigger is
  our decision, which is the whole reason it coexists with the non-refundable clause.
