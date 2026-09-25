use std::env;
use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tracing::info;
use uuid::Uuid;

use crate::config::{AppConfig, WalletConfig};
use crate::error::AppError;
use crate::routes::proxy::AppState;

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

fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

async fn resolve_account_from_cookie(pool: &PgPool, headers: &HeaderMap) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = $1 AND revoked_at IS NULL AND expires_at > now()",
            )
            .bind(token_hash)
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.try_get("account_id")?);
            }
        }
    }

    Err(AppError::Unauthenticated)
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
}
