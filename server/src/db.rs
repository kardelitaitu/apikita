use sqlx::{PgPool, Postgres, Transaction, Row};
use uuid::Uuid;
use chrono::Utc;
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

    // 1. Debit wallet
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - $1, updated_at = now() WHERE account_id = $2 RETURNING balance_idr",
    )
    .bind(cost_idr)
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await?;

    let new_balance: i64 = wallet.get("balance_idr");

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
