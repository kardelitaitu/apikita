use crate::error::AppError;
use chrono::Utc;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
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
    /// variant, mirroring `RefundResult::NotSettled`.
    NotSettleable { status: String },
}

/// The ONE ledger `ref` every money event of a topup is filed under.
///
/// docs/website/02-data-model.md:79 defines the column - "ref TEXT, -- topup id,
/// usage batch id, etc." - and the same document's credit transaction writes
/// `ref` as the topup id (lines 386-387). So a topup's ledger rows are keyed by
/// the TOPUP id, and the credit and the refund of one top-up must be filed under
/// the SAME value or no join can pair them and the audit trail cannot answer
/// "what happened to top-up X". Both write paths go through here so the
/// vocabulary cannot drift again (the refund used to write the Midtrans
/// `order_id` instead).
pub fn topup_ledger_ref(topup_id: Uuid) -> String {
    topup_id.to_string()
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
pub async fn credit_topup_transaction(
    pool: &SqlitePool,
    order_id: &str,
    webhook_amount_idr: i64,
) -> Result<TopupCreditResult, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 1. Settle the row in one statement. `status = 'pending'` and the amount
    //    check are the guard; `rows_affected()` decides whether it fired.
    let now = Utc::now();
    let settled = sqlx::query(
        "UPDATE topups SET status = 'settled', settled_at = ? \
         WHERE order_id = ? AND status = 'pending' AND amount_idr = ? \
         RETURNING id, account_id",
    )
    .bind(now)
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
    //    id (`topup_ledger_ref`), the same value the refund files under, so the
    //    credit and its reversal can be joined (docs/website/02-data-model.md:79).
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

/// What a refund attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum RefundResult {
    /// The wallet was debited and a `refund` ledger row appended.
    Refunded { new_balance: i64 },
    /// This order was already refunded - a replayed webhook. Nothing written.
    AlreadyRefunded,
    /// No such order.
    NotFound,
    /// The topup was never settled, so there is nothing to give back.
    NotSettled { status: String },
    /// The refund amount disagrees with the STORED `topups.amount_idr`. NOTHING
    /// was written.
    ///
    /// The exact mirror of `TopupCreditResult::AmountMismatch`: the payload is
    /// never trusted over our own row (docs/server/api-spec.md:284 - "compare
    /// amount against the stored row; mismatch -> reject", and :295 - "the amount
    /// comes from our stored row, never the payload").
    ///
    /// This is a refusal, not a refund. Without it the amount the caller sent was
    /// the amount debited, so a signed notification naming more than the top-up
    /// drained the wallet and left the row reading `refunded` for a figure it
    /// never held.
    AmountMismatch,
    /// The wallet cannot cover the refund: the money has already been spent.
    ///
    /// NOTHING was written - not the ledger, not the topup status - so the
    /// operator can see the topup still sitting in `settled` and resolve it by
    /// hand. Deliberately distinct from `Refunded`: a refund that cannot be
    /// applied is a real-world event, not a success.
    InsufficientBalance { balance_idr: i64, required_idr: i64 },
}

/// Whether a topup in `status` may be refunded, and if not, why.
#[derive(Debug, PartialEq, Eq)]
pub enum RefundDecision {
    Refund,
    AlreadyRefunded,
    NotSettled,
}

/// The pure decision behind the refund, split out so it is testable without a
/// database.
///
/// `refunded` is checked before `settled`: a second refund of the same order is
/// a replay, not a refund, and must not debit twice.
pub fn refund_decision(status: &str) -> RefundDecision {
    match status {
        "refunded" => RefundDecision::AlreadyRefunded,
        "settled" => RefundDecision::Refund,
        // `pending`, `denied`, `expired`: money never arrived, so there is
        // nothing to give back. Refunding these would create money.
        _ => RefundDecision::NotSettled,
    }
}

