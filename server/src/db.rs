use crate::error::AppError;
use chrono::Utc;
use sqlx::{PgPool, Postgres, Row, Transaction};
use tracing::error;
use uuid::Uuid;

pub async fn init_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(database_url)
        .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum TopupCreditResult {
    Settled { new_balance: i64 },
    AlreadySettled,
    NotFound,
    AmountMismatch,
}

/// Atomically settles a topup and credits the wallet, recording an append-only ledger row.
pub async fn credit_topup_transaction(
    pool: &PgPool,
    order_id: &str,
    webhook_amount_idr: i64,
) -> Result<TopupCreditResult, AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

    // 1. Lock the topups row
    let topup = sqlx::query(
        "SELECT id, account_id, amount_idr, status FROM topups WHERE order_id = $1 FOR UPDATE",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;

    let topup = match topup {
        Some(t) => t,
        None => return Ok(TopupCreditResult::NotFound),
    };

    let topup_id: Uuid = topup.get("id");
    let account_id: Uuid = topup.get("account_id");
    let amount_idr: i64 = topup.get("amount_idr");
    let status: String = topup.get("status");

    // 2. Check idempotency
    if status == "settled" {
        return Ok(TopupCreditResult::AlreadySettled);
    }

    // 3. Amount must match stored row
    if amount_idr != webhook_amount_idr {
        return Ok(TopupCreditResult::AmountMismatch);
    }

    // 4. Update topups row
    sqlx::query("UPDATE topups SET status = 'settled', settled_at = now() WHERE id = $1")
        .bind(topup_id)
        .execute(&mut *tx)
        .await?;

    // 5. Update wallet balance
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + $1, updated_at = now() WHERE account_id = $2 RETURNING balance_idr",
    )
    .bind(amount_idr)
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await?;

    let new_balance: i64 = wallet.get("balance_idr");

    // 6. Append to ledger
    let ref_str = topup_id.to_string();
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES ($1, $2, 'topup', $3, $4, now())",
    )
    .bind(account_id)
    .bind(amount_idr)
    .bind(ref_str)
    .bind(new_balance)
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
/// The debit carries `balance_idr >= $amount` as a predicate on the UPDATE itself,
/// the same guard `debit_usage_transaction` uses: a concurrent request cannot race
/// the check, and `CHECK (balance_idr >= 0)` is the backstop rather than the thing
/// that refuses the debit (docs/decisions.md D3 - wallets are non-negative).
///
/// Idempotent under replay: the topup row is locked `FOR UPDATE` and its status
/// decides, so a second refund of the same order is a no-op.
pub async fn refund_topup_transaction(
    pool: &PgPool,
    order_id: &str,
    amount_idr: i64,
) -> Result<RefundResult, AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

    // 1. Lock the topups row, so two concurrent refunds cannot both pass the
    //    status check below.
    let topup =
        sqlx::query("SELECT id, account_id, status FROM topups WHERE order_id = $1 FOR UPDATE")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;

    let Some(topup) = topup else {
        return Ok(RefundResult::NotFound);
    };

    let topup_id: Uuid = topup.get("id");
    let account_id: Uuid = topup.get("account_id");
    let status: String = topup.get("status");

    match refund_decision(&status) {
        RefundDecision::AlreadyRefunded => return Ok(RefundResult::AlreadyRefunded),
        RefundDecision::NotSettled => return Ok(RefundResult::NotSettled { status }),
        RefundDecision::Refund => {}
    }

    // 2. Debit the wallet. The guard is inside the statement: when it matches no
    //    row the account cannot cover the refund, and nothing may be written.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - $1, updated_at = now() WHERE account_id = $2 AND balance_idr >= $1 RETURNING balance_idr",
    )
    .bind(amount_idr)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;

    let new_balance: i64 = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            // The decision was already made by the predicate; this read only fills
            // in the error detail, and failing it must not turn a visible refusal
            // into an opaque 500.
            let balance_idr: i64 =
                sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);

            // Roll back explicitly: nothing is written, so the topup stays
            // `settled` and the ledger gains no row it cannot back.
            tx.rollback().await?;

            return Ok(RefundResult::InsufficientBalance {
                balance_idr,
                required_idr: amount_idr,
            });
        }
    };

    // 3. Mark the topup refunded.
    sqlx::query("UPDATE topups SET status = 'refunded' WHERE id = $1")
        .bind(topup_id)
        .execute(&mut *tx)
        .await?;

    // 4. Append the refund row. `delta_idr` is negative: the ledger sums to the
    //    balance, and a refund takes money out.
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES ($1, $2, 'refund', $3, $4, now())",
    )
    .bind(account_id)
    .bind(-amount_idr)
    .bind(order_id)
    .bind(new_balance)
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
    pool: &PgPool,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
    reserved_idr: i64,
) -> Result<UsageSettlement, AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

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
    // `balance_idr >= $1` is the guard and it lives inside the statement, not in a
    // preceding read: when a concurrent transaction has already updated the row,
    // Postgres re-evaluates the predicate against the latest row version under the
    // row lock, so two racing debits cannot both pass against one stale balance.
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
    mut tx: Transaction<'_, Postgres>,
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

    // Upsert usage_daily
    let today = Utc::now().date_naive();
    sqlx::query(
        r#"
        INSERT INTO usage_daily (
            account_id, api_key_id, day,
            input_tokens, cache_read_tokens, output_tokens, cost_idr
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (account_id, api_key_id, day) DO UPDATE
        SET input_tokens = usage_daily.input_tokens + EXCLUDED.input_tokens,
            cache_read_tokens = usage_daily.cache_read_tokens + EXCLUDED.cache_read_tokens,
            output_tokens = usage_daily.output_tokens + EXCLUDED.output_tokens,
            cost_idr = usage_daily.cost_idr + EXCLUDED.cost_idr
        "#,
    )
    .bind(account_id)
    .bind(api_key_id)
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
    tx: &mut Transaction<'_, Postgres>,
    account_id: Uuid,
) -> Result<i64, AppError> {
    // Annotated, not inferred: `unwrap_or(0)` alone would leave the scalar type to
    // default to i32, which Postgres decodes as INT4 and rejects against BIGINT.
    let balance: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
        .bind(account_id)
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
    tx: &mut Transaction<'_, Postgres>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - $1, updated_at = now() WHERE account_id = $2 AND balance_idr >= $1 RETURNING balance_idr",
    )
    .bind(amount)
    .bind(account_id)
    .fetch_optional(&mut **tx)
    .await?;

    Ok(wallet.map(|w| w.get("balance_idr")))
}

