#![cfg_attr(
    not(test),
    // THE MONEY MODULE, so its arithmetic is denied too.
    //
    // lib.rs denies unwrap_used and indexing_slicing and, before that, claimed
    // these also catch "arithmetic that can overflow in release". They do not:
    // i64 addition, multiplication and subtraction were all measured as ACCEPTED
    // by the real gate. The lint that does say it is
    // clippy::arithmetic_side_effects, and it was not enabled crate-wide because
    // it reports sites in modules where an overflow costs a wrong counter rather
    // than wrong money.
    //
    // This module is the other half of the money path. Every balance, hold and
    // ledger delta is computed here, inside transactions whose whole point is that
    // the figures are exact, and [profile.release] leaves overflow-checks OFF - so
    // an overflow in the shipped build WRAPS rather than panicking. A wrapped
    // ledger delta is invisible to reconcile.sh, which compares SUM(delta_idr) to
    // the balance: a wrap that lands on a plausible positive figure satisfies the
    // check it was supposed to fail.
    //
    // It is not free here - this module has nine sites, in seven functions - so
    // each function that carries one states WHY its operands are bounded, as an
    // explicit #[allow]. A blanket allow at the top of the file would have been the
    // cheaper option and would have been worth nothing: it would silence the lint
    // for every site added afterwards, which is the whole point of adding it.
    deny(clippy::arithmetic_side_effects)
)]

use crate::error::AppError;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use std::str::FromStr;
use std::time::Duration;
use tracing::error;
use uuid::fmt::Hyphenated;
use uuid::Uuid;

/// Opens the application pool.
///
/// The options are not decoration. Each is a measured trap from the plan's
/// section 4.3, and each was re-measured against this exact option set:
///
/// - `foreign_keys(true)` — `PRAGMA foreign_keys` is **per connection**, not per
///   database. sqlx already defaults it ON (unlike raw SQLite, where it is OFF),
///   but it is set explicitly because with it off every `ON DELETE CASCADE` in
///   the schema is silently inert and the `ON DELETE RESTRICT` that is supposed
///   to make hard-deleting a funded account impossible stops working. Measured
///   with this option set: a dangling reference is refused, code 787.
/// - `journal_mode(Wal)` — a persistent property of the file, and `bin/migrate.rs`
///   sets it too. Setting it here as well means a database that somehow lost WAL
///   is put back into it rather than quietly running in rollback-journal mode.
///   Measured: `PRAGMA journal_mode` reads back `wal`.
/// - `synchronous(Normal)` — safe under WAL (a power loss can lose the last few
///   transactions, it cannot corrupt the database) and avoids an fsync per
///   commit, which is most of what WAL buys on this write-heavy path. Measured:
///   `PRAGMA synchronous` reads back 1, i.e. NORMAL.
/// - `busy_timeout(5s)` — this is what replaces the `FOR UPDATE` waiting the
///   Postgres code relied on. sqlx's default is already 5s; it is set here so the
///   number is visible and does not silently depend on a dependency default.
///   Measured: `PRAGMA busy_timeout` reads back 5000.
///
/// `create_if_missing` is deliberately NOT set. Creation belongs to
/// `bin/migrate.rs`, which the deploy order runs before the server. Measured: with
/// it unset, connecting to an absent file fails with `unable to open database
/// file` — which is the failure this wants, because a server that silently
/// creates an empty, schema-less database fails later and far less legibly.
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
}

/// Begins a transaction that takes SQLite's write lock up front.
///
/// `pool.begin()` issues a DEFERRED `BEGIN`. A deferred transaction that reads
/// and then writes can be refused at the lock upgrade with
/// `SQLITE_BUSY_SNAPSHOT`, and that error **cannot be resolved by retrying** —
/// the transaction has to be rolled back and restarted (plan section 4.3, trap 2).
/// `BEGIN IMMEDIATE` takes the write lock at the start, so there is no upgrade to
/// lose.
///
/// Every transaction in this module writes, and every one of them now begins with
/// its write rather than a preceding read, so the specific hazard is already
/// absent. This helper is used anyway: it makes the lock acquisition explicit
/// rather than a consequence of statement ordering, so a later edit that adds a
/// read to the top of one of these functions cannot reintroduce the trap.
///
/// sqlx executes the statement and then verifies the connection really is in a
/// transaction, failing with `BeginFailed` otherwise (measured: a statement that
/// does not open one is refused), so a typo here is loud rather than silent.
pub(crate) async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<Transaction<'static, Sqlite>, AppError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

#[derive(Debug, PartialEq, Eq)]
pub enum TopupCreditResult {
    /// The topup was settled and the wallet credited.
    Settled { new_balance: i64 },
    /// This order was already settled - a replayed webhook. Nothing written.
    AlreadySettled,
    /// No such order.
    NotFound,
    /// The order exists but the stored amount disagrees with the webhook's.
    /// Nothing written.
    AmountMismatch,
    /// The order exists and the amount agrees, but its status is not `pending`,
    /// so there is nothing to settle. `denied`, `expired` and `refunded` all
    /// land here. Nothing was written.
    ///
    /// This variant exists because the settlement guard is now
    /// `status = 'pending'` in SQL, which is STRICTER than the Rust check it
    /// replaced. That check short-circuited only on `'settled'`, so a replayed
    /// settlement webhook arriving after a refund fell through to settlement and
    /// RE-CREDITED the wallet, flipping the row back to `settled` and duplicating
    /// money. The stricter predicate closes that. Reporting the closed case as
    /// `AlreadySettled` would be a lie about a refunded order, so it gets its own
    /// variant.
    NotSettleable { status: String },
}

/// The ledger `ref` a topup's credit is filed under.
///
/// docs/website/02-data-model.md:79 defines the column - "ref TEXT, -- topup id,
/// usage batch id, etc." - and the same document's credit transaction writes
/// `ref` as the topup id (lines 386-387). So a topup's ledger rows are keyed by
/// the TOPUP id, and the audit trail must be able to answer "what happened to
/// top-up X" from that one value. The credit path goes through here so the
/// vocabulary cannot drift again (it used to write the Midtrans `order_id`
/// instead).
///
/// The refund path that used to share this ref is gone: this platform does not
/// do refunds.
pub fn topup_ledger_ref(topup_id: Uuid) -> String {
    topup_id.to_string()
}

/// The shipped credit lifetime, in months, for callers that are not exercising
/// the window itself.
///
/// This mirrors `config::default_credit_expiry_months` and `config/apikita.toml`
/// deliberately rather than reading them. A test that wants the window to matter
/// passes its own value; a test that merely needs a settlement to happen passes
/// this and states, in the call, that the window is not what it is testing. The
/// alternative - loading the real config - would make a test's behaviour depend
/// on a file an operator edits.
#[cfg(test)]
pub const SHIPPED_CREDIT_EXPIRY_MONTHS: u32 = 24;

/// The instant a deposit's credit stops being spendable: `settled_at` plus
/// `months`, as CALENDAR months.
///
/// Calendar, not `30 * months` days. A deposit on the 15th of March expires on
/// the 15th of March two years later; 730 days later is a different day in a
/// leap year, and "2 years from your deposit" is what the terms say. `chrono`'s
/// `checked_add_months` clamps a 31st onto the shorter month's last day rather
/// than rolling into the next month, which is the behaviour this wants: a
/// January 31st deposit expires at the end of February's month, never in March.
///
/// `None` when `months` is 0 (expiry disabled) or when the result is outside the
/// representable date range.
///
/// IT RETURNS `Option` AND THE CALLER CHECKS IT RATHER THAN STORING `None` FOR
/// BOTH CASES. An unrepresentable instant and a disabled feature are different
/// facts and are stored differently: disabled writes SQL NULL and the sweep
/// ignores the row; unrepresentable is refused at config load
/// (`config::AppConfig::validate`), so it cannot reach here in a shipped build.
/// This function still returns `None` for it rather than panicking, because the
/// alternative is a panic inside a settlement transaction.
pub fn credit_expiry_instant(settled_at: DateTime<Utc>, months: u32) -> Option<DateTime<Utc>> {
    if months == 0 {
        return None;
    }
    // `u32 -> u32` for `Months::new`, which takes the count directly. No cast,
    // so no `as` conversion and nothing for the arithmetic lint to flag.
    settled_at.checked_add_months(chrono::Months::new(months))
}

/// Atomically settles a topup and credits the wallet, recording an append-only ledger row.
///
/// The whole decision is one conditional `UPDATE`. The Postgres original took a
/// row lock (`SELECT ... FOR UPDATE`) and then decided in Rust; SQLite has no
/// row locks and rejects `FOR UPDATE` as a syntax error (measured), so the guard
/// moves into the `WHERE` clause and `RETURNING` supplies the row identity the
/// credit needs. This needs no lock at all and is correct under any isolation
/// level: the write lock SQLite takes for the `UPDATE` is what serializes the
/// check against the write.
///
/// `credit_expiry_months` stamps `topups.credit_expires_at`, which is what makes
/// expiry PER DEPOSIT: the instant is fixed at this settlement and never
/// recomputed, so a later spend cannot move it. 0 means the customer's `[wallet]`
/// setting disables expiry; the column stays NULL and nothing ever ages out.
pub async fn credit_topup_transaction(
    pool: &SqlitePool,
    order_id: &str,
    webhook_amount_idr: i64,
    credit_expiry_months: u32,
) -> Result<TopupCreditResult, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 1. Settle the row in one statement. `status = 'pending'` and the amount
    //    check are the guard; `rows_affected()` decides whether it fired.
    //
    //    `credit_expires_at` is stamped HERE, in the same statement, so the
    //    deposit's date and its expiry instant are one write and cannot be made
    //    to disagree by a crash between them. `None` (expiry disabled) writes
    //    NULL, which the sweep reads as "nothing to age out".
    let now = Utc::now();
    let credit_expires_at = credit_expiry_instant(now, credit_expiry_months);
    let settled = sqlx::query(
        "UPDATE topups SET status = 'settled', settled_at = ?, credit_expires_at = ? \
         WHERE order_id = ? AND status = 'pending' AND amount_idr = ? \
         RETURNING id, account_id",
    )
    .bind(now)
    .bind(credit_expires_at)
    .bind(order_id)
    .bind(webhook_amount_idr)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(settled) = settled else {
        // 2. Nothing was written. One SELECT decides which refusal this is.
        //    Status is tested before the amount, matching the precedence the
        //    original Rust checks had: a `settled` row with a mismatched amount
        //    is a replay, not a mismatch.
        let existing = sqlx::query("SELECT status, amount_idr FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;

        tx.rollback().await?;

        return Ok(match existing {
            None => TopupCreditResult::NotFound,
            Some(row) => {
                let status: String = row.get("status");
                let stored_amount_idr: i64 = row.get("amount_idr");
                if status == "settled" {
                    TopupCreditResult::AlreadySettled
                } else if stored_amount_idr != webhook_amount_idr {
                    TopupCreditResult::AmountMismatch
                } else {
                    TopupCreditResult::NotSettleable { status }
                }
            }
        });
    };

    let topup_id: Uuid = settled.get::<Hyphenated, _>("id").into_uuid();
    let account_id: Uuid = settled.get::<Hyphenated, _>("account_id").into_uuid();

    // 3. Credit the wallet. `webhook_amount_idr` is the same value the guard
    //    proved equal to the stored amount, so it is what the ledger must carry.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(webhook_amount_idr)
    .bind(now)
    .bind(account_id.hyphenated())
    .fetch_one(&mut *tx)
    .await?;

    let new_balance: i64 = wallet.get("balance_idr");

    // 4. Append to ledger. `created_at` shares the settle instant, so the row,
    //    the wallet and the ledger all carry one timestamp. The ref is the TOPUP
    //    id (`topup_ledger_ref`), so the row is selectable by top-up
    //    (docs/website/02-data-model.md:79).
    let ref_str = topup_ledger_ref(topup_id);
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'topup', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(webhook_amount_idr)
    .bind(ref_str)
    .bind(new_balance)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(TopupCreditResult::Settled { new_balance })
}

/// What one pass of the expiry sweep did.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct CreditExpirySweep {
    /// Deposits whose credit was retired by this pass.
    pub expired: u64,
    /// The IDR retired across them.
    pub expired_idr: i64,
}

/// Retires the credit of every deposit whose `credit_expires_at` has passed,
/// one deposit at a time, each in its own transaction.
///
/// ## Why this cannot be a flag or a wallet decrement
///
/// `wallets.balance_idr` must equal `SUM(ledger.delta_idr)` for the account -
/// `tools/reconcile/reconcile.sql`, launch Gate 2, and the invariant
/// `debit_usage_transaction` and `try_debit` are both written to preserve. So
/// retiring credit means appending a NEGATIVE ledger row and decrementing the
/// wallet TOGETHER, exactly as `try_debit` does, with `balance_after` carrying
/// the balance the row leaves behind.
///
/// ## Why `reason = 'usage'` and not a new value
///
/// The `ledger.reason` CHECK is frozen at
/// `('topup','usage','adjustment','refund')` and widening a CHECK on SQLite is a
/// table rebuild (`docs/decisions.md:70`). Expiry is not a refund (no money
/// moves) and not an operator adjustment. That leaves `usage`, and the `ref`
/// carries the distinction a customer or an auditor needs: it is
/// `expiry:<topup id>`, so an expiry row is selectable by the deposit it retired
/// and is never mistaken for a request that consumed credit.
///
/// ## Why each deposit is retired separately, oldest first
///
/// The amount retired is `topups.amount_idr` capped by what is left in the
/// wallet. That cap is not a column: the schema holds ONE un-aged balance, so
/// the sweep can only retire what remains. Ordering by `credit_expires_at` makes
/// a partially-spent wallet give up its OLDEST credit first, which is the only
/// case where "2 years from each deposit" and "2 years from the wallet" differ -
/// and the oldest-first reading is the one the policy states.
///
/// A deposit with nothing left to retire writes NO ledger row and is still
/// marked retired: the credit is already gone, and a zero-delta row would be a
/// ledger entry for an event that moved no money.
///
/// `now` is bound from Rust, never SQLite's `now()`.
pub async fn expire_credit(
    pool: &SqlitePool,
    now: DateTime<Utc>,
) -> Result<CreditExpirySweep, AppError> {
    // Candidates: settled deposits past their instant and not yet retired,
    // oldest first. `credit_expires_at IS NULL` is excluded, and that exclusion
    // is the fail-closed choice made visible: a NULL means either "expiry was
    // disabled at settlement" or "this row predates the column", and this sweep
    // treats both as NOTHING TO RETIRE. The migration that added the column says
    // the opposite (a NULL ages out into exclusion) - see the note on
    // `credit_retired_at` there for why the sweep cannot be the one to enforce
    // it without a second, distinct signal for "disabled".
    let candidates = sqlx::query_as::<_, (String, i64)>(
        "SELECT id, amount_idr FROM topups \
         WHERE status = 'settled' \
           AND credit_retired_at IS NULL \
           AND credit_expires_at IS NOT NULL \
           AND credit_expires_at <= ? \
         ORDER BY credit_expires_at, id",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;

    let mut sweep = CreditExpirySweep::default();

    for (topup_id, amount_idr) in candidates {
        let mut tx = begin_immediate(pool).await?;
        let swept = expire_one_deposit(&mut tx, &topup_id, amount_idr, now).await?;
        tx.commit().await?;

        if let Some(retired) = swept {
            // `checked_add` rather than `+=`: the crate denies
            // `clippy::arithmetic_side_effects` and a counter is not an exception.
            // These totals are reported to an operator, so they must be a fact
            // rather than a wrapped number.
            sweep.expired = sweep
                .expired
                .checked_add(1)
                .ok_or_else(|| AppError::Internal("credit expiry count overflowed u64".into()))?;
            sweep.expired_idr = sweep
                .expired_idr
                .checked_add(retired)
                .ok_or_else(|| AppError::Internal("credit expiry total overflowed i64".into()))?;
        }
    }

    Ok(sweep)
}

/// Retires one deposit's remaining credit inside an open transaction, returning
/// the IDR retired, or `None` when there was nothing left to take.
async fn expire_one_deposit(
    tx: &mut Transaction<'_, Sqlite>,
    topup_id: &str,
    amount_idr: i64,
    now: DateTime<Utc>,
) -> Result<Option<i64>, AppError> {
    // The account and its live balance, read in the same transaction that will
    // debit it. `credit_retired_at IS NULL` in the predicate means a concurrent
    // pass that retired this deposit first leaves nothing to find.
    let row = sqlx::query(
        "SELECT t.account_id AS account_id, COALESCE(w.balance_idr, 0) AS balance_idr \
         FROM topups t LEFT JOIN wallets w ON w.account_id = t.account_id \
         WHERE t.id = ? AND t.credit_retired_at IS NULL",
    )
    .bind(topup_id)
    .fetch_optional(&mut **tx)
    .await?;

    let Some(row) = row else {
        // Another pass retired it first. Nothing to do, and not an error.
        return Ok(None);
    };

    let account_id: Uuid = row.get::<Hyphenated, _>("account_id").into_uuid();
    let balance: i64 = row.get("balance_idr");

    // Retire at most the deposit's own amount, and at most what is left. The
    // second bound is what stops this sweep from taking a LATER deposit's credit
    // to satisfy an earlier one's expiry.
    let retired = amount_idr.min(balance);

    // Nothing left to take: mark it retired so the sweep stops revisiting it,
    // and write no ledger row. `balance` cannot go below zero here because no
    // debit is issued at all.
    if retired <= 0 {
        mark_credit_retired(tx, topup_id, now).await?;
        return Ok(None);
    }

    // THE GUARDED DEBIT, the same shape `try_debit` uses: the floor is inside
    // the statement, so a concurrent spend that emptied the wallet between the
    // read above and this write turns into no row rather than a negative
    // balance. `?1` is referenced twice so the debit and the floor cannot drift
    // apart.
    //
    // The arithmetic is `retired`, not `-retired`: `retired >= 1` and
    // `retired <= balance <= i64::MAX` were both just proved, so the only
    // subtraction that could overflow is `balance - retired`, and that is
    // between 0 and `i64::MAX`.
    #[allow(clippy::arithmetic_side_effects)]
    let debited = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 \
         WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
    )
    .bind(retired)
    .bind(now)
    .bind(account_id.hyphenated())
    .fetch_optional(&mut **tx)
    .await?;

    let Some(debited) = debited else {
        // The wallet could not cover it after all - spent concurrently. Mark the
        // deposit retired anyway: the credit is gone, and leaving it pending
        // would make every future sweep retry an impossible debit.
        mark_credit_retired(tx, topup_id, now).await?;
        return Ok(None);
    };

    let new_balance: i64 = debited.get("balance_idr");

    // `-retired` is safe for the reason stated above: `retired >= 1`, so the
    // negated operand cannot be `i64::MIN`.
    #[allow(clippy::arithmetic_side_effects)]
    let delta = -retired;

    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) \
         VALUES (?, ?, 'usage', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(delta)
    .bind(credit_expiry_ref(topup_id))
    .bind(new_balance)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    mark_credit_retired(tx, topup_id, now).await?;

    Ok(Some(retired))
}

/// The ledger `ref` an expiry row is filed under: the TOPUP id it retired,
/// prefixed so the row is never confused with a request batch id.
///
/// `topup_ledger_ref` deliberately does NOT do this. A deposit's own credit ref
/// is its bare id; an expiry row has to be distinguishable from it while still
/// being selectable by the same deposit, and the prefix is what gives both.
pub fn credit_expiry_ref(topup_id: &str) -> String {
    format!("expiry:{topup_id}")
}

/// Stamps `credit_retired_at`, guarded on it being NULL so a second attempt
/// updates no row rather than moving the recorded instant.
///
/// It takes the TRANSACTION, not a bare connection, because that is what every
/// caller holds: the mark and the ledger debit that accompanies it must commit
/// together, and accepting a loose connection would let a caller mark a deposit
/// retired outside the transaction that pays for it. Taking the wider type also
/// keeps the call sites a plain reborrow instead of `&mut **tx`.
async fn mark_credit_retired(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    topup_id: &str,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE topups SET credit_retired_at = ? WHERE id = ? AND credit_retired_at IS NULL",
    )
    .bind(now)
    .bind(topup_id)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// What a settlement attempt actually did.
///
/// The signature used to be `i64` (the new balance). It had to change: the caller
/// must be able to tell a settlement that collected the full cost from one that
/// collected only what the wallet held, because only the second is a loss. A bare
/// balance made those two identical, which is how the shortfall went unnoticed.
#[derive(Debug, PartialEq, Eq)]
pub enum UsageSettlement {
    /// The wallet covered `cost_idr` in full.
    Settled { new_balance: i64 },
    /// The wallet could not cover `cost_idr` at the moment of settlement. The real
    /// token counters WERE recorded and the ledger gained a row for exactly
    /// `debited_idr`, so `balance_idr = SUM(ledger.delta_idr)` still holds;
    /// `shortfall_idr` is money that was consumed and not collected, and is always
    /// positive here. The balance is never negative (docs/decisions.md D3).
    Partial {
        new_balance: i64,
        debited_idr: i64,
        shortfall_idr: i64,
    },
}

/// The pure shortfall decision: how much of `cost_idr` the wallet can actually pay.
///
/// Split out from the SQL so the clamp is testable without a database. The rule is
/// a clamp of the DEBIT, never of the balance:
/// the debit can be at most what the wallet holds, so the balance lands on 0 and
/// `CHECK (balance_idr >= 0)` is the backstop rather than the thing that refuses
/// the debit. Forcing the full debit would drive the balance negative, which
/// docs/decisions.md ratified as impossible.
///
/// A zero or negative balance debits nothing and the whole cost is shortfall: the
/// usage row is still written, because the tokens were genuinely consumed.
// SAFE BY CONSTRUCTION, and the argument is short enough to be worth writing.
//
//   debited = cost.max(0).min(available.max(0)), so 0 <= debited <= cost.max(0).
//   * cost >= 0:  0 <= debited <= cost,  so cost - debited lies in [0, cost].
//   * cost <  0:  debited == 0,         so cost - debited == cost.
//
// Neither arm can overflow: in the first the subtraction is bounded above by cost
// and below by zero; in the second one operand is literally zero. There is no input
// for which this panics in a debug build or wraps in a release one - the property
// the lint cannot see, and the one the deny would otherwise assert on our behalf.
//
// The second element is the SHORTFALL. The argument above is not only prose:
// `clamp_debit_holds_every_money_invariant_over_the_whole_domain` runs this function
// over the boundary grid (i64::MIN, i64::MIN+1, i64::MAX-1, i64::MAX, zero and the
// near-zero shapes money really takes), the overflow edge crossed with that grid on
// both sides, and 40 000 fixed-seed pairs of which half are drawn from the raw i64
// plane - so an input that broke the bound above would be found, and in a debug
// build an overflow would panic rather than quietly produce the wrong figure.
#[allow(clippy::arithmetic_side_effects)]
pub fn clamp_debit(cost_idr: i64, available_idr: i64) -> (i64, i64) {
    // A negative cost is not a charge; refusing to "collect" it must not turn into
    // a credit. Floored at zero, and a negative available balance debits nothing.
    let debited_idr = cost_idr.max(0).min(available_idr.max(0));
    (debited_idr, cost_idr - debited_idr)
}

