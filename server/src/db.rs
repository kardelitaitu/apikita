use sqlx::{PgPool, Postgres, Transaction, Row};
use uuid::Uuid;
use chrono::Utc;
use serde_json::json;
use crate::error::AppError;

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

/// Atomically settles usage: debits wallet, inserts ledger row, and upserts daily usage.
///
/// An unaffordable debit is rejected as `InsufficientBalance` (402) with nothing
/// written. The balance check is a predicate on the UPDATE itself, so it cannot
/// race a concurrent request, and `CHECK (balance_idr >= 0)` is never the thing
/// that refuses the debit (which would surface as an opaque 500).
pub async fn debit_usage_transaction(
    pool: &PgPool,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
) -> Result<i64, AppError> {
    let mut tx: Transaction<'_, Postgres> = pool.begin().await?;

    // 1. Debit wallet.
    //
    // `balance_idr >= $1` is the guard and it lives inside the statement, not in a
    // preceding read: when a concurrent transaction has already updated the row,
    // Postgres re-evaluates the predicate against the latest row version under the
    // row lock, so two racing debits cannot both pass against one stale balance.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - $1, updated_at = now() WHERE account_id = $2 AND balance_idr >= $1 RETURNING balance_idr",
    )
    .bind(cost_idr)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;

    let new_balance: i64 = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            // Zero rows: the account cannot cover `cost_idr`, or it has no wallet.
            // This read only fills in the error detail - the decision was already
            // made by the predicate, and failing to read it must not turn a 402
            // into a 500.
            let current_balance: i64 =
                sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                    .bind(account_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);

            // Roll back explicitly: a rejected debit writes no ledger row and no
            // usage row, so balance_idr = SUM(ledger.delta_idr) still holds
            // (docs/observability.md - reconciliation check).
            tx.rollback().await?;

            return Err(AppError::InsufficientBalance {
                details: Some(json!({
                    "balance_idr": current_balance,
                    "required_idr": cost_idr
                })),
            });
        }
    };

    // 2. Append ledger debit
    let delta = -cost_idr;
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES ($1, $2, 'usage', $3, $4, now())",
    )
    .bind(account_id)
    .bind(delta)
    .bind(ref_batch)
    .bind(new_balance)
    .execute(&mut *tx)
    .await?;

    // 3. Upsert usage_daily
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
    .bind(cost_idr)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(new_balance)
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

    /// A debit the wallet cannot cover must be rejected as 402 with nothing
    /// written - no ledger row, no usage row - and the wallet/ledger
    /// reconciliation must still hold. A debit it CAN cover must still work.
    ///
    /// This needs a live, migrated Postgres. Docker is unavailable in this
    /// environment, so the test is #[ignore]d rather than silently skipped or
    /// rewritten to assert nothing: run it with
    /// `DATABASE_URL=... cargo test --lib -- --ignored`.
    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn overdraft_debit_is_rejected_with_402_and_writes_nothing() {
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

        // 1. Debit one rupiah more than the wallet holds.
        let rejected = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            opening_balance + 1,
            Some("test_overdraft"),
        )
        .await;

        let err = rejected.expect_err("an unaffordable debit must not succeed");
        assert!(
            matches!(err, AppError::InsufficientBalance { .. }),
            "expected InsufficientBalance, got {err:?}"
        );
        assert_eq!(
            err.status_code(),
            axum::http::StatusCode::PAYMENT_REQUIRED,
            "an overdraft must surface as 402, not a 500 from the CHECK constraint"
        );

        // 2. Nothing was written.
        let balance_after: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(
            balance_after, opening_balance,
            "a rejected debit must not move the balance"
        );

        let ledger_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("count ledger");
        assert_eq!(ledger_rows, 0, "a rejected debit must not append a ledger row");

        let usage_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("count usage");
        assert_eq!(usage_rows, 0, "a rejected debit must not record usage");

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );

        // 3. A debit the wallet CAN cover still settles and still reconciles.
        let cost_idr: i64 = 250;
        let new_balance = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            cost_idr,
            Some("test_covered"),
        )
        .await
        .expect("an affordable debit must succeed");

        assert_eq!(new_balance, opening_balance - cost_idr);
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
