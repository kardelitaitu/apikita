use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

use crate::config::{AppConfig, WalletConfig};
use crate::error::AppError;
use crate::routes::proxy::AppState;
use crate::routes::resolve_account_from_cookie;

#[derive(Debug, Serialize)]
pub struct MeResponse {
    pub account_id: Uuid,
    pub balance_idr: i64,
    pub usage_today: UsageTodayDto,
    pub telegram_linked: bool,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct UsageTodayDto {
    pub input_tokens: i64,
    pub cache_read_tokens: i64,
    pub output_tokens: i64,
    pub cost_idr: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateTopupRequest {
    pub amount_idr: i64,
}

#[derive(Debug, Serialize)]
pub struct CreateTopupResponse {
    pub topup_id: Uuid,
    pub order_id: String,
    pub snap_token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<i64>,
}

pub async fn get_me(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let account = sqlx::query("SELECT status FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&pool)
        .await?;

    let status: String = account.try_get("status")?;

    let wallet = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&pool)
        .await?;

    let balance_idr: i64 = wallet
        .map(|w| w.try_get::<i64, _>("balance_idr"))
        .transpose()?
        .unwrap_or(0);

    let today = Utc::now().date_naive();
    let usage_today = sqlx::query(
        r#"
        SELECT
            COALESCE(SUM(input_tokens), 0)::bigint AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0)::bigint AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0)::bigint AS output_tokens,
            COALESCE(SUM(cost_idr), 0)::bigint AS cost_idr
        FROM usage_daily
        WHERE account_id = $1 AND day = $2
        "#,
    )
    .bind(account_id)
    .bind(today)
    .fetch_one(&pool)
    .await?;

    let tg_link = sqlx::query("SELECT telegram_id FROM telegram_links WHERE account_id = $1")
        .bind(account_id)
        .fetch_optional(&pool)
        .await?;

    Ok(Json(MeResponse {
        account_id,
        balance_idr,
        usage_today: UsageTodayDto {
            input_tokens: usage_today.try_get("input_tokens")?,
            cache_read_tokens: usage_today.try_get("cache_read_tokens")?,
            output_tokens: usage_today.try_get("output_tokens")?,
            cost_idr: usage_today.try_get("cost_idr")?,
        },
        telegram_linked: tg_link.is_some(),
        status,
    }))
}

