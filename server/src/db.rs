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

/// Atomically settles usage: debits wallet, inserts ledger row, and upserts daily usage.
///
/// The balance check is a predicate on the UPDATE itself, so it cannot race a
/// concurrent request, and `CHECK (balance_idr >= 0)` is never the thing that
/// refuses the debit (which would surface as an opaque 500).
///
/// An unaffordable debit is NOT dropped: the reported usage is still recorded and
/// the debit is clamped to the balance (`UsageSettlement::Partial`). See
/// `clamp_debit` for why the debit is clamped rather than the balance forced.
pub async fn debit_usage_transaction(
    pool: &PgPool,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
) -> Result<UsageSettlement, AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

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
                current_balance,
            )
            .await;
        }
    };

    // 2. Append the ledger debit and the usage row, then commit.
    record_usage(
        tx,
        account_id,
        api_key_id,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cost_idr,
        ref_batch,
        -cost_idr,
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
/// 30-day spend reporting read; `delta_idr` is what the ledger records. The two
/// differ only when the wallet could not cover the cost in full.
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
    delta_idr: i64,
    new_balance: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES ($1, $2, 'usage', $3, $4, now())",
    )
    .bind(account_id)
    .bind(delta_idr)
    .bind(ref_batch)
    .bind(new_balance)
    .execute(&mut *tx)
    .await?;

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
        -debited_idr,
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

    /// A debit the wallet cannot cover must NOT be discarded: the reported usage
    /// is recorded and the debit is clamped to the balance, so reconciliation
    /// still holds and the shortfall is visible. A debit it CAN cover still
    /// settles in full.
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

        let opening_balance: i64 = 1_000;
        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, $2)")
            .bind(account_id)
            .bind(opening_balance)
            .execute(&pool)
            .await
            .expect("create wallet");

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
        let ledger_delta: i64 =
            sqlx::query_scalar("SELECT delta_idr FROM ledger WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("the clamped debit must still append a ledger row");
        assert_eq!(ledger_delta, -opening_balance);

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

        // 4. A debit the wallet CAN cover still settles in full. Top the wallet up
        //    again first - the clamp above left it at zero.
        sqlx::query("UPDATE wallets SET balance_idr = $2 WHERE account_id = $1")
            .bind(account_id)
            .bind(opening_balance)
            .execute(&pool)
            .await
            .expect("refill wallet");
        sqlx::query("INSERT INTO ledger (account_id, delta_idr, reason, balance_after) VALUES ($1, $2, 'adjustment', $3)")
            .bind(account_id)
            .bind(opening_balance)
            .bind(opening_balance)
            .execute(&pool)
            .await
            .expect("refill ledger");

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

        // Cleanup, in FK order (ledger and wallets are ON DELETE RESTRICT).
        sqlx::query("DELETE FROM usage_daily WHERE account_id = $1")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("cleanup usage");
        sqlx::query("DELETE FROM ledger WHERE account_id = $1")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("cleanup ledger");
        sqlx::query("DELETE FROM api_keys WHERE account_id = $1")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("cleanup api keys");
        sqlx::query("DELETE FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("cleanup wallet");
        sqlx::query("DELETE FROM accounts WHERE id = $1")
            .bind(account_id)
            .execute(&pool)
            .await
            .expect("cleanup account");
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
            COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
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