/// The two ledger deltas a settlement writes, as a pure function of what was held
/// and what the request truly cost.
///
/// `reserve_balance_transaction` already wrote `-reserved_idr` when the request
/// started. Adding the two deltas here gives the whole request's net ledger move:
///
///   -reserved_idr + release_delta + charge_delta = -cost_idr
///
/// which is what `balance_idr = SUM(ledger.delta_idr)` requires at the commit
/// point. The release is written as its OWN row rather than folded into the charge
/// so the hold's reversal stays visible in an append-only log: `-reserved`,
/// `+reserved`, `-cost` is three auditable facts; `-cost` alone is one.
///
/// A negative argument is floored: a charge is never a credit and a release is
/// never a second hold.
// SAFE BY CONSTRUCTION, and this is the one that LOOKS like an overflow at a
// glance. The negation is applied to cost_idr.max(0), never to the raw value, so
// the negated operand is always >= 0:
//
//   * cost_idr == i64::MIN  ->  max(0) == 0  ->  -0 == 0.  No overflow, because
//     the magnitude was discarded BEFORE the negation.
//   * cost_idr >  i64::MIN  ->  max(0) is the value itself, and negating a
//     non-negative i64 is always representable - i64::MIN is the only value whose
//     negation overflows, and that case is the one handled above.
//
// Negating the RAW value instead is the classic money bug: -i64::MIN wraps to
// i64::MIN in a release build, which is a large POSITIVE delta, which is a credit.
#[allow(clippy::arithmetic_side_effects)]
pub fn settlement_ledger_deltas(released_idr: i64, cost_idr: i64) -> (i64, i64) {
    (released_idr.max(0), -cost_idr.max(0))
}

/// Atomically settles usage: releases the reservation, debits wallet, appends the
/// ledger rows, and upserts daily usage.
///
/// The balance check is a predicate on the UPDATE itself, so it cannot race a
/// concurrent request, and `CHECK (balance_idr >= 0)` is never the thing that
/// refuses the debit (which would surface as an opaque 500).
///
/// An unaffordable debit is NOT dropped: the reported usage is still recorded and
/// the debit is clamped to the balance (`UsageSettlement::Partial`). See
/// `clamp_debit` for why the debit is clamped rather than the balance forced.
///
/// `reserved_idr` is the hold `reserve_balance_transaction` took before the
/// request went upstream. Releasing it and charging the real cost happen in THIS
/// transaction, in that order, so no commit point ever shows a balance that the
/// ledger cannot explain: the hold is out of the wallet for the whole upstream
/// call, and the release row is written before the charge row.
#[allow(clippy::too_many_arguments)]
pub async fn debit_usage_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    // The model the request was routed to. Written to `usage_events` so the
    // dashboard's per-request history can name it; the aggregate tables do not
    // carry a model.
    model: &str,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
    reserved_idr: i64,
) -> Result<UsageSettlement, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 0. Release the hold first. The guard is the same one the take used: the
    //    wallet can never have spent more than its own balance, so the release
    //    always matches when a hold was actually taken, and a zero reservation
    //    (nothing held) skips the statement entirely.
    let released_idr = if reserved_idr > 0 {
        match try_credit(&mut tx, account_id, reserved_idr).await? {
            Some(_) => reserved_idr,
            None => {
                // No wallet row: nothing was ever held, so there is nothing to
                // give back. Recording a release anyway would credit money the
                // ledger never took.
                error!(
                    account_id = %account_id,
                    reserved_idr,
                    "Reservation release matched no wallet; nothing released"
                );
                0
            }
        }
    } else {
        0
    };

    // 1. Debit wallet.
    //
    // `balance_idr >= ?1` is the guard and it lives inside the statement, not in a
    // preceding read: the statement is its own check, and because SQLite admits one
    // writer at a time, two racing debits cannot both pass against one stale
    // balance.
    let new_balance: i64 = match try_debit(&mut tx, account_id, cost_idr).await? {
        Some(new_balance) => new_balance,
        None => {
            // No row: the account cannot cover `cost_idr`, or it has no wallet. This
            // read only fills in the decision - the predicate already made it.
            let current_balance = read_balance(&mut tx, account_id).await?;

            // The answer has already been streamed to the client by the time this
            // runs (proxy.rs: settlement is detached, deliberately). Discarding the
            // usage the upstream reported here is a money defect: the request is
            // delivered for free AND leaves no ledger row and no usage_daily row, so
            // the reconciliation check in docs/observability.md sees nothing. That
            // is NOT the documented `washed` case (docs/failover.md:140-144), which
            // is an upstream that reported no usage at all.
            //
            // So record what actually happened, in this same transaction: the real
            // token counters and a ledger row for what was ACTUALLY debited. The
            // shortfall is logged at error level - a silent undercharge is the same
            // class of defect as money silently moved twice.
            return settle_partial_usage(
                tx,
                account_id,
                api_key_id,
                model,
                input_tokens,
                cache_read_tokens,
                output_tokens,
                cost_idr,
                ref_batch,
                released_idr,
                current_balance,
            )
            .await;
        }
    };

    // 2. Release the hold, append the ledger debit and the usage row, then commit.
    //    `released_idr` is what step 0 credited back; passing it here is what
    //    writes the matching `+hold` ledger row. Omitting it credits the wallet
    //    without a ledger row, which is exactly the drift this invariant catches.
    record_usage(
        tx,
        account_id,
        api_key_id,
        model,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cost_idr,
        ref_batch,
        released_idr,
        cost_idr,
        new_balance,
    )
    .await?;

    Ok(UsageSettlement::Settled { new_balance })
}

/// How long per-request usage (`usage_events`) is retained, in days.
///
/// `docs/data-retention.md` settles this at **90 days**: it must outlast the
/// 30-day rolling spend window plus a dispute window. The constant is the
/// contract and stays as documented; a boundary that looks wrong is fixed in the
/// comparison, never by nudging this to compensate.
pub const USAGE_EVENTS_RETENTION_DAYS: i64 = 90;

/// How long daily usage aggregates are retained, in days (~24 months).
///
/// `docs/data-retention.md`: "Usage daily | 24 months | Billing disputes, then
/// aggregate only". 730 days is 24 months to the day at the common 365-day year;
/// the doc states the period in months, and a day count is what the comparison
/// needs.
pub const USAGE_DAILY_RETENTION_DAYS: i64 = 730;

/// How long an expired or revoked session row is kept, in days.
///
/// `docs/data-retention.md`: "Sessions (expired/revoked) | 30 days | Tidy up, but
/// keep recent for security review". The window runs from the session's own
/// `expires_at` (or `revoked_at` when it was logged out early), not from
/// creation.
pub const SESSION_RETENTION_DAYS: i64 = 30;

/// What one retention sweep deleted, per table. Named fields rather than a tuple
/// so a caller logging the result cannot silently swap two counts.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PurgedUsage {
    pub usage_events: u64,
    pub usage_daily: u64,
    pub sessions: u64,
    /// Expired verification and password-reset links.
    ///
    /// ADDED LAST, and it was the table this sweep had been missing. Its own
    /// purge existed (`identity::tokens::purge_expired`) with a unit test and NO
    /// PRODUCTION CALLER - no binary, no scheduler entry, nothing - so the
    /// function's own doc-comment, "the reason this runs at all is that the
    /// privacy page promises expired links do not persist", described a promise
    /// nothing kept. Every expired link a customer ever asked for was still on
    /// disk.
    ///
    /// It belongs HERE rather than in a third binary for the reason this struct's
    /// function already gives: two retention jobs are two places the policy can be
    /// forgotten, and this is the second time the same forgetting happened.
    pub identity_tokens: u64,
    /// Rows removed from `link_codes` — terminal Telegram link codes past their
    /// published day of grace.
    ///
    /// FOUND THE OPPOSITE WAY ROUND from `identity_tokens` above. That one was a
    /// purge with no caller; this one was a caller-shaped hole with no purge, and
    /// what pointed at it was this struct's own doc-comment two paragraphs down,
    /// which listed `link_codes` among the tables the sweep "deliberately does NOT
    /// touch" and gave the reason. The reason was that its rule is a different shape.
    /// By the time this field was added that was no longer true, because
    /// `identity_tokens` had already brought an expires-then-delete table into the
    /// sweep - so the exception was left over from a design the sweep had since
    /// outgrown, and it was holding a published window ("Until used or expired +
    /// 24h", `docs/data-retention.md:77` and `website/src/lib/privacy.ts`) that
    /// nothing implemented.
    ///
    /// A stale refusal is harder to find than a missing call, because it looks like
    /// a decision. It reads as "someone thought about this" and it is filed next to
    /// tables that genuinely do belong elsewhere.
    pub link_codes: u64,
}

/// The whole nightly retention sweep: `usage_events`, `usage_daily`, expired
/// `sessions`, expired verification/reset links, and terminal Telegram link
/// codes. Returns what each table lost.
///
/// THAT SENTENCE ENUMERATES FIVE TABLES AND THE SWEEP DELETES FROM TEN. The other
/// five (`key_ip_seen`, `key_ip_daily`, `link_redemption_attempts`, `auth_attempts`
/// and `link_code_issues`) are swept by `ip_tracking::purge_expired`, which this
/// function does not call — the maintenance entrypoint runs both, in inline SQL.
/// The list above is what THIS function removes, and it has been wrong in the
/// smaller direction twice: it once named three, and the two it gained were each a
/// published window with no code behind it. It is written out rather than left
/// vague because a doc-comment that overstates a sweep is how the next omission
/// gets missed.
///
/// ONE job sweeps every table with an age-based period, deliberately. Two
/// retention jobs means two places the policy can be forgotten, and that is not
/// hypothetical here: `usage_events` shipped populated with NO purge at all, and
/// `usage_daily`/`sessions` were in the same state, because the policy lived in
/// a document and nothing connected it to the code. Sweeping them together makes
/// the document's retention table the thing the code executes.
///
/// AND IT HAPPENED A SECOND TIME, which is why `identity_tokens` is in this list.
/// The identity port gave expired links a purge function with a unit test and no
/// caller of any kind, so an entire table the privacy page makes a promise about
/// was written and never swept. Finding it required reading the purge function's
/// doc-comment and then grepping for its callers, which is exactly the shape of
/// check this sweep's design was supposed to make unnecessary.
///
/// What this deliberately does NOT touch:
/// - `ledger` and `topups` — financial records, kept **forever**.
/// - `reviews` / `review_history` — kept **indefinitely**, and NOT "until the user
///   deletes them".
///
///   **THAT SECOND SENTENCE USED TO READ "kept until the user deletes them", and it
///   described a deletion that does not exist. It is corrected in place rather than
///   removed because the shape of the mistake is the one this whole doc-comment is
///   about.** There is no `DELETE FROM reviews` anywhere in
///   `server/src`. The only user-facing act is `POST /api/reviews/withdraw`, which sets
///   `withdrawn_at` and deliberately does not delete — the row has to keep occupying
///   the account's single slot, which is what "one review per account" means. So a
///   review is kept **indefinitely**.
///
///   **AND ITS AUTHOR CANNOT BE DELETED EITHER, which this paragraph used to get
///   backwards.** It read "`reviews.account_id` is `ON DELETE SET NULL`, so a review
///   outlives the account that wrote it and stays in the public aggregate with nobody
///   attached". That cannot happen: `reviews` carries
///   `CHECK (account_id IS NOT NULL OR telegram_id IS NOT NULL)`, and a website-written
///   review has `telegram_id IS NULL`, so the FK action clearing `account_id` would leave
///   (NULL, NULL) and the CHECK refuses the ENTIRE `DELETE FROM accounts`. MEASURED with
///   `PRAGMA foreign_keys=ON` (the CLI defaults to OFF, which made a first attempt at this
///   meaningless — no FK action fires, the DELETE appears to succeed, and the review still
///   shows its `account_id` exactly as if it had been protected). Both a funded account
///   and one with no wallet row are refused with "CHECK constraint failed".
///   `tools/sqlite-probes/validate-migration-schema.py` pins the constraint now, because
///   removing it would falsify this paragraph with no red build.
///
///   The one path that clears a body is account closure, and `status = 'closed'` is set by
///   NO code in this crate (the schema allows it; `grep -rn closed server/src` is empty),
///   so in practice nothing clears one either. And closure could not delete the account
///   while a review exists, for the CHECK above — it would have to clear the body and keep
///   the row, which is what "forever, unless the account is closed" means.
///   A retention row that says "until the user deletes them" reads as a bounded window
///   and is not one. `docs/data-retention.md` now says "forever, unless the account is
///   closed", and that correction is the reason to keep this paragraph rather than
///   quietly reword the line above.
/// - `key_ip_*` and `link_redemption_attempts` — swept by `ip-purge`, which owns
///   the salted-hash retention and the salt-rotation contract.
///
/// `link_codes` USED to be on that list, with the reason "its own `+24h after
/// use/expiry` rule is a different shape". That reason stopped being true when
/// `identity_tokens` was folded in above — an expires-then-delete rule is exactly
/// this sweep's shape, and it had been sitting out here while the privacy page and
/// `docs/data-retention.md:77` stated a twenty-four-hour window that no code
/// implemented. It is now swept by `routes::telegram::purge_terminal`, called below.
/// The entry stayed in this document for a while AFTER it became false, which is the
/// point: an exclusion list is read as a set of decisions, so a decision that has
/// been overtaken looks exactly like a decision that still holds.
///
/// Every cutoff is the same inclusive `<=` at midnight UTC of the cutoff day, for
/// the reason documented on `purge_expired_usage`: an exclusive comparison
/// silently retains N+1 days against an N-day promise.
/// How far a table has drifted past its own retention window, in days.
///
/// `Some(days)` is the AGE of the table's oldest row when that age EXCEEDS the
/// window - i.e. the sweep has left a row it promised to delete. `None` is "inside
/// the window", including the empty table.
///
/// **THIS MEASURES EVERY TABLE THE NIGHTLY SWEEP DELETES, and it did not always.**
/// The nightly job purges key_ip_seen (7 days), key_ip_daily (90) and
/// link_redemption_attempts (7) as well as the three below, and it once measured only
/// the three below - so if the sweep broke on the IP-hash tables the rows grew without
/// limit, the privacy page kept stating 7 and 90 days, and no alert, metric or log line
/// said so. `anything_behind` feeds the db_disk alert, whose stated condition was "any
/// age-based table holding a row past its retention window" - false for half of them,
/// and now corrected in alerts.tsv.
///
/// `auth_attempts` joins it on the same terms, and for the sharpest version of the same
/// reason: it is the credential-guessing counter behind the five `_per_hour` caps, its
/// IP-keyed half is a salted hash, and a sweep that stopped on it would leave that
/// history in the database while every document still promised 7 days. The account-keyed
/// half shares the window and therefore the field - see the field's doc.
///
/// `link_code_issues` is deliberately NOT here, and the reason is that it is not swept
/// on this path at all: `ip_tracking::purge_expired` deletes it and that function is
/// reachable only through `bin/ip-purge.rs`, which the maintenance entrypoint logs as
/// NOT WIRED. So a field for it would measure a table nothing on the nightly path
/// touches, and it would report "behind" forever the moment it held a row older than
/// seven days. That the table has a documented 7-day window and no wired sweep is a
/// real gap; it is recorded here rather than papered over with a measurement that would
/// fire permanently and be learned as noise.
///
/// The fix for the measured tables was three fields here, three more in
/// `oldest_days_by_table` (whose array length is a literal, so the compiler points at
/// each one), the lag query, and a constant for the link window, which until then lived
/// only in the entrypoint as a shell literal. That constant is the reason to do it
/// carefully: once the number is in Rust, the sweep guard can require the shell to
/// match it, and no such check existed before it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RetentionLag {
    /// Age of the oldest `usage_events` row, when past the 90-day window.
    pub usage_events: Option<i64>,
    /// Age of the oldest `usage_daily` row, when past the 730-day window.
    pub usage_daily: Option<i64>,
    /// Age of the oldest expired/revoked `sessions` row, when past the 30-day window.
    pub sessions: Option<i64>,
    /// Age of the oldest `key_ip_seen` day, when past the 7-day window.
    pub key_ip_seen: Option<i64>,
    /// Age of the oldest `key_ip_daily` day, when past the 90-day window.
    pub key_ip_daily: Option<i64>,
    /// Age of the oldest `link_redemption_attempts` row, when past the 7-day window.
    pub link_redemption_attempts: Option<i64>,
    /// Age of the oldest `auth_attempts` row, when past the 7-day window.
    ///
    /// Both keyings share the one window and therefore the one field: the table
    /// holds IP-keyed rows (a salted hash) and account-keyed rows (an account id
    /// and a timestamp), and `purge_expired` deletes both on `created_at` against
    /// `AUTH_ATTEMPT_RETENTION_DAYS`. A second field would have to measure the same
    /// column against the same number and could only disagree with this one.
    pub auth_attempts: Option<i64>,
    /// Age of the oldest EXPIRED verification/reset link, in days.
    ///
    /// MEASURED ON `expires_at`, NOT `created_at`, and that is the difference from
    /// every other field here. A link is not stale N days after it was made - it is
    /// stale the moment it expires, which is why `identity::tokens::purge_expired`
    /// compares against `now` rather than a midnight cutoff. A `created_at`
    /// measurement would report a yesterdays-created 30-minute reset link as
    /// inside its window while the sweep correctly deleted it, so the alert would
    /// disagree with the job it is watching.
    ///
    /// The window is therefore expressed as ZERO days past `expires_at`: anything
    /// expired at all is lagging.
    pub identity_tokens: Option<i64>,
    /// Age of the oldest `link_code_issues` row, in days, when past the 7-day window.
    ///
    /// FOUND BY THE GUARD THAT COMPARES THIS STRUCT TO THE PAYLOAD'S `windows_days`
    /// MAP, which is the kind of omission only a cross-check catches: the table WAS
    /// swept (by `ip_tracking::purge_expired`), so a sweep that stopped would have
    /// gone unnoticed, and it had no entry in the metrics payload either. Both ends
    /// of that pair are now present, and either going missing fails a test.
    ///
    /// An INSTANT on `created_at`, like `auth_attempts` and unlike the two `key_ip`
    /// tables: the column carries the same RFC3339 `+00:00` form.
    pub link_code_issues: Option<i64>,
    /// Age of the oldest TERMINAL `link_codes` row, in days, when past the
    /// 1-day grace.
    ///
    /// MEASURED ON `COALESCE(used_at, expires_at)`, the same expression
    /// `routes::telegram::purge_terminal` filters on — see that function for why a
    /// redeemed code ages from `used_at` and an unused one from `expires_at`. A
    /// measurement on either column alone would call rows behind that the sweep had
    /// already, correctly, deleted.
    ///
    /// That is not a hypothetical mismatch: it is the same mistake as the
    /// `created_at`-vs-`expires_at` one this struct's `identity_tokens` field
    /// documents, arrived at from the other direction. Both were caught by asking
    /// what the SWEEP compares, rather than what the table records.
    ///
    /// The window is the grace plus nothing, because the sweep has no midnight
    /// boundary to reach back to: an instant is stale once its own day of grace has
    /// passed.
    pub link_codes: Option<i64>,
}

/// The link-redemption window, which until now existed only as the `7` literal in the
/// maintenance entrypoint. It lives here so the measurement and the promise are the same
/// number, and so the sweep guard can require the shell to agree with this constant rather
/// than with whatever someone typed into a script.
pub const LINK_ATTEMPT_RETENTION_DAYS: i64 = 7;

/// How long an EXPIRED link may linger before the lag report calls it a problem.
///
/// ZERO, and it is the only window here that is not a policy number. The others
/// answer "how long do we KEEP this?"; this one answers "how long after a link
/// stops working may its row still exist?", and the answer the sweep implements is
/// "none" - `identity::tokens::purge_expired` deletes on `expires_at <= now`, with
/// no grace period at all.
///
/// Writing it as a constant rather than a bare `0` at the call site is what lets
/// the two be compared: a `0` inline reads like a placeholder someone forgot to
/// fill in, and the next reader would have to open the purge to learn that zero is
/// the intended value and not an oversight.
pub const IDENTITY_TOKEN_LAG_DAYS: i64 = 0;

/// How long a USED OR EXPIRED Telegram link code may linger before the lag report
/// calls it a problem.
///
/// The grace — one day — is `routes::telegram::LINK_CODE_RETENTION_GRACE_DAYS`, and
/// this constant exists so the measurement uses that same number rather than a `1`
/// typed here. The two were separate on purpose: the purge owns the policy, and a lag
/// report that carried its own copy could disagree with the job it is watching and
/// say a table was behind while the sweep was running correctly.
pub const LINK_CODE_LAG_GRACE_DAYS: i64 = crate::routes::telegram::LINK_CODE_RETENTION_GRACE_DAYS;

impl RetentionLag {
    /// Whether ANY age-based table is holding a row past its retention period.
    pub fn anything_behind(&self) -> bool {
        self.usage_events.is_some()
            || self.usage_daily.is_some()
            || self.sessions.is_some()
            || self.key_ip_seen.is_some()
            || self.key_ip_daily.is_some()
            || self.link_redemption_attempts.is_some()
            || self.auth_attempts.is_some()
            || self.identity_tokens.is_some()
            || self.link_code_issues.is_some()
            || self.link_codes.is_some()
    }