/// Appends one append-only ledger row. `balance_after` is the wallet balance the
/// row leaves behind, which is what makes the ledger self-explaining after a
/// crash: the running balance can be replayed without the wallet row.
async fn insert_ledger_row(
    tx: &mut Transaction<'_, Postgres>,
    account_id: Uuid,
    delta_idr: i64,
    ref_batch: Option<&str>,
    balance_after: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES ($1, $2, 'usage', $3, $4, now())",
    )
    .bind(account_id)
    .bind(delta_idr)
    .bind(ref_batch)
    .bind(balance_after)
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
    tx: &mut Transaction<'_, Postgres>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + $1, updated_at = now() WHERE account_id = $2 RETURNING balance_idr",
    )
    .bind(amount)
    .bind(account_id)
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
/// `balance_idr >= $amount` is a predicate on the UPDATE, and Postgres re-evaluates
/// it against the latest row version under the row lock, so exactly as many
/// concurrent requests as the balance can pay for are admitted and the rest match
/// no row. Concurrency is serialized by the database, not by a read.
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
    pool: &PgPool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<ReservationResult, AppError> {
    if reserved_idr <= 0 {
        return Ok(ReservationResult::Zero);
    }

    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

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
    pool: &PgPool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<Option<i64>, AppError> {
    if reserved_idr <= 0 {
        return Ok(None);
    }

    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

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
    mut tx: Transaction<'_, Postgres>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows returned by the reconciliation check in docs/observability.md:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr).
    async fn ledger_drift_rows(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = $1
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// Everything an operator needs to see when reconciliation fails: the wallet
    /// balance, the ledger sum, and every ledger row that produced it. A bare
    /// "drift" count says money is wrong but not which row is missing.
    async fn drift_report(pool: &PgPool, account_id: Uuid) -> String {
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(pool)
                .await
                .expect("read balance");

        let ledger_sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0)::bigint FROM ledger WHERE account_id = $1",
        )
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("sum ledger");

        let rows: Vec<(i64, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT delta_idr, reason, balance_after, ref FROM ledger WHERE account_id = $1 ORDER BY id",
        )
        .bind(account_id)
        .fetch_all(pool)
        .await
        .expect("read ledger rows");

        format!("balance_idr={balance} ledger_sum={ledger_sum} rows={rows:?}")
    }

    /// Deletes every row a fixture created, in FK order (`ledger` and `wallets`
    /// are ON DELETE RESTRICT).
    ///
    /// A leftover wallet with no matching ledger row is not merely untidy: it is
    /// permanent drift in a database other runs share, and it makes the next run
    /// fail for a reason that has nothing to do with the code under test.
    async fn delete_fixture_rows(pool: &PgPool, account_id: Uuid) {
        for statement in [
            "DELETE FROM usage_daily WHERE account_id = $1",
            "DELETE FROM ledger WHERE account_id = $1",
            "DELETE FROM api_keys WHERE account_id = $1",
            "DELETE FROM topups WHERE account_id = $1",
            "DELETE FROM wallets WHERE account_id = $1",
            "DELETE FROM accounts WHERE id = $1",
        ] {
            sqlx::query(statement)
                .bind(account_id)
                .execute(pool)
                .await
                .unwrap_or_else(|err| panic!("cleanup failed on `{statement}`: {err}"));
        }
    }

    /// A debit the wallet cannot cover must NOT be discarded: the reported usage
    /// is recorded and the debit is clamped to the balance, so reconciliation
    /// still holds and the shortfall is visible. A debit it CAN cover still
    /// settles in full.
    ///
    /// Every reconciliation assertion is scoped to THIS fixture's `account_id`, not
    /// to the whole database: this schema is shared with other tests and fixtures,
    /// and a global drift check fails for concurrent writers rather than for the code
    /// under test.
    ///
    /// This needs a live, migrated Postgres, so it is #[ignore]d rather than
    /// silently skipped or rewritten to assert nothing: run it with
    /// `DATABASE_URL=... cargo test --lib -- --ignored`. The pure clamp rule it
    /// depends on is unit-tested below without a database.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn overdraft_debit_is_clamped_to_the_balance_and_records_the_usage() {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");

        let pool = init_pool(&database_url).await.expect("connect to Postgres");

        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(&pool)
                .await
                .expect("create account");

        // The assertions run in their own task so a panicking one still reaches the
        // cleanup below. Tokio turns a task panic into a JoinError instead of
        // unwinding through this frame, which is what makes the teardown
        // unconditional.
        let assertions = tokio::spawn(overdraft_settlement_assertions(pool.clone(), account_id));
        let outcome = assertions.await;

        delete_fixture_rows(&pool, account_id).await;

        outcome.expect("the settlement assertions panicked");
    }

    /// The body of the live test, minus the fixture it is handed and the teardown
    /// its caller owns.
    async fn overdraft_settlement_assertions(pool: PgPool, account_id: Uuid) {
        let opening_balance: i64 = 1_000;

        // The fixture opens the wallet exactly the way production does, in two steps:
        // the zero-balance row the login path creates (routes/auth.rs), then a real
        // top-up. Money only ever enters a wallet through `credit_topup_transaction`,
        // which writes the matching `+` ledger row in the same transaction. Seeding
        // `wallets.balance_idr` directly manufactures the very drift this test then
        // asserts against - a fixture that cannot pass while the code under test is
        // correct. A zero-balance wallet with no ledger rows is consistent on its own
        // (0 = SUM of nothing), so this starting point reconciles.
        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("create the zero-balance wallet the login path would create");

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(opening_balance)
            .bind(&order_id)
            .execute(&pool)
            .await
            .expect("create topup");

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
        let key_id: Uuid = sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES ($1, $2, 'apk_test') RETURNING id",
        )
        .bind(account_id)
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(&pool)
        .await
        .expect("create api key");

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
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
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
            "SELECT delta_idr FROM ledger WHERE account_id = $1 AND reason = 'usage'",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("the clamped debit must still append a ledger row");
        assert_eq!(
            ledger_delta, -opening_balance,
            "the ledger must record exactly what was debited, not the full cost"
        );

        let (input, cache_read, output, usage_cost): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr FROM usage_daily WHERE account_id = $1",
        )
        .bind(account_id)
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
        let refill_order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(opening_balance)
            .bind(&refill_order_id)
            .execute(&pool)
            .await
            .expect("create refill topup");

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
    /// Needs a live, migrated Postgres, so it is #[ignore]d:
    /// `DATABASE_URL=... cargo test --lib -- --ignored`.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn concurrent_requests_cannot_overdraw_a_one_request_balance() {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        let pool = init_pool(&database_url).await.expect("connect to Postgres");

        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(&pool)
                .await
                .expect("create account");

        let assertions = tokio::spawn(overdraw_concurrency_assertions(pool.clone(), account_id));
        let outcome = assertions.await;
        delete_fixture_rows(&pool, account_id).await;
        outcome.expect("the concurrency assertions panicked");
    }

    /// The body of the live concurrency test, minus the fixture and teardown its
    /// caller owns.
    async fn overdraw_concurrency_assertions(pool: PgPool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const CONCURRENCY: usize = 5;

        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("create the zero-balance wallet the login path would create");

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(RESERVATION)
            .bind(&order_id)
            .execute(&pool)
            .await
            .expect("create topup");

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

        assert_eq!(held, 1, "exactly one request may be funded by a one-request balance");
        assert_eq!(refused, CONCURRENCY - 1, "the rest must be refused");

        let balance: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("read balance");
        assert_eq!(balance, 0, "the single hold consumed the whole balance");

        let held_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = $1 AND delta_idr < 0")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("count holds");
        assert_eq!(held_rows, 1, "a refused reservation must write no ledger row");
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after concurrent holds"
        );

        // usage_daily.api_key_id is part of the primary key, so a real key row is
        // needed before any usage can be recorded.
        let key_id: Uuid = sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES ($1, $2, 'apk_test') RETURNING id",
        )
        .bind(account_id)
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(&pool)
        .await
        .expect("create api key");

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
            "SELECT COALESCE(SUM(delta_idr), 0)::bigint FROM ledger WHERE account_id = $1 AND delta_idr > 0 AND reason = 'usage'",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("sum release rows");
        assert_eq!(release_row, RESERVATION, "the release must write a +hold ledger row");

        // Now the same path repeatedly, asserting the invariant after EVERY
        // settlement rather than only at the end. A drift that appears mid-run and
        // is later masked is the failure mode this catches.
        for round in 0..5 {
            let refill = format!("test_topup_refill_{round}");
            sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
                .bind(account_id)
                .bind(RESERVATION)
                .bind(&refill)
                .execute(&pool)
                .await
                .expect("create refill topup");
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
    /// without a database: the SQL path itself (the guarded UPDATE, the ledger
    /// insert, the usage_daily upsert) is NOT unit-testable without a live
    /// migrated Postgres, and is covered by the #[ignore]d test above.
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
            ReservationResult::Held { reserved_idr: 0, new_balance: 0 },
            ReservationResult::Insufficient { balance_idr: 0 }
        );
        assert_ne!(ReservationResult::Zero, ReservationResult::Insufficient { balance_idr: 0 });

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
    ///
    /// Needs a live, migrated Postgres - run with
    /// `DATABASE_URL=... cargo test --lib -- --ignored`.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn a_failed_or_paired_settlement_never_strands_the_hold() {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        let pool = init_pool(&database_url).await.expect("connect to Postgres");

        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(&pool)
                .await
                .expect("create account");

        let assertions =
            tokio::spawn(hold_never_strands_assertions(pool.clone(), account_id));
        let outcome = assertions.await;
        delete_fixture_rows(&pool, account_id).await;
        outcome.expect("the stranded-hold assertions panicked");
    }

    /// The body of the live regression test, minus the fixture and teardown its
    /// caller owns.
    async fn hold_never_strands_assertions(pool: PgPool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const COST: i64 = 250;

        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("create the zero-balance wallet");

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(RESERVATION)
            .bind(&order_id)
            .execute(&pool)
            .await
            .expect("create topup");
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, RESERVATION)
                .await
                .expect("fund the wallet"),
            TopupCreditResult::Settled {
                new_balance: RESERVATION
            },
            "fund the wallet through the real top-up path"
        );

        let key_id: Uuid = sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES ($1, $2, 'apk_test') RETURNING id",
        )
        .bind(account_id)
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(&pool)
        .await
        .expect("create api key");

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
            unpaired_hold_rows(&pool, account_id).await.expect("detect before release"),
            1,
            "a held reservation with no release must be reported as unpaired"
        );

        // Simulate the failed-settlement arm: proxy.rs calls release_quietly, which
        // calls exactly this. The balance must return to its pre-hold value.
        let pre_hold: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("read pre-release balance");
        release_reservation_transaction(&pool, account_id, RESERVATION, Some(&failed_ref))
            .await
            .expect("the failed settlement releases the hold");
        let after_release: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("read post-release balance");
        assert_eq!(
            after_release, pre_hold + RESERVATION,
            "FINDING 1/2: a failed settlement must return the whole hold to the wallet"
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect after release"),
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
            unpaired_hold_rows(&pool, account_id).await.expect("detect before settle"),
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
        assert_eq!(settled, UsageSettlement::Settled { new_balance: RESERVATION - COST });
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect after settle"),
            0,
            "FINDING 3: a paired settlement must not leave a stranded hold"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a paired settlement"
        );
    }

    // =====================================================================
    // Live-database tests for the money-moving transactions that had none:
    // the four documented outcomes of credit_topup_transaction,
    // refund_topup_transaction, release_reservation_transaction,
    // verify_wallet_reconciliation and unpaired_hold_rows.
    //
    // FIXTURE RULE (the one this repo has broken repeatedly): every wallet is
    // opened through the REAL path - the zero-balance row the login path creates,
    // then credit_topup_transaction, which writes the matching +ledger row in the
    // same transaction. Writing wallets.balance_idr directly manufactures the very
    // drift these tests then assert against, i.e. a fixture that cannot pass while
    // the code under test is correct. Teardown is delete_fixture_rows in FK order,
    // and it runs whether the assertions pass or panic.
    // =====================================================================

    async fn live_pool() -> PgPool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        init_pool(&database_url).await.expect("connect to Postgres")
    }

    /// Runs the assertions against a fresh account, then deletes the fixture in FK
    /// order whether it passed or panicked. The future is spawned, so a panic
    /// inside it arrives as a JoinError rather than unwinding through the teardown
    /// - which is what makes the cleanup unconditional.
    async fn run_with_teardown<F, Fut>(pool: PgPool, assertions: F)
    where
        F: FnOnce(PgPool, Uuid) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(&pool)
                .await
                .expect("create account");

        let outcome = tokio::spawn(assertions(pool.clone(), account_id)).await;

        delete_fixture_rows(&pool, account_id).await;

        outcome.expect("the live assertions panicked");
    }

    /// A second, wallet-less account for the "no wallet row" arms. Callers own its
    /// teardown.
    async fn bare_account(pool: &PgPool) -> Uuid {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
            .bind(&pb_user_id)
            .fetch_one(pool)
            .await
            .expect("create account")
    }

    /// The zero-balance wallet the login path creates (routes/auth.rs).
    async fn open_zero_balance_wallet(pool: &PgPool, account_id: Uuid) {
        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(pool)
            .await
            .expect("create the zero-balance wallet the login path would create");
    }

    async fn create_topup(pool: &PgPool, account_id: Uuid, amount_idr: i64, order_id: &str) {
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(amount_idr)
            .bind(order_id)
            .execute(pool)
            .await
            .expect("create topup");
    }

    /// Funds the wallet through the real path and returns the order id that did it.
    async fn fund_through_topup(pool: &PgPool, account_id: Uuid, amount_idr: i64) -> String {
        open_zero_balance_wallet(pool, account_id).await;

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(pool, account_id, amount_idr, &order_id).await;

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

    async fn create_api_key(pool: &PgPool, account_id: Uuid) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO api_keys (account_id, key_hash, prefix) VALUES ($1, $2, 'apk_test') RETURNING id",
        )
        .bind(account_id)
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .fetch_one(pool)
        .await
        .expect("create api key")
    }

    async fn wallet_balance(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(pool)
            .await
            .expect("read balance")
    }

    async fn topup_id(pool: &PgPool, order_id: &str) -> Uuid {
        sqlx::query_scalar("SELECT id FROM topups WHERE order_id = $1")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup id")
    }

    async fn topup_status(pool: &PgPool, order_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM topups WHERE order_id = $1")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup status")
    }

    /// Every ledger row for the account with that reason, oldest first, as
    /// (delta_idr, ref). Ordering by id keeps the assertion about the append order,
    /// not about whatever the planner returns.
    async fn ledger_rows(
        pool: &PgPool,
        account_id: Uuid,
        reason: &str,
    ) -> Vec<(i64, Option<String>)> {
        sqlx::query_as(
            "SELECT delta_idr, ref FROM ledger WHERE account_id = $1 AND reason = $2 ORDER BY id",
        )
        .bind(account_id)
        .bind(reason)
        .fetch_all(pool)
        .await
        .expect("read ledger rows")
    }

    async fn ledger_row_count(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(pool)
            .await
            .expect("count ledger rows")
    }

    /// The net ledger move under one ref. A hold and its release must sum to zero.
    async fn ledger_sum_for_ref(pool: &PgPool, account_id: Uuid, reference: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0)::bigint FROM ledger WHERE account_id = $1 AND ref = $2",
        )
        .bind(account_id)
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
    /// Needs a live, migrated Postgres - DATABASE_URL=... cargo test --lib -- --ignored.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn credit_topup_settles_replays_and_refuses_bad_input() {
        run_with_teardown(live_pool().await, credit_topup_assertions).await;
    }

    async fn credit_topup_assertions(pool: PgPool, account_id: Uuid) {
        const AMOUNT: i64 = 50_000;
        const OTHER: i64 = 10_000;

        open_zero_balance_wallet(&pool, account_id).await;

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        create_topup(&pool, account_id, AMOUNT, &order_id).await;
        let stored_id = topup_id(&pool, &order_id).await;

        // 1. A fresh credit moves the wallet and writes exactly ONE +ledger row.
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, AMOUNT)
                .await
                .expect("a fresh top-up must settle"),
            TopupCreditResult::Settled { new_balance: AMOUNT },
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
    /// debits the wallet by the amount and appends a refund row with a NEGATIVE
    /// delta; a replay does not debit twice; a topup that never settled is refused
    /// (refunding it would create money); and a refund the balance cannot cover
    /// writes NOTHING and leaves the topup settled for an operator. The reconciliation
    /// invariant is asserted after EVERY case.
    ///
    /// Needs a live, migrated Postgres - DATABASE_URL=... cargo test --lib -- --ignored.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn refund_debits_once_refuses_unsettled_and_writes_nothing_when_short() {
        run_with_teardown(live_pool().await, refund_assertions).await;
    }

    async fn refund_assertions(pool: PgPool, account_id: Uuid) {
        const TOPUP: i64 = 50_000;
        const REFUND: i64 = 20_000;

        let settled_order = fund_through_topup(&pool, account_id, TOPUP).await;

        // 1. A settled topup is refunded: the wallet is DEBITED and the ledger gains
        //    a NEGATIVE row under the order id.
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
            vec![(-REFUND, Some(settled_order.clone()))],
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
        let key_id = create_api_key(&pool, account_id).await;
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

    /// release_reservation_transaction had no direct test. Releasing a hold must
    /// credit the wallet by exactly the held amount and append a matching POSITIVE
    /// row under the SAME reserve_% ref, so the pair nets to zero, the stranded-hold
    /// detector returns 0, and the wallet is back where it started.
    ///
    /// Needs a live, migrated Postgres - DATABASE_URL=... cargo test --lib -- --ignored.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn release_reservation_returns_the_hold_and_pairs_the_ledger() {
        run_with_teardown(live_pool().await, release_reservation_assertions).await;
    }

    async fn release_reservation_assertions(pool: PgPool, account_id: Uuid) {
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
        delete_fixture_rows(&pool, bare).await;
    }

    /// verify_wallet_reconciliation had no direct test, and a checker that always
    /// returns true is worse than none: the property under test is its ability to
    /// DETECT. A consistent fixture reports clean; a deliberate direct UPDATE of
    /// balance_idr (the one thing production never does) is REPORTED as drift;
    /// restoring it reports clean again.
    ///
    /// Needs a live, migrated Postgres - DATABASE_URL=... cargo test --lib -- --ignored.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn reconciliation_reports_drift_instead_of_always_passing() {
        run_with_teardown(live_pool().await, reconciliation_assertions).await;
    }

    async fn reconciliation_assertions(pool: PgPool, account_id: Uuid) {
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
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr + 1 WHERE account_id = $1")
            .bind(account_id)
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
        sqlx::query("UPDATE wallets SET balance_idr = balance_idr - 1 WHERE account_id = $1")
            .bind(account_id)
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
        delete_fixture_rows(&pool, bare).await;
    }

    /// unpaired_hold_rows: a hold with no matching release is money that left the
    /// wallet and came back nowhere, so it must be COUNTED; a matched pair and a
    /// clean account must both be zero. This is the detector the hold sweep and the
    /// operator rely on, so a detector that never fires is the failure mode.
    ///
    /// Needs a live, migrated Postgres - DATABASE_URL=... cargo test --lib -- --ignored.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn unpaired_hold_rows_counts_a_stranded_hold_and_clears_a_matched_one() {
        run_with_teardown(live_pool().await, unpaired_hold_assertions).await;
    }

    async fn unpaired_hold_assertions(pool: PgPool, account_id: Uuid) {
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
}

/// Verification query: confirms that wallet balance equals sum of ledger entries.
pub async fn verify_wallet_reconciliation(
    pool: &PgPool,
    account_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        r#"
        SELECT
            w.balance_idr AS wallet_balance,
            -- Cast: Postgres SUM(bigint) is NUMERIC, and sqlx refuses to decode
            -- NUMERIC into i64, so without this the query fails on EVERY wallet
            -- and the checker never returns an answer at all.
            COALESCE(SUM(l.delta_idr), 0)::bigint AS ledger_sum
        FROM wallets w
        LEFT JOIN ledger l ON l.account_id = w.account_id
        WHERE w.account_id = $1
        GROUP BY w.balance_idr
        "#,
    )
    .bind(account_id)
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
pub async fn unpaired_hold_rows(pool: &PgPool, account_id: Uuid) -> Result<i64, AppError> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM (
            SELECT l.account_id, l.ref AS r
            FROM ledger l
            WHERE l.account_id = $1
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
    .bind(account_id)
    .fetch_one(pool)
    .await?;
    Ok(count)
}