pub async fn get_usage(
    State(pool): State<PgPool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let rows = sqlx::query(
        r#"
        SELECT
            day,
            SUM(input_tokens)::bigint AS input_tokens,
            SUM(cache_read_tokens)::bigint AS cache_read_tokens,
            SUM(output_tokens)::bigint AS output_tokens,
            SUM(cost_idr)::bigint AS cost_idr
        FROM usage_daily
        WHERE account_id = $1
        GROUP BY day
        ORDER BY day DESC
        LIMIT 30
        "#,
    )
    .bind(account_id)
    .fetch_all(&pool)
    .await?;

    let result: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| -> Result<serde_json::Value, AppError> {
            let day: chrono::NaiveDate = r.try_get("day")?;
            let in_tok: Option<i64> = r.try_get("input_tokens")?;
            let cache_tok: Option<i64> = r.try_get("cache_read_tokens")?;
            let out_tok: Option<i64> = r.try_get("output_tokens")?;
            let cost: Option<i64> = r.try_get("cost_idr")?;
            Ok(json!({
                "day": day,
                "input_tokens": in_tok.unwrap_or(0),
                "cache_read_tokens": cache_tok.unwrap_or(0),
                "output_tokens": out_tok.unwrap_or(0),
                "cost_idr": cost.unwrap_or(0),
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(result))
}

pub async fn get_topups(
    State(pool): State<PgPool>,
    headers: HeaderMap,
    Query(query): Query<LimitQuery>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let rows = sqlx::query(
        r#"
        SELECT id, amount_idr, order_id, status, created_at, settled_at
        FROM topups
        WHERE account_id = $1
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(account_id)
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let result: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| -> Result<serde_json::Value, AppError> {
            let id: Uuid = r.try_get("id")?;
            let amount_idr: i64 = r.try_get("amount_idr")?;
            let order_id: String = r.try_get("order_id")?;
            let status: String = r.try_get("status")?;
            let created_at: chrono::DateTime<Utc> = r.try_get("created_at")?;
            let settled_at: Option<chrono::DateTime<Utc>> = r.try_get("settled_at")?;
            Ok(json!({
                "id": id,
                "amount_idr": amount_idr,
                "order_id": order_id,
                "status": status,
                "created_at": created_at,
                "settled_at": settled_at,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(result))
}

// ---------------------------------------------------------------------------
// Midtrans Snap client
// ---------------------------------------------------------------------------

/// Sandbox host per Midtrans docs; production is the same path on the live host.
const SNAP_SANDBOX_URL: &str = "https://app.sandbox.midtrans.com/snap/v1/transactions";
const SNAP_PRODUCTION_URL: &str = "https://app.midtrans.com/snap/v1/transactions";

/// Creating a Snap transaction can block on Midtrans; never inherit the 120s
/// upstream-proxy timeout for a customer-facing dashboard call.
const SNAP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

static WALLET_CONFIG: OnceLock<WalletConfig> = OnceLock::new();

/// WalletConfig is not part of the router state (State<PgPool>), so the config
/// file is read once per process and cached. Same resolution order as
/// auth::sessions_config and main.rs: APIKITA_CONFIG_PATH, then config/, then
/// ../config/.
fn wallet_config() -> Result<&'static WalletConfig, AppError> {
    if let Some(config) = WALLET_CONFIG.get() {
        return Ok(config);
    }

    let path = env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let loaded = AppConfig::load_from_file(&path)
        .or_else(|_| AppConfig::load_from_file("../config/apikita.toml"))
        .map_err(|e| AppError::Internal(format!("failed to load wallet config: {e}")))?;

    Ok(WALLET_CONFIG.get_or_init(|| loaded.wallet))
}

/// The Snap host for MIDTRANS_ENV. Anything other than an explicit
/// "production" stays on sandbox, so a typo can never move real money.
fn snap_endpoint(midtrans_env: Option<&str>) -> &'static str {
    match midtrans_env.map(str::trim) {
        Some(env) if env.eq_ignore_ascii_case("production") => SNAP_PRODUCTION_URL,
        _ => SNAP_SANDBOX_URL,
    }
}

/// First deposit has a higher minimum than every later one. Returns Ok(()) when
/// the amount clears the configured floor, otherwise a 422 naming the limit.
fn check_deposit_limit(
    amount_idr: i64,
    settled_topups: i64,
    wallet: &WalletConfig,
) -> Result<(), AppError> {
    let is_first_deposit = settled_topups == 0;
    let min_required = if is_first_deposit {
        wallet.min_first_deposit
    } else {
        wallet.min_topup
    };

    if amount_idr < min_required as i64 {
        let which = if is_first_deposit {
            "first deposit"
        } else {
            "top-up"
        };
        return Err(AppError::ValidationFailed {
            message: format!("Amount must be at least {min_required} IDR for a {which}"),
            // docs/error-model.md rule 5: name the field so the top-up form can
            // highlight the amount input without parsing the message.
            field: "amount_idr".into(),
        });
    }

    Ok(())
}

/// Builds the Snap /transactions body. Field names follow the Midtrans Snap API:
/// transaction_details is required, customer_details.email is optional and
/// omitted when the account has no email on record.
fn build_snap_payload(order_id: &str, amount_idr: i64, customer_email: Option<&str>) -> Value {
    let mut payload = Map::new();
    payload.insert(
        "transaction_details".to_string(),
        json!({ "order_id": order_id, "gross_amount": amount_idr }),
    );

    if let Some(email) = customer_email.map(str::trim).filter(|e| !e.is_empty()) {
        payload.insert("customer_details".to_string(), json!({ "email": email }));
    }

    Value::Object(payload)
}

/// The email Snap should attach to the transaction. The accounts table holds
/// only the PocketBase record id, so the address is read from PocketBase itself
/// (POCKETBASE_URL, the same address auth.rs uses). It is best-effort: a failure
/// there degrades the receipt and must never block a top-up.
async fn account_email(http: &reqwest::Client, pb_user_id: &str) -> Option<String> {
    let base = env::var("POCKETBASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8090".to_string());
    let url = format!(
        "{}/api/collections/users/records/{}",
        base.trim_end_matches('/'),
        pb_user_id
    );

    let response = http.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let body: Value = response.json().await.ok()?;
    body.get("email")?.as_str().map(|e| e.trim().to_string())
}

/// Creates the Snap transaction and returns (token, redirect_url).
///
/// HTTP Basic auth with the server key as the username and an empty password --
/// Midtrans' documented scheme. A non-2xx response, a missing token, or a
/// transport failure is an error; this function writes nothing to the database.
async fn create_snap_transaction(
    http: &reqwest::Client,
    server_key: &str,
    endpoint: &str,
    payload: &Value,
) -> Result<(String, Option<String>), AppError> {
    let response = http
        .post(endpoint)
        .basic_auth(server_key, Some(""))
        .header("Accept", "application/json")
        .json(payload)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Midtrans Snap request failed: {e}")))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| AppError::Internal(format!("Midtrans Snap response unreadable: {e}")))?;

    if !status.is_success() {
        // Midtrans returns error_messages (an array) or a status_message string.
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| {
                v.get("error_messages")
                    .and_then(|m| m.as_array())
                    .map(|m| {
                        m.iter()
                            .filter_map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    })
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        v.get("status_message")
                            .and_then(|m| m.as_str())
                            .map(str::to_string)
                    })
            })
            .unwrap_or_else(|| body.chars().take(200).collect());

        return Err(AppError::Internal(format!(
            "Midtrans Snap rejected the transaction ({status}): {detail}"
        )));
    }

    let parsed: Value = serde_json::from_str(&body).map_err(|e| {
        AppError::Internal(format!("Midtrans Snap response is not valid JSON: {e}"))
    })?;

    let token = parsed
        .get("token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| AppError::Internal("Midtrans Snap response contained no token".to_string()))?
        .to_string();

    let redirect_url = parsed
        .get("redirect_url")
        .and_then(|u| u.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);

    Ok((token, redirect_url))
}

pub async fn create_topup(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateTopupRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // Abuse guard FIRST, before anything expensive: `limits.topup_per_hour`
    // (docs/decisions.md) is enforced from the `topups` rows, so a hammering
    // account is refused without a PocketBase lookup or a Midtrans session
    // being created. Same reasoning as the proxy throttling a key before it
    // touches the wallet. The cap comes from the config the app already owns.
    crate::abuse::enforce_creation_cap(
        &state.pool,
        "topups",
        crate::abuse::topup_window(),
        state.config.limits.topup_per_hour,
        account_id,
        Utc::now(),
    )
    .await?;

    // Minimums are configured, not hardcoded (docs/website/04-payments.md).
    let wallet = wallet_config()?;

    let past_settled = sqlx::query(
        "SELECT count(*) AS count FROM topups WHERE account_id = $1 AND status = 'settled'",
    )
    .bind(account_id)
    .fetch_one(&state.pool)
    .await?;

    let settled_count: i64 = past_settled.try_get("count")?;
    check_deposit_limit(payload.amount_idr, settled_count, wallet)?;

    let server_key = env::var("MIDTRANS_SERVER_KEY").map_err(|_| {
        AppError::Internal("MIDTRANS_SERVER_KEY environment variable is not configured".to_string())
    })?;
    let endpoint = snap_endpoint(env::var("MIDTRANS_ENV").ok().as_deref());

    let topup_id = Uuid::new_v4();
    let order_id = format!("topup_{}", topup_id);

    // accounts stores only the PocketBase record id; the email lives in PocketBase.
    let pb_user_id: String = sqlx::query("SELECT pb_user_id FROM accounts WHERE id = $1")
        .bind(account_id)
        .fetch_one(&state.pool)
        .await?
        .try_get("pb_user_id")?;

    // Built per request: this path is one call per customer action, far too cold
    // to justify a process-wide pool, and SNAP_REQUEST_TIMEOUT is this call's
    // budget rather than the proxy's 120s.
    let snap_http = reqwest::Client::builder()
        .timeout(SNAP_REQUEST_TIMEOUT)
        .build()
        .map_err(|e| AppError::Internal(format!("failed to build Snap HTTP client: {e}")))?;

    let email = account_email(&snap_http, &pb_user_id).await;
    let snap_payload = build_snap_payload(&order_id, payload.amount_idr, email.as_deref());

    // Midtrans first. Any failure here returns before a row exists, so a rejected
    // or unreachable Snap call never leaves a pending topup behind.
    let (snap_token, redirect_url) =
        create_snap_transaction(&snap_http, &server_key, endpoint, &snap_payload).await?;

    sqlx::query(
        "INSERT INTO topups (id, account_id, amount_idr, order_id, status, snap_token) VALUES ($1, $2, $3, $4, 'pending', $5)",
    )
    .bind(topup_id)
    .bind(account_id)
    .bind(payload.amount_idr)
    .bind(&order_id)
    .bind(&snap_token)
    .execute(&state.pool)
    .await?;

    info!(
        topup_id = %topup_id,
        order_id = %order_id,
        amount_idr = payload.amount_idr,
        redirect_url = redirect_url.as_deref().unwrap_or(""),
        "Created Midtrans Snap transaction"
    );

    Ok((
        StatusCode::CREATED,
        Json(CreateTopupResponse {
            topup_id,
            order_id,
            snap_token: Some(snap_token),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wallet(min_topup: u64, min_first_deposit: u64) -> WalletConfig {
        WalletConfig {
            min_topup,
            min_first_deposit,
            min_monthly_tokens: 0,
            dormancy_days: 0,
            reserve_settlement_cycles: 1,
            low_balance_threshold_idr: 10_000,
            low_balance_max_per_day: 1,
        }
    }

    #[test]
    fn first_deposit_uses_the_first_deposit_minimum() {
        let w = wallet(10_000, 50_000);

        assert!(check_deposit_limit(50_000, 0, &w).is_ok());
        assert!(check_deposit_limit(49_999, 0, &w).is_err());
        assert!(check_deposit_limit(10_000, 0, &w).is_err());
    }

    #[test]
    fn subsequent_topups_use_the_lower_minimum() {
        let w = wallet(10_000, 50_000);

        assert!(check_deposit_limit(10_000, 1, &w).is_ok());
        assert!(check_deposit_limit(9_999, 1, &w).is_err());
    }

    #[test]
    fn deposit_limit_breach_is_a_422_naming_the_limit() {
        let err = check_deposit_limit(10_000, 0, &wallet(10_000, 50_000)).unwrap_err();

        assert_eq!(err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        let message = err.to_string();
        assert!(message.contains("50000"), "message was {message}");

        // docs/error-model.md rule 5: the top-up form needs the field name to
        // highlight the amount input without parsing the message.
        assert_eq!(
            err.details(),
            Some(serde_json::json!({ "field": "amount_idr" })),
            "a 422 from the deposit check must carry details.field = \"amount_idr\""
        );
    }

    #[test]
    fn deposit_limits_come_from_config_not_constants() {
        // Same amount, different config: the decision follows the config.
        assert!(check_deposit_limit(20_000, 0, &wallet(10_000, 10_000)).is_ok());
        assert!(check_deposit_limit(20_000, 0, &wallet(10_000, 50_000)).is_err());
    }

    #[test]
    fn snap_endpoint_defaults_to_sandbox() {
        assert_eq!(snap_endpoint(None), SNAP_SANDBOX_URL);
        assert_eq!(snap_endpoint(Some("sandbox")), SNAP_SANDBOX_URL);
        assert_eq!(snap_endpoint(Some(" production ")), SNAP_PRODUCTION_URL);
        assert_eq!(snap_endpoint(Some("Production")), SNAP_PRODUCTION_URL);
        // A typo must never reach the live host.
        assert_eq!(snap_endpoint(Some("prod")), SNAP_SANDBOX_URL);
        assert_eq!(snap_endpoint(Some("")), SNAP_SANDBOX_URL);
    }

    #[test]
    fn snap_payload_has_transaction_details() {
        let payload = build_snap_payload("topup_abc", 50_000, None);

        assert_eq!(
            payload["transaction_details"],
            json!({ "order_id": "topup_abc", "gross_amount": 50_000 })
        );
        assert!(payload.get("customer_details").is_none());
    }

    #[test]
    fn snap_payload_carries_the_customer_email_when_present() {
        let payload = build_snap_payload("topup_abc", 10_000, Some(" user@example.com "));

        assert_eq!(payload["customer_details"]["email"], "user@example.com");
        assert_eq!(
            payload["transaction_details"]["gross_amount"],
            json!(10_000)
        );
    }

    // -----------------------------------------------------------------------
    // LIVE-DATABASE TESTS
    //
    // Everything above is pure: it covers the helpers and never touches a
    // database, so none of the four DB-backed handlers (get_me, get_usage,
    // get_topups, create_topup) had ever been executed by this suite. The
    // tests below run them against a real, migrated Postgres.
    //
    // They are #[ignore]d rather than skipped, so the default suite stays green
    // without a database. Run them with:
    //
    //   DATABASE_URL=postgres://postgres:dev@localhost:5432/apikita \
    //     cargo test --lib -- --ignored
    // -----------------------------------------------------------------------

    use crate::db::credit_topup_transaction;
    use crate::routes::events::RealtimeHub;
    use axum::body::to_bytes;
    use std::sync::Arc;

    /// A SMALL pool per test, deliberately.
    ///
    /// `crate::db::init_pool` opens up to 20 connections per call, and the live
    /// suite already runs several such pools in parallel. Five more of those
    /// exhausted Postgres' 100-connection limit and made an unrelated db.rs test
    /// fail with `PoolTimedOut` - a connection-budget failure, not a code
    /// failure. Four is ample for one test, and because the pool is NOT shared,
    /// no sibling test can starve this one's teardown.
    async fn live_pool() -> PgPool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&database_url)
            .await
            .expect("connect to Postgres")
    }

    /// Deletes every row a fixture created, in FK order. wallets, ledger and
    /// topups are ON DELETE RESTRICT, so the order is load-bearing.
    async fn delete_fixture_rows(pool: &PgPool, account_ids: &[Uuid]) {
        for account_id in account_ids {
            for statement in [
                "DELETE FROM usage_daily WHERE account_id = $1",
                "DELETE FROM ledger WHERE account_id = $1",
                "DELETE FROM api_keys WHERE account_id = $1",
                "DELETE FROM topups WHERE account_id = $1",
                "DELETE FROM sessions WHERE account_id = $1",
                "DELETE FROM wallets WHERE account_id = $1",
                "DELETE FROM accounts WHERE id = $1",
            ] {
                sqlx::query(statement)
                    .bind(account_id)
                    .execute(pool)
                    .await
                    .unwrap_or_else(|err| panic!("cleanup failed on {statement}: {err}"));
            }
        }
    }

    /// The reconciliation check from docs/observability.md, scoped to one
    /// account: wallets.balance_idr must equal SUM(ledger.delta_idr).
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

    /// An account with a live session cookie, built the way the login path
    /// builds one: an accounts row, a sessions row holding only the SHA-256 of
    /// the token, and the zero-balance wallets row.
    struct LiveAccount {
        account_id: Uuid,
        token: String,
    }

    async fn live_account(pool: &PgPool) -> LiveAccount {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(pool)
                .await
                .expect("create account");

        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO sessions (account_id, token_hash, expires_at) VALUES ($1, $2, $3)",
        )
        .bind(account_id)
        .bind(crate::routes::hash_token(&token))
        .bind(Utc::now() + chrono::Duration::days(30))
        .execute(pool)
        .await
        .expect("create session");

        // A zero-balance wallet with no ledger rows is consistent on its own
        // (0 = SUM of nothing), so this starting point reconciles.
        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(pool)
            .await
            .expect("create the zero-balance wallet the login path would create");

        LiveAccount { account_id, token }
    }

    fn cookie_header(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// Money enters a wallet only through the REAL path: a topups row, then
    /// credit_topup_transaction, which settles it and appends the matching +
    /// ledger row in the same transaction. Writing wallets.balance_idr directly
    /// would manufacture the very drift the drift assertion then reports - a
    /// fixture that cannot pass while the code under test is correct.
    async fn settle_topup(pool: &PgPool, account_id: Uuid, amount_idr: i64) -> Uuid {
        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        let topup_id: Uuid = sqlx::query_scalar(
            "INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(account_id)
        .bind(amount_idr)
        .bind(&order_id)
        .fetch_one(pool)
        .await
        .expect("create topup");

        let credited = credit_topup_transaction(pool, &order_id, amount_idr)
            .await
            .expect("credit the top-up through the real money path");
        assert!(
            matches!(credited, crate::db::TopupCreditResult::Settled { .. }),
            "the fixture must settle the top-up, got {credited:?}"
        );

        topup_id
    }

    /// Drives a handler exactly the way the router does and reads its body.
    async fn respond<F, T>(result: F) -> (StatusCode, Value)
    where
        F: std::future::Future<Output = Result<T, AppError>>,
        T: IntoResponse,
    {
        let res = match result.await {
            Ok(ok) => ok.into_response(),
            Err(err) => err.into_response(),
        };
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body");
        let body: Value = serde_json::from_slice(&bytes)
            .expect("docs/error-model.md:10 - every response is JSON");
        (status, body)
    }

    /// The AppState the router would hand create_topup, built from the same
    /// config file the server loads.
    fn live_app_state(pool: PgPool) -> AppState {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load for the live tests");
        let trusted = crate::ip_tracking::parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("the config validates its own trusted proxy rules");

        AppState {
            pool,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("build a test HTTP client"),
            events: Arc::new(RealtimeHub::new(&config.realtime)),
            config: Arc::new(config),
            ip_salt: Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies: Arc::from(trusted.into_boxed_slice()),
        }
    }

    /// The configured minimums, read from the real config file. The deposit
    /// tests assert against THESE rather than hardcoded numbers, so a config
    /// change cannot silently invalidate them.
    fn configured_wallet() -> &'static WalletConfig {
        wallet_config().expect("config/apikita.toml must resolve for the live tests")
    }

    // -----------------------------------------------------------------------
    // 1. get_me
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_get_me_returns_the_real_balance_and_the_documented_fields() {
        let pool = live_pool().await;
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(get_me_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        // The assertions run in their own task so a panicking one still reaches
        // the cleanup below: Tokio turns a task panic into a JoinError instead
        // of unwinding through this frame, which is what makes the teardown
        // unconditional. Awaiting BEFORE the deletes also stops the fixture
        // from racing its own cleanup.
        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[primary.account_id, other.account_id]).await;
        outcome.expect("the get_me assertions panicked");
    }

    async fn get_me_assertions(
        pool: PgPool,
        account_id: Uuid,
        token: String,
        other_account_id: Uuid,
    ) {
        let opening = 73_500;
        settle_topup(&pool, account_id, opening).await;

        // The second account carries a DIFFERENT balance, so a handler reading
        // the wrong row - or returning a constant - cannot satisfy both.
        let other_opening = 11_000;
        settle_topup(&pool, other_account_id, other_opening).await;

        let (status, body) = respond(get_me(State(pool.clone()), cookie_header(&token))).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        // Cross-checked against a direct SELECT, so the test cannot pass on a
        // stale or hardcoded value.
        let stored: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("read the wallet");
        assert_eq!(
            stored, opening,
            "the fixture must have credited the opening balance through the real path"
        );
        assert_eq!(
            body["balance_idr"],
            json!(opening),
            "docs/server/api-spec.md GET /api/me: the REAL wallet balance. body: {body}"
        );

        // The documented shape, exactly.
        let mut keys: Vec<&str> = body
            .as_object()
            .expect("docs/server/api-spec.md: /api/me is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["account_id", "balance_idr", "status", "telegram_linked", "usage_today"],
            "docs/server/api-spec.md GET /api/me: {body}"
        );
        assert_eq!(body["account_id"], json!(account_id));
        assert_eq!(body["status"], json!("active"));
        assert_eq!(body["telegram_linked"], json!(false));

        let usage = body["usage_today"]
            .as_object()
            .expect("usage_today is an object");
        assert_eq!(usage.len(), 4, "usage_today: {body}");
        for field in ["input_tokens", "cache_read_tokens", "output_tokens", "cost_idr"] {
            assert_eq!(
                usage.get(field),
                Some(&json!(0)),
                "usage_today.{field} must be present and zero: {body}"
            );
        }

        // TENANCY: the other account balance must not appear anywhere.
        assert_ne!(
            body["balance_idr"],
            json!(other_opening),
            "another account balance was returned: {body}"
        );
        assert_ne!(body["account_id"], json!(other_account_id));

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );
    }

    // -----------------------------------------------------------------------
    // 2. get_usage - the most important one
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_get_usage_keeps_the_three_token_classes_separate() {
        let pool = live_pool().await;
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(get_usage_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[primary.account_id, other.account_id]).await;
        outcome.expect("the get_usage assertions panicked");
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

    async fn insert_usage(
        pool: &PgPool,
        account_id: Uuid,
        key_id: Uuid,
        day: chrono::NaiveDate,
        tokens: (i64, i64, i64, i64),
    ) {
        sqlx::query(
            r#"
            INSERT INTO usage_daily (
                account_id, api_key_id, day,
                input_tokens, cache_read_tokens, output_tokens, cost_idr
            ) VALUES ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(account_id)
        .bind(key_id)
        .bind(day)
        .bind(tokens.0)
        .bind(tokens.1)
        .bind(tokens.2)
        .bind(tokens.3)
        .execute(pool)
        .await
        .expect("insert usage row");
    }

    async fn get_usage_assertions(
        pool: PgPool,
        account_id: Uuid,
        token: String,
        other_account_id: Uuid,
    ) {
        let today = Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);

        // usage_daily.api_key_id is part of the primary key, so a real key row
        // is needed before any usage can be recorded.
        let key_a = create_api_key(&pool, account_id).await;
        let key_b = create_api_key(&pool, other_account_id).await;

        // THREE DISTINCT values per class, pairwise distinct AND distinct from
        // their own sum, so a handler that merged the classes could not pass by
        // coincidence.
        let (in_a, cache_a, out_a, cost_a) = (111_111i64, 222_222i64, 333_333i64, 4_444i64);
        let (in_y, cache_y, out_y, cost_y) = (1_001i64, 2_002i64, 3_003i64, 55i64);
        let merged = in_a + cache_a + out_a;

        insert_usage(&pool, account_id, key_a, today, (in_a, cache_a, out_a, cost_a)).await;
        insert_usage(&pool, account_id, key_a, yesterday, (in_y, cache_y, out_y, cost_y)).await;
        // The other account row lands on the SAME day with wildly different
        // numbers: if the query lost its account scope, they would show up.
        insert_usage(
            &pool,
            other_account_id,
            key_b,
            today,
            (9_999_999, 8_888_888, 7_777_777, 999_999),
        )
        .await;

        let (status, body) = respond(get_usage(State(pool.clone()), cookie_header(&token))).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let rows = body
            .as_array()
            .expect("docs/server/api-spec.md GET /api/usage: daily buckets are an array");
        assert_eq!(
            rows.len(),
            2,
            "one bucket per day, and ONLY this account days: {body}"
        );
        assert_eq!(rows[0]["day"], json!(today), "day DESC: {body}");
        assert_eq!(rows[1]["day"], json!(yesterday), "day DESC: {body}");

        let today_row = &rows[0];
        // THE CONTRACT: three separate counters, never summed.
        assert_eq!(
            today_row["input_tokens"],
            json!(in_a),
            "input_tokens must be its own counter: {today_row}"
        );
        assert_eq!(
            today_row["cache_read_tokens"],
            json!(cache_a),
            "cache_read_tokens must be its own counter: {today_row}"
        );
        assert_eq!(
            today_row["output_tokens"],
            json!(out_a),
            "output_tokens must be its own counter: {today_row}"
        );
        assert_eq!(today_row["cost_idr"], json!(cost_a), "{today_row}");

        // And explicitly: no counter carries the merged figure.
        for field in ["input_tokens", "cache_read_tokens", "output_tokens", "cost_idr"] {
            assert_ne!(
                today_row[field],
                json!(merged),
                "docs/local-development.md: cache-read vs input is the most expensive \
                 possible accounting error. {field} carries a summed figure: {today_row}"
            );
        }

        // Each bucket exposes exactly the documented keys - a total_tokens would
        // be the merge this test exists to forbid.
        for row in rows {
            let mut keys: Vec<&str> = row
                .as_object()
                .expect("a bucket is an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(
                keys,
                [
                    "cache_read_tokens",
                    "cost_idr",
                    "day",
                    "input_tokens",
                    "output_tokens"
                ],
                "three token classes, never summed: {row}"
            );
        }

        let yesterday_row = &rows[1];
        assert_eq!(yesterday_row["input_tokens"], json!(in_y), "{yesterday_row}");
        assert_eq!(
            yesterday_row["cache_read_tokens"],
            json!(cache_y),
            "{yesterday_row}"
        );
        assert_eq!(yesterday_row["output_tokens"], json!(out_y), "{yesterday_row}");
        assert_eq!(yesterday_row["cost_idr"], json!(cost_y), "{yesterday_row}");

        // TENANCY: none of the other account numbers leaked in.
        let rendered = body.to_string();
        for leaked in ["9999999", "8888888", "7777777"] {
            assert!(
                !rendered.contains(leaked),
                "another account usage leaked into the response: {body}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 3. get_topups
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_get_topups_returns_only_this_accounts_topups() {
        let pool = live_pool().await;
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(get_topups_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[primary.account_id, other.account_id]).await;
        outcome.expect("the get_topups assertions panicked");
    }

    async fn get_topups_assertions(
        pool: PgPool,
        account_id: Uuid,
        token: String,
        other_account_id: Uuid,
    ) {
        // One SETTLED top-up through the real money path, and one PENDING row
        // created a minute earlier so the ordering is deterministic.
        let settled_id = settle_topup(&pool, account_id, 50_000).await;

        let pending_order = format!("test_pending_{}", Uuid::new_v4().simple());
        let pending_id: Uuid = sqlx::query_scalar(
            "INSERT INTO topups (account_id, amount_idr, order_id, created_at) \
             VALUES ($1, $2, $3, now() - interval '1 minute') RETURNING id",
        )
        .bind(account_id)
        .bind(25_000)
        .bind(&pending_order)
        .fetch_one(&pool)
        .await
        .expect("create a pending topup");

        let other_order = format!("test_other_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(other_account_id)
            .bind(1_234_567)
            .bind(&other_order)
            .execute(&pool)
            .await
            .expect("create the other account topup");

        let (status, body) = respond(get_topups(
            State(pool.clone()),
            cookie_header(&token),
            Query(LimitQuery { limit: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let rows = body.as_array().expect("a top-up history is an array");
        assert_eq!(rows.len(), 2, "only this account top-ups: {body}");

        // The documented fields, and nothing Midtrans-secret shaped. Sorted
        // because the comparison below is against the response's key set.
        const DOCUMENTED: [&str; 6] =
            ["amount_idr", "created_at", "id", "order_id", "settled_at", "status"];
        const ALLOWED_STATUS: [&str; 5] =
            ["pending", "settled", "denied", "expired", "refunded"];

        for row in rows {
            let mut keys: Vec<&str> = row
                .as_object()
                .expect("a top-up is an object")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, DOCUMENTED, "docs/server/api-spec.md: {row}");

            let status = row["status"].as_str().expect("status is a string");
            assert!(
                ALLOWED_STATUS.contains(&status),
                "{status:?} is not one of the documented statuses: {row}"
            );
        }

        let rendered = body.to_string();
        assert!(
            !rendered.contains(&other_order),
            "another account top-up leaked into the history: {body}"
        );
        assert!(
            !rendered.contains("1234567"),
            "another account amount leaked into the history: {body}"
        );
        assert!(
            !rendered.contains("snap_token"),
            "the top-up history must never expose a Midtrans secret: {body}"
        );

        // The settled one carries its settled_at; the pending one does not.
        let settled_row = rows
            .iter()
            .find(|r| r["id"] == json!(settled_id))
            .unwrap_or_else(|| panic!("the settled top-up is missing from {body}"));
        assert_eq!(settled_row["status"], json!("settled"));
        assert_eq!(settled_row["amount_idr"], json!(50_000));
        assert!(
            !settled_row["settled_at"].is_null(),
            "a settled top-up carries settled_at: {settled_row}"
        );

        let pending_row = rows
            .iter()
            .find(|r| r["id"] == json!(pending_id))
            .unwrap_or_else(|| panic!("the pending top-up is missing from {body}"));
        assert_eq!(pending_row["status"], json!("pending"));
        assert!(
            pending_row["settled_at"].is_null(),
            "a pending top-up has no settled_at: {pending_row}"
        );

        // limit is honoured, newest first.
        let (status, limited) = respond(get_topups(
            State(pool.clone()),
            cookie_header(&token),
            Query(LimitQuery { limit: Some(1) }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        let limited = limited.as_array().expect("an array").clone();
        assert_eq!(limited.len(), 1, "limit=1 returns one row: {limited:?}");
        assert_eq!(
            limited[0]["id"],
            json!(settled_id),
            "created_at DESC: the newest top-up comes first"
        );
    }

    // -----------------------------------------------------------------------
    // 4. create_topup
    //
    // Midtrans Snap endpoint is hardcoded in snap_endpoint and there is no
    // local Snap fake, so a handler-level SUCCESS path cannot be driven here.
    // What IS asserted: the handler persists nothing when Snap cannot be
    // reached, and the client never fabricates a token.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_create_topup_persists_nothing_when_snap_cannot_be_reached() {
        let pool = live_pool().await;
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(create_topup_failure_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[primary.account_id]).await;
        outcome.expect("the create_topup assertions panicked");
    }

    async fn create_topup_failure_assertions(pool: PgPool, account_id: Uuid, token: String) {
        let wallet = configured_wallet();
        let amount = wallet.min_first_deposit as i64;

        // An explicitly INVALID server key, so the outcome cannot depend on
        // whatever the developer shell exports. Snap answers 401 to it, and an
        // unreachable network fails the same way - either is a failure to
        // obtain a token, and neither may leave a row behind.
        std::env::set_var("MIDTRANS_SERVER_KEY", "SB-Mid-server-INVALID-LIVE-TEST-KEY");

        let state = live_app_state(pool.clone());
        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&token),
            Json(CreateTopupRequest { amount_idr: amount }),
        ))
        .await;

        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a top-up that cannot get a Snap token is an internal failure, not a \
             created top-up. body: {body}"
        );
        assert_eq!(body["error"]["code"], json!("internal_error"), "{body}");
        assert!(
            body.get("snap_token").is_none(),
            "no token may be returned when none was obtained: {body}"
        );

        // The load-bearing assertion: Midtrans first, row second. A rejected or
        // unreachable Snap call must leave NO row - so there is nothing pending
        // to settle later and no fabricated token persisted.
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(&pool)
            .await
            .expect("count topups");
        assert_eq!(
            rows, 0,
            "a failed Snap call must not persist a topups row (no token, no pending row)"
        );

        let settled: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = $1 AND status = 'settled'",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("count settled");
        assert_eq!(settled, 0, "nothing may be settled or paid");

        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance, 0, "a failed top-up must not move money");

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );
    }

    // -----------------------------------------------------------------------
    // 5 + 6. Deposit limits against REAL settled history
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_deposit_limits_follow_the_real_settled_history() {
        let pool = live_pool().await;
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(deposit_limit_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        delete_fixture_rows(&pool, &[primary.account_id]).await;
        outcome.expect("the deposit limit assertions panicked");
    }

    async fn deposit_limit_assertions(pool: PgPool, account_id: Uuid, token: String) {
        let wallet = configured_wallet();
        // Pinned against the real config file, so this test cannot drift into
        // asserting numbers the server does not actually use.
        assert_eq!(
            wallet.min_first_deposit, 50_000,
            "config/apikita.toml [wallet] min_first_deposit"
        );
        assert_eq!(
            wallet.min_topup, 10_000,
            "config/apikita.toml [wallet] min_topup"
        );

        std::env::set_var("MIDTRANS_SERVER_KEY", "SB-Mid-server-INVALID-LIVE-TEST-KEY");

        // --- No settled history: the FIRST-deposit minimum applies. ---
        let history: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = $1 AND status = 'settled'",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("count settled history");
        assert_eq!(history, 0, "the fixture starts with no settled history");

        let state = live_app_state(pool.clone());
        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&token),
            Json(CreateTopupRequest {
                amount_idr: wallet.min_topup as i64,
            }),
        ))
        .await;

        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "docs/website/04-payments.md: the first deposit has a 50,000 IDR \
             minimum. body: {body}"
        );
        assert_eq!(body["error"]["code"], json!("validation_failed"), "{body}");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("50000"),
            "the refusal must name the limit it enforced: {body}"
        );
        assert_eq!(
            body["error"]["details"]["field"],
            json!("amount_idr"),
            "docs/error-model.md rule 5: a validation error names the field. {body}"
        );

        // The refused attempt created nothing.
        let after_refusal: i64 =
            sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("count topups");
        assert_eq!(after_refusal, 0, "a refused amount must not create a row");

        // --- ONE SETTLED TOP-UP: the lower minimum now applies. ---
        settle_topup(&pool, account_id, wallet.min_first_deposit as i64).await;

        let history: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = $1 AND status = 'settled'",
        )
        .bind(account_id)
        .fetch_one(&pool)
        .await
        .expect("count settled history");
        assert_eq!(history, 1, "the fixture must have exactly one settled top-up");

        // The SAME amount that was refused a moment ago. The decision is now
        // driven by the real topups row above, not by a synthetic argument -
        // which is the integration the pure test cannot reach.
        let state = live_app_state(pool.clone());
        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&token),
            Json(CreateTopupRequest {
                amount_idr: wallet.min_topup as i64,
            }),
        ))
        .await;

        assert_ne!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "with one settled top-up the lower minimum applies, so the amount must \
             clear the deposit check. body: {body}"
        );
        assert_eq!(
            body["error"]["code"],
            json!("internal_error"),
            "past the deposit check the request reaches Snap, which is unavailable \
             here. body: {body}"
        );

        // Still exactly one row: the deposit check passed, but the Snap failure
        // persisted nothing.
        let final_rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = $1")
                .bind(account_id)
                .fetch_one(&pool)
                .await
                .expect("count topups");
        assert_eq!(
            final_rows, 1,
            "only the settled fixture row exists; the Snap failure wrote nothing"
        );

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );
    }

    // -----------------------------------------------------------------------
    // The Snap client contract, without Midtrans and without a database.
    // -----------------------------------------------------------------------

    /// A one-shot local HTTP stub standing in for Midtrans Snap: it answers the
    /// next request with the given response and closes. Network-free on purpose
    /// - the subject is the client contract, not Midtrans availability.
    async fn snap_stub(response: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the Snap stub");
        let addr = listener.local_addr().expect("stub address");

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{addr}/snap/v1/transactions")
    }

    fn http_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn snap_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("build the stub HTTP client")
    }

    /// The stored token is whatever the call RETURNED. The old code wrote a
    /// fabricated mock_snap_<uuid>; this pins the real contract.
    #[tokio::test]
    async fn snap_client_returns_exactly_the_token_the_response_carried() {
        let endpoint = snap_stub(http_response(
            "200 OK",
            r#"{"token":"snap-token-from-midtrans","redirect_url":"https://app.sandbox.midtrans.com/snap/v3/redirection/abc"}"#,
        ))
        .await;

        let (token, redirect) = create_snap_transaction(
            &snap_client(),
            "SB-Mid-server-TEST",
            &endpoint,
            &build_snap_payload("topup_x", 50_000, None),
        )
        .await
        .expect("a 200 carrying a token is a success");

        assert_eq!(
            token, "snap-token-from-midtrans",
            "the token must be the one Snap returned, verbatim"
        );
        assert!(
            !token.starts_with("mock_snap_"),
            "a fabricated token is not a Snap token"
        );
        assert_eq!(
            redirect.as_deref(),
            Some("https://app.sandbox.midtrans.com/snap/v3/redirection/abc")
        );
    }

    /// A failure or a tokenless 200 is an error - never a fabricated token.
    #[tokio::test]
    async fn snap_client_refuses_rather_than_fabricating_a_token() {
        let payload = build_snap_payload("topup_x", 50_000, None);

        // The shape Midtrans returns for a bad server key.
        let endpoint = snap_stub(http_response(
            "401 Unauthorized",
            r#"{"error_messages":["Access denied due to unauthorized transaction"]}"#,
        ))
        .await;
        let err = create_snap_transaction(&snap_client(), "bad-key", &endpoint, &payload)
            .await
            .expect_err("a 401 is not a token");
        assert_eq!(err.status_code(), StatusCode::INTERNAL_SERVER_ERROR);

        // A 200 that carries no token must NOT be turned into one.
        let endpoint = snap_stub(http_response("200 OK", r#"{"status_code":"201"}"#)).await;
        assert!(
            create_snap_transaction(&snap_client(), "key", &endpoint, &payload)
                .await
                .is_err(),
            "a tokenless 200 must be an error, never a fabricated token"
        );
    }
}