    /// Each table with the age of its oldest row, in the order the docs list them.
    ///
    /// The NAME is carried alongside the value so a caller cannot swap two tables in
    /// a log line, the same reasoning as `PurgedUsage`'s named fields.
    ///
    /// The array LENGTH is a literal on purpose: adding a field to the struct without
    /// adding it here is a compile error rather than a silently shorter report, which is
    /// the failure this change exists to remove. It caught the one test that built a
    /// `RetentionLag` literally, and that is the whole argument for writing the count out
    /// rather than trimming the report to fit.
    pub fn oldest_days_by_table(&self) -> [(&'static str, Option<i64>); 10] {
        [
            ("usage_events", self.usage_events),
            ("usage_daily", self.usage_daily),
            ("sessions", self.sessions),
            ("key_ip_seen", self.key_ip_seen),
            ("key_ip_daily", self.key_ip_daily),
            ("link_redemption_attempts", self.link_redemption_attempts),
            ("auth_attempts", self.auth_attempts),
            ("identity_tokens", self.identity_tokens),
            ("link_code_issues", self.link_code_issues),
            ("link_codes", self.link_codes),
        ]
    }
}

/// Whether the age-based tables still hold rows older than their retention windows.
///
/// WHY THIS EXISTS, and why it is not a disk percentage. The "DB disk" row of the
/// Alerts table in `docs/observability.md` states the `db_disk` alert with the
/// condition "volume usage" but the ACTION
/// "Usage rows growing; check retention" - so the operator's real question is whether
/// RETENTION IS WORKING. A volume figure cannot answer that: 80% full is normal for a
/// database doing its job, and a sweep that silently STOPPED is an incident at any
/// size, because it means a published data-retention promise is being broken.
///
/// That is not hypothetical here. `purge_expired_usage`'s own doc-comment records
/// that these tables once "shipped populated with NO purge at all", because the
/// policy lived in a document and nothing connected it to the code. Nothing could
/// detect a REPEAT of that either - a sweep that ran yesterday and not since looks
/// identical to a healthy one - and this query is what closes that gap.
///
/// ONE query per table rather than a UNION, because each table has its own window and
/// its own timestamp column, and a UNION would need the cutoff expression three times
/// in one statement with three different bound values. (SQLite binds positionally.)
///
/// The comparison mirrors `purge_expired_usage`'s inclusive `<=` at midnight UTC. If
/// the two disagreed, a row could be simultaneously "old enough to delete" and "not
/// lagging", and nothing would catch the drift.
///
/// NO aggregation over the whole table, deliberately: `MIN(created_at)` on an indexed
/// column is a single index seek, so this is cheap enough to serve from the metrics
/// route that a operator polls. A COUNT of stale rows would be a full scan and is not
/// needed to answer "is retention working".
/// The age of the OLDEST row in one age-based table, when that age exceeds the
/// table's retention window. `None` when the table is empty or every row is inside
/// the window.
///
/// A free function rather than a closure: it needs its own `async` body and an
/// explicit error type, and a closure carrying a lifetime-bound `pool` reference
/// fights the borrow checker for no benefit.
// SAFE, but for a different reason than the money functions, and worth being
// explicit about: chrono's date arithmetic PANICS on an out-of-range result
// rather than wrapping. It cannot produce the silent corruption the release
// profile allows elsewhere, so the failure mode here is a panic and not a wrong
// ledger figure - which is a better failure and still a failure.
//
// The bound is the retention window itself. `days` is a call-site constant (
// USAGE_EVENTS_RETENTION_DAYS and its siblings, all under a few hundred), never a
// request-supplied value, and `today` is the current date. Subtracting a few
// hundred days from a date near the chrono epoch is the only way to leave the
// representable range, and the epoch is 262143 BCE.
//
// If a caller ever passes an unbounded `days`, the panic becomes the correct
// behaviour and this comment becomes wrong - which is the point of writing the
// assumption down where the deny can no longer see it.
#[allow(clippy::arithmetic_side_effects)]
async fn oldest_row_past_window(
    pool: &SqlitePool,
    table: &'static str,
    column: &'static str,
    days: i64,
    today: chrono::NaiveDate,
) -> Result<Option<i64>, AppError> {
    // The cutoff at midnight UTC of the cutoff day, matching the purge exactly.
    let cutoff = (today - chrono::Duration::days(days))
        .and_hms_opt(0, 0, 0)
        .expect("midnight is a valid time")
        .and_utc();

    // A DATE-keyed table stores `2023-12-14`, not an RFC3339 instant, and decoding
    // that as a DateTime is an ERROR rather than a wrong number - which is what made
    // the whole retention query fail on any database holding usage_daily rows, and the
    // metrics endpoint report `behind: null` forever. The probe turns that into a
    // standing db_disk alert, so the monitoring was not merely blind but LOUD.
    //
    // The branch is on the column name because SQLite exposes no per-column type sqlx
    // can ask for on this path, and a wrong guess fails LOUDLY - a decode error - rather
    // than silently comparing the wrong thing. That is the right direction for a type
    // test; the alternative is a comparison that quietly measures the wrong column.
    if column == "day" {
        let oldest: Option<chrono::NaiveDate> =
            sqlx::query_scalar(&format!("SELECT MIN({column}) FROM {table}"))
                .fetch_one(pool)
                .await?;
        let cutoff_day = today - chrono::Duration::days(days);
        return Ok(oldest
            .filter(|d| *d <= cutoff_day)
            .map(|d| (today - d).num_days()));
    }
    // The oldest row overall. `MIN` over an empty table is NULL, which decodes to
    // `None` - the "no rows" case, distinct from "an old row".
    let oldest: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar(&format!("SELECT MIN({column}) FROM {table}"))
            .fetch_one(pool)
            .await?;

    // INSIDE the window (or empty) -> not lagging. The boundary is the same `<=` the
    // purge uses, so a row AT the cutoff is lagging in both and they cannot drift.
    match oldest {
        Some(at) if at <= cutoff => {
            // Age in days, measured to the SAME midnight the cutoff uses, so a row
            // exactly on the boundary reports exactly the window length.
            let midnight_today = today
                .and_hms_opt(0, 0, 0)
                .expect("midnight is a valid time")
                .and_utc();
            Ok(Some((midnight_today - at).num_days()))
        }
        _ => Ok(None),
    }
}
pub async fn retention_lag(
    pool: &SqlitePool,
    today: chrono::NaiveDate,
) -> Result<RetentionLag, AppError> {
    let usage_events = oldest_row_past_window(
        pool,
        "usage_events",
        "created_at",
        USAGE_EVENTS_RETENTION_DAYS,
        today,
    )
    .await?;
    // `usage_daily` is keyed by DAY, not an instant, so its column is `day`.
    let usage_daily = oldest_row_past_window(
        pool,
        "usage_daily",
        "day",
        USAGE_DAILY_RETENTION_DAYS,
        today,
    )
    .await?;
    // Sessions age from `expires_at`, matching the purge.
    let sessions = oldest_row_past_window(
        pool,
        "sessions",
        "expires_at",
        SESSION_RETENTION_DAYS,
        today,
    )
    .await?;

    // The four salted-hash-adjacent tables, each measured on the SAME column the sweep
    // filters on. The two key_ip tables are DATE-keyed and take the branch added for
    // usage_daily; link_redemption_attempts is an instant like usage_events, and
    // auth_attempts is an instant for the same reason (`created_at` is an RFC3339 TEXT
    // instant, not a date, and the sweep binds midnight UTC of the cutoff day to
    // compare against it).
    let key_ip_seen = oldest_row_past_window(
        pool,
        "key_ip_seen",
        "day",
        crate::ip_tracking::SEEN_RETENTION_DAYS,
        today,
    )
    .await?;
    let key_ip_daily = oldest_row_past_window(
        pool,
        "key_ip_daily",
        "day",
        crate::ip_tracking::DAILY_RETENTION_DAYS,
        today,
    )
    .await?;
    let link_redemption_attempts = oldest_row_past_window(
        pool,
        "link_redemption_attempts",
        "attempted_at",
        LINK_ATTEMPT_RETENTION_DAYS,
        today,
    )
    .await?;
    // The credential-guessing counter, on the SAME `created_at` column the sweep
    // filters on. An INSTANT like `link_redemption_attempts` and unlike the two
    // key_ip tables, because `auth_attempts.created_at` carries the same RFC3339
    // `+00:00` form - so it takes the `day`-vs-instant branch's instant side, and
    // the two keyings (IP and account) share the one window and the one field.
    let auth_attempts = oldest_row_past_window(
        pool,
        "auth_attempts",
        "created_at",
        crate::ip_tracking::AUTH_ATTEMPT_RETENTION_DAYS,
        today,
    )
    .await?;

    // EXPIRED LINKS, measured on `expires_at` with a ZERO-day window: the sweep
    // deletes a link the moment it expires, so "behind" means "expired and still
    // here". A `created_at` measurement (what every other field uses) would call a
    // 30-minute reset link made yesterday "inside its window" while the purge had
    // already - correctly - removed it, and the alert would then disagree with the
    // job it exists to watch.
    let identity_tokens = oldest_row_past_window(
        pool,
        "identity_tokens",
        "expires_at",
        IDENTITY_TOKEN_LAG_DAYS,
        today,
    )
    .await?;

    // The link-code issuance counter, which the sweep has always covered and this
    // report never did. An INSTANT on `created_at`, the same column and form as
    // `auth_attempts` directly above, so the same helper path.
    let link_code_issues = oldest_row_past_window(
        pool,
        "link_code_issues",
        "created_at",
        crate::ip_tracking::LINK_CODE_ISSUE_RETENTION_DAYS,
        today,
    )
    .await?;

    // TERMINAL LINK CODES, measured on the same `COALESCE(used_at, expires_at)` the
    // purge filters on, with the same one-day grace. Not `expires_at` alone: a code
    // redeemed immediately is stale from `used_at`, and measuring its `expires_at`
    // would report it behind for the rest of a five-minute TTL it no longer has.
    let link_codes = oldest_row_past_window(
        pool,
        "link_codes",
        "COALESCE(used_at, expires_at)",
        LINK_CODE_LAG_GRACE_DAYS,
        today,
    )
    .await?;

    Ok(RetentionLag {
        usage_events,
        usage_daily,
        sessions,
        key_ip_seen,
        key_ip_daily,
        link_redemption_attempts,
        auth_attempts,
        identity_tokens,
        link_code_issues,
        link_codes,
    })
}
// SAFE for the same reason as `oldest_row_past_window`: chrono panics rather
// than wraps on a date out of range, and the operands are retention-window
// constants against the current date. See that function's comment for the full
// argument - it is the same shape and repeating it here would rot independently.
#[allow(clippy::arithmetic_side_effects)]
pub async fn purge_expired_usage(
    pool: &SqlitePool,
    today: chrono::NaiveDate,
) -> Result<PurgedUsage, AppError> {
    let midnight = |days: i64| {
        (today - chrono::Duration::days(days))
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc()
    };

    let usage_events = sqlx::query("DELETE FROM usage_events WHERE created_at <= ?")
        .bind(midnight(USAGE_EVENTS_RETENTION_DAYS))
        .execute(pool)
        .await?
        .rows_affected();

    // `day` is a DATE in `YYYY-MM-DD` form (a TEXT column), so the bound is a
    // date, not an instant. Binding an instant here would make the longer string
    // sort AFTER the stored dates and the DELETE would match nothing.
    let usage_daily = sqlx::query("DELETE FROM usage_daily WHERE day <= ?")
        .bind(today - chrono::Duration::days(USAGE_DAILY_RETENTION_DAYS))
        .execute(pool)
        .await?
        .rows_affected();

    // A session is removable once it stopped being USABLE at least 30 days ago.
    // "Stopped being usable" is `revoked_at` when it was logged out early, and
    // `expires_at` otherwise — so the governing instant is
    // `COALESCE(revoked_at, expires_at)`.
    //
    // ONLY that column is compared. Adding `AND expires_at <= cutoff` would be
    // wrong the other way: it would retain a session revoked early whose
    // `expires_at` is still in the future, which is the common case for a user who
    // logs out promptly — the row would linger long past the 30 days the doc
    // promises.
    //
    // The cutoff is a full RFC3339 instant because both columns are timestamps.
    let sessions = sqlx::query("DELETE FROM sessions WHERE COALESCE(revoked_at, expires_at) <= ?")
        .bind(midnight(SESSION_RETENTION_DAYS))
        .execute(pool)
        .await?
        .rows_affected();

    // EXPIRED LINKS, and this one compares against NOW rather than a midnight
    // cutoff, because a link's lifetime is not a retention policy - it is the
    // token's own expiry, and `consume` refuses a row past it. A token that
    // expired an hour ago is already useless; keeping it would be keeping a
    // credential that either cannot be redeemed or, worse, one whose expiry the
    // verifier came to disagree with.
    //
    // Measured against the SAME instant `identity::tokens::purge_expired` uses, so
    // the two cannot disagree about which rows are stale - that function is called
    // here rather than the SQL being written twice.
    let identity_tokens = crate::identity::tokens::purge_expired(pool, chrono::Utc::now()).await?;

    // LINK CODES, on the same terms as the links above and for the same reason: the
    // boundary is the code's own lifetime plus the published day of grace, not a
    // midnight cutoff. `purge_terminal` owns the `COALESCE(used_at, expires_at)`
    // predicate; the SQL is not restated here.
    //
    // This is the third table to be added to this sweep because a document promised
    // a window and no code implemented it. The first two were found by grepping for
    // callers of an orphaned purge; this one was found the other way round - by
    // reading the doc-comment below that explained why `link_codes` was deliberately
    // left out, and checking whether the reason was still true. It was not: the
    // stated reason was that the rule "is a different shape", and by then
    // `identity_tokens` had already established that an expires-then-delete rule
    // belongs in this sweep rather than in one of its own.
    let link_codes = crate::routes::telegram::purge_terminal(pool, chrono::Utc::now()).await?;

    Ok(PurgedUsage {
        usage_events,
        usage_daily,
        sessions,
        identity_tokens,
        link_codes,
    })
}

/// Writes the two rows a settlement owns, then commits: the append-only ledger
/// debit and the `usage_daily` upsert.
///
/// Shared by the settled and the partial path so the token counters (input,
/// cache_read and output kept separate - never summed), the daily cost and the
/// `balance_after` invariant cannot drift apart between them.
///
/// `usage_cost_idr` is what the request cost, and is what the dashboard and the
/// 30-day spend reporting read; `charged_idr` is what the ledger records. The two
/// differ only when the wallet could not cover the cost in full.
///
/// `released_idr` is a reservation being handed back, and it is written as its
/// own row BEFORE the charge. The hold was appended when the reservation was
/// taken, so reversing it here is what keeps the ledger invariant
/// (`balance_idr = SUM(ledger.delta_idr)`) true at the commit point: across the
/// whole request the ledger moves `-reserved + released - charged`, which is
/// exactly `-charged` because the whole hold comes back.
#[allow(clippy::too_many_arguments)]
async fn record_usage(
    mut tx: Transaction<'_, Sqlite>,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    model: &str,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    usage_cost_idr: i64,
    ref_batch: Option<&str>,
    released_idr: i64,
    charged_idr: i64,
    new_balance: i64,
) -> Result<(), AppError> {
    // The release and the charge, from one pure rule so the ledger cannot drift:
    // `-reserved + release_delta + charge_delta` is exactly `-cost`.
    let (release_delta, charge_delta) = settlement_ledger_deltas(released_idr, charged_idr);

    if release_delta != 0 {
        // CHECKED RATHER THAN ALLOWED. This is the one arithmetic site in the
        // settlement path that is worth spending a check on, because its result is
        // the balance the ledger row claims to leave behind - the figure
        // reconcile.sh sums - rather than a derived quantity.
        //
        // The bound is enormous: charge_delta is at most the cost of one request,
        // which is derived from token counts capped at streaming.hard_max_output_tokens
        // (384,000) times a per-million rate, so reaching i64::MAX (9.2e18 IDR, about
        // 6e11 USD) is not reachable by any input the proxy will accept. "Not
        // reachable today" is exactly the kind of claim that silently stops being
        // true, and the failure mode if it became false is the worst in this file:
        // a wrapped NEGATIVE balance written into an append-only ledger, which the
        // reconciliation query would then flag - or worse, a wrap that lands on a
        // plausible positive figure, which it would not.
        //
        // So the check costs one branch on a cold path and turns an unrepresentable
        // result into a refused settlement, which the caller handles by releasing
        // the hold. Every other site in this file is bounded by construction and
        // carries a written argument instead; this one is bounded by MAGNITUDE,
        // which is a different and weaker kind of argument.
        let balance_after_release = new_balance.checked_sub(charge_delta).ok_or_else(|| {
            AppError::Internal(format!(
                "settlement balance overflow: {new_balance} minus charge {charge_delta}"
            ))
        })?;
        // The balance the release left: the charge below has not been taken yet.
        insert_ledger_row(
            &mut tx,
            account_id,
            release_delta,
            ref_batch,
            balance_after_release,
        )
        .await?;
    }

    insert_ledger_row(&mut tx, account_id, charge_delta, ref_batch, new_balance).await?;

    // Upsert usage_daily.
    //
    // The conflict target is the expression index, not a column list. Measured:
    // the Postgres-shaped `ON CONFLICT (account_id, api_key_id, day)` is refused
    // outright with "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE
    // constraint", because the real key is
    // `usage_daily_scope_uniq (account_id, day, COALESCE(api_key_id, ''))`.
    // SQLite has no `ON CONFLICT ON CONSTRAINT <name>` form, so the expression
    // has to be spelled out. Measured: the expression form accumulates a NULL
    // key and a real key into two separate rows (15 and 10 from 10+5 and 7+3),
    // which is the behaviour the COALESCE index exists to provide.
    let today = crate::ip_tracking::today_utc();
    sqlx::query(
        r#"
        INSERT INTO usage_daily (
            account_id, api_key_id, day,
            input_tokens, cache_read_tokens, output_tokens, cost_idr
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_id, day, COALESCE(api_key_id, '')) DO UPDATE
        SET input_tokens = usage_daily.input_tokens + EXCLUDED.input_tokens,
            cache_read_tokens = usage_daily.cache_read_tokens + EXCLUDED.cache_read_tokens,
            output_tokens = usage_daily.output_tokens + EXCLUDED.output_tokens,
            cost_idr = usage_daily.cost_idr + EXCLUDED.cost_idr
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(api_key_id.map(|k| k.hyphenated()))
    .bind(today)
    .bind(input_tokens)
    .bind(cache_read_tokens)
    .bind(output_tokens)
    .bind(usage_cost_idr)
    .execute(&mut *tx)
    .await?;

    // Append the per-request row, in the SAME transaction as the counters it
    // summarises. `usage_daily` is an aggregate with no model and no ordering;
    // `usage_events` is the detail behind the dashboard's "recent requests", and
    // writing it here is what makes the two sum to the same tokens. A row written
    // in a separate transaction could commit while the aggregate rolled back (or
    // the reverse), and the per-request view would then disagree with the totals.
    //
    // The id is a fresh UUID rather than a natural key: two identical requests in
    // the same second are legitimately two rows, and nothing needs to dedupe them.
    // `ref` is the same reservation ref the ledger rows carry, so one request's
    // ledger movement and its usage event can be tied together by hand.
    sqlx::query(
        r#"
        INSERT INTO usage_events (
            id, account_id, api_key_id, model,
            input_tokens, cache_read_tokens, output_tokens, cost_idr, ref, created_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(api_key_id.map(|k| k.hyphenated()))
    .bind(model)
    .bind(input_tokens)
    .bind(cache_read_tokens)
    .bind(output_tokens)
    // The event records the REAL cost of the request, matching what usage_daily
    // accumulated (`usage_cost_idr`), not `charged_idr`. When the wallet could
    // not cover the cost in full the two differ, and the dashboard must show what
    // the request cost, with the ledger already recording what was collected.
    .bind(usage_cost_idr)
    .bind(ref_batch)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

/// The wallet balance, or 0 when the account has no wallet row.
async fn read_balance(tx: &mut Transaction<'_, Sqlite>, account_id: Uuid) -> Result<i64, AppError> {
    // Annotated, not inferred: `unwrap_or(0)` alone would leave the scalar type to
    // default to i32, which is wrong for money. `balance_idr` is a 64-bit INTEGER,
    // and a balance above `i32::MAX` would fail to decode rather than read back.
    let balance: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(0);

    Ok(balance)
}

/// Debits `amount` with the balance guard inside the statement, returning the
/// balance after it, or `None` when the wallet cannot cover it.
///
/// `amount` of 0 updates nothing and returns the current balance: a zero debit is
/// a real outcome here (an empty wallet), not a reason to skip the statement.
async fn try_debit(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    // `?1` is referenced twice — once as the debit, once as the balance floor.
    // Measured: sqlx accepts the numbered form, takes three binds for `?1 ?2 ?3`,
    // and the reuse really does compare against the one value (a 150 wallet
    // debited by 100 leaves 50, and the same statement against 50 matches no
    // row). Plain `?` with a duplicated bind also works; the numbered form is
    // used because it cannot drift out of sync with the predicate.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
    )
    .bind(amount)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut **tx)
    .await?;

    Ok(wallet.map(|w| w.get("balance_idr")))
}

/// Appends one append-only ledger row. `balance_after` is the wallet balance the
/// row leaves behind, which is what makes the ledger self-explaining after a
/// crash: the running balance can be replayed without the wallet row.
async fn insert_ledger_row(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    delta_idr: i64,
    ref_batch: Option<&str>,
    balance_after: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'usage', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(delta_idr)
    .bind(ref_batch)
    .bind(balance_after)
    .bind(Utc::now())
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Credits `amount` to the wallet, returning the balance after it.
///
/// Unconditional on purpose: this is only ever a RESERVATION being handed back,
/// never money arriving from outside. A hold the wallet took itself can always be
/// returned, so there is nothing to guard against — unlike `try_debit`, which
/// must never let the balance go negative.
async fn try_credit(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(amount)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut **tx)
    .await?;

    Ok(wallet.map(|w| w.get("balance_idr")))
}

/// What taking a reservation did.
#[derive(Debug, PartialEq, Eq)]
pub enum ReservationResult {
    /// The wallet is debited by `reserved_idr` and the ledger holds the matching
    /// negative row. The money is out of the balance for the whole request, which
    /// is the point: a concurrent request sees it gone.
    Held { reserved_idr: i64, new_balance: i64 },
    /// Nothing was held: the wallet cannot cover `reserved_idr` right now, or the
    /// account has no wallet at all.
    Insufficient { balance_idr: i64 },
    /// Nothing to hold. A zero-amount reservation must not write a ledger row — a
    /// zero delta is noise in an append-only money log.
    Zero,
}

/// Takes the worst-case reservation BEFORE the request goes upstream, and returns
/// whether it was held.
///
/// This is the fix for the overdraw defect. The old check read `balance_idr` in
/// one statement and debited nothing, so N concurrent requests from one account
/// all passed the same point-in-time value and an account holding 1 IDR could run
/// unbounded expensive requests. Here the check IS the debit:
/// `balance_idr >= ?1` is a predicate on the UPDATE, and the UPDATE is its own
/// check: SQLite admits one writer at a time, so exactly as many concurrent
/// requests as the balance can pay for are admitted and the rest match no row.
/// Concurrency is serialized by the database's write lock, not by a read.
///
/// The hold is a real, guarded debit with its own ledger row, taken in one
/// transaction and committed before the upstream is called. `balance_idr` and
/// `SUM(ledger.delta_idr)` therefore move together, and a crash between the hold
/// and the settlement leaves the money debited and the row written — a visible
/// held reservation, never a balance the ledger cannot explain.
///
/// This is NOT `allow_negative_balance_overdraft`: the CHECK constraint is never
/// bypassed and the balance never goes negative (docs/decisions.md D3).
pub async fn reserve_balance_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<ReservationResult, AppError> {
    if reserved_idr <= 0 {
        return Ok(ReservationResult::Zero);
    }

    let mut tx = begin_immediate(pool).await?;

    // The guard lives inside the statement, never in a preceding read.
    match try_debit(&mut tx, account_id, reserved_idr).await? {
        Some(new_balance) => {
            // SAFE BY THE GUARD ABOVE, not by arithmetic luck. `reserved_idr <= 0`
            // already returned `ReservationResult::Zero`, so the negated operand is
            // a strictly positive i64, and i64::MIN - the only value whose negation
            // overflows - is negative and cannot reach here. Negating a positive i64
            // is always representable, in a debug build and a release one alike.
            #[allow(clippy::arithmetic_side_effects)]
            insert_ledger_row(&mut tx, account_id, -reserved_idr, ref_batch, new_balance).await?;
            tx.commit().await?;

            Ok(ReservationResult::Held {
                reserved_idr,
                new_balance,
            })
        }
        None => {
            // No row matched: the decision is already made. This read only fills in
            // the detail the caller shows the customer, and failing it must not turn
            // a visible refusal into an opaque 500. Nothing was written, so there is
            // nothing to roll back beyond the empty transaction.
            let balance_idr = read_balance(&mut tx, account_id).await.unwrap_or(0);
            tx.rollback().await?;

            Ok(ReservationResult::Insufficient { balance_idr })
        }
    }
}

/// Gives a held reservation back IN FULL, in its own transaction.
///
/// Used on the paths where no billable usage exists: the upstream was never
/// reached, the stream ended without a usage report (the documented washed case,
/// docs/failover.md (Mid-stream failure and billing)), or the settlement channel closed with no outcome.
/// The credit is the exact inverse of the guarded debit that took the hold, so the
/// ledger nets to zero and no money is created.
///
/// `Ok(None)` means nothing was released — a zero reservation, or no wallet row.
pub async fn release_reservation_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<Option<i64>, AppError> {
    if reserved_idr <= 0 {
        return Ok(None);
    }

    let mut tx = begin_immediate(pool).await?;

    let Some(new_balance) = try_credit(&mut tx, account_id, reserved_idr).await? else {
        // No wallet row: nothing was ever held, so nothing is released and no
        // ledger row is written. A credit the ledger cannot back is the one thing
        // this function must never do.
        tx.rollback().await?;
        return Ok(None);
    };

    insert_ledger_row(&mut tx, account_id, reserved_idr, ref_batch, new_balance).await?;
    tx.commit().await?;

    Ok(Some(new_balance))
}

/// Records usage the wallet could not cover in full, and returns what was lost.
///
/// Runs on the transaction `debit_usage_transaction` already opened: the guarded
/// UPDATE matching no row is the decision that the balance is short. The clamp is
/// computed from the balance read inside that same transaction.
#[allow(clippy::too_many_arguments)]
async fn settle_partial_usage(
    mut tx: Transaction<'_, Sqlite>,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    model: &str,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
    released_idr: i64,
    available_idr: i64,
) -> Result<UsageSettlement, AppError> {
    let (debited_idr, _) = clamp_debit(cost_idr, available_idr);

    // The clamped debit fits by construction, but it can still miss: a concurrent
    // settlement can take the balance between the caller's guarded UPDATE and the
    // read that sized this clamp, and a row failing the guard is not locked. Re-clamp
    // once against the balance as it is now. A second miss leaves the debit at zero,
    // which is the safe floor: a zero ledger delta cannot break
    // balance_idr = SUM(ledger.delta_idr) whatever the other transaction did, and the
    // usage is recorded either way - discarding it is the defect being fixed.
    let (debited_idr, new_balance) = match try_debit(&mut tx, account_id, debited_idr).await? {
        Some(new_balance) => (debited_idr, new_balance),
        None => {
            let available_now = read_balance(&mut tx, account_id).await?;
            let (retry_idr, _) = clamp_debit(cost_idr, available_now);
            match try_debit(&mut tx, account_id, retry_idr).await? {
                Some(new_balance) => (retry_idr, new_balance),
                None => (0, read_balance(&mut tx, account_id).await?),
            }
        }
    };

    // Recomputed rather than carried: the retry above can change what was debited.
    //
    // SAFE BY CONSTRUCTION, and it is the same argument as `clamp_debit` above:
    // `debited_idr` came out of that function (or out of a retry through it, or is
    // a literal 0), so `0 <= debited_idr <= cost_idr.max(0)` and the subtraction is
    // bounded above by cost and below by zero. The shortfall is a NON-NEGATIVE
    // figure by definition - it is money the customer owed and did not have.
    #[allow(clippy::arithmetic_side_effects)]
    let shortfall_idr = cost_idr - debited_idr;

    // usage_daily carries the FULL cost: the tokens were consumed and the counters
    // drive the dashboard and the 30-day spend window (routes/keys.rs). The ledger
    // carries only what was actually taken, which is what keeps
    // balance_idr = SUM(ledger.delta_idr) true.
    record_usage(
        tx,
        account_id,
        api_key_id,
        model,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cost_idr,
        ref_batch,
        released_idr,
        debited_idr,
        new_balance,
    )
    .await?;

    // A concurrent topup can land between the failed attempt and the retry, in
    // which case the cost WAS collected in full and this is an ordinary
    // settlement. Reporting it as a shortfall would be a false alarm.
    if shortfall_idr <= 0 {
        return Ok(UsageSettlement::Settled { new_balance });
    }

    // Loud, and only after the transaction committed: this is now on the books.
    error!(
        account_id = %account_id,
        key_id = ?api_key_id,
        cost_idr,
        debited_idr,
        shortfall_idr,
        new_balance,
        "Usage settled PARTIALLY: balance could not cover the reported cost"
    );

    Ok(UsageSettlement::Partial {
        new_balance,
        debited_idr,
        shortfall_idr,
    })
}

// THE MONEY GATE IS NOT HERE.
//
// A function used to stand in this place, and it was the third time in this crate's
// history that a copy of a money rule outlived the thing that superseded it and then
// read like the real thing. Deleted, with the reason:
//
// - IT WAS PRODUCTION-DEAD. Its only callers were its own four tests. The benchmark
//   doc that named it was struck once that claim was checked, and it named this
//   function as the thing the benchmark ran.
// - IT WAS WEAKER THAN THE REAL GATE. It anchored on wallets with a LEFT JOIN, so an
//   account with ledger money and NO wallets row - the cache missing entirely - was
//   invisible to it. tools/reconcile/reconcile.sql uses a FULL OUTER JOIN for exactly
//   that case, and its comment records the incident: a +250000 adjustment with no
//   wallet row used to return nothing and the gate reported PASSING.
// - THE TWO DISAGREED IN KIND. This returned Err(NotFound) for a missing wallet;
//   the gate returns a row saying NO WALLET ROW and then fails. A caller that treated
//   an error as "not drift" would have passed a real incident.
// - IT LOOKED COVERED. Four tests exercised it, and that is the part that made it
//   dangerous: a dead function with green tests is indistinguishable from a live one
//   until somebody reads the callers.
//
// WHAT THE INVARIANT NOW RESTS ON: tools/reconcile/reconcile.sh running
// reconcile.sql, which tools/reconcile-check mutation-tests in both directions - a
// drifted database must fail and NAME the account, a consistent one must pass, and
// the no-wallet-row arm is a case it breaks on purpose.
//
// The per-account test in this file still covers the arithmetic, through the local
// helper. That helper is deliberately a copy, and it is not the mistake the deleted
// function was: a test helper asserts a rule in isolation, where the shipped gate is
// verified where it actually runs.

/// Reconciliation sweep for STRANDED HOLDS - the money-loss defect this fix
/// closes (a reservation taken but never paired with a release or a charge, so
/// the customer's money sits debited against a request that was never billed).
///
/// The proxy reserves with ref = 'reserve_<uuid>' and writes a NEGATIVE
/// -reserved ledger row (reason = 'usage'). A healthy reservation is later
/// paired in debit_usage_transaction (releasing the hold in the SAME
/// transaction, writing a POSITIVE +reserved row under the SAME ref) or by
/// release_reservation_transaction. So a reserve ref with a negative row but NO
/// positive row under the same ref is money that left the wallet and came back
/// nowhere.
///
/// ledger.ref has no unique constraint (deliberately - many rows share one
/// reservation ref), so this correlated query is the only way to find the
/// stranded ones. Run it on a schedule; ZERO rows is the invariant. A non-zero
/// count means a release failed to land and an operator must investigate, or the
/// guard's fire-and-forget Drop (proxy.rs) did not reach the database.
pub async fn unpaired_hold_rows(pool: &SqlitePool, account_id: Uuid) -> Result<i64, AppError> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM (
            SELECT l.account_id, l.ref AS r
            FROM ledger l
            WHERE l.account_id = ?
              AND l.ref LIKE 'reserve_%'
              AND l.delta_idr < 0
            GROUP BY l.account_id, r
            HAVING NOT EXISTS (
                SELECT 1 FROM ledger m
                WHERE m.account_id = l.account_id
                  AND m.ref = l.ref
                  AND m.delta_idr > 0
            )
        ) AS stranded
        "#,
    )
    .bind(account_id.hyphenated())
    .fetch_one(pool)
    .await?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};