/// Atomically refunds a settled topup: debits the wallet and appends a `refund`
/// ledger row, in one transaction, so `balance_idr = SUM(delta_idr)` still holds.
///
/// The debit carries `balance_idr >= ?1` as a predicate on the UPDATE itself,
/// the same guard `debit_usage_transaction` uses: a concurrent request cannot race
/// the check, and `CHECK (balance_idr >= 0)` is the backstop rather than the thing
/// that refuses the debit (docs/decisions.md D3 - wallets are non-negative).
///
/// Idempotent under replay: the row is moved out of `settled` by a conditional
/// UPDATE, and only the statement that performs that move proceeds, so a second
/// refund of the same order is a no-op.
///
/// The amount is validated against the STORED `topups.amount_idr` and the debit
/// is taken from that stored value, never from the caller's figure - the same
/// rule the credit path applies (docs/server/api-spec.md:284, :295). A mismatch
/// is `AmountMismatch` with NOTHING written: the claim in step 1 is rolled back.
///
/// `webhook_amount_idr` is what the CALLER claims, not what the top-up was: the
/// parameter was named `stored_amount_idr` while carrying the webhook payload's
/// value, and the debit followed the name's promise instead of the value.
pub async fn refund_topup_transaction(
    pool: &SqlitePool,
    order_id: &str,
    webhook_amount_idr: i64,
) -> Result<RefundResult, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 1. Claim the refund by moving the row out of `settled`, in one conditional
    //    statement. The Postgres original locked the row with `FOR UPDATE` and
    //    then decided in Rust; SQLite has no row locks and rejects `FOR UPDATE`
    //    (measured), so the status transition IS the guard and the write lock it
    //    takes is what serializes two concurrent refunds.
    //
    //    The claim reads `amount_idr` back as well, because the amount that moves
    //    is OUR row's, never the payload's: the caller's figure is only ever
    //    compared against it.
    //
    //    Claiming before the amount check is safe because both happen in this one
    //    transaction: the amount-mismatch path and the insufficient-balance path
    //    below BOTH roll the claim back, leaving the topup `settled` exactly as
    //    the original did - nothing written, no money moved.
    let claimed = sqlx::query(
        "UPDATE topups SET status = 'refunded' WHERE order_id = ? AND status = 'settled' \
         RETURNING id, account_id, amount_idr",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(claimed) = claimed else {
        // 2. Nothing was written. One SELECT decides which refusal this is.
        let existing = sqlx::query("SELECT status FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;

        tx.rollback().await?;

        return Ok(match existing {
            None => RefundResult::NotFound,
            Some(row) => {
                let status: String = row.get("status");
                match refund_decision(&status) {
                    RefundDecision::AlreadyRefunded => RefundResult::AlreadyRefunded,
                    // `Refund` is unreachable: the claim above would have matched
                    // a `settled` row. Every other status had no money arrive, so
                    // there is nothing to give back.
                    RefundDecision::Refund | RefundDecision::NotSettled => {
                        RefundResult::NotSettled { status }
                    }
                }
            }
        });
    };

    let topup_id: Uuid = claimed.get::<Hyphenated, _>("id").into_uuid();
    let account_id: Uuid = claimed.get::<Hyphenated, _>("account_id").into_uuid();
    let stored_amount_idr: i64 = claimed.get("amount_idr");

    // 3. The amount must match the STORED row, exactly as the credit path
    //    requires (step 2 there). Checked AFTER the status decision - so a
    //    replayed refund still reads as a replay - and BEFORE the debit, so a
    //    mismatch leaves the topup `settled`, the wallet untouched and the ledger
    //    empty. The claim above is rolled back for exactly that reason.
    //
    //    `webhook_amount_idr` is what the CALLER claims, not what the top-up was:
    //    the parameter was named `stored_amount_idr` while carrying the webhook
    //    payload's value, and the debit followed the name's promise instead of the
    //    value. The debit below moves `stored_amount_idr` - the value read from
    //    our own row - and only after the two have been proved equal.
    if stored_amount_idr != webhook_amount_idr {
        tx.rollback().await?;
        return Ok(RefundResult::AmountMismatch);
    }

    // 4. Debit the wallet, by the STORED amount. The guard is inside the
    //    statement: when it matches no row the account cannot cover the refund,
    //    and nothing may be written. `?1` is referenced twice, as the debit and as
    //    the floor (measured working, with three binds for `?1 ?2`).
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
    )
    .bind(stored_amount_idr)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut *tx)
    .await?;

    let new_balance: i64 = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            // The decision was already made by the predicate; this read only fills
            // in the error detail, and failing it must not turn a visible refusal
            // into an opaque 500.
            let balance_idr: i64 =
                sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                    .bind(account_id.hyphenated())
                    .fetch_optional(&mut *tx)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);

            // Roll back explicitly: nothing is written - including the status
            // claim above - so the topup stays `settled` and the ledger gains no
            // row it cannot back.
            tx.rollback().await?;

            return Ok(RefundResult::InsufficientBalance {
                balance_idr,
                required_idr: stored_amount_idr,
            });
        }
    };

    // 5. Append the refund row. `delta_idr` is negative: the ledger sums to the
    //    balance, and a refund takes money out. The row is already `refunded` -
    //    the claim in step 1 is what moved it, and nothing here writes the status
    //    again.
    //
    //    The ref is the SAME topup id the credit wrote (topup_ledger_ref), not the
    //    Midtrans order id: one logical top-up must be selectable by one ref value,
    //    or the credit and its refund cannot be joined and the audit trail cannot
    //    answer "what happened to top-up X" (docs/website/02-data-model.md:79).
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'refund', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(-stored_amount_idr)
    .bind(topup_ledger_ref(topup_id))
    .bind(new_balance)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(RefundResult::Refunded { new_balance })
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
/// Split out from the SQL so the clamp is testable without a database, exactly as
/// `refund_decision` is. The rule is a clamp of the DEBIT, never of the balance:
/// the debit can be at most what the wallet holds, so the balance lands on 0 and
/// `CHECK (balance_idr >= 0)` is the backstop rather than the thing that refuses
/// the debit. Forcing the full debit would drive the balance negative, which
/// docs/decisions.md ratified as impossible.
///
/// A zero or negative balance debits nothing and the whole cost is shortfall: the
/// usage row is still written, because the tokens were genuinely consumed.
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
            // class of defect as a silent refund.
            return settle_partial_usage(
                tx,
                account_id,
                api_key_id,
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
        // The balance the release left: the charge below has not been taken yet.
        insert_ledger_row(
            &mut tx,
            account_id,
            release_delta,
            ref_batch,
            new_balance - charge_delta,
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
    let today = Utc::now().date_naive();
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

    tx.commit().await?;

    Ok(())
}

/// The wallet balance, or 0 when the account has no wallet row.
async fn read_balance(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
) -> Result<i64, AppError> {
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
/// docs/failover.md:138-144), or the settlement channel closed with no outcome.
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
    let shortfall_idr = cost_idr - debited_idr;

    // usage_daily carries the FULL cost: the tokens were consumed and the counters
    // drive the dashboard and the 30-day spend window (routes/keys.rs). The ledger
    // carries only what was actually taken, which is what keeps
    // balance_idr = SUM(ledger.delta_idr) true.
    record_usage(
        tx,
        account_id,
        api_key_id,
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

/// Verification query: confirms that wallet balance equals sum of ledger entries.
pub async fn verify_wallet_reconciliation(
    pool: &SqlitePool,
    account_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        r#"
        SELECT
            w.balance_idr AS wallet_balance,
            COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
        FROM wallets w
        LEFT JOIN ledger l ON l.account_id = w.account_id
        WHERE w.account_id = ?
        GROUP BY w.balance_idr
        "#,
    )
    .bind(account_id.hyphenated())
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => {
            let wallet_balance: i64 = r.get("wallet_balance");
            let ledger_sum: i64 = r.get("ledger_sum");
            Ok(wallet_balance == ledger_sum)
        }
        None => Err(AppError::NotFound("Wallet not found".into())),
    }
}

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

    /// Rows returned by the reconciliation check in docs/observability.md:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr).
    async fn ledger_drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = ?
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
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

        let credited = credit_topup_transaction(&pool, &order_id, opening_balance)
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

        credit_topup_transaction(&pool, &refill_order_id, opening_balance)
            .await
            .expect("refill the wallet");

        let settled_cost: i64 = 250;
        let outcome = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
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
            credit_topup_transaction(&pool, &order_id, RESERVATION)
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
            credit_topup_transaction(&pool, &refill, RESERVATION)
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

    /// A refund is only ever applied to a topup that actually settled, and only
    /// once. `refunded` is checked first, so a replayed webhook cannot debit
    /// twice; `pending`/`denied`/`expired` never had money, so refunding them
    /// would create money out of nothing.
    #[test]
    fn refund_decision_only_refunds_a_settled_topup_once() {
        assert_eq!(refund_decision("settled"), RefundDecision::Refund);

        // Replay: the second refund of the same order must not debit again.
        assert_eq!(refund_decision("refunded"), RefundDecision::AlreadyRefunded);

        // No money ever arrived for these.
        assert_eq!(refund_decision("pending"), RefundDecision::NotSettled);
        assert_eq!(refund_decision("denied"), RefundDecision::NotSettled);
        assert_eq!(refund_decision("expired"), RefundDecision::NotSettled);

        // An unknown status is refused rather than assumed refundable.
        assert_eq!(refund_decision("something-new"), RefundDecision::NotSettled);
    }

    /// The refund outcome must stay distinguishable: an operator has to be able
    /// to tell a completed refund from one the wallet could not cover, because
    /// the second leaves the topup `settled` and needs a human.
    #[test]
    fn refund_outcomes_are_distinct_and_carry_the_amounts() {
        assert_ne!(
            RefundResult::Refunded { new_balance: 0 },
            RefundResult::InsufficientBalance {
                balance_idr: 0,
                required_idr: 0
            }
        );
        assert_ne!(
            RefundResult::Refunded { new_balance: 1 },
            RefundResult::AlreadyRefunded
        );
        assert_ne!(
            RefundResult::NotFound,
            RefundResult::NotSettled {
                status: "pending".to_string()
            }
        );

        // An amount that disagrees with the stored row is its OWN outcome, not a
        // flavour of success: it is the refusal that replaced the unbounded debit.
        assert_ne!(
            RefundResult::AmountMismatch,
            RefundResult::Refunded { new_balance: 0 }
        );
        assert_ne!(RefundResult::AmountMismatch, RefundResult::AlreadyRefunded);
        assert_ne!(
            RefundResult::AmountMismatch,
            RefundResult::InsufficientBalance {
                balance_idr: 0,
                required_idr: 0
            }
        );

        // The refusal carries both figures, so the log and the operator can see
        // the shortfall without another query.
        let refusal = RefundResult::InsufficientBalance {
            balance_idr: 1000,
            required_idr: 50000,
        };
        match refusal {
            RefundResult::InsufficientBalance {
                balance_idr,
                required_idr,
            } => {
                assert_eq!(balance_idr, 1000);
                assert_eq!(required_idr, 50000);
            }
            other => panic!("wrong variant: {other:?}"),
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
            credit_topup_transaction(&pool, &order_id, RESERVATION)
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
    /// Every NOT NULL column is bound: the strict schema has no DEFAULT for `id`
    /// or `created_at`, so the Postgres `INSERT INTO topups (account_id, ...)`
    /// shape fails at runtime with a NOT NULL constraint error.
    async fn create_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64, order_id: &str) {
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, created_at)
             VALUES (?, ?, ?, ?, 'pending', ?)",
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
        test_support::wallet(pool, account_id).await;

        let order_id = test_support::pending_topup(pool, account_id, amount_idr).await;

        assert_eq!(
            credit_topup_transaction(pool, &order_id, amount_idr)
                .await
                .expect("credit the opening balance"),
            TopupCreditResult::Settled {
                new_balance: amount_idr
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
            credit_topup_transaction(&pool, &order_id, AMOUNT)
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
            credit_topup_transaction(&pool, &order_id, AMOUNT)
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
            credit_topup_transaction(&pool, &mismatch_order, OTHER - 1)
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
            credit_topup_transaction(&pool, &unknown, AMOUNT)
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

    /// The refund TRANSACTION, not just the pure decision: refunding a settled topup
    /// debits the wallet by the STORED amount and appends a refund row with a
    /// NEGATIVE delta; a replay does not debit twice; a topup that never settled is
    /// refused (refunding it would create money); and a refund the balance cannot
    /// cover writes NOTHING and leaves the topup settled for an operator. The
    /// reconciliation invariant is asserted after EVERY case.
    ///
    /// The fixture refunds the whole stored amount because that is the only amount
    /// a refund may move - `refund_topup_transaction` validates the caller's figure
    /// against `topups.amount_idr` and refuses a disagreement. This test used to
    /// refund 20_000 of a 50_000 top-up, which was only possible while the debit
    /// followed the caller instead of the row.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn refund_debits_once_refuses_unsettled_and_writes_nothing_when_short() {
        run_with_teardown(refund_assertions).await;
    }

    async fn refund_assertions(pool: SqlitePool, account_id: Uuid) {
        const TOPUP: i64 = 50_000;
        // The amount a refund moves IS the stored amount: anything else is
        // refused as `AmountMismatch` before a single write.
        const REFUND: i64 = TOPUP;

        let settled_order = fund_through_topup(&pool, account_id, TOPUP).await;

        // 1. A settled topup is refunded: the wallet is DEBITED and the ledger gains
        //    a NEGATIVE row under the SAME topup id the credit used, so the pair
        //    joins (docs/website/02-data-model.md:79).
        let settled_topup_id = topup_id(&pool, &settled_order).await;
        assert_eq!(
            refund_topup_transaction(&pool, &settled_order, REFUND)
                .await
                .expect("refunding a settled topup"),
            RefundResult::Refunded {
                new_balance: TOPUP - REFUND
            },
            "the refund must debit the wallet by the refunded amount"
        );
        assert_eq!(wallet_balance(&pool, account_id).await, TOPUP - REFUND);
        assert_eq!(
            ledger_rows(&pool, account_id, "refund").await,
            vec![(-REFUND, Some(settled_topup_id.to_string()))],
            "the refund must append ONE row with reason=refund and a negative delta"
        );
        assert_eq!(
            topup_status(&pool, &settled_order).await,
            "refunded",
            "a completed refund must mark the topup refunded"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a refund"
        );

        // 2. REPLAYED refund: a second webhook for the same order is a no-op. The
        //    check is on the status, so it cannot debit twice.
        assert_eq!(
            refund_topup_transaction(&pool, &settled_order, REFUND)
                .await
                .expect("a replayed refund is a recorded outcome, not an error"),
            RefundResult::AlreadyRefunded,
            "a replayed refund must not debit twice"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            TOPUP - REFUND,
            "a replayed refund must leave the balance alone"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "refund").await.len(),
            1,
            "a replayed refund must not append a second ledger row"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 3. A topup that was NEVER settled is refused. Its money never arrived, so
        //    a refund would take it from the customer's existing balance - money
        //    created out of nothing.
        let pending_order = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, 10_000, &pending_order).await;
        assert_eq!(
            refund_topup_transaction(&pool, &pending_order, 10_000)
                .await
                .expect("an unsettled topup is a recorded outcome, not an error"),
            RefundResult::NotSettled {
                status: "pending".to_string()
            },
            "refunding a topup that never settled must be refused"
        );
        assert_eq!(
            topup_status(&pool, &pending_order).await,
            "pending",
            "a refused refund must not touch the topup"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            TOPUP - REFUND,
            "a refused refund must not move the wallet"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "refund").await.len(),
            1,
            "a refused refund must not append a ledger row"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 4. A refund the balance cannot cover: the money has already been spent.
        //    NOTHING is written - not the ledger, not the topup status - so the topup
        //    stays visible as settled for a human, and the balance does not go negative
        //    (docs/decisions.md: "Overdraft - Not permitted").
        let short_order = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, 10_000, &short_order).await;
        assert_eq!(
            credit_topup_transaction(&pool, &short_order, 10_000)
                .await
                .expect("settle the topup to be refunded"),
            TopupCreditResult::Settled {
                new_balance: TOPUP - REFUND + 10_000
            }
        );

        // Spend the whole balance, so the refund has nothing to draw on.
        let key_id = test_support::api_key(&pool, account_id).await;
        assert_eq!(
            debit_usage_transaction(
                &pool,
                account_id,
                Some(key_id),
                100,
                0,
                50,
                TOPUP - REFUND + 10_000,
                Some("test_refund_drain"),
                0,
            )
            .await
            .expect("drain the wallet"),
            UsageSettlement::Settled { new_balance: 0 }
        );
        assert_eq!(wallet_balance(&pool, account_id).await, 0);

        let ledger_before = ledger_row_count(&pool, account_id).await;
        assert_eq!(
            refund_topup_transaction(&pool, &short_order, 10_000)
                .await
                .expect("an unaffordable refund is a recorded outcome, not an error"),
            RefundResult::InsufficientBalance {
                balance_idr: 0,
                required_idr: 10_000
            },
            "a refund the balance cannot cover must be reported, not forced"
        );
        assert_eq!(
            topup_status(&pool, &short_order).await,
            "settled",
            "an unaffordable refund must leave the topup settled, so an operator can see it"
        );
        assert_eq!(
            wallet_balance(&pool, account_id).await,
            0,
            "an unaffordable refund must not drive the balance negative"
        );
        assert_eq!(
            ledger_row_count(&pool, account_id).await,
            ledger_before,
            "an unaffordable refund must write nothing"
        );
        assert_eq!(
            ledger_rows(&pool, account_id, "refund").await.len(),
            1,
            "an unaffordable refund must not append a refund row"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after an unaffordable refund"
        );
    }

    /// ONE logical top-up must be selectable by ONE ledger ref.
    ///
    /// docs/website/02-data-model.md:79 defines the column as "topup id, usage
    /// batch id, etc." - the top-up's OWN id. The credit path wrote that id; the
    /// refund path wrote the Midtrans order_id instead, so the two rows for one
    /// top-up carried two different identities: no join paired them, and
    /// ledger_sum_for_ref could not answer "what happened to top-up X".
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn credit_and_refund_of_one_topup_share_one_ledger_ref() {
        run_with_teardown(one_ref_assertions).await;
    }

    async fn one_ref_assertions(pool: SqlitePool, account_id: Uuid) {
        const TOPUP: i64 = 50_000;
        // A refund moves the stored amount and nothing else, so the fixture's
        // refund equals the top-up it reverses.
        const REFUND: i64 = TOPUP;

        // Fund through the REAL credit path, then refund through the real refund
        // path: this is one logical money event, written by two transactions.
        let order_id = fund_through_topup(&pool, account_id, TOPUP).await;
        let stored_id = topup_id(&pool, &order_id).await;
        let topup_ref = stored_id.to_string();

        assert_eq!(
            refund_topup_transaction(&pool, &order_id, REFUND)
                .await
                .expect("refund the settled top-up"),
            RefundResult::Refunded {
                new_balance: TOPUP - REFUND
            },
            "the fixture must refund through the real refund path"
        );

        // The defect, stated as the property the audit trail needs: ONE ref value
        // selects the whole ledger history of this top-up - the credit AND the
        // refund, in the order they happened.
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &topup_ref).await,
            vec![
                ("topup".to_string(), TOPUP),
                ("refund".to_string(), -REFUND)
            ],
            "the credit and its refund must share ONE ref (the topup id), or no join can pair them"
        );

        // And the net move under that one ref is the top-up net of what was given
        // back - the question reconciliation asks about a top-up.
        assert_eq!(
            ledger_sum_for_ref(&pool, account_id, &topup_ref).await,
            TOPUP - REFUND,
            "one ref must net to what this top-up actually left in the wallet"
        );

        // The order id must NOT be a second identity for the same rows.
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &order_id).await,
            Vec::<(String, i64)>::new(),
            "the Midtrans order id must not be a second ref vocabulary for a top-up"
        );

        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);
    }

    /// A refund whose amount is NOT the stored one must be REFUSED, with nothing
    /// written. This is the refund mirror of the credit test's case 3 above.
    ///
    /// THE DEFECT: the refund took its amount from the webhook PAYLOAD and debited
    /// it without ever reading `topups.amount_idr`, so the only thing bounding the
    /// debit was `balance_idr >= ?`. A signed notification naming more than the
    /// top-up drained the whole wallet in one call, the topup still read `refunded`
    /// for a figure it never held, and the ledger agreed with the wallet - the row
    /// was written from the same unchecked value - so reconciliation saw nothing.
    ///
    /// The wallet here holds TWO top-ups, so the inflated refund is AFFORDABLE: the
    /// balance guard cannot be what saves us, which is the point.
    ///
    /// Runs by default against its own migrated SQLite database (port phase 5).
    #[tokio::test]
    async fn refund_rejects_an_amount_that_is_not_the_stored_one() {
        run_with_teardown(refund_amount_mismatch_assertions).await;
    }

    async fn refund_amount_mismatch_assertions(pool: SqlitePool, account_id: Uuid) {
        const TOPUP: i64 = 50_000;
        const INFLATED: i64 = 999_999;

        let order_id = fund_through_topup(&pool, account_id, TOPUP).await;
        let topup_ref = topup_id(&pool, &order_id).await.to_string();

        // A second, independent top-up: the wallet now holds 100_000, so the
        // inflated figure below is a refund the balance COULD cover. Written with
        // the raw helpers rather than `fund_through_topup`, which opens a
        // zero-balance wallet and would collide with the one just funded.
        let other_order = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, TOPUP, &other_order).await;
        assert_eq!(
            credit_topup_transaction(&pool, &other_order, TOPUP)
                .await
                .expect("settle the second top-up"),
            TopupCreditResult::Settled {
                new_balance: 2 * TOPUP
            }
        );
        assert_eq!(wallet_balance(&pool, account_id).await, 2 * TOPUP);
        let ledger_before = ledger_row_count(&pool, account_id).await;

        assert_eq!(
            refund_topup_transaction(&pool, &order_id, INFLATED)
                .await
                .expect("a mismatch is a recorded outcome, not an error"),
            RefundResult::AmountMismatch,
            "an amount that disagrees with the stored top-up must be refused"
        );

        assert_eq!(
            wallet_balance(&pool, account_id).await,
            2 * TOPUP,
            "a refused refund must not debit - not the payload amount, not the stored one"
        );
        assert_eq!(
            topup_status(&pool, &order_id).await,
            "settled",
            "a refused refund must not mark the top-up refunded"
        );
        assert_eq!(
            ledger_row_count(&pool, account_id).await,
            ledger_before,
            "a refused refund must append no ledger row"
        );
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &topup_ref).await,
            vec![("topup".to_string(), TOPUP)],
            "the only ledger row under this top-up must still be the credit"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // The STORED amount still refunds, and it debits exactly that - so the
        // refusal is about the amount, not about refusing refunds.
        assert_eq!(
            refund_topup_transaction(&pool, &order_id, TOPUP)
                .await
                .expect("refund the stored amount"),
            RefundResult::Refunded { new_balance: TOPUP },
            "the stored amount must still refund cleanly"
        );
        assert_eq!(wallet_balance(&pool, account_id).await, TOPUP);
        assert_eq!(topup_status(&pool, &order_id).await, "refunded");
        assert_eq!(
            ledger_rows_for_ref(&pool, account_id, &topup_ref).await,
            vec![("topup".to_string(), TOPUP), ("refund".to_string(), -TOPUP)]
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

    async fn reconciliation_assertions(pool: SqlitePool, account_id: Uuid) {
        const AMOUNT: i64 = 50_000;

        fund_through_topup(&pool, account_id, AMOUNT).await;

        // 1. A consistent fixture is clean.
        assert!(
            verify_wallet_reconciliation(&pool, account_id)
                .await
                .expect("verify a consistent wallet"),
            "a wallet whose balance is its ledger sum must verify clean"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 2. Manufacture drift the only way it can happen: a balance the ledger
        //    cannot explain. The checker must SEE it.
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr + 1 WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("manufacture drift");

        assert!(
            !verify_wallet_reconciliation(&pool, account_id)
                .await
                .expect("verify a drifted wallet"),
            "the checker must report a balance the ledger cannot explain: {}",
            drift_report(&pool, account_id).await
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            1,
            "the drift the checker reports must be the drift the sweep finds"
        );

        // 3. Restore, and the checker agrees again - so it is reading the data, not
        //    answering from a constant.
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr - 1 WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .execute(&pool)
            .await
            .expect("restore the balance");

        assert!(
            verify_wallet_reconciliation(&pool, account_id)
                .await
                .expect("verify the restored wallet"),
            "after restoring the balance the checker must report clean again"
        );
        assert_eq!(ledger_drift_rows(&pool, account_id).await, 0);

        // 4. No wallet at all is an error, not a silent "clean".
        let bare = bare_account(&pool).await;
        match verify_wallet_reconciliation(&pool, bare).await {
            Err(AppError::NotFound(_)) => {}
            other => panic!("a missing wallet must be NotFound, got {other:?}"),
        }
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
            debit_usage_transaction(&db.pool, account_id, None, 100, 10, 200, 5_000, None, 0)
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
    /// executing: settle → refund → the original SETTLEMENT webhook replays.
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
    #[tokio::test]
    async fn a_settlement_replayed_after_a_refund_is_refused_and_credits_nothing() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        const AMOUNT: i64 = 10_000;

        let order_id = test_support::pending_topup(&db.pool, account_id, AMOUNT).await;

        assert_eq!(
            credit_topup_transaction(&db.pool, &order_id, AMOUNT)
                .await
                .expect("settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            }
        );
        assert_eq!(test_support::balance(&db.pool, account_id).await, AMOUNT);

        assert!(
            matches!(
                refund_topup_transaction(&db.pool, &order_id, AMOUNT)
                    .await
                    .expect("refund"),
                RefundResult::Refunded { new_balance: 0 }
            ),
            "a settled topup must refund"
        );
        assert_eq!(test_support::balance(&db.pool, account_id).await, 0);
        assert_eq!(
            test_support::ledger_sum(&db.pool, account_id).await,
            0,
            "a settle followed by a refund must net to zero, not to a credit"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);

        // The replayed SETTLEMENT. This is the defect: before the fix this
        // credited AMOUNT again and flipped the row back to `settled`.
        let replay = credit_topup_transaction(&db.pool, &order_id, AMOUNT)
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

    /// REGRESSION for plan section 4.3, trap 2, and the real concurrency test
    /// section 9 check 4 still owed.
    ///
    /// A deferred `BEGIN` that reads and then writes can be refused at the lock
    /// upgrade with `SQLITE_BUSY_SNAPSHOT` — an error that **cannot be resolved by
    /// retrying**, because the transaction has to be rolled back and restarted. It
    /// appears only under contention, so no test that opens one transaction at a
    /// time can see it.
    ///
    /// Every transaction here takes the write lock up front through
    /// `begin_immediate`, so five concurrent refunds serialize instead of
    /// deadlocking on an upgrade: exactly one refunds, the rest observe the row
    /// already refunded, and **none returns an error**. That last clause is the
    /// assertion — an `Err` here would mean either the unrecoverable upgrade or a
    /// `database is locked` once the bounded `busy_timeout` wait ran out.
    #[tokio::test]
    async fn concurrent_refunds_serialize_without_losing_the_write_lock() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        const AMOUNT: i64 = 10_000;
        const CONCURRENCY: usize = 5;

        let order_id = test_support::pending_topup(&db.pool, account_id, AMOUNT).await;
        assert_eq!(
            credit_topup_transaction(&db.pool, &order_id, AMOUNT)
                .await
                .expect("settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            }
        );

        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for _ in 0..CONCURRENCY {
            let pool = db.pool.clone();
            let order_id = order_id.clone();
            tasks.push(tokio::spawn(async move {
                refund_topup_transaction(&pool, &order_id, AMOUNT).await
            }));
        }

        let mut refunded = 0;
        let mut already = 0;
        for task in tasks {
            // This `expect` IS the check: a refund must either win the lock or see
            // the row already refunded. An error means the port lost the write lock.
            match task
                .await
                .expect("a refund task panicked")
                .expect("a refund must never error under contention")
            {
                RefundResult::Refunded { .. } => refunded += 1,
                RefundResult::AlreadyRefunded => already += 1,
                other => panic!("unexpected refund outcome: {other:?}"),
            }
        }

        assert_eq!(
            refunded, 1,
            "exactly one of five concurrent refunds may debit the wallet"
        );
        assert_eq!(
            already,
            CONCURRENCY - 1,
            "the rest must see the row already refunded"
        );

        assert_eq!(
            test_support::balance(&db.pool, account_id).await,
            0,
            "the refund must happen exactly once"
        );
        assert_eq!(
            ledger_drift_rows(&db.pool, account_id).await,
            0,
            "{}",
            drift_report(&db.pool, account_id).await
        );

        db.close().await;
    }
}
