-- Credit expiry: a deposit's credit lives 24 months from the deposit's own date.
--
-- THE POLICY. Settled in `docs/decisions.md:76` and repeated in the terms:
-- **2 years (24 months) from each deposit's own date**. Per deposit. NOT per
-- account, and NOT from last activity -- the last-activity reading silently
-- extends old credit every time a customer spends, which is the reading the
-- policy was written to reject. It stays rejected here.
--
-- WHAT WAS MISSING. `docs/decisions.md:77` recorded the gap: "no per-deposit
-- expiry column, no sweep job, and no refusal of a spend against aged credit".
-- `docs/decisions.md:69` explains why the gap could not be closed by reading
-- the existing schema: it holds ONE un-aged `wallets.balance_idr` and no
-- per-deposit date, so "expired" and "live" credit were the same number.
--
-- WHY THE DATE GOES ON `topups` AND NOT ON A NEW TABLE. A deposit's own date is
-- the instant it SETTLED, and `topups.settled_at` already records exactly that,
-- bound from Rust in `credit_topup_transaction` (`server/src/db.rs:167`). The
-- expiry instant is therefore a pure function of a row that already exists:
-- `settled_at + 24 months`. A second table would be a copy of `topups` that
-- could disagree with it.
--
-- WHY IT IS STORED RATHER THAN COMPUTED. Computing `settled_at + 24 months` in
-- SQL would put a date function in the query path, and SQLite's `datetime()`
-- with a month modifier is not what this needs: "24 months" is CALENDAR
-- arithmetic, and `date(settled_at, '+24 months')` normalises a 31st-of-month
-- deposit to the 28th/30th rather than keeping the day. Binding the instant
-- from Rust once, at settlement, keeps the stored value the single source of
-- truth and keeps every comparison in Rust -- the rule this schema's header
-- states and the identity migration repeats.
--
-- NULL means "unknown", and unknown ages out into exclusion, never inclusion:
-- a row settled before this column existed has no recorded expiry instant, and
-- the sweep treats a NULL as already expired. That is the fail-closed
-- direction. (There is no such row in production -- the service has no
-- customers -- but the direction still has to be chosen, and this is it.)
--
-- THE LEDGER CONSTRAINT. `wallets.balance_idr` must equal
-- `SUM(ledger.delta_idr)` for the account -- `tools/reconcile/reconcile.sql`,
-- launch Gate 2. An expiry therefore CANNOT be a flag, a side table, or a
-- wallet-only decrement: it must be a negative `ledger` row written in the same
-- transaction as the wallet decrement, exactly as `try_debit` does
-- (`server/src/db.rs:1263`). See `server/src/db.rs` `expire_credit_transaction`
-- for the writer.
--
-- `ledger.reason` is reused, not extended. The CHECK is frozen at
-- `('topup','usage','adjustment','refund')` and widening a CHECK on SQLite is a
-- table rebuild (`docs/decisions.md:70`). Expiry is neither a refund nor an
-- operator adjustment; it is credit ceasing to be spendable without being
-- spent, which is closest to `usage` -- and a customer reading their ledger
-- needs to tell the two apart, so the `ref` carries the distinction instead.

-- ---------------------------------------------------------------------------
-- When this deposit's credit stops being spendable
-- ---------------------------------------------------------------------------

-- Calendar arithmetic, not an interval: a deposit on 2026-03-15 expires on
-- 2028-03-15. Written by `credit_topup_transaction` at the same instant it sets
-- `settled_at`, so the two can never describe different events.
ALTER TABLE topups ADD COLUMN credit_expires_at TEXT;

-- The sweep and the spend-time refusal both ask "which settlements have aged
-- out", and both ask it per account. Partial, because the NULL rows are the
-- fail-closed case the sweep handles in one pass of its own and do not want an
-- index that cannot seek them.
CREATE INDEX topups_credit_expires_idx
    ON topups (account_id, credit_expires_at)
    WHERE credit_expires_at IS NOT NULL AND status = 'settled';

-- ---------------------------------------------------------------------------
-- What has already been aged out
-- ---------------------------------------------------------------------------

-- Expiry is not idempotent by nature: running the sweep twice would append two
-- negative ledger rows for one deposit and destroy the balance while looking
-- like a tidy-up. This records the one instant a deposit's credit was retired,
-- and `topups_credit_retired_uniq` makes a second attempt a constraint failure
-- rather than a second debit.
--
-- ON DELETE CASCADE, matching `review_history`: this table is a record of what
-- the ledger already says, not money. The ledger row is the authority and is
-- never deleted.
ALTER TABLE topups ADD COLUMN credit_retired_at TEXT;

-- One retirement per deposit, enforced by the database rather than by a check
-- in Rust that a second code path could forget to make.
CREATE UNIQUE INDEX topups_credit_retired_uniq
    ON topups (id)
    WHERE credit_retired_at IS NOT NULL;