    /// Rows returned by the reconciliation check: wallets.balance_idr must equal
    /// SUM(ledger.delta_idr).
    ///
    /// This is a TRANSCRIPTION of tools/reconcile/reconcile.sql, and it is now faithful
    /// rather than convenient. It used to anchor on wallets with a LEFT JOIN, so it
    /// could not see an account whose wallets row is MISSING, the cache gone entirely,
    /// and every assertion built on it was checking a weaker rule than the one that
    /// ships.
    ///
    /// The SQL was fixed for that case long ago; its comment records a +250000 adjustment
    /// with no wallet row reporting a PASS, which is the exact figure the case below
    /// reproduces. A transcription is only safe if it is the same rule, so it now joins
    /// the same way and refuses in the same two directions.
    async fn ledger_drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT COALESCE(w.account_id, l.account_id) AS account_id
                FROM wallets w
                FULL OUTER JOIN ledger l ON l.account_id = w.account_id
                WHERE COALESCE(w.account_id, l.account_id) = ?
                GROUP BY w.account_id, l.account_id, w.balance_idr
                HAVING w.account_id IS NULL
                    OR w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// Everything an operator needs to see when reconciliation fails: the wallet
    /// balance, the ledger sum, and every ledger row that produced it. A bare
    /// "drift" count says money is wrong but not which row is missing.
    async fn drift_report(pool: &SqlitePool, account_id: Uuid) -> String {
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(pool)
                .await
                .expect("read balance");

        let ledger_sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("sum ledger");

        let rows: Vec<(i64, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT delta_idr, reason, balance_after, ref FROM ledger WHERE account_id = ? ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .fetch_all(pool)
        .await
        .expect("read ledger rows");

        format!("balance_idr={balance} ledger_sum={ledger_sum} rows={rows:?}")
    }

    /// A debit the wallet cannot cover must NOT be discarded: the reported usage
    /// is recorded and the debit is clamped to the balance, so reconciliation
    /// still holds and the shortfall is visible. A debit it CAN cover still
    /// settles in full.
    ///
    /// Every reconciliation assertion is scoped to THIS fixture's `account_id`, not
    /// to the whole database: a global drift check would fail for a concurrent
    /// writer rather than for the code under test.
    #[tokio::test]
    async fn overdraft_debit_is_clamped_to_the_balance_and_records_the_usage() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        overdraft_settlement_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the test, minus the database its caller owns.
    async fn overdraft_settlement_assertions(pool: SqlitePool, account_id: Uuid) {
        let opening_balance: i64 = 1_000;

        // The fixture opens the wallet exactly the way production does, in two steps:
        // the zero-balance row the login path creates (routes/auth.rs), then a real
        // top-up. Money only ever enters a wallet through `credit_topup_transaction`,
        // which writes the matching `+` ledger row in the same transaction. Seeding
        // `wallets.balance_idr` directly manufactures the very drift this test then
        // asserts against - a fixture that cannot pass while the code under test is
        // correct. A zero-balance wallet with no ledger rows is consistent on its own
        // (0 = SUM of nothing), so this starting point reconciles.
        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, opening_balance).await;

        let credited = credit_topup_transaction(
            &pool,
            &order_id,
            opening_balance,
            SHIPPED_CREDIT_EXPIRY_MONTHS,
        )
        .await
        .expect("credit the opening balance");

        assert_eq!(
            credited,
            TopupCreditResult::Settled {
                new_balance: opening_balance
            },
            "the fixture must open the wallet through the real top-up path"
        );

        // usage_daily.api_key_id is part of the primary key, so a real key row is
        // needed before any usage can be recorded.
        let key_id = test_support::api_key(&pool, account_id).await;

        // 1. A cost one rupiah above the wallet. The answer was already streamed
        //    to the client by now, so the usage must still be recorded.
        let cost_idr = opening_balance + 1;
        let outcome = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            "flash",
            200,
            0,
            150,
            cost_idr,
            Some("test_overdraft"),
            0,
        )
        .await
        .expect("a partial settlement is a recorded outcome, not an error");

        assert_eq!(
            outcome,
            UsageSettlement::Partial {
                new_balance: 0,
                debited_idr: opening_balance,
                shortfall_idr: 1,
            },
            "the debit must be clamped to the balance, with the rest a visible shortfall"
        );

        // 2. The balance is spent down to exactly zero, never negative.
        let balance_after: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance_after, 0);

        // 3. The ledger records only what was taken, and the usage row records the
        //    FULL cost and the real counters.
        //
        //    Scoped to the 'usage' row: the opening top-up also wrote a ledger row, so
        //    an unscoped read would find two and `fetch_one` would refuse it.
        let ledger_delta: i64 = sqlx::query_scalar(
            "SELECT delta_idr FROM ledger WHERE account_id = ? AND reason = 'usage'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("the clamped debit must still append a ledger row");
        assert_eq!(
            ledger_delta, -opening_balance,
            "the ledger must record exactly what was debited, not the full cost"
        );

        let (input, cache_read, output, usage_cost): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("the reported usage must still be recorded");
        assert_eq!(
            (input, cache_read, output, usage_cost),
            (200, 0, 150, cost_idr),
            "counters stay separate and the full cost is on the row"
        );

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );

        // 4. A debit the wallet CAN cover still settles in full. The clamp above left
        //    the wallet at zero, so refill it through the same real path the opening
        //    balance used - a second top-up, which writes its own `+` ledger row.
        //    Never write `balance_idr` alone: that is the drift the fixture must not
        //    create.
        let refill_order_id = test_support::pending_topup(&pool, account_id, opening_balance).await;

        credit_topup_transaction(
            &pool,
            &refill_order_id,
            opening_balance,
            SHIPPED_CREDIT_EXPIRY_MONTHS,
        )
        .await
        .expect("refill the wallet");

        let settled_cost: i64 = 250;
        let outcome = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            "flash",
            200,
            0,
            150,
            settled_cost,
            Some("test_covered"),
            0,
        )
        .await
        .expect("an affordable debit must succeed");

        assert_eq!(
            outcome,
            UsageSettlement::Settled {
                new_balance: opening_balance - settled_cost
            }
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a settled debit"
        );

        // Teardown belongs to the caller, which runs it whether these assertions
        // pass or panic.
    }

    /// REAL CONCURRENCY PROOF for the overdraw defect, plus the per-settlement
    /// reconciliation invariant.
    ///
    /// The account is funded for EXACTLY ONE request, through the real top-up path
    /// — never by writing `balance_idr`, which would manufacture the very drift this
    /// test then asserts against. Five reservations are taken concurrently. The
    /// guarded UPDATE serializes them on the wallet row, so exactly one matches and
    /// the rest are refused. Before the fix all five passed a point-in-time read.
    ///
    /// Then the winner settles: the hold is released and the true cost charged in
    /// one transaction. `ledger_drift_rows` is asserted ZERO after every single
    /// settlement, sequential and concurrent, because a missing release row is
    /// exactly the money leak this invariant exists to catch.
    ///
    /// This is the test that was `#[ignore = "requires live Postgres"]` and so was
    /// never executed automatically. SQLite turned it into a temp file, so it now
    /// runs on every CI pass — which is the whole benefit plan section 5.5 claims.
    #[tokio::test]
    async fn concurrent_requests_cannot_overdraw_a_one_request_balance() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        overdraw_concurrency_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the concurrency test, minus the database its caller owns.
    async fn overdraw_concurrency_assertions(pool: SqlitePool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const CONCURRENCY: usize = 5;

        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, RESERVATION).await;

        assert_eq!(
            credit_topup_transaction(&pool, &order_id, RESERVATION, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("fund the wallet"),
            TopupCreditResult::Settled {
                new_balance: RESERVATION
            },
            "the fixture must fund the wallet through the real top-up path"
        );

        // Five at once, each asking for the whole balance: at most one can be held.
        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for i in 0..CONCURRENCY {
            let pool = pool.clone();
            let reference = format!("test_reserve_{i}");
            tasks.push(tokio::spawn(async move {
                reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&reference)).await
            }));
        }

        let mut held = 0;
        let mut refused = 0;
        for task in tasks {
            match task
                .await
                .expect("a reservation task panicked")
                .expect("reserve")
            {
                ReservationResult::Held { reserved_idr, .. } => {
                    assert_eq!(reserved_idr, RESERVATION);
                    held += 1;
                }
                ReservationResult::Insufficient { .. } => refused += 1,
                ReservationResult::Zero => panic!("a non-zero reservation was reported as zero"),
            }
        }

        assert_eq!(
            held, 1,
            "exactly one request may be funded by a one-request balance"
        );
        assert_eq!(refused, CONCURRENCY - 1, "the rest must be refused");

        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance, 0, "the single hold consumed the whole balance");

        let held_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ledger WHERE account_id = ? AND delta_idr < 0",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count holds");
        assert_eq!(
            held_rows, 1,
            "a refused reservation must write no ledger row"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after concurrent holds"
        );

        // usage_daily.api_key_id is part of the primary key, so a real key row is
        // needed before any usage can be recorded.
        let key_id = test_support::api_key(&pool, account_id).await;

        // The winner settles: the hold comes back and the true cost is charged, in
        // ONE transaction. Drift must be zero immediately afterwards — a release
        // that credits the wallet without a ledger row is the leak being guarded.
        let cost_idr = 250;
        let settled = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            "flash",
            200,
            0,
            150,
            cost_idr,
            Some("test_reserve"),
            RESERVATION,
        )
        .await
        .expect("settle the winner");
        assert_eq!(
            settled,
            UsageSettlement::Settled {
                new_balance: RESERVATION - cost_idr
            }
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "the release and the charge must net to the true cost: {}",
            drift_report(&pool, account_id).await
        );

        // The release row must be ON THE BOOKS, not merely reflected in the balance:
        // the whole hold back out, and exactly the cost in.
        let release_row: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ? AND delta_idr > 0 AND reason = 'usage'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("sum release rows");
        assert_eq!(
            release_row, RESERVATION,
            "the release must write a +hold ledger row"
        );

        // Now the same path repeatedly, asserting the invariant after EVERY
        // settlement rather than only at the end. A drift that appears mid-run and
        // is later masked is the failure mode this catches.
        for round in 0..5 {
            let refill = test_support::pending_topup(&pool, account_id, RESERVATION).await;
            credit_topup_transaction(&pool, &refill, RESERVATION, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("refill through the real top-up path");

            let reference = format!("test_round_reserve_{round}");
            let reservation =
                reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&reference))
                    .await
                    .expect("reserve");
            assert!(matches!(reservation, ReservationResult::Held { .. }));
            assert_eq!(
                ledger_drift_rows(&pool, account_id).await,
                0,
                "drift after the hold of round {round}"
            );

            debit_usage_transaction(
                &pool,
                account_id,
                Some(key_id),
                "flash",
                10,
                0,
                5,
                66,
                Some(&reference),
                RESERVATION,
            )
            .await
            .expect("settle");
            assert_eq!(
                ledger_drift_rows(&pool, account_id).await,
                0,
                "drift after settlement {round}"
            );
        }
    }

    /// The clamp rule behind a settlement the balance cannot cover in full.
    ///
    /// This is the whole money decision and it is pure, so it is tested here
    /// without a database. The SQL path itself (the guarded UPDATE, the ledger
    /// insert, the usage_daily upsert) is covered by the database tests above,
    /// which since phase 5 run by default rather than behind `#[ignore]`.
    #[test]
    fn clamp_debit_collects_at_most_the_balance() {
        // Covered in full: nothing clamped, nothing lost.
        assert_eq!(clamp_debit(250, 1_000), (250, 0));

        // Exactly covered: the boundary must settle in full, not as a shortfall.
        assert_eq!(clamp_debit(1_000, 1_000), (1_000, 0));

        // One rupiah short: collect the balance, and the rest is the shortfall.
        assert_eq!(clamp_debit(1_001, 1_000), (1_000, 1));

        // Wildly unaffordable: still collect every rupiah available.
        assert_eq!(clamp_debit(50_000, 1_000), (1_000, 49_000));

        // Zero balance: debit nothing, the whole cost is shortfall. The usage row
        // is still written - the tokens were really consumed.
        assert_eq!(clamp_debit(50_000, 0), (0, 50_000));
        assert_eq!(clamp_debit(0, 0), (0, 0));

        // A negative balance cannot happen (CHECK balance_idr >= 0), but if it
        // ever did, the clamp must not turn the deficit into a credit.
        assert_eq!(clamp_debit(500, -10), (0, 500));

        // A negative cost is not a charge and must not become a credit.
        assert_eq!(clamp_debit(-500, 1_000), (0, -500));
    }

    /// The clamp never moves the balance below zero: what it collects plus what it
    /// leaves behind is always the full cost, and what it collects never exceeds
    /// what the wallet holds. This is the invariant the CHECK constraint backs.
    #[test]
    fn clamp_debit_never_overdraws_and_never_invents_money() {
        for cost in [0_i64, 1, 999, 1_000, 1_001, 250_000] {
            for available in [0_i64, 1, 999, 1_000, 1_001, 250_000] {
                let (debited, shortfall) = clamp_debit(cost, available);
                assert_eq!(
                    debited + shortfall,
                    cost,
                    "cost {cost} against {available} must fully account for the charge"
                );
                assert!(
                    debited <= available.max(0),
                    "cost {cost} against {available} debited more than the wallet holds"
                );
                assert!(debited >= 0, "a debit is never a credit");
            }
        }
    }

    /// The reservation arithmetic, as a pure rule: the two deltas a settlement
    /// writes must undo the hold and leave exactly the true cost behind.
    ///
    /// `reserve_balance_transaction` already wrote `-reserved` when the request
    /// started, so the whole request's net ledger move is
    /// `-reserved + release_delta + charge_delta`, and it must be `-cost` for
    /// `balance_idr = SUM(ledger.delta_idr)` to hold at the commit point.
    #[test]
    fn a_reservation_and_its_release_net_to_the_true_cost() {
        for reserved in [0_i64, 1, 66, 1_000, 250_000] {
            for cost in [0_i64, 1, 66, 999, 1_000, 250_000] {
                let (release_delta, charge_delta) = settlement_ledger_deltas(reserved, cost);

                assert_eq!(
                    -reserved + release_delta + charge_delta,
                    -cost,
                    "reserved {reserved}, cost {cost}: the ledger must net to the true cost"
                );
                assert!(release_delta >= 0, "a release is never a second hold");
                assert!(charge_delta <= 0, "a charge is never a credit");
            }
        }
    }

    /// Nothing is held when there is nothing to hold, so nothing is released: a
    /// zero-delta ledger row is noise in an append-only money log.
    #[test]
    fn a_zero_reservation_writes_no_release_row() {
        assert_eq!(settlement_ledger_deltas(0, 0), (0, 0));
        assert_eq!(settlement_ledger_deltas(0, 500), (0, -500));
    }

    /// A negative argument must never become money: a negative cost is not a
    /// charge and a negative release is not a hold being returned.
    #[test]
    fn a_negative_delta_is_floored_not_inverted() {
        assert_eq!(settlement_ledger_deltas(-100, -100), (0, 0));
        assert_eq!(settlement_ledger_deltas(-1, 0), (0, 0));
    }

    /// A reservation is either held or refused, and the refusal carries the
    /// balance the customer actually has, so the 402 detail is not a guess.
    #[test]
    fn a_refused_reservation_is_not_mistakable_for_a_held_one() {
        assert_ne!(
            ReservationResult::Held {
                reserved_idr: 0,
                new_balance: 0
            },
            ReservationResult::Insufficient { balance_idr: 0 }
        );
        assert_ne!(
            ReservationResult::Zero,
            ReservationResult::Insufficient { balance_idr: 0 }
        );

        match (ReservationResult::Insufficient { balance_idr: 1 }) {
            ReservationResult::Insufficient { balance_idr } => assert_eq!(balance_idr, 1),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A partial settlement must stay distinguishable from a full one, and must
    /// carry the three figures an operator needs: what was taken, what was lost,
    /// and where the balance landed.
    #[test]
    fn a_partial_settlement_is_not_mistakable_for_a_full_one() {
        assert_ne!(
            UsageSettlement::Settled { new_balance: 0 },
            UsageSettlement::Partial {
                new_balance: 0,
                debited_idr: 0,
                shortfall_idr: 0
            }
        );

        match (UsageSettlement::Partial {
            new_balance: 0,
            debited_idr: 1_000,
            shortfall_idr: 49_000,
        }) {
            UsageSettlement::Partial {
                new_balance,
                debited_idr,
                shortfall_idr,
            } => {
                assert_eq!(new_balance, 0);
                assert_eq!(debited_idr, 1_000);
                assert_eq!(shortfall_idr, 49_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// REGRESSION for the two money-loss defects this fix closes:
    ///
    /// 1. FINDING 1/2 - a settlement that FAILS (or is cancelled) must release the
    ///    hold. The fix is the `ReservationGuard` in proxy.rs, whose Drop calls
    /// `release_quietly` -> `release_reservation_transaction`. This test drives
    /// that exact release path directly and proves the money comes back.
    /// 2. FINDING 3 - a settlement that PAIRS its release with the same `reserve_*`
    ///    ref leaves the detection query (`unpaired_hold_rows`) at ZERO, so a hold
    /// is never mistaken for lost money.
    ///
    /// The test would FAIL before the fix on both counts: `release_quietly` was
    /// never called on the failure arm (the hold stayed debited forever), and the
    /// settlement passed `ref_batch = None` so the hold row had no matching
    /// positive row and the detection query flagged it as stranded.
    #[tokio::test]
    async fn a_failed_or_paired_settlement_never_strands_the_hold() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        hold_never_strands_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the regression test, minus the database its caller owns.
    async fn hold_never_strands_assertions(pool: SqlitePool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const COST: i64 = 250;

        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, RESERVATION).await;
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, RESERVATION, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("fund the wallet"),
            TopupCreditResult::Settled {
                new_balance: RESERVATION
            },
            "fund the wallet through the real top-up path"
        );

        let key_id = test_support::api_key(&pool, account_id).await;

        // --- Scenario A: a settlement FAILS, the hold must come back. ---
        let failed_ref = format!("reserve_{}", Uuid::new_v4().simple());
        let held = reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&failed_ref))
            .await
            .expect("reserve");
        assert!(
            matches!(held, ReservationResult::Held { .. }),
            "the wallet must be able to cover the worst case"
        );

        // A taken hold with no release yet MUST be flagged as unpaired - that is
        // how an operator tells a live hold from lost money.
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect before release"),
            1,
            "a held reservation with no release must be reported as unpaired"
        );

        // Simulate the failed-settlement arm: proxy.rs calls release_quietly, which
        // calls exactly this. The balance must return to its pre-hold value.
        let pre_hold: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read pre-release balance");
        release_reservation_transaction(&pool, account_id, RESERVATION, Some(&failed_ref))
            .await
            .expect("the failed settlement releases the hold");
        let after_release: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read post-release balance");
        assert_eq!(
            after_release,
            pre_hold + RESERVATION,
            "FINDING 1/2: a failed settlement must return the whole hold to the wallet"
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect after release"),
            0,
            "after the release the detection query must find no stranded hold"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after the release"
        );

        // --- Scenario B: a settlement PAIRS its release with the reservation ref.
        let paired_ref = format!("reserve_{}", Uuid::new_v4().simple());
        let held = reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&paired_ref))
            .await
            .expect("reserve");
        assert!(matches!(held, ReservationResult::Held { .. }));
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect before settle"),
            1,
            "the paired hold is unpaired until it settles"
        );

        let settled = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            "flash",
            200,
            0,
            150,
            COST,
            // FINDING 3: the settlement passes the SAME ref the hold used, so the
            // release row carries it and the detection query stays at zero.
            Some(&paired_ref),
            RESERVATION,
        )
        .await
        .expect("settle the paired request");
        assert_eq!(
            settled,
            UsageSettlement::Settled {
                new_balance: RESERVATION - COST
            }
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect after settle"),
            0,
            "FINDING 3: a paired settlement must not leave a stranded hold"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a paired settlement"
        );
    }

    /// Runs the assertions against a fresh account in its OWN migrated SQLite
    /// database, then closes that database whether the assertions passed or
    /// panicked. The future is spawned, so a panic inside it arrives as a JoinError
    /// rather than unwinding through the teardown - which is what makes the close
    /// unconditional, and what removes the temp directory rather than leaking one
    /// per failing test.
    ///
    /// Ported from the Postgres original, which took a `PgPool` and deleted its
    /// fixture rows in FK order. SQLite needs neither: the database is a file, so
    /// `TestDb` builds one per test and `close()` removes it. The fixture rule the
    /// original spelled out here now lives in the `crate::test_support` module
    /// comment, which is where the fixtures themselves are.
    async fn run_with_teardown<F, Fut>(assertions: F)
    where
        F: FnOnce(SqlitePool, Uuid) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        let outcome = tokio::spawn(assertions(db.pool.clone(), account_id)).await;

        db.close().await;

        outcome.expect("the live assertions panicked");
    }

    /// A second, wallet-less account for the "no wallet row" arms. Callers own its
    /// teardown.
    async fn bare_account(pool: &SqlitePool) -> Uuid {
        test_support::account(pool).await
    }

    /// A `pending` topup under a CALLER-CHOSEN `order_id`.
    ///
    /// `test_support::pending_topup` mints its own order id, which is what most
    /// fixtures want; these tests need to name the order because they replay the
    /// same one (a replayed webhook, a second refund) and assert on it by name.
    /// Every NOT NULL column is bound: the strict schema has no DEFAULT for `id`,
    /// `created_at` or `rail`, so the Postgres `INSERT INTO topups (account_id, ...)`
    /// shape fails at runtime with a NOT NULL constraint error.
    async fn create_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64, order_id: &str) {
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at)
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(amount_idr)
        .bind(order_id)
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("create topup");
    }

    /// Funds the wallet through the real path and returns the order id that did it.
    ///
    /// Ported: the Postgres original opened the wallet row itself and then wrote
    /// the topup. `test_support` owns both now - `wallet` is the zero-balance row
    /// the login path creates, `pending_topup` is the row a webhook would settle -
    /// so this is the same two-step fixture the module comment describes, with the
    /// INSERT shapes in one place.
    async fn fund_through_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) -> String {
        fund_through_topup_expecting(pool, account_id, amount_idr, amount_idr).await
    }

    /// `fund_through_topup` for an account that already holds credit: the
    /// resulting balance is the sum, not this deposit's amount.
    ///
    /// Kept separate rather than made a parameter of the common case, because the
    /// one-argument version's assertion - "the balance after this deposit IS this
    /// deposit" - is the assertion that catches a fixture that quietly skipped the
    /// real path, and weakening it everywhere to support the second deposit would
    /// retire that check.
    async fn fund_through_topup_again(
        pool: &SqlitePool,
        account_id: Uuid,
        amount_idr: i64,
        expected_balance: i64,
    ) -> String {
        fund_through_topup_expecting(pool, account_id, amount_idr, expected_balance).await
    }

    async fn fund_through_topup_expecting(
        pool: &SqlitePool,
        account_id: Uuid,
        amount_idr: i64,
        expected_balance: i64,
    ) -> String {
        // Only create the wallet if it is not there: the login path creates it
        // once, and `wallets.account_id` is the primary key, so a second INSERT
        // is a UNIQUE violation rather than a second wallet.
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("count the wallet");
        if existing == 0 {
            test_support::wallet(pool, account_id).await;
        }

        let order_id = test_support::pending_topup(pool, account_id, amount_idr).await;

        assert_eq!(
            credit_topup_transaction(pool, &order_id, amount_idr, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("credit the opening balance"),
            TopupCreditResult::Settled {
                new_balance: expected_balance
            },
            "the fixture must open the wallet through the real top-up path"
        );

        order_id
    }

    async fn wallet_balance(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read balance")
    }

    async fn topup_id(pool: &SqlitePool, order_id: &str) -> Uuid {
        // Ported: `topups.id` is TEXT in the SQLite schema, so it decodes through
        // `Hyphenated` and never as a bare 16-byte `Uuid` - the decode error is
        // `ParseByteLength { len: 36 }`, which is the hyphenated string arriving
        // where a raw uuid was expected.
        let id: Hyphenated = sqlx::query_scalar("SELECT id FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup id");
        id.into_uuid()
    }

    async fn topup_status(pool: &SqlitePool, order_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup status")
    }

    /// Every ledger row for the account with that reason, oldest first, as
    /// (delta_idr, ref). Ordering by id keeps the assertion about the append order,
    /// not about whatever the planner returns.
    async fn ledger_rows(
        pool: &SqlitePool,
        account_id: Uuid,
        reason: &str,
    ) -> Vec<(i64, Option<String>)> {
        sqlx::query_as(
            "SELECT delta_idr, ref FROM ledger WHERE account_id = ? AND reason = ? ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .bind(reason)
        .fetch_all(pool)
        .await
        .expect("read ledger rows")
    }

    async fn ledger_row_count(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("count ledger rows")
    }

    /// Every ledger row under one ref, oldest first, as (reason, delta_idr).
    ///
    /// This is the join the audit trail depends on: one ref value must select every
    /// row that belongs to one logical money event.
    async fn ledger_rows_for_ref(
        pool: &SqlitePool,
        account_id: Uuid,
        reference: &str,
    ) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT reason, delta_idr FROM ledger WHERE account_id = ? AND ref = ? ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .bind(reference)
        .fetch_all(pool)
        .await
        .expect("read ledger rows for ref")
    }

    /// The net ledger move under one ref. A hold and its release must sum to zero.
    async fn ledger_sum_for_ref(pool: &SqlitePool, account_id: Uuid, reference: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ? AND ref = ?",
        )
        .bind(account_id.hyphenated())
        .bind(reference)
        .fetch_one(pool)
        .await
        .expect("sum ledger rows for ref")
    }

    /// The four outcomes credit_topup_transaction documents: a fresh credit settles
    /// and writes exactly ONE +topup row; a replay credits exactly once
    /// (AlreadySettled, no second row, balance unchanged); an amount disagreeing with
    /// the stored topup is AmountMismatch with NO write; an unknown order id is
    /// NotFound with NO write.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn credit_topup_settles_replays_and_refuses_bad_input() {
        run_with_teardown(credit_topup_assertions).await;
    }

    async fn credit_topup_assertions(pool: SqlitePool, account_id: Uuid) {
        const AMOUNT: i64 = 50_000;
        const OTHER: i64 = 10_000;

        test_support::wallet(&pool, account_id).await;

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, AMOUNT, &order_id).await;
        let stored_id = topup_id(&pool, &order_id).await;

        // 1. A fresh credit moves the wallet and writes exactly ONE +ledger row.
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("a fresh top-up must settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            },
            "a fresh credit must move the wallet by the stored amount"
        );
        assert_eq!(wallet_balance(&pool, account_id).await, AMOUNT);
        assert_eq!(topup_status(&pool, &order_id).await, "settled");
        assert_eq!(
            ledger_rows(&pool, account_id, "topup").await,
            vec![(AMOUNT, Some(stored_id.to_string()))],
            "the credit must append exactly one +topup row, ref'd to the topup id"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a credit"
        );

        // 2. A REPLAY of the same order credits exactly once: idempotency is the
        //    unique order_id plus the settled status, not a second credit.
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("a replay is a recorded outcome, not an error"),
            TopupCreditResult::AlreadySettled,
            "the second webhook for one order must not credit again"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            AMOUNT,
            "a replayed top-up must leave the balance alone"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "topup").await.len(),
            1,
            "a replayed top-up must not append a second ledger row"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 3. An amount that disagrees with the stored topup is refused with NO
        //    write: not the topup status, not the wallet, not the ledger. The webhook
        //    payload is never trusted over the stored record (docs/decisions.md:
        //    "Credit source - Midtrans webhook only ... never the payload amount").
        let mismatch_order = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, OTHER, &mismatch_order).await;
        let mismatch_id = topup_id(&pool, &mismatch_order).await;
        let ledger_before = ledger_row_count(&pool, account_id).await;

        assert_eq!(
            credit_topup_transaction(
                &pool,
                &mismatch_order,
                OTHER - 1,
                SHIPPED_CREDIT_EXPIRY_MONTHS
            )
            .await
            .expect("a mismatch is a recorded outcome, not an error"),
            TopupCreditResult::AmountMismatch,
            "an amount that disagrees with the stored topup must be refused"
        );
        assert_eq!(
            topup_status(&pool, &mismatch_order).await,
            "pending",
            "a refused credit must not settle the topup"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            AMOUNT,
            "a refused credit must not move the wallet"
        );
        assert_eq!(
            ledger_row_count(&pool, account_id).await,
            ledger_before,
            "a refused credit must not append a ledger row"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "topup").await,
            vec![(AMOUNT, Some(stored_id.to_string()))],
            "the only topup ledger row must still be the first order's"
        );
        assert_eq!(
            ledger_sum_for_ref(&pool, account_id, &mismatch_id.to_string()).await,
            0,
            "the mismatched order must have written nothing at all"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 4. An unknown order id is NotFound, with nothing written.
        let unknown = format!("test_topup_unknown_{}", Uuid::new_v4().simple());
        assert_eq!(
            credit_topup_transaction(&pool, &unknown, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("an unknown order is a recorded outcome, not an error"),
            TopupCreditResult::NotFound,
            "an unknown order_id must be reported as NotFound"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            AMOUNT,
            "an unknown order must not move the wallet"
        );
        assert_eq!(
            ledger_row_count(&pool, account_id).await,
            ledger_before,
            "an unknown order must not append a ledger row"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);
    }

    /// ONE logical top-up must be selectable by ONE ledger ref.
    ///
    /// docs/website/02-data-model.md:79 defines the column as "topup id, usage
    /// batch id, etc." - the top-up's OWN id. The credit path must write that id,
    /// never the Midtrans order_id, or the audit trail cannot answer "what
    /// happened to top-up X" from a single ref value.
    ///
    /// This used to be a credit-and-refund pair, asserting the two rows shared one
    /// ref. The refund path is gone - this platform does not do refunds - so only
    /// the credit half remains, which is the half the join key depends on.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn a_topup_credit_is_filed_under_the_topup_id_not_the_order_id() {
        run_with_teardown(one_ref_assertions).await;
    }

    async fn one_ref_assertions(pool: SqlitePool, account_id: Uuid) {
        const TOPUP: i64 = 50_000;

        let order_id = fund_through_topup(&pool, account_id, TOPUP).await;
        let stored_id = topup_id(&pool, &order_id).await;
        let topup_ref = stored_id.to_string();

        // The property the audit trail needs: the topup's OWN id selects the
        // credit, with the delta the wallet actually moved by.
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &topup_ref).await,
            vec![("topup".to_string(), TOPUP)],
            "the credit must be filed under the topup id"
        );
        assert_eq!(
            ledger_sum_for_ref(&pool, account_id, &topup_ref).await,
            TOPUP,
            "one ref must net to what this top-up left in the wallet"
        );

        // The order id must NOT be a second identity for the same row.
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &order_id).await,
            Vec::<(String, i64)>::new(),
            "the Midtrans order id must not be a second ref vocabulary for a top-up"
        );

        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);
    }
    /// release_reservation_transaction had no direct test. Releasing a hold must
    /// credit the wallet by exactly the held amount and append a matching POSITIVE
    /// row under the SAME reserve_% ref, so the pair nets to zero, the stranded-hold
    /// detector returns 0, and the wallet is back where it started.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn release_reservation_returns_the_hold_and_pairs_the_ledger() {
        run_with_teardown(release_reservation_assertions).await;
    }

    async fn release_reservation_assertions(pool: SqlitePool, account_id: Uuid) {
        const FUNDING: i64 = 50_000;
        const HOLD: i64 = 10_000;

        fund_through_topup(&pool, account_id, FUNDING).await;

        let hold_ref = format!("reserve_{}", Uuid::new_v4().simple());
        assert_eq!(
            reserve_balance_transaction(&pool, account_id, HOLD, Some(&hold_ref))
                .await
                .expect("reserve"),
            ReservationResult::Held {
                reserved_idr: HOLD,
                new_balance: FUNDING - HOLD
            },
            "the hold must be a real, guarded debit"
        );
        assert_eq!(wallet_balance(&pool, account_id).await, FUNDING - HOLD);
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect the live hold"),
            1,
            "a hold with no release yet is money the wallet cannot explain"
        );

        // The release: exactly the held amount back, under the SAME ref.
        assert_eq!(
            release_reservation_transaction(&pool, account_id, HOLD, Some(&hold_ref))
                .await
                .expect("the hold comes back"),
            Some(FUNDING),
            "the release must credit the whole hold back"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            FUNDING,
            "after the release the wallet must be exactly where it started"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "usage").await,
            vec![
                (-HOLD, Some(hold_ref.clone())),
                (HOLD, Some(hold_ref.clone()))
            ],
            "the release must append a POSITIVE row under the same reserve_ ref"
        );
        assert_eq!(
            ledger_sum_for_ref(&pool, account_id, &hold_ref).await,
            0,
            "the hold and its release must net to zero"
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("detect after release"),
            0,
            "the pair must no longer appear in the unpaired-hold detector"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a release"
        );

        // Nothing held, nothing released: a zero-delta ledger row is noise in an
        // append-only money log.
        let rows_before = ledger_row_count(&pool, account_id).await;
        assert_eq!(
            release_reservation_transaction(&pool, account_id, 0, Some("reserve_zero"))
                .await
                .expect("a zero release is not an error"),
            None,
            "a zero reservation has nothing to release"
        );
        assert_eq!(
            ledger_row_count(&pool, account_id).await,
            rows_before,
            "a zero release must not append a zero-delta ledger row"
        );
        assert_eq!(wallet_balance(&pool, account_id).await, FUNDING);

        // No wallet row: nothing was ever held, so a credit would be money the
        // ledger cannot back.
        let bare = bare_account(&pool).await;
        assert_eq!(
            release_reservation_transaction(&pool, bare, HOLD, Some("reserve_no_wallet"))
                .await
                .expect("releasing against a wallet-less account"),
            None,
            "an account with no wallet row has nothing to release"
        );
        assert_eq!(
            ledger_row_count(&pool, bare).await,
            0,
            "a release with no wallet row must write no ledger row"
        );
    }

    /// verify_wallet_reconciliation had no direct test, and a checker that always
    /// returns true is worse than none: the property under test is its ability to
    /// DETECT. A consistent fixture reports clean; a deliberate direct UPDATE of
    /// balance_idr (the one thing production never does) is REPORTED as drift;
    /// restoring it reports clean again.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn reconciliation_reports_drift_instead_of_always_passing() {
        run_with_teardown(reconciliation_assertions).await;
    }

    // The per-account arithmetic, through the local helper, after the dead duplicate
    // of this rule was deleted from the file above.
    //
    // The helper is deliberately a COPY, and that is not the mistake the deleted
    // function was. A test helper that reimplements a rule asserts the arithmetic in
    // isolation; the shipped gate is verified where it actually runs, by
    // tools/reconcile-check against reconcile.sql, including the no-wallet-row arm
    // that neither Rust copy could see.
    async fn reconciliation_assertions(pool: SqlitePool, account_id: Uuid) {
        const AMOUNT: i64 = 50_000;
        fund_through_topup(&pool, account_id, AMOUNT).await;

        // A consistent wallet reports clean.
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // Drift the ledger cannot explain must be FOUND, not missed.
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr + 1 WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("manufacture drift");
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            1,
            "the drift the checker reports must be the drift the sweep finds: {}",
            drift_report(&pool, account_id).await
        );

        // Restoring it clears the report, so the query is reading the data rather than
        // answering from a constant.
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr - 1 WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("restore the balance");
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);
    }

    /// The gate must report an account whose WALLET ROW IS MISSING, and this is its
    /// own test rather than a fourth step above, for a reason worth stating.
    ///
    /// It was tried as a fourth step and it broke a NEIGHBOURING test: that test asserts
    /// a release against a wallet-less account writes no ledger row, and seeding this
    /// case first moved what it counted. A case that mutates the shape another test
    /// depends on belongs in its own database, which run_with_teardown gives every
    /// #[tokio::test]. Coupling them for brevity is how a suite ends up with tests that
    /// pass only in the order they happen to run.
    ///
    /// The fixture is built through the REAL money path and then the cache row removed,
    /// so the ledger row carries a valid reason and this is a state the system can
    /// reach. A hand-inserted row was tried first and the STRICT schema refused it on
    /// the reason vocabulary, which is the schema working.
    #[tokio::test]
    async fn the_gate_reports_an_account_whose_wallet_row_is_missing() {
        run_with_teardown(ledger_only_assertions).await;
    }

    async fn ledger_only_assertions(pool: SqlitePool, account_id: Uuid) {
        // A funded account reconciles clean, so the failure below is the DELETION
        // doing it and not a fixture that was never sound.
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        fund_through_topup(&pool, account_id, 250_000).await;
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        sqlx::query("DELETE FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("remove the cache row, leaving ledger money with no wallet");

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            1,
            "an account with ledger money and NO wallet row is the worst case, not an ignorable one. Anchoring on wallets with a LEFT JOIN reports nothing for it, which is how a +250000 adjustment once reported a clean reconcile."
        );
    }

    /// unpaired_hold_rows: a hold with no matching release is money that left the
    /// wallet and came back nowhere, so it must be COUNTED; a matched pair and a
    /// clean account must both be zero. This is the detector the hold sweep and the
    /// operator rely on, so a detector that never fires is the failure mode.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn unpaired_hold_rows_counts_a_stranded_hold_and_clears_a_matched_one() {
        run_with_teardown(unpaired_hold_assertions).await;
    }

    async fn unpaired_hold_assertions(pool: SqlitePool, account_id: Uuid) {
        const FUNDING: i64 = 50_000;
        const HOLD: i64 = 10_000;

        fund_through_topup(&pool, account_id, FUNDING).await;

        // A clean account: the topup row is not a hold.
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("sweep a clean account"),
            0,
            "a clean account has no stranded holds"
        );

        // A hold with no release.
        let stranded_ref = format!("reserve_{}", Uuid::new_v4().simple());
        assert!(matches!(
            reserve_balance_transaction(&pool, account_id, HOLD, Some(&stranded_ref))
                .await
                .expect("reserve"),
            ReservationResult::Held { .. }
        ));
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("sweep a stranded hold"),
            1,
            "a hold with no matching release must be counted"
        );

        // The matching release clears it.
        assert_eq!(
            release_reservation_transaction(&pool, account_id, HOLD, Some(&stranded_ref))
                .await
                .expect("release the hold"),
            Some(FUNDING)
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("sweep a matched pair"),
            0,
            "a matched pair must not be reported as stranded"
        );

        // The detector is scoped to the reserve_% refs the proxy writes (see the
        // docs on unpaired_hold_rows). A hold under any other ref is outside its
        // scope by construction, so it is not counted - which is exactly why the
        // reservation ref must stay reserve_<uuid> on every call site.
        let other_ref = format!("other_{}", Uuid::new_v4().simple());
        assert!(matches!(
            reserve_balance_transaction(&pool, account_id, HOLD, Some(&other_ref))
                .await
                .expect("reserve under a non-reserve ref"),
            ReservationResult::Held { .. }
        ));
        assert_eq!(
            unpaired_hold_rows(&pool, account_id)
                .await
                .expect("sweep a non-reserve ref"),
            0,
            "the detector is scoped to reserve_% refs"
        );
        assert_eq!(
            release_reservation_transaction(&pool, account_id, HOLD, Some(&other_ref))
                .await
                .expect("release the non-reserve hold"),
            Some(FUNDING)
        );
        assert_eq!(wallet_balance(&pool, account_id).await, FUNDING);
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after every case"
        );
    }

    // ---------------------------------------------------------------------
    // Exhaustive property sweeps over the two pure money rules.
    //
    // The example tests above pin exact figures. What they cannot do is cover
    // the DOMAIN: `clamp_debit` IS the billing decision (db.rs:266) and
    // `settlement_ledger_deltas` IS the ledger move (db.rs:288), so a wrong
    // answer anywhere in the i64 plane is money invented or money vanished.
    // These sweeps walk every interesting boundary plus a deterministic
    // pseudo-random sample of the whole range. No new dependency: a fixed-seed
    // xorshift64* is enough to be reproducible, and a fixed seed keeps a
    // counterexample in the assert message stable across runs.
    // ---------------------------------------------------------------------

    /// The boundary grid. `i64::MIN` is in it deliberately: every negation and
    /// every `.max(0)` in these two rules is at its most dangerous there, and a
    /// debug build turns an overflow into a panic rather than a wrong number.
    /// The rest are the shapes money really takes: 0 (nothing), 1 (one rupiah),
    /// 2, 100 (sub-rupiah noise), and the -1/-2 a defect would produce if a
    /// balance or a cost ever went below zero.
    const MONEY_GRID: [i64; 10] = [
        i64::MIN,
        i64::MIN + 1,
        -2,
        -1,
        0,
        1,
        2,
        100,
        i64::MAX - 1,
        i64::MAX,
    ];

    /// Deterministic xorshift64*. The seed is fixed so a failure is
    /// reproducible and the printed counterexample is stable.
    struct XorShift64(u64);

    impl XorShift64 {
        fn new(seed: u64) -> Self {
            // xorshift is degenerate at zero.
            Self(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            })
        }

        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        /// A raw i64, so the sweep reaches the negative half of the plane too.
        fn next_i64(&mut self) -> i64 {
            self.next_u64() as i64
        }
    }

    /// Every (a, b) pair the sweeps run over, in one place so both rules see
    /// the same domain:
    ///
    ///   1. the exhaustive 10x10 boundary grid (100 pairs);
    ///   2. i64::MIN/i64::MAX and their immediate neighbours, crossed with the
    ///      whole grid on both sides - the off-by-one hiding at the overflow edge;
    ///   3. 20 000 fixed-seed pairs drawn from the raw i64 plane;
    ///   4. 20 000 fixed-seed MONEY-SHAPED pairs: small non-negative rupiah
    ///      amounts, which is the only region production actually walks.
    fn sweep_pairs() -> Vec<(i64, i64)> {
        let mut pairs = Vec::new();

        for a in MONEY_GRID {
            for b in MONEY_GRID {
                pairs.push((a, b));
            }
        }

        for edge in [i64::MIN, i64::MAX] {
            for delta in [-2_i64, -1, 0, 1, 2] {
                let near = edge.saturating_add(delta);
                for other in MONEY_GRID {
                    pairs.push((near, other));
                    pairs.push((other, near));
                }
            }
        }

        let mut rng = XorShift64::new(0x9E37_79B9_7F4A_7C15);
        for _ in 0..20_000 {
            pairs.push((rng.next_i64(), rng.next_i64()));
        }
        for _ in 0..20_000 {
            pairs.push((
                (rng.next_u64() % 10_000_000) as i64,
                (rng.next_u64() % 10_000_000) as i64,
            ));
        }

        pairs
    }

    /// Every invariant the billing model rests on, asserted for ONE
    /// `(cost_idr, available_idr)` pair. Called by the sweep below for every
    /// pair in `sweep_pairs()`.
    fn assert_clamp_debit_invariants(cost_idr: i64, available_idr: i64) {
        let (debited_idr, lost_idr) = clamp_debit(cost_idr, available_idr);
        let true_cost = cost_idr.max(0);
        let held = available_idr.max(0);

        // (2) Never debit more than is there, and never a negative debit.
        assert!(
            debited_idr >= 0,
            "cost {cost_idr} against {available_idr}: a debit is never a credit"
        );
        assert!(
            debited_idr <= held,
            "cost {cost_idr} against {available_idr}: debited {debited_idr} exceeds the {held} held"
        );
        // (3) Never collect more than the true cost.
        assert!(
            debited_idr <= true_cost,
            "cost {cost_idr} against {available_idr}: debited {debited_idr} exceeds the true cost {true_cost}"
        );
        // The clamp is exactly the smaller of the two floors - no third rule.
        assert_eq!(
            debited_idr,
            true_cost.min(held),
            "cost {cost_idr} against {available_idr}: the debit must be the smaller of the two floors"
        );

        if cost_idr >= 0 {
            // (1) Conservation: no money invented, none vanishes. This is the
            // single most important property of the whole money model.
            assert_eq!(
                debited_idr + lost_idr,
                true_cost,
                "cost {cost_idr} against {available_idr}: {debited_idr} collected + {lost_idr} lost must be the whole charge"
            );
            // (4) A shortfall is never negative.
            assert!(
                lost_idr >= 0,
                "cost {cost_idr} against {available_idr}: a shortfall is never negative"
            );
            // (5) A FULL settlement loses nothing, and ONLY a full settlement
            // does. The balance is floored at zero, so "affordable" is
            // `cost <= available.max(0)`; for available >= 0 - the production
            // domain, backed by CHECK (balance_idr >= 0) - that is exactly the
            // documented `cost <= available`.
            assert_eq!(
                lost_idr == 0,
                cost_idr <= held,
                "cost {cost_idr} against {available_idr}: lost {lost_idr} must be zero exactly when the charge is affordable"
            );
        } else {
            // A negative cost is not a charge, so the debit floors to zero and
            // never becomes a credit. NOTE: the pair is `(0, cost_idr)`, i.e.
            // the negative cost passes straight through into the shortfall
            // slot as a NEGATIVE number, because db.rs:270 computes
            // `cost_idr - debited_idr` rather than `cost_idr.max(0) - debited_idr`.
            // Invariants (1) and (4) as literally stated therefore do NOT hold
            // below zero. That divergence is pinned here, loudly, so a change to
            // it cannot be silent.
            assert_eq!(
                (debited_idr, lost_idr),
                (0, cost_idr),
                "cost {cost_idr} against {available_idr}: a negative cost debits nothing and is not inverted"
            );
        }
    }

    /// Every invariant the ledger rule rests on, for ONE
    /// `(released_idr, cost_idr)` pair.
    fn assert_settlement_ledger_invariants(released_idr: i64, cost_idr: i64) {
        let (release_delta, charge_delta) = settlement_ledger_deltas(released_idr, cost_idr);
        let floored_release = released_idr.max(0);
        let floored_cost = cost_idr.max(0);

        // (6) A release is never a second hold and a charge is never a credit.
        assert!(
            release_delta >= 0,
            "release {released_idr} cost {cost_idr}: a release is never a second hold"
        );
        assert!(
            charge_delta <= 0,
            "release {released_idr} cost {cost_idr}: a charge is never a credit"
        );

        // The two deltas ARE the floored arguments: i64::MIN must floor to 0
        // and must not overflow on the way, because the negation applies to the
        // floor and never to the raw value.
        assert_eq!(
            release_delta, floored_release,
            "release {released_idr} cost {cost_idr}: the release delta is the floored release"
        );
        assert_eq!(
            charge_delta, -floored_cost,
            "release {released_idr} cost {cost_idr}: the charge delta is the negated floored cost"
        );

        // The pair's net move. `checked_add` on purpose: summing the two must
        // not overflow either.
        assert_eq!(
            release_delta
                .checked_add(charge_delta)
                .expect("the two ledger deltas must not overflow when summed"),
            floored_release - floored_cost,
            "release {released_idr} cost {cost_idr}: the pair must net to the floored release minus the floored cost"
        );

        // The pairing requirement the doc comment states: the two never
        // collapse into one another. An argument at or below zero floors to a
        // ZERO delta - a zero release is never written as a negative and a
        // zero charge is never written as a positive - so the release and the
        // charge stay two auditable facts, never one sign-flipped one.
        assert_eq!(
            release_delta == 0,
            released_idr <= 0,
            "release {released_idr} cost {cost_idr}: a zero release must be a zero, never a negative"
        );
        assert_eq!(
            charge_delta == 0,
            cost_idr <= 0,
            "release {released_idr} cost {cost_idr}: a zero charge must be a zero, never a positive"
        );
    }

    /// (1)-(5) and (7): `clamp_debit` over the whole grid + fixed-seed sample.
    #[test]
    fn clamp_debit_holds_every_money_invariant_over_the_whole_domain() {
        let pairs = sweep_pairs();
        assert!(
            pairs.len() > 40_000,
            "the sweep must cover the boundaries AND a real sample, got {} pairs",
            pairs.len()
        );

        for (cost_idr, available_idr) in pairs {
            assert_clamp_debit_invariants(cost_idr, available_idr);
        }
    }

    /// (6) and (7): `settlement_ledger_deltas` over the same domain.
    #[test]
    fn settlement_ledger_deltas_hold_every_invariant_over_the_whole_domain() {
        for (released_idr, cost_idr) in sweep_pairs() {
            assert_settlement_ledger_invariants(released_idr, cost_idr);
        }
    }

    /// The identity that ties the two rules together, derived from the code:
    ///
    ///   * `reserve_balance_transaction` already wrote `-held` when the
    ///     request started (db.rs:276-284).
    ///   * `debit_usage_transaction` releases the hold IN FULL - `released_idr`
    ///     is the whole hold, or 0 when nothing was held (db.rs:326-343) - and
    ///     charges what the clamp allows: `charged_idr` is
    ///     `clamp_debit(cost, available).0` (db.rs:694-735 on the partial path;
    ///     db.rs:390-402 passes the full cost on the settled path, where the
    ///     guard already proved it affordable, so the clamp returns it whole).
    ///   * `record_usage` writes `settlement_ledger_deltas(released, charged)`
    ///     (db.rs:441).
    ///
    /// So the whole request's net ledger move is
    ///
    ///   -held + release_delta + charge_delta == -clamp_debit(cost, available).0
    ///
    /// in BOTH paths: a full settlement nets `-cost`, exactly the doc comment's
    /// `-reserved + release + charge = -cost`; a partial one nets only the
    /// clamped debit, which is strictly MORE money than `-cost` because the
    /// uncollected `cost - clamped` shortfall never reaches the ledger at all.
    /// That last fact is why `record_usage` takes `usage_cost_idr` and
    /// `charged_idr` as two separate arguments.
    #[test]
    fn a_requests_net_ledger_move_is_the_negative_clamped_debit() {
        for (held_idr, cost_idr) in sweep_pairs() {
            // A hold is non-negative by construction: release_reservation_transaction
            // refuses `reserved_idr <= 0` (db.rs:656) and the reserve path only
            // ever holds a positive amount.
            if held_idr < 0 {
                continue;
            }

            let available_idr = held_idr;
            let (debited_idr, lost_idr) = clamp_debit(cost_idr, available_idr);
            let (release_delta, charge_delta) = settlement_ledger_deltas(held_idr, debited_idr);
            let net_ledger_move = -held_idr + release_delta + charge_delta;

            assert_eq!(
                net_ledger_move, -debited_idr,
                "held {held_idr} cost {cost_idr}: the request must net to the clamped debit"
            );
            // The ledger can never move more money than the wallet held.
            assert_eq!(
                -net_ledger_move, debited_idr,
                "held {held_idr} cost {cost_idr}: the money out is exactly what was debited"
            );
            assert!(
                debited_idr <= available_idr.max(0),
                "held {held_idr} cost {cost_idr}: the ledger must never overdraw the wallet"
            );

            if cost_idr >= 0 && cost_idr <= available_idr.max(0) {
                // Full settlement: the hold comes back and the true cost lands.
                assert_eq!(
                    (debited_idr, lost_idr, net_ledger_move),
                    (cost_idr, 0, -cost_idr),
                    "held {held_idr} cost {cost_idr}: a full settlement nets exactly -cost"
                );
            } else if cost_idr > available_idr.max(0) {
                // Partial settlement: only the clamped debit lands, and the
                // shortfall stays out of the ledger.
                assert_eq!(
                    (debited_idr, lost_idr, net_ledger_move),
                    (
                        available_idr.max(0),
                        cost_idr - available_idr.max(0),
                        -available_idr.max(0)
                    ),
                    "held {held_idr} cost {cost_idr}: a partial settlement nets only the clamped debit"
                );
                assert!(
                    net_ledger_move > -cost_idr,
                    "held {held_idr} cost {cost_idr}: a partial settlement must move less than the true cost"
                );
            }
        }
    }

    /// REGRESSION for plan section 4.3, trap 3: a `NULL` `api_key_id` must not
    /// duplicate a usage row.
    ///
    /// SQLite does not enforce `NOT NULL` on a composite key, and it treats NULLs as
    /// distinct in unique indexes. So an upsert targeted at a plain
    /// `(account_id, api_key_id, day)` never fires its conflict clause for a
    /// NULL-keyed row and degrades into a plain insert — one row per call, forever,
    /// with a per-key aggregate that silently under-reports. Measured in the port:
    /// three identical upserts produced three rows.
    ///
    /// Latent rather than live today, because the proxy always passes a key. It
    /// becomes live the moment a caller passes `None`, which the signature allows.
    /// The `COALESCE` unique index plus a matching conflict target is the fix, and
    /// this is the test the naive port would have failed.
    #[tokio::test]
    async fn two_null_key_settlements_accumulate_into_one_usage_row() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, account_id, 100_000).await;

        // Account-level usage, twice on the same day: no key.
        for _ in 0..2 {
            debit_usage_transaction(
                &db.pool, account_id, None, "flash", 100, 10, 200, 5_000, None, 0,
            )
            .await
            .expect("a settlement against a funded wallet");
        }

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count usage rows");
        assert_eq!(
            rows, 1,
            "two NULL-keyed settlements must land on ONE row, not one each"
        );

        let (input, cache_read, output, cost): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr
             FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&db.pool)
        .await
        .expect("read the accumulated row");
        assert_eq!(
            (input, cache_read, output, cost),
            (200, 20, 400, 10_000),
            "the second call must accumulate onto the first, not start a new row"
        );

        // A keyed settlement on the same day is a DIFFERENT scope and must get its
        // own row, so the per-key aggregate stays separate from the account-level
        // one. The fix must not collapse the two scopes into one.
        let key_id = test_support::api_key(&db.pool, account_id).await;
        debit_usage_transaction(
            &db.pool,
            account_id,
            Some(key_id),
            "flash",
            100,
            10,
            200,
            5_000,
            None,
            0,
        )
        .await
        .expect("a keyed settlement");

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count usage rows");
        assert_eq!(
            rows, 2,
            "a keyed row is a separate scope from the NULL-keyed one"
        );

        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);
        db.close().await;
    }

    /// REGRESSION for the money-duplication defect plan section 4.5 found while
    /// executing: settle → an OUT-OF-BAND refund → the original SETTLEMENT
    /// webhook replays.
    ///
    /// The guard this replaced short-circuited only on `status == 'settled'`, so a
    /// row that had already been refunded fell through and was settled a second
    /// time: the wallet gained the top-up back and the row returned to `settled`,
    /// silently undoing the refund. The money then existed twice.
    ///
    /// The fix is the `status = 'pending'` predicate — any row that is not pending
    /// is refused — plus `TopupCreditResult::NotSettleable`, a fourth outcome,
    /// because reporting a refunded order as "already settled" is a lie an operator
    /// would act on.
    ///
    /// The refund is written here as raw SQL rather than through a function: the
    /// platform does not do refunds, so the only way a row reaches `refunded` is an
    /// operator acting outside this codebase. The guard must still hold.
    #[tokio::test]
    async fn a_settlement_replayed_after_a_refund_is_refused_and_credits_nothing() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        const AMOUNT: i64 = 10_000;

        let order_id = test_support::pending_topup(&db.pool, account_id, AMOUNT).await;

        assert_eq!(
            credit_topup_transaction(&db.pool, &order_id, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            }
        );
        assert_eq!(test_support::balance(&db.pool, account_id).await, AMOUNT);

        // The refund is now an OUT-OF-BAND action: this platform does not do
        // refunds, so NO code path performs one. This fixture reproduces exactly
        // what such an action would leave behind - the row moved to `refunded`,
        // the wallet debited, and a `refund` ledger row - written as raw SQL so
        // the test does not depend on a money-moving function that no longer
        // exists. `reason = 'refund'` is reserved-but-unreachable in production.
        sqlx::query("UPDATE topups SET status = 'refunded' WHERE order_id = ?")
            .bind(&order_id)
            .execute(&db.pool)
            .await
            .expect("mark the topup refunded out of band");
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr - ? WHERE account_id = ?")
            .bind(AMOUNT)
            .bind(account_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("debit the wallet the way an out-of-band refund would");
        sqlx::query(
            "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'refund', ?, 0, ?)",
        )
        .bind(account_id.hyphenated())
        .bind(-AMOUNT)
        .bind(topup_id(&db.pool, &order_id).await.to_string())
        .bind(Utc::now())
        .execute(&db.pool)
        .await
        .expect("append the out-of-band refund row");
        assert_eq!(test_support::balance(&db.pool, account_id).await, 0);
        assert_eq!(
            test_support::ledger_sum(&db.pool, account_id).await,
            0,
            "a settle followed by a refund must net to zero, not to a credit"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);

        // The replayed SETTLEMENT. This is the defect: before the fix this
        // credited AMOUNT again and flipped the row back to `settled`.
        let replay =
            credit_topup_transaction(&db.pool, &order_id, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("a replay is a recorded outcome, not an error");
        assert!(
            matches!(&replay, TopupCreditResult::NotSettleable { status } if status == "refunded"),
            "a settlement replayed after a refund must be refused: {replay:?}"
        );
        assert_eq!(
            test_support::balance(&db.pool, account_id).await,
            0,
            "the refund must stand: no money may be re-credited"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);

        // The row itself must still say `refunded`, so an operator reading it does
        // not see a settled order that has already been paid back.
        let status: String = sqlx::query_scalar("SELECT status FROM topups WHERE order_id = ?")
            .bind(&order_id)
            .fetch_one(&db.pool)
            .await
            .expect("read the topup status");
        assert_eq!(
            status, "refunded",
            "the replay must not flip the row back to settled"
        );

        db.close().await;
    }

    /// A settlement writes exactly one `usage_events` row, and its counters match
    /// what the request cost. This is the per-request detail behind the
    /// dashboard's "recent requests"; before it was written, `usage_events` was a
    /// table nothing populated.
    ///
    /// The two properties that matter: the event carries the SAME token split as
    /// `usage_daily` (so the detail cannot disagree with the aggregate), and it
    /// names the model (which no aggregate does).
    #[tokio::test]
    async fn a_settlement_writes_one_usage_event_with_the_real_tokens() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, account_id, 100_000).await;
        let key_id = test_support::api_key(&db.pool, account_id).await;

        debit_usage_transaction(
            &db.pool,
            account_id,
            Some(key_id),
            "deepseek-v4-flash",
            120,
            30,
            80,
            4_242,
            Some("ref_usage_event"),
            0,
        )
        .await
        .expect("a settlement");

        let row = sqlx::query(
            "SELECT api_key_id, model, input_tokens, cache_read_tokens, output_tokens, cost_idr, ref
             FROM usage_events WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&db.pool)
        .await
        .expect("the settlement wrote one usage event");

        let model: String = row.try_get("model").unwrap();
        let input: i64 = row.try_get("input_tokens").unwrap();
        let cache: i64 = row.try_get("cache_read_tokens").unwrap();
        let output: i64 = row.try_get("output_tokens").unwrap();
        let cost: i64 = row.try_get("cost_idr").unwrap();
        let key: Option<String> = row.try_get("api_key_id").unwrap();
        let reference: Option<String> = row.try_get("ref").unwrap();

        assert_eq!(model, "deepseek-v4-flash");
        assert_eq!((input, cache, output), (120, 30, 80));
        assert_eq!(cost, 4_242);
        assert_eq!(
            key.as_deref(),
            Some(key_id.hyphenated().to_string().as_str())
        );
        // The event is tied to the same reservation the ledger rows carry.
        assert_eq!(reference.as_deref(), Some("ref_usage_event"));

        // The aggregate holds the identical token split, so the two cannot drift.
        let agg = sqlx::query(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr
             FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&db.pool)
        .await
        .expect("the aggregate row");
        assert_eq!(agg.try_get::<i64, _>("input_tokens").unwrap(), input);
        assert_eq!(agg.try_get::<i64, _>("cache_read_tokens").unwrap(), cache);
        assert_eq!(agg.try_get::<i64, _>("output_tokens").unwrap(), output);
        assert_eq!(agg.try_get::<i64, _>("cost_idr").unwrap(), cost);

        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);
        db.close().await;
    }

    // ---------------------------------------------------------------------
    // retention_lag: whether a retention PROMISE is being broken
    // ---------------------------------------------------------------------
    //
    // The "DB disk" row of the Alerts table in `docs/observability.md` states the
    // db_disk alert as "volume usage" but its
    // ACTION is "Usage rows growing; check retention" - the operator's real question
    // is whether RETENTION IS WORKING. A volume percentage cannot answer that (80% is
    // normal for a working database), while a sweep that silently stopped is an
    // incident regardless of free space. `purge_expired_usage`'s own doc records that
    // the tables once "shipped populated with NO purge at all"; this is the same class
    // of defect, one step later - nothing can tell you the sweep stopped.
    //
    // The model is a LAG IN DAYS, not a boolean alone: "retention is behind" without an
    // age is an alert an operator cannot act on.

    /// A DATE-keyed table is MEASURED, not merely declared.
    ///
    /// This is the regression that made the whole retention query fail. `usage_daily`
    /// stores `day` as a DATE, and the lag helper decoded every column as an RFC3339
    /// instant, so the read returned a decode ERROR on any database with a single
    /// usage_daily row in it. The metrics endpoint turned that into `behind: null`
    /// forever, and the probe reports that as a standing db_disk alert - so the
    /// monitoring was not merely blind, it was permanently firing.
    ///
    /// Nothing caught it because no test seeded a usage_daily row and asked for the
    /// lag. The health-report tests construct a `RetentionLag` literal, so the query
    /// behind them was never exercised for a DATE-keyed table. This seeds one row
    /// and requires the query to answer.
    #[tokio::test]
    async fn a_date_keyed_table_is_measured_rather_than_failing_to_decode() {
        let db = TestDb::new().await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let account = test_support::account(&db.pool).await;
        let key_id = test_support::api_key(&db.pool, account).await;

        // NEGATIVE CONTROL: with no rows the query must SUCCEED, not error. The
        // old code also succeeded here, which is why the failure needed a row to
        // show up at all.
        assert!(
            !retention_lag(&db.pool, today)
                .await
                .unwrap()
                .anything_behind(),
            "an empty database is not behind"
        );

        // 900 days old, well past the 730-day window.
        sqlx::query(
            "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) VALUES (?, ?, ?, 5)",
        )
        .bind(account.hyphenated())
        .bind(key_id.hyphenated())
        .bind((today - chrono::Duration::days(900)).to_string())
        .execute(&db.pool)
        .await
        .expect("seed usage_daily");

        // THE POINT: this used to return Err, not a value.
        let lag = retention_lag(&db.pool, today)
            .await
            .expect("the lag query answers for a DATE-keyed table");
        assert!(
            lag.anything_behind(),
            "a 900-day-old usage_daily row is behind a 730-day window"
        );
        assert_eq!(
            lag.usage_daily,
            Some(900),
            "the age is measured in whole days from the DATE column"
        );

        // A row INSIDE the window must not report, so the branch is not simply
        // returning an age for anything it finds.
        sqlx::query("DELETE FROM usage_daily")
            .execute(&db.pool)
            .await
            .expect("clear");
        sqlx::query(
            "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens) VALUES (?, ?, ?, 5)",
        )
        .bind(account.hyphenated())
        .bind(key_id.hyphenated())
        .bind((today - chrono::Duration::days(400)).to_string())
        .execute(&db.pool)
        .await
        .expect("seed an in-window row");
        let inside = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            inside.usage_daily, None,
            "400 days is inside the 730-day window"
        );
        assert!(!inside.anything_behind(), "and nothing is behind");

        db.close().await;
    }
    /// The salted-hash tables are MEASURED, not merely declared.
    ///
    /// A field that exists and is always `None` is indistinguishable from a working
    /// one in every other test, and the privacy promise would be unmonitored exactly
    /// as before - so this seeds a row past each window and requires the lag to name
    /// it. The negative control comes first, because an empty database reading clean
    /// is also what a broken measurement would report.
    ///
    /// Two of the four are DATE-keyed, which is the branch the previous regression
    /// added; the other two are instants. Between them they exercise both paths, so
    /// this is also the test that would notice one of them being mis-keyed.
    #[tokio::test]
    async fn an_ip_hash_table_past_its_window_is_reported_as_behind() {
        let db = TestDb::new().await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let account = test_support::account(&db.pool).await;
        let key_id = test_support::api_key(&db.pool, account).await;

        // NEGATIVE CONTROL: nothing seeded, nothing behind, every table named.
        let empty = retention_lag(&db.pool, today).await.unwrap();
        assert!(!empty.anything_behind(), "an empty database is not behind");
        assert_eq!(
            empty.oldest_days_by_table().len(),
            10,
            "the report names every table the sweep deletes, not the three it used to. The \
             count moved 7 -> 8 when `identity_tokens` was added (a purge function, a unit \
             test and NO caller), 8 -> 9 when `link_code_issues` was added (swept since \
             it existed, measured and published by nothing), and 9 -> 10 when `link_codes` \
             was added (measured by nothing AND swept by nothing, behind a published \
             24-hour window). All three were found by a guard that compares this list to \
             another hand-kept list, which is the only way an omission here is visible at \
             all."
        );

        // 200 days old: behind all four windows, which are 7, 90, 7 and 7.
        let old = (today - chrono::Duration::days(200)).to_string();
        sqlx::query("INSERT INTO key_ip_seen (api_key_id, day, ip_hash) VALUES (?, ?, ?)")
            .bind(key_id.hyphenated())
            .bind(&old)
            .bind("h")
            .execute(&db.pool)
            .await
            .expect("seed key_ip_seen");
        sqlx::query("INSERT INTO key_ip_daily (api_key_id, day, distinct_ips) VALUES (?, ?, ?)")
            .bind(key_id.hyphenated())
            .bind(&old)
            .bind(1i64)
            .execute(&db.pool)
            .await
            .expect("seed key_ip_daily");
        sqlx::query("INSERT INTO link_redemption_attempts (ip_hash, attempted_at) VALUES (?, ?)")
            .bind("h")
            .bind(
                today
                    .checked_sub_signed(chrono::Duration::days(200))
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .to_rfc3339(),
            )
            .execute(&db.pool)
            .await
            .expect("seed link_redemption_attempts");
        // `auth_attempts` is the fourth, and the one this test was extended for. It is
        // seeded ACCOUNT-keyed, which is the half whose retention is a deliberate
        // decision rather than an inheritance from key_ip_seen: an account id and a
        // timestamp is still a log of who tried to sign in, and it ages out on the
        // same 7 days.
        sqlx::query(
            "INSERT INTO auth_attempts (id, ip_hash, account_id, kind, created_at) \
             VALUES (?, '', ?, 'login', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account.hyphenated())
        .bind(
            today
                .checked_sub_signed(chrono::Duration::days(200))
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339(),
        )
        .execute(&db.pool)
        .await
        .expect("seed auth_attempts");

        // `identity_tokens` is the FIFTH, and unlike the other four it is measured on
        // `expires_at` rather than `created_at`. Seeding an expired link is therefore
        // not enough on its own: the row below was CREATED a moment ago, so a
        // `created_at` measurement would report it inside the window and this test
        // would still pass with the field wired to the wrong column. The old-but-
        // freshly-made shape is what makes the two distinguishable.
        //
        // The window is ZERO days past `expires_at` - the sweep deletes a link the
        // moment it expires - so 200 days past expiry is unambiguously behind.
        sqlx::query(
            "INSERT INTO identity_tokens (id, account_id, purpose, token_hash, expires_at, created_at) \
             VALUES (?, ?, 'verification', ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account.hyphenated())
        .bind("h")
        .bind(
            today
                .checked_sub_signed(chrono::Duration::days(200))
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339(),
        )
        .bind(
            chrono::Utc::now()
                .checked_sub_signed(chrono::Duration::days(1))
                .unwrap()
                .to_rfc3339(),
        )
        .execute(&db.pool)
        .await
        .expect("seed identity_tokens");

        // `link_code_issues` is the SIXTH, and the one this test was extended for last.
        // It is an INSTANT on `created_at` like `auth_attempts` directly above, but it
        // goes through `ip_tracking::purge_expired` rather than `db.rs`, which is why
        // it was missing from this report for as long as it has existed: the table was
        // swept correctly and measured by nothing, so a sweep that stopped would have
        // been invisible.
        sqlx::query("INSERT INTO link_code_issues (account_id, created_at) VALUES (?, ?)")
            .bind(account.hyphenated())
            .bind(
                today
                    .checked_sub_signed(chrono::Duration::days(200))
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_utc()
                    .to_rfc3339(),
            )
            .execute(&db.pool)
            .await
            .expect("seed link_code_issues");

        // `link_codes` is the SEVENTH, and the only one on this list that was measured by
        // nothing AND swept by nothing. It is an INSTANT, but on an EXPRESSION rather
        // than a column: `COALESCE(used_at, expires_at)`, because a redeemed code stops
        // being usable at `used_at` and an unredeemed one at `expires_at`. The seed
        // below is REDEEMED 200 days ago with an `expires_at` far in the FUTURE, so a
        // measurement that read `expires_at` would call it inside the window and this
        // assertion would fail while the field looked wired.
        sqlx::query(
            "INSERT INTO link_codes (code, account_id, expires_at, used_at, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind("seeded-old")
        .bind(account.hyphenated())
        .bind(
            chrono::Utc::now()
                .checked_add_signed(chrono::Duration::days(3))
                .unwrap()
                .to_rfc3339(),
        )
        .bind(
            today
                .checked_sub_signed(chrono::Duration::days(200))
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339(),
        )
        .bind(
            chrono::Utc::now()
                .checked_sub_signed(chrono::Duration::days(1))
                .unwrap()
                .to_rfc3339(),
        )
        .execute(&db.pool)
        .await
        .expect("seed link_codes");

        let lag = retention_lag(&db.pool, today)
            .await
            .expect("the lag query answers");
        assert!(
            lag.anything_behind(),
            "seven seeded tables are past their windows"
        );
        for table in [
            "key_ip_seen",
            "key_ip_daily",
            "link_redemption_attempts",
            "auth_attempts",
            "identity_tokens",
            "link_code_issues",
            "link_codes",
        ] {
            let named = lag
                .oldest_days_by_table()
                .into_iter()
                .any(|(name, age)| name == table && age.is_some());
            assert!(
                named,
                "{table} is seeded 200 days old and must be reported by name; a field that is always None is the failure this test exists to prevent"
            );
        }

        // And a row INSIDE every window reports nothing, so the branch is not simply
        // returning an age for anything it finds.
        sqlx::query("DELETE FROM key_ip_seen")
            .execute(&db.pool)
            .await
            .expect("clear");
        let fresh = (today - chrono::Duration::days(3)).to_string();
        sqlx::query("INSERT INTO key_ip_seen (api_key_id, day, ip_hash) VALUES (?, ?, ?)")
            .bind(key_id.hyphenated())
            .bind(&fresh)
            .bind("h2")
            .execute(&db.pool)
            .await
            .expect("seed an in-window row");
        let inside = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            inside.key_ip_seen, None,
            "3 days is inside the 7-day window"
        );

        // EACH TABLE IS MEASURED AGAINST ITS OWN WINDOW, which the 200-day seed above
        // cannot establish: that row is past 7, 90 and 7 alike, so swapping one constant
        // for a neighbour's changes nothing and the test still passes. A row at 30 days
        // does distinguish them - past the 7-day window, comfortably inside the 90-day
        // one - so this is the assertion that notices a table measured against its
        // neighbour's number, which is exactly what copying a constant invites.
        sqlx::query("DELETE FROM key_ip_seen")
            .execute(&db.pool)
            .await
            .expect("clear");
        let straddling = (today - chrono::Duration::days(30)).to_string();
        sqlx::query("INSERT INTO key_ip_seen (api_key_id, day, ip_hash) VALUES (?, ?, ?)")
            .bind(key_id.hyphenated())
            .bind(&straddling)
            .bind("h3")
            .execute(&db.pool)
            .await
            .expect("seed a 30-day row");
        let thirty = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            thirty.key_ip_seen,
            Some(30),
            "30 days is past the 7-day window; if this is None then the table is being measured against a LONGER one, which is the failure a copied constant invites"
        );

        // The same for `key_ip_daily`, whose 90-day window is the one a reader is most
        // likely to doubt: 400 days is past 90 and comfortably inside usage_daily's 730,
        // so a row at that age reports against the 90 and not against whichever constant
        // happens to be adjacent in the source. The third table, link_redemption_attempts
        // at 7 days, cannot be straddled against key_ip_seen because both windows are 7 -
        // they are the same number, so there is nothing to confuse them with.
        sqlx::query("DELETE FROM key_ip_daily")
            .execute(&db.pool)
            .await
            .expect("clear");
        let four_hundred = (today - chrono::Duration::days(400)).to_string();
        sqlx::query("INSERT INTO key_ip_daily (api_key_id, day, distinct_ips) VALUES (?, ?, ?)")
            .bind(key_id.hyphenated())
            .bind(&four_hundred)
            .bind(2i64)
            .execute(&db.pool)
            .await
            .expect("seed a 400-day row");
        let four = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            four.key_ip_daily,
            Some(400),
            "400 days is past key_ip_dailys 90-day window and inside usage_dailys 730; if this is None the two are being measured against the same number"
        );

        db.close().await;
    }
    /// An EMPTY database is not lagging, and reports no oldest row.
    #[tokio::test]
    async fn an_empty_database_has_no_retention_lag() {
        let db = TestDb::new().await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        let lag = retention_lag(&db.pool, today).await.unwrap();

        assert!(
            !lag.anything_behind(),
            "a database with no rows cannot be behind its retention"
        );
        // No rows means NO oldest row: distinct from "an old row", and the distinction
        // matters because `None` must not be rendered as an age of 0.
        for (table, oldest) in lag.oldest_days_by_table() {
            assert_eq!(oldest, None, "{table} has no rows, so it has no oldest row");
        }

        db.close().await;
    }

    /// A row INSIDE its window is not lagging.
    #[tokio::test]
    async fn a_row_inside_its_window_is_not_behind() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        // One day old: comfortably inside a 90-day window.
        let recent = today.and_hms_opt(12, 0, 0).unwrap().and_utc() - chrono::Duration::days(1);
        sqlx::query(
            "INSERT INTO usage_events (id, account_id, model, input_tokens, created_at)
             VALUES (?, ?, 'flash', 1, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(recent)
        .execute(&db.pool)
        .await
        .unwrap();

        let lag = retention_lag(&db.pool, today).await.unwrap();
        assert!(
            lag.usage_events.is_none(),
            "a row inside the window is not lagging"
        );
        assert!(!lag.anything_behind());

        db.close().await;
    }

    /// A row PAST its window IS lagging, and its AGE is reported.
    #[tokio::test]
    async fn a_row_past_its_window_is_reported_as_behind() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        // 200 days old against a 90-day promise: the sweep has not run for a long time,
        // or it is broken. Either way the PROMISE is being broken right now.
        let stale = today.and_hms_opt(12, 0, 0).unwrap().and_utc() - chrono::Duration::days(200);
        sqlx::query(
            "INSERT INTO usage_events (id, account_id, model, input_tokens, created_at)
             VALUES (?, ?, 'flash', 1, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(stale)
        .execute(&db.pool)
        .await
        .unwrap();

        let lag = retention_lag(&db.pool, today).await.unwrap();
        assert!(
            lag.anything_behind(),
            "a 200-day-old row breaks a 90-day promise"
        );
        let days_old = lag
            .usage_events
            .expect("usage_events is the lagging table and must be named");
        assert!(
            days_old >= 199,
            "the reported age must be the row's real age in days, got {days_old}"
        );

        db.close().await;
    }

    /// The boundary AGREES with the purge, so the two cannot disagree about one row.
    ///
    /// The subtle one. `purge_expired_usage` deletes at an inclusive `<=` on midnight
    /// UTC of the cutoff day, and its doc explains why: "an exclusive comparison
    /// silently retains N+1 days against an N-day promise". If the lag query drew the
    /// boundary elsewhere, a row could be at once "old enough to delete" and "not
    /// lagging", and the two would drift with nothing to catch it.
    ///
    /// The purge boundary is midnight of `today - 90`. A row one second AFTER that
    /// instant is older than 90 days by every reasonable reading, so it IS behind -
    /// and the test asserts exactly that, because getting it backwards here would
    /// make the alert silent for a whole extra day.
    #[tokio::test]
    async fn a_row_past_the_purge_cutoff_is_reported_as_behind() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        let cutoff = (today - chrono::Duration::days(USAGE_EVENTS_RETENTION_DAYS))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();

        // TWO rows, one on each side of the purge boundary, asserted together so the
        // lag query and the purge CANNOT disagree about the boundary:
        //
        //   * AT the cutoff (midnight, 90 days ago): the purge DELETES this row
        //     (asserted by `usage_events_retention_deletes_the_boundary_day_and_keeps_89`),
        //     so a surviving copy means the sweep did not run - BEHIND.
        //   * ONE SECOND LATER: this row is 89d 23:59:59 old, INSIDE the 90-day
        //     window, and the purge correctly KEEPS it. Reporting it as behind would
        //     be a false alarm on a database whose retention is working perfectly.
        for created in [cutoff, cutoff + chrono::Duration::seconds(1)] {
            sqlx::query(
                "INSERT INTO usage_events (id, account_id, model, input_tokens, created_at)
                 VALUES (?, ?, 'flash', 1, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(created)
            .execute(&db.pool)
            .await
            .unwrap();
        }

        // The OLDEST row decides, and the oldest is AT the cutoff, so this is behind.
        let lag = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            lag.usage_events,
            Some(USAGE_EVENTS_RETENTION_DAYS),
            "a row exactly ON the cutoff day is one the purge would have deleted, so it is behind by the full window"
        );

        // Now prove the SECOND row alone is NOT behind, which is the half that would
        // otherwise be untested: delete the boundary row and the answer must flip.
        sqlx::query("DELETE FROM usage_events WHERE created_at <= ?")
            .bind(cutoff)
            .execute(&db.pool)
            .await
            .unwrap();
        let lag = retention_lag(&db.pool, today).await.unwrap();
        assert_eq!(
            lag.usage_events, None,
            "an instant just INSIDE the window is not behind: treating it as behind would fire the alert on a database whose retention is working"
        );

        db.close().await;
    }
    /// later is KEPT. The inclusive comparison is the whole point — with `<` the
    /// table silently retains 91 days against a 90-day promise.
    #[tokio::test]
    async fn usage_events_retention_deletes_the_boundary_day_and_keeps_89() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;

        // Three rows at hand-chosen instants relative to a fixed "today".
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let cutoff = today - chrono::Duration::days(USAGE_EVENTS_RETENTION_DAYS);
        let at_cutoff = cutoff.and_hms_opt(0, 0, 0).unwrap().and_utc();
        let just_inside = at_cutoff + chrono::Duration::seconds(1);
        let well_kept = today.and_hms_opt(12, 0, 0).unwrap().and_utc();

        for (i, created) in [at_cutoff, just_inside, well_kept].iter().enumerate() {
            sqlx::query(
                "INSERT INTO usage_events (id, account_id, model, input_tokens, created_at)
                 VALUES (?, ?, 'flash', ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(i as i64)
            .bind(created)
            .execute(&db.pool)
            .await
            .expect("seed a usage event");
        }

        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(
            purged.usage_events, 1,
            "only the row AT the cutoff day is deleted"
        );

        // The two survivors are the second-after-cutoff and the recent one.
        let kept: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_events WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            kept, 2,
            "90 days are RETAINED, so the boundary day goes and 89 stay"
        );

        db.close().await;
    }

    /// Idempotent: a second sweep in the same day removes nothing.
    #[tokio::test]
    async fn usage_events_retention_is_idempotent() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        let old = (today - chrono::Duration::days(USAGE_EVENTS_RETENTION_DAYS + 5))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        sqlx::query(
            "INSERT INTO usage_events (id, account_id, model, created_at) VALUES (?, ?, 'flash', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(old)
        .execute(&db.pool)
        .await
        .unwrap();

        assert_eq!(
            purge_expired_usage(&db.pool, today)
                .await
                .unwrap()
                .usage_events,
            1
        );
        assert_eq!(
            purge_expired_usage(&db.pool, today)
                .await
                .unwrap()
                .usage_events,
            0,
            "the second sweep removes nothing"
        );

        db.close().await;
    }

    /// The constant is the contract: 90 days, as docs/data-retention.md states.
    #[test]
    fn usage_events_retention_constant_is_documented() {
        assert_eq!(USAGE_EVENTS_RETENTION_DAYS, 90);
    }

    /// THE SWEEP DELETES EXPIRED LINKS, which for a while nothing did.
    ///
    /// `identity::tokens::purge_expired` existed with its own unit test and NO
    /// production caller - no binary, no scheduler entry - so expired verification
    /// and password-reset links accumulated forever while the privacy page said they
    /// did not. Folding it into this sweep is the fix, and THIS test is what makes
    /// the fold real: without it, deleting the call from `purge_expired_usage` would
    /// leave every other test green, which is precisely how the gap survived the
    /// first time.
    ///
    /// The boundary is `expires_at <= now`, an INSTANT rather than a midnight
    /// cutoff - a link is stale the moment it expires, not at the end of some day.
    /// Both sides are seeded so a comparison that ran the wrong way is visible: the
    /// expired row must go, the live row must stay.
    #[tokio::test]
    async fn the_retention_sweep_deletes_expired_links_and_keeps_live_ones() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        // `expires_at` is the only column the purge reads, and it is seeded far in
        // the past for one row and far in the future for the other. `created_at` is
        // set to the SAME recent instant on both, so a purge that read `created_at`
        // would delete neither and the assertion below would fail.
        let created = chrono::Utc::now();
        for (id, expires_at) in [
            ("expired", created - chrono::Duration::days(7)),
            ("live", created + chrono::Duration::days(7)),
        ] {
            sqlx::query(
                "INSERT INTO identity_tokens (id, account_id, purpose, token_hash, expires_at, created_at) \
                 VALUES (?, ?, 'verification', ?, ?, ?)",
            )
            .bind(id)
            .bind(account.hyphenated())
            .bind(format!("hash-{id}"))
            .bind(expires_at.to_rfc3339())
            .bind(created.to_rfc3339())
            .execute(&db.pool)
            .await
            .expect("seed an identity token");
        }

        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(
            purged.identity_tokens, 1,
            "the sweep must delete the EXPIRED link and leave the live one. Zero here \
             means the call was dropped from the sweep; two means it read the wrong \
             column."
        );

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT id FROM identity_tokens WHERE account_id = ?")
                .bind(account.hyphenated())
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining,
            vec!["live".to_string()],
            "a link that has not expired must survive: deleting it would break a \
             verification in flight"
        );

        db.close().await;
    }

    /// Telegram link codes: a REDEEMED code ages from `used_at`, an UNUSED one from
    /// `expires_at`, and both get the published day of grace. Three rows, and each
    /// one exists to kill a different wrong implementation.
    ///
    /// The `COALESCE` is the rule, so the seeds are chosen so that reading EITHER
    /// column alone gives the wrong answer:
    /// - `redeemed` has `expires_at` in the FUTURE and `used_at` two days ago. A
    ///   purge that read only `expires_at` keeps it, correctly, forever-until-TTL -
    ///   so it must GO, and it only goes if `used_at` is consulted.
    /// - `stale-unused` has `used_at` NULL and `expires_at` two days ago, so it must
    ///   GO. A purge that read only `used_at` would never match a NULL and it would
    ///   stay forever.
    /// - `live` is inside its TTL and unused, so it must STAY. Deleting it would
    ///   break a link code a customer has open in a chat window right now.
    ///
    /// `LINK_CODE_RETENTION_GRACE_DAYS` is 1, so the terminal rows are seeded 25
    /// HOURS back: past the grace by one hour, and short of two days. Both margins
    /// are deliberate. Seeded "two days ago" they would still be deleted by a purge
    /// with a ZERO-day grace, so changing the constant to 0 would leave this test
    /// green and the number would be decoration; seeded "one hour ago" a correct
    /// purge would spare them and the test would have to assert the opposite. 25
    /// hours is the only band that pins the value from both sides.
    #[tokio::test]
    async fn the_retention_sweep_ages_a_link_code_from_whichever_instant_ended_it() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let now = chrono::Utc::now();

        // The tuple type is named rather than written inline: `clippy` flags the
        // four-field spelling here as "very complex type", and a local alias is
        // the honest fix - the shape is what this test is about.
        type Seed = (
            &'static str,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
        );

        let rows: [Seed; 4] = [
            // A terminal row 25 HOURS back: past the one-day grace, and only just.
            // The margin matters - see the doc-comment above for why these two are
            // 25 hours and not "two days".
            (
                "redeemed",
                now + chrono::Duration::days(3),
                Some(now - chrono::Duration::hours(25)),
            ),
            ("stale-unused", now - chrono::Duration::hours(25), None),
            // THE GRACE ITSELF. Terminal 2 hours ago: INSIDE the one-day grace, so a
            // correct sweep spares it, and a sweep with a ZERO-day grace deletes it.
            // This row is what pins `LINK_CODE_RETENTION_GRACE_DAYS` from below;
            // without it the constant could be changed to 0 and every assertion here
            // would still hold, which makes the number decoration rather than policy.
            (
                "recently-redeemed",
                now + chrono::Duration::days(3),
                Some(now - chrono::Duration::hours(2)),
            ),
            // Never redeemed, still inside its five-minute TTL.
            ("live", now + chrono::Duration::minutes(4), None),
        ];
        for (code, expires_at, used_at) in rows {
            sqlx::query(
                "INSERT INTO link_codes (code, account_id, expires_at, used_at, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(code)
            .bind(account.hyphenated())
            .bind(expires_at.to_rfc3339())
            .bind(used_at.map(|at| at.to_rfc3339()))
            .bind((now - chrono::Duration::days(3)).to_rfc3339())
            .execute(&db.pool)
            .await
            .expect("seed a link code");
        }

        // A FAR-PAST `created_at` on every row, deliberately: `created_at` is the
        // column every OTHER table in this sweep ages from, so a purge that reached
        // for it here would delete all three and the survivor assertion below would
        // fail. The column this rule uses is not the column the sweep is named for.
        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(
            purged.link_codes, 2,
            "a redeemed code and an expired unused one must both go, a terminal one \
             inside its day of grace must stay, and a live one must stay. One deleted \
             means only one of the two terminal states is handled - `used_at` or \
             `expires_at`, not the COALESCE; zero means the call was dropped from the \
             sweep; three means the grace is zero; four means it aged from `created_at`."
        );

        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT code FROM link_codes WHERE account_id = ? ORDER BY code")
                .bind(account.hyphenated())
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(
            remaining,
            vec!["live".to_string(), "recently-redeemed".to_string()],
            "the unused code inside its TTL must survive because a customer may be \
             holding it in a chat window, and the code that became terminal 2 hours \
             ago must survive because the page promises used or expired PLUS A DAY -  \
             deleting either would make the published sentence false"
        );

        db.close().await;
    }

    /// `usage_daily` is kept 24 months: a row at the cutoff DAY is deleted, one a
    /// day later is kept. `day` is a TEXT date, so the bound is a date, not an
    /// instant — binding an instant would sort after every stored date and the
    /// DELETE would match nothing.
    #[tokio::test]
    async fn usage_daily_retention_deletes_the_cutoff_day_and_keeps_the_rest() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();

        let at_cutoff = today - chrono::Duration::days(USAGE_DAILY_RETENTION_DAYS);
        let just_inside = at_cutoff + chrono::Duration::days(1);
        for day in [at_cutoff, just_inside, today] {
            sqlx::query(
                "INSERT INTO usage_daily (account_id, api_key_id, day, input_tokens)
                 VALUES (?, NULL, ?, 1)",
            )
            .bind(account_id.hyphenated())
            .bind(day)
            .execute(&db.pool)
            .await
            .expect("seed a daily row");
        }

        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(purged.usage_daily, 1, "only the cutoff DAY is deleted");

        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(kept, 2, "the cutoff day goes; the two newer days stay");

        db.close().await;
    }

    /// Sessions are swept 30 days after they STOPPED being usable — which is
    /// `revoked_at` for an early logout, not `expires_at`. A session revoked
    /// early but with a far-future `expires_at` is the common case, and a naive
    /// `expires_at <= cutoff` predicate would retain it well past the promise.
    #[tokio::test]
    async fn session_retention_uses_the_instant_it_stopped_being_usable() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        let today = chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let cutoff = (today - chrono::Duration::days(SESSION_RETENTION_DAYS))
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let future = cutoff + chrono::Duration::days(300);

        // A local helper, not a closure: a closure that borrows the pool cannot be
        // called three times without moving it.
        async fn seed_session(
            pool: &SqlitePool,
            account_id: Uuid,
            expires: chrono::DateTime<chrono::Utc>,
            revoked: Option<chrono::DateTime<chrono::Utc>>,
        ) {
            let token = format!("apk_sess_{}", Uuid::new_v4().simple());
            sqlx::query(
                "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at, revoked_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(crate::routes::hash_token(&token))
            .bind(expires)
            .bind(expires)
            .bind(expires)
            .bind(revoked)
            .execute(pool)
            .await
            .expect("seed a session");
        }

        // (a) Long expired, never revoked -> deleted.
        seed_session(
            &db.pool,
            account_id,
            cutoff - chrono::Duration::days(1),
            None,
        )
        .await;
        // (b) Revoked early with a FUTURE expires_at -> deleted, on `revoked_at`.
        seed_session(
            &db.pool,
            account_id,
            future,
            Some(cutoff - chrono::Duration::days(1)),
        )
        .await;
        // (c) Still live (expires in the future, not revoked) -> kept.
        seed_session(&db.pool, account_id, future, None).await;

        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(
            purged.sessions, 2,
            "the expired row and the early-revoked row both go; the live one stays"
        );

        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(kept, 1, "the live session survives");

        db.close().await;
    }

    /// The sweep never touches the tables with NO age-based period: financial
    /// records and user-owned reviews.
    #[tokio::test]
    async fn retention_sweep_leaves_financial_records_alone() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, account_id, 5_000).await;
        let today = chrono::NaiveDate::from_ymd_opt(2099, 1, 1).unwrap();

        let purged = purge_expired_usage(&db.pool, today).await.unwrap();
        assert_eq!(purged.usage_events, 0);
        assert_eq!(purged.usage_daily, 0);
        assert_eq!(purged.sessions, 0);

        // The ledger row from the funding survives a far-future sweep.
        let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(ledger, 1, "the ledger is kept forever");

        db.close().await;
    }

    /// A reservation of zero (or less) holds nothing and writes nothing: the
    /// guard short-circuits before any transaction is opened. Covers db.rs:900.
    #[tokio::test]
    async fn a_zero_reservation_holds_nothing_and_writes_nothing() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        for requested in [0, -1] {
            let outcome =
                reserve_balance_transaction(&db.pool, account_id, requested, Some("zero_ref"))
                    .await
                    .expect("a zero reservation is not an error");
            assert_eq!(outcome, ReservationResult::Zero, "requested {requested}");
        }

        // Nothing was written: no ledger row exists for the account.
        let ledger: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count ledger rows");
        assert_eq!(ledger, 0, "a zero reservation must write no ledger rows");
        db.close().await;
    }

    /// A settlement whose reported hold was reserved against a wallet that does
    /// not exist releases NOTHING and still records the usage: crediting a hold
    /// the ledger never took would create money. This is the guard at
    /// db.rs:314-324, and it also drives the re-clamp retry inside
    /// settle_partial_usage (db.rs:995-999) the honest way - with no wallet row,
    /// the clamped debit matches no row and the retry floors the debit at zero.
    #[tokio::test]
    async fn a_settlement_whose_hold_matched_no_wallet_releases_nothing() {
        let db = TestDb::new().await;
        // No wallets row at all: the hold could never have been taken, yet the
        // caller reports one - the exact state the guard must not "make whole".
        let account_id = test_support::account(&db.pool).await;
        let key_id = test_support::api_key(&db.pool, account_id).await;

        let outcome = debit_usage_transaction(
            &db.pool,
            account_id,
            Some(key_id),
            "flash",
            200,
            0,
            150,
            1_000,
            Some("no_wallet_ref"),
            500, // a hold the (missing) wallet row cannot back
        )
        .await
        .expect("a settlement against a missing wallet is a recorded outcome, not an error");

        // The usage is recorded (the answer was already streamed), the release is
        // zero, and the debit floors at zero with the whole cost a shortfall.
        match &outcome {
            UsageSettlement::Partial {
                debited_idr,
                shortfall_idr,
                ..
            } => {
                assert_eq!(*debited_idr, 0, "no wallet means nothing could be debited");
                assert_eq!(
                    *shortfall_idr, 1_000,
                    "the whole cost is a visible shortfall"
                );
            }
            other => panic!("expected a partial settlement, got {other:?}"),
        }
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // Credit expiry (`docs/decisions.md:76-78`)
    // -----------------------------------------------------------------------

    /// Settles a deposit and then MOVES ITS EXPIRY into the past, so a test can
    /// age a deposit without waiting two years.
    ///
    /// This writes `credit_expires_at` directly and that is the only place in
    /// these tests that touches a money column outside the real path. There is no
    /// alternative: the expiry instant is stamped from `Utc::now()` at
    /// settlement, and no argument to the real path can put it in the past. The
    /// fields being aged are TIMESTAMPS, not the balance, so the ledger/wallet
    /// invariant the direct write could break is untouched - and
    /// `the_sweep_keeps_the_ledger_equal_to_the_wallet` re-checks it anyway.
    ///
    /// `None` expires the deposit as of NOW; `Some(d)` as of `d`.
    async fn age_the_deposit(
        pool: &SqlitePool,
        order_id: &str,
        expires_at: Option<DateTime<Utc>>,
    ) -> Uuid {
        let id = topup_id(pool, order_id).await;
        sqlx::query("UPDATE topups SET credit_expires_at = ? WHERE id = ?")
            .bind(expires_at.unwrap_or_else(Utc::now))
            .bind(id.hyphenated())
            .execute(pool)
            .await
            .expect("age the deposit");
        id
    }

    /// The `ref` values of every `usage`-reasoned ledger row for the account,
    /// oldest first.
    async fn usage_refs(pool: &SqlitePool, account_id: Uuid) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT ref FROM ledger WHERE account_id = ? AND reason = 'usage' ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .fetch_all(pool)
        .await
        .expect("read usage refs")
    }

    async fn credit_retired_at(pool: &SqlitePool, topup_id: Uuid) -> Option<DateTime<Utc>> {
        sqlx::query_scalar("SELECT credit_retired_at FROM topups WHERE id = ?")
            .bind(topup_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read credit_retired_at")
    }

    /// Spends `amount_idr` out of the wallet the way a usage settlement does -
    /// a `usage` ledger row and a matching wallet decrement, together - so the
    /// expiry tests can start from a partly-spent wallet without going through
    /// the whole request pipeline.
    ///
    /// This is the ONLY place in these tests that moves money outside the real
    /// top-up and debit paths, and it preserves the invariant they preserve, so
    /// `ledger_drift_rows` stays 0 across it. Reaching for it instead of letting
    /// the wallet hold the full amount is what makes the "cannot take more than
    /// is there" case testable at all: a wallet that always holds every deposit
    /// in full can never be the constrained case.
    async fn retire_balance(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) {
        let mut tx = begin_immediate(pool).await.expect("begin the spend");
        let debited = sqlx::query(
            "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 \
             WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
        )
        .bind(amount_idr)
        .bind(Utc::now())
        .bind(account_id.hyphenated())
        .fetch_optional(&mut *tx)
        .await
        .expect("read the debit")
        .expect("the fixture must not spend more than the wallet holds");
        let new_balance: i64 = debited.get("balance_idr");
        sqlx::query(
            "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) \
             VALUES (?, ?, 'usage', 'test-spend', ?, ?)",
        )
        .bind(account_id.hyphenated())
        .bind(-amount_idr)
        .bind(new_balance)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await
        .expect("record the spend");
        tx.commit().await.expect("commit the spend");
    }

    #[tokio::test]
    async fn expiry_disabled_stamps_no_instant_and_the_sweep_cannot_take_the_credit() {
        // `credit_expiry_months = 0` is the "disabled" setting, and the point of
        // this test is that disabled means UNREACHABLE rather than "expired
        // immediately". A NULL that the sweep read as "long overdue" would turn
        // the development setting into a nightly confiscation of every balance.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        test_support::wallet(&db.pool, account).await;

        let order_id = test_support::pending_topup(&db.pool, account, 50_000).await;
        credit_topup_transaction(&db.pool, &order_id, 50_000, 0)
            .await
            .expect("settle with expiry disabled");

        let stored: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT credit_expires_at FROM topups WHERE order_id = ?")
                .bind(&order_id)
                .fetch_one(&db.pool)
                .await
                .expect("read the stamped expiry");
        assert_eq!(stored, None, "0 must stamp NULL, not now-plus-nothing");

        // Run the sweep far in the future: NULL is still not a candidate.
        let sweep = expire_credit(&db.pool, Utc::now() + chrono::Duration::days(3_650))
            .await
            .expect("sweep a disabled deposit");
        assert_eq!(sweep.expired, 0);
        assert_eq!(sweep.expired_idr, 0);

        assert_eq!(
            test_support::balance(&db.pool, account).await,
            50_000,
            "a disabled expiry must leave the money alone"
        );
        assert_eq!(
            test_support::ledger_sum(&db.pool, account).await,
            50_000,
            "and must append no ledger row"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_retires_a_deposit_whose_credit_has_aged_out() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let order_id = fund_through_topup(&db.pool, account, 50_000).await;
        let id = age_the_deposit(&db.pool, &order_id, None).await;

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");

        assert_eq!(sweep.expired, 1, "one deposit aged out");
        assert_eq!(sweep.expired_idr, 50_000, "and gave up its whole amount");

        assert_eq!(
            test_support::balance(&db.pool, account).await,
            0,
            "the wallet must not still hold retired credit"
        );
        assert_eq!(
            test_support::ledger_sum(&db.pool, account).await,
            0,
            "the ledger is authoritative and must agree with the wallet"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        assert!(credit_retired_at(&db.pool, id).await.is_some());
        db.close().await;
    }

    #[tokio::test]
    async fn an_expiry_row_is_filed_under_the_deposit_it_retired() {
        // The `ref` is what makes an expiry row auditable - and what keeps it
        // distinguishable from the deposit's OWN credit row, which
        // `topup_ledger_ref` files under the bare topup id. Without the prefix
        // the two rows would be indistinguishable in the ledger, and both are
        // `reason = 'usage'` because the CHECK set is frozen
        // (`docs/decisions.md:70`).
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let order_id = fund_through_topup(&db.pool, account, 50_000).await;
        let id = age_the_deposit(&db.pool, &order_id, None).await;

        expire_credit(&db.pool, Utc::now()).await.expect("sweep");

        assert_eq!(
            usage_refs(&db.pool, account).await,
            vec![credit_expiry_ref(&id.hyphenated().to_string())],
            "the retirement must be selectable by the deposit it retired"
        );

        // The credit row and the expiry row are two rows about the same deposit,
        // so the deposit id alone does not identify one of them.
        let credit_ref = topup_ledger_ref(id);
        assert_ne!(
            credit_expiry_ref(&id.hyphenated().to_string()),
            credit_ref,
            "the two rows about one deposit must not collide"
        );
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_leaves_credit_that_has_not_aged_out() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let order_id = fund_through_topup(&db.pool, account, 50_000).await;

        // The real path stamped this two years out; the sweep runs today.
        let expiry: DateTime<Utc> =
            sqlx::query_scalar("SELECT credit_expires_at FROM topups WHERE order_id = ?")
                .bind(&order_id)
                .fetch_one(&db.pool)
                .await
                .expect("read stamped expiry");
        assert!(
            expiry > Utc::now(),
            "the real path must stamp a FUTURE expiry, not a past one"
        );

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired, 0);
        assert_eq!(test_support::balance(&db.pool, account).await, 50_000);
        db.close().await;
    }

    #[tokio::test]
    async fn the_boundary_is_inclusive_so_a_deposit_expiring_this_instant_is_retired() {
        // `<=`, not `<`. A deposit expiring exactly now HAS expired; an exclusive
        // bound would leave it spendable for one more scheduler interval.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let order_id = fund_through_topup(&db.pool, account, 50_000).await;

        let now = Utc::now();
        age_the_deposit(&db.pool, &order_id, Some(now)).await;

        let sweep = expire_credit(&db.pool, now).await.expect("sweep");
        assert_eq!(
            sweep.expired, 1,
            "a deposit whose expiry IS the sweep instant has expired"
        );
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_is_idempotent_and_does_not_move_the_recorded_instant() {
        // The sweep runs nightly and will be re-run by hand after a failure, so
        // "already retired" has to be a no-op rather than a second debit. The
        // `credit_retired_at IS NULL` guard in both the candidate query and the
        // stamp is what makes that true.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let order_id = fund_through_topup(&db.pool, account, 50_000).await;
        let id = age_the_deposit(&db.pool, &order_id, None).await;

        let first = expire_credit(&db.pool, Utc::now())
            .await
            .expect("first sweep");
        assert_eq!(first.expired, 1);
        let stamped = credit_retired_at(&db.pool, id).await;

        let rows_after_first = usage_refs(&db.pool, account).await.len();

        let second = expire_credit(&db.pool, Utc::now())
            .await
            .expect("second sweep");
        assert_eq!(second.expired, 0, "a retired deposit is not a candidate");
        assert_eq!(second.expired_idr, 0);
        assert_eq!(
            credit_retired_at(&db.pool, id).await,
            stamped,
            "the recorded instant must not move on a re-run"
        );
        assert_eq!(
            usage_refs(&db.pool, account).await.len(),
            rows_after_first,
            "a re-run must append no second expiry row"
        );
        assert_eq!(test_support::balance(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn a_partly_spent_wallet_gives_up_its_oldest_credit_first() {
        // Two deposits of the same size, the OLDER one expiring now. The wallet
        // holds one un-aged balance, so the sweep can only choose what to take -
        // and the policy is per-deposit, which means the older deposit's credit is
        // the credit that has aged out.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;

        let older = fund_through_topup(&db.pool, account, 50_000).await;
        let older_id = age_the_deposit(&db.pool, &older, None).await;
        let newer = fund_through_topup_again(&db.pool, account, 50_000, 100_000).await;
        let newer_id = topup_id(&db.pool, &newer).await;
        // Pin the new deposit's expiry into the future explicitly, so this test
        // cannot pass by both deposits having aged out.
        sqlx::query("UPDATE topups SET credit_expires_at = ? WHERE id = ?")
            .bind(Utc::now() + chrono::Duration::days(30))
            .bind(newer_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("hold the newer deposit");

        // Spend 60,000 of the 100,000 through the ledger, as a usage settlement
        // would: 40,000 is left, and the aged deposit's own amount (50,000) no
        // longer fits inside it. The sweep must take 40,000 rather than either
        // going negative or reaching into the newer deposit.
        retire_balance(&db.pool, account, 60_000).await;

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired, 1, "only the aged deposit is retired");
        assert_eq!(
            sweep.expired_idr, 40_000,
            "the aged deposit gives up what is left, which no longer covers its \
             own amount - and it must not reach into the newer deposit"
        );

        assert!(
            credit_retired_at(&db.pool, older_id).await.is_some(),
            "the OLDEST deposit is the one retired"
        );
        assert_eq!(
            credit_retired_at(&db.pool, newer_id).await,
            None,
            "the un-aged deposit keeps its credit"
        );
        assert_eq!(
            test_support::balance(&db.pool, account).await,
            0,
            "40,000 of the aged credit was taken and the other 40,000 is the newer \
             deposit's, not this deposit's to give"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_retires_the_oldest_credit_even_when_its_id_sorts_last() {
        // THE ORDERING WAS NOT PINNED, and this test is the one that pins it.
        //
        // Every other expiry test ages the FIRST-INSERTED deposit, so insertion order and
        // age order coincide and `ORDER BY credit_expires_at, id` is indistinguishable
        // from `ORDER BY id`. Measured: mutating the candidate query's ordering to
        // `ORDER BY id` left the whole expiry suite GREEN.
        //
        // The shape that separates them needs the OLDER deposit to sort LAST by id. Then
        // the two orderings name different first candidates, and the sweep's choice is
        // observable:
        //
        //   ORDER BY credit_expires_at, id  ->  the older deposit  (correct)
        //   ORDER BY id                     ->  the newer deposit  (the mutation)
        //
        // `order_id` is what makes the ids differ, since `topups.id` is derived from it.
        // The wallet is then spent down to cover exactly one deposit, so which one is
        // retired is the only thing that can differ.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;

        // Inserted FIRST, so this one takes the alphabetically-EARLIER id...
        let newer = fund_through_topup(&db.pool, account, 50_000).await;
        // ...and inserted SECOND, so this takes the LATER id, while being the OLDER
        // deposit by expiry. `credit_expires_at` is what the ordering must honour.
        let older = fund_through_topup_again(&db.pool, account, 50_000, 100_000).await;

        let newer_id = topup_id(&db.pool, &newer).await;
        let older_id = topup_id(&db.pool, &older).await;

        // `topups.id` is a random UUID, so insertion order does NOT determine id order -
        // the first attempt at this fixture asserted that it did and failed, correctly.
        // Set the ids explicitly so the OLDER deposit sorts LAST, which is the whole point:
        // it is the only shape in which `ORDER BY credit_expires_at, id` and `ORDER BY id`
        // name different first candidates.
        let low = Uuid::from_u128(0x0000_0000_0000_0000_0000_0000_0000_0001);
        let high = Uuid::from_u128(0xffff_ffff_ffff_ffff_ffff_ffff_ffff_fffe);
        sqlx::query("UPDATE topups SET id = ? WHERE id = ?")
            .bind(low.hyphenated())
            .bind(newer_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("give the newer deposit the LOWER id");
        sqlx::query("UPDATE topups SET id = ? WHERE id = ?")
            .bind(high.hyphenated())
            .bind(older_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("give the older deposit the HIGHER id");
        let newer_id = low;
        let older_id = high;
        assert!(
            newer_id < older_id,
            "the fixture is only meaningful if the NEWER deposit's id sorts FIRST"
        );

        // Both expire, but at different instants: the one with the later id is the older
        // deposit. Both are in the past, so both are candidates.
        sqlx::query("UPDATE topups SET credit_expires_at = ? WHERE id = ?")
            .bind(Utc::now() - chrono::Duration::days(60))
            .bind(older_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("age the older deposit");
        sqlx::query("UPDATE topups SET credit_expires_at = ? WHERE id = ?")
            .bind(Utc::now() - chrono::Duration::days(1))
            .bind(newer_id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("age the newer deposit");

        // Leave only enough for ONE of them, so the sweep's choice is visible.
        retire_balance(&db.pool, account, 50_000).await;

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired_idr, 50_000, "the sweep takes what is left");
        assert_eq!(
            sweep.expired, 1,
            "only ONE retirement is an expiry EVENT: the deposit that gave up money"
        );

        assert!(
            credit_retired_at(&db.pool, older_id).await.is_some(),
            "the OLDER deposit must be retired first; if the LATER-id one was taken \
             instead, the candidate query is ordered by id rather than by expiry"
        );
        // The newer deposit is ALSO marked retired, with nothing taken - the documented
        // `retired <= 0` path at `expire_one_deposit` (db.rs:424-430), which marks a
        // deposit the sweep cannot pay out so it stops being revisited. That is not an
        // expiry EVENT and writes no ledger row, which is what `sweep.expired == 1`
        // above distinguishes. What matters for THIS test is that no MONEY moved for it.
        assert_eq!(
            test_support::balance(&db.pool, account).await,
            0,
            "exactly one deposit's worth was taken"
        );
        // The sweep's own ledger row is the one filed against the AGED deposit's id, so
        // this checks both that it exists and that it names the OLDER deposit - which is
        // the ordering claim itself. (`usage_refs` also returns the row `retire_balance`
        // wrote to spend the wallet down, which is why this filters by ref rather than
        // counting.)
        let refs = usage_refs(&db.pool, account).await;
        let expiry_refs: Vec<&String> = refs.iter().filter(|r| r.starts_with("expiry:")).collect();
        assert_eq!(
            expiry_refs.len(),
            1,
            "exactly ONE expiry row was written, so the second deposit gave up no money: {refs:?}"
        );
        assert_eq!(
            expiry_refs.first().map(|s| s.as_str()),
            Some(format!("expiry:{older_id}").as_str()),
            "the expiry row must name the OLDER deposit"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_cannot_take_more_than_the_wallet_holds() {
        // The wallet has been spent down below the aged deposit's amount. The
        // sweep takes what is there and marks the deposit retired - it must NOT
        // go negative, and it must not take a LATER deposit's credit to make up
        // the difference.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;

        let aged = fund_through_topup(&db.pool, account, 50_000).await;
        let aged_id = age_the_deposit(&db.pool, &aged, None).await;
        let held = fund_through_topup_again(&db.pool, account, 50_000, 100_000).await;

        // Empty the wallet through the ledger, as a spend would - so the aged
        // deposit has nothing left to retire and the un-aged one must not be
        // reached into to make up the difference.
        retire_balance(&db.pool, account, 100_000).await;

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");

        assert_eq!(
            sweep.expired, 0,
            "a deposit with nothing left to retire is not an expiry event"
        );
        assert_eq!(sweep.expired_idr, 0, "and retires no money");
        assert_eq!(
            test_support::balance(&db.pool, account).await,
            0,
            "the wallet must never go negative to satisfy an expiry"
        );
        assert_eq!(
            test_support::ledger_sum(&db.pool, account).await,
            0,
            "and it must not write a zero-delta ledger row for a no-op"
        );
        assert!(
            credit_retired_at(&db.pool, aged_id).await.is_some(),
            "the deposit is still marked retired, so the sweep stops revisiting it"
        );
        assert!(
            credit_retired_at(&db.pool, topup_id(&db.pool, &held).await)
                .await
                .is_none(),
            "the un-aged deposit is untouched"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_keeps_the_ledger_equal_to_the_wallet() {
        // The one invariant every money path is written to preserve, re-checked
        // after the sweep because the sweep is the newest path that debits. The
        // reconciliation query is the same FULL OUTER JOIN
        // `tools/reconcile/reconcile.sql` runs.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;

        // Three deposits, so the sweep has to keep the books straight across
        // several retirements in a row rather than over one lucky case. The
        // balance after each is the RUNNING total, which is what
        // `fund_through_topup_again` is for - the one-argument form asserts the
        // balance equals that single deposit, and would fail on the second call.
        let mut running = 0;
        for _ in 0..3 {
            running += 50_000;
            let order_id = fund_through_topup_again(&db.pool, account, 50_000, running).await;
            age_the_deposit(&db.pool, &order_id, None).await;
        }

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired, 3);
        assert_eq!(sweep.expired_idr, 150_000);

        assert_eq!(
            test_support::balance(&db.pool, account).await,
            test_support::ledger_sum(&db.pool, account).await,
            "wallets is a cache of the ledger and the sweep must not break that"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_survives_a_second_account_and_leaves_it_alone() {
        // Each deposit is retired in its own transaction, so one account's aged
        // credit cannot be affected by another's. This also pins that the
        // candidate query is account-agnostic (it sweeps the platform) while the
        // DEBIT is per account.
        let db = TestDb::new().await;
        let aged_account = test_support::account(&db.pool).await;
        let live_account = test_support::account(&db.pool).await;

        let aged = fund_through_topup(&db.pool, aged_account, 50_000).await;
        age_the_deposit(&db.pool, &aged, None).await;
        fund_through_topup(&db.pool, live_account, 70_000).await;

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired, 1);
        assert_eq!(sweep.expired_idr, 50_000);

        assert_eq!(test_support::balance(&db.pool, aged_account).await, 0);
        assert_eq!(
            test_support::balance(&db.pool, live_account).await,
            70_000,
            "the other account's balance is not swept up"
        );
        assert_eq!(ledger_drift_rows(&db.pool, live_account).await, 0);
        db.close().await;
    }

    #[tokio::test]
    async fn the_sweep_ignores_a_deposit_that_never_settled() {
        // Only settled deposits carry credit. A pending one has no
        // `credit_expires_at` at all and must not be a candidate - if it were, a
        // staff-side expiry could strike a deposit before the webhook that
        // legitimises it.
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        test_support::wallet(&db.pool, account).await;

        let pending = test_support::pending_topup(&db.pool, account, 50_000).await;
        let id = topup_id(&db.pool, &pending).await;
        sqlx::query("UPDATE topups SET credit_expires_at = ? WHERE id = ?")
            .bind(Utc::now() - chrono::Duration::days(1))
            .bind(id.hyphenated())
            .execute(&db.pool)
            .await
            .expect("age the unsettled deposit");

        let sweep = expire_credit(&db.pool, Utc::now()).await.expect("sweep");
        assert_eq!(sweep.expired, 0, "an unsettled deposit has no credit");
        assert_eq!(
            credit_retired_at(&db.pool, id).await,
            None,
            "and is not marked retired either"
        );
        assert_eq!(topup_status(&db.pool, &pending).await, "pending");
        db.close().await;
    }
}
