# Admin & Operations Surface

Operator capabilities. Referenced as a gap in
[`abuse-runbook.md`](abuse-runbook.md) — suspending an account is a required action with
no specified mechanism.

> Model: [`architecture/identity.md`](architecture/identity.md) · Schema:
> [`website/02-data-model.md`](website/02-data-model.md)

## The governing rule

**Every admin action goes through the same API and the same ledger as normal
traffic. There is no back door.**

| Anti-pattern | Why it breaks |
| --- | --- |
| `UPDATE wallets SET balance_idr = ...` by hand | The ledger no longer sums to the balance. Reconciliation fails forever |
| Editing PocketBase records directly for money | Identity system holding money state; the two stores diverge |
| Deleting rows to "fix" data | Destroys the audit trail that makes disputes resolvable |

**An adjustment is a ledger row, not an edit.** The `ledger.reason` enum already
has `adjustment` and `refund` for exactly this.

## Required capabilities

Inferred from the abuse runbook, ToS, and support scenarios:

| Action | Kind | Needed by |
| --- | --- | --- |
| Suspend / restore account | state | Abuse responses, ToS §7 |
| Revoke a user's key | state | Abuse response (revoke first, then assess) |
| View account, ledger, usage | read | Every billing question |
| View top-up and webhook history | read | Payment disputes |
| View abuse signals per account | read | Abuse detection |
| **Credit an adjustment** | **money** | Reconciliation fixes, goodwill |
| **Refund a top-up** | **money** | ToS §3 non-delivery exception, disputes |
| Force-logout (revoke sessions) | security | Account takeover |
| Cancel a pending link code | security | Linking abuse |
| Moderate a review | content | Review room |

## Access model

**Start with a single operator role.** Multi-role RBAC before you have employees is
speculative complexity.

| Role | Can |
| --- | --- |
| `user` | Their own account only |
| `operator` | Everything above, across accounts |

### Authentication

The admin surface is **the same API**, distinguished by an account flag — not a
separate service with its own auth.

- `accounts.is_operator BOOLEAN NOT NULL DEFAULT false` (excluded from all
  customer-facing responses).
- Same session mechanism as customers.
- **Not a separate admin password**, which becomes a shared secret.

**A separate admin app is a later decision.** The endpoints are the same either way;
a dedicated UI is a convenience, not an architecture.

### Why a flag and not a separate app

One code path for authorization means one place to audit. A separate admin service
duplicates auth, session handling, and revocation — and the duplication is where
the holes appear.

## Money actions — the ones that need care

Two actions move money outside the Midtrans webhook. Both are necessary; both are
dangerous.

### Adjustment

Used for: reconciliation discrepancies, goodwill, correcting an operator mistake.

```json
POST /api/admin/accounts/:id/adjust
{ "delta_idr": -5000, "reason": "reconciliation fix", "note": "..." }
```

Rules:

1. **Writes a ledger row** with `reason='adjustment'`, delta, and `balance_after`.
2. **Atomic with the wallet update** — same transaction.
3. **Requires a note.** An unexplained adjustment is indistinguishable from theft
   during an audit.
4. **Logged with the operator's account id**, not just the affected account.

### Refund

Used for: the ToS non-delivery exception, and disputes arriving from the payer.

```json
POST /api/admin/topups/:id/refund
{ "reason": "service not delivered", "note": "..." }
```

Rules:

1. Sets `topups.status = 'refunded'`.
2. **Debits the wallet** via a ledger row with `reason='refund'`.
3. **Refuses if the balance is insufficient** — the customer may have spent it.
   That case is a decision, not an automatic negative balance.
4. Where Midtrans is involved, the money movement happens **with Midtrans**, and
   our ledger records the consequence. Do not represent a Midtrans refund as one
   we performed unilaterally.

**A refund that would drive the balance negative must not silently succeed.** The
customer spent credit they are now claiming back; that is a business decision with a
real cost, and it needs a human.

## Non-money actions

| Endpoint | Effect | Notes |
| --- | --- | --- |
| `POST /api/admin/accounts/:id/suspend` | `status='suspended'` | **Revokes sessions and keys** — otherwise it does nothing |
| `POST /api/admin/accounts/:id/restore` | `status='active'` | Does **not** restore keys; the customer reissues |
| `POST /api/admin/keys/:id/revoke` | `revoked_at` | Same as the user's own revoke |
| `POST /api/admin/accounts/:id/logout-all` | Revoke all sessions | Takeover response |
| `DELETE /api/admin/link-codes/:id` | Invalidate a pending code | |
| `POST /api/admin/reviews/:id/hide` | Sets a hidden flag | **Never deletes**; see the review rules |

**Suspension must revoke sessions and keys.** Setting a status flag alone leaves a
live session and working keys — the account keeps working while appearing suspended.
This is the single easiest way to get the suspension wrong.

## Audit trail

**Every admin action writes an audit row.** Not optional, and not derived from logs.

**Definition and indexes: [`docs/website/02-data-model.md`](website/02-data-model.md)** (single
source). `admin_audit` holds: `id`, `operator_id` -> accounts (RESTRICT), `action`,
`target_type`, `target_id`, `detail` (JSONB), `created_at`.

- **`ON DELETE RESTRICT` on operator** — the trail outlives the operator's account.
- Written **in the same transaction as the effect**. An audit row for an action that
  rolled back is as bad as no audit row.
- Money actions store the amount and the resulting balance in `detail`.

## Safety rails

| Rail | Why |
| --- | --- |
| **Operator accounts cannot act on themselves** | Prevents self-crediting |
| **Money actions require a note** | Forces an explanation at the moment of intent |
| **Adjustments above a threshold require a second operator** | Cheap two-person rule; specify the threshold |
| **Suspension revokes sessions and keys atomically** | Otherwise it does not suspend |
| **No hard deletes, ever** | Closure is a status; withdrawal is a flag |
| **Admin endpoints excluded from customer API docs** | Reduces surface for probing |

**The two-person rule needs a decision:** below what amount is one operator enough?
A reasonable starting point is any adjustment or refund above a few hundred thousand
IDR.

## What the admin surface must NOT do

| Not | Why |
| --- | --- |
| Read customer prompts | We never store them; see [`data-retention.md`](data-retention.md) |
| View API keys in plaintext | Only hashes exist |
| Bypass limits or model allowlists | An operator's own key is just a key |
| Set a balance directly | Ledger only |
| Delete an account | Closure only; the ledger is retained |

## Observability for admin actions

| Signal | Why |
| --- | --- |
| Every action in `admin_audit` | The record |
| **Alert on any money action** | Rare by design; each one deserves attention |
| Alert on admin login from a new IP | Operator account compromise is total compromise |
| Daily summary of actions | Passive review catches drift |

See [`observability.md`](observability.md).

## Rollout

| Phase | Surface |
| --- | --- |
| **Launch** | Read-only + suspend/restore + key revoke. Money actions via SQL by the owner, documented |
| **With revenue** | Adjustments and refunds as endpoints with notes and audit |
| **Later** | Second-operator threshold, dedicated UI, more roles |

**Starting read-only is deliberate.** The dangerous actions are the money ones, and
they should be implemented once there is revenue to misfile — not on day one when
nobody but the owner is operating and a mis-keyed adjustment is easy to spot.

## Open items

- [x] Second-operator threshold **500,000 IDR**.
- [ ] Whether operators authenticate from a restricted IP range.
- [x] `is_operator` lives on `accounts` — a column, not a table.
- [ ] Abuse-signal dashboard: which metrics, where computed.
- [ ] Whether admin actions are exposed to the customer in their own audit view.