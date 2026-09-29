#![cfg_attr(
    not(test),
    // THE CUSTOMER WALLET SURFACE, fenced like the money modules for the same
    // structural reason. This is where a deposit limit is compared and where the
    // wallet figures a customer is shown are assembled, so an overflow here would be
    // a limit that silently stops biting or a balance that reads wrong in the
    // dashboard.
    //
    // It costs nothing, which is the finding rather than the assumption: measured
    // before adding it, this module has ZERO arithmetic_side_effects sites outside
    // its tests. Every money computation happens in db.rs and this module routes to
    // it rather than repeating it. That is worth locking in - a fence is only
    // useful if it is cheap, and this one was.
    deny(clippy::arithmetic_side_effects)
)]

use std::borrow::Cow;
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
use sqlx::{Row, SqlitePool};
use tracing::info;
use uuid::fmt::Hyphenated;
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
    /// Whether this account may use the operator surface. Sent so the browser
    /// can decide whether to render the admin entry points at all, rather than
    /// showing links that answer 403. It is a rendering hint, NOT the
    /// authorization: every admin route re-checks `accounts.is_operator` server
    /// side (admin.rs::require_operator), so a forged value here buys nothing.
    pub is_operator: bool,
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
    /// Which Midtrans environment this session was created against: exactly
    /// "sandbox" or "production".
    ///
    /// Sent so the browser can cross-check its own PUBLIC_MIDTRANS_ENV against
    /// the server's MIDTRANS_ENV instead of guessing from the client key's
    /// prefix - a rule Midtrans does not document, and the client key is not
    /// the value that decides the host anyway. Derived from the same decision
    /// that picks the Snap endpoint (see `midtrans_environment`), never from a
    /// second read of the variable, so the two cannot drift.
    pub environment: String,
}

#[derive(Debug, Deserialize)]
pub struct LimitQuery {
    pub limit: Option<i64>,
}

/// The window parameters docs/server/api-spec.md:97 advertises on
/// `GET /api/usage`. Held as raw strings, not `NaiveDate`, so a malformed
/// value produces OUR JSON 422 naming the field instead of axum's plain-text
/// extractor rejection - docs/error-model.md:10 requires every response to be
/// JSON.
#[derive(Debug, Deserialize)]
pub struct UsageQuery {
    pub from: Option<String>,
    pub to: Option<String>,
}

/// SHA-256 hex of a session token.
///
/// Carried over from the Postgres branch, where the shared
/// `crate::routes::resolve_account_from_cookie` in mod.rs took a `PgPool`. That
/// resolver is now a `SqlitePool` too, so this local copy is equivalent to it:
/// this module keeps its own so the handler and its tests exercise the same
/// resolver. Two copies of the hash-then-compare rule, so a divergence fails a
/// test rather than passing silently
/// into mod.rs. Same rule either way: the token is hashed, never stored raw.
fn hash_string(s: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(s.as_bytes());
    hex::encode(hasher.finalize())
}

async fn resolve_account_from_cookie(
    pool: &SqlitePool,
    headers: &HeaderMap,
) -> Result<Uuid, AppError> {
    let cookie_hdr = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Unauthenticated)?;

    for piece in cookie_hdr.split(';') {
        let piece = piece.trim();
        if let Some(token) = piece.strip_prefix("session=") {
            let token_hash = hash_string(token);
            let session = sqlx::query(
                "SELECT account_id FROM sessions WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
            )
            .bind(token_hash)
            .bind(Utc::now())
            .fetch_optional(pool)
            .await?;

            if let Some(s) = session {
                return Ok(s.try_get::<Hyphenated, _>("account_id")?.into_uuid());
            }
        }
    }

    Err(AppError::Unauthenticated)
}

pub async fn get_me(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let account = sqlx::query("SELECT status, is_operator FROM accounts WHERE id = ?")
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await?;

    let status: String = account.try_get("status")?;
    let is_operator: bool = account.try_get("is_operator")?;

    let wallet = sqlx::query("SELECT balance_idr FROM wallets WHERE account_id = ?")
        .bind(account_id.hyphenated())
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
            COALESCE(SUM(input_tokens), 0) AS input_tokens,
            COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens,
            COALESCE(SUM(output_tokens), 0) AS output_tokens,
            COALESCE(SUM(cost_idr), 0) AS cost_idr
        FROM usage_daily
        WHERE account_id = ? AND day = ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(today)
    .fetch_one(&pool)
    .await?;

    let tg_link = sqlx::query("SELECT telegram_id FROM telegram_links WHERE account_id = ?")
        .bind(account_id.hyphenated())
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
        is_operator,
    }))
}

/// One bound of the `GET /api/usage` window, parsed from `?from=`/`?to=`.
///
/// An absent or empty parameter is "unbounded"; anything else must be a date,
/// because a silently dropped bound is the bug this exists to fix. The format
/// is the ISO date the rest of the API already emits for `day`
/// (`YYYY-MM-DD`), and the error is a 422 naming the offending field, per
/// docs/error-model.md rule 5.
fn parse_usage_day(
    value: Option<&str>,
    field: &str,
) -> Result<Option<chrono::NaiveDate>, AppError> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };

    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| AppError::ValidationFailed {
            message: format!("{field} must be a date in YYYY-MM-DD form, got \"{raw}\""),
            field: field.to_string(),
        })
}

/// Buckets returned when the caller asks for no window at all. The spec names
/// `?from=&to=` but documents no default, so the historical behaviour - the
/// last 30 buckets - is preserved rather than invented anew.
const USAGE_DEFAULT_LIMIT: i64 = 30;

pub async fn get_usage(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
    Query(query): Query<UsageQuery>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let from = parse_usage_day(query.from.as_deref(), "from")?;
    let to = parse_usage_day(query.to.as_deref(), "to")?;

    // docs/server/api-spec.md:97 advertises `?from=&to=`. Both bounds are
    // INCLUSIVE - the spec draws no exclusive boundary, so `from == to` must
    // return that one day - and a bound that is not supplied simply does not
    // constrain the window: the caller asked a bounded question, so an
    // undisclosed default must not quietly narrow the answer. A range that
    // matches nothing returns an empty array, not the default window.
    let limit = if from.is_none() && to.is_none() {
        USAGE_DEFAULT_LIMIT
    } else {
        i64::MAX
    };

    let rows = sqlx::query(
        r#"
        SELECT
            day,
            SUM(input_tokens) AS input_tokens,
            SUM(cache_read_tokens) AS cache_read_tokens,
            SUM(output_tokens) AS output_tokens,
            SUM(cost_idr) AS cost_idr
        FROM usage_daily
        WHERE account_id = ?
          AND (? IS NULL OR day >= ?)
          AND (? IS NULL OR day <= ?)
        GROUP BY day
        ORDER BY day DESC
        LIMIT ?
        "#,
    )
    // Ported: the Postgres original compared with `?::date`/`?::date` casts.
    // SQLite has no `::date` and its `day` column is TEXT in `YYYY-MM-DD` form,
    // which compares correctly against the same `YYYY-MM-DD` string chrono
    // encodes a `NaiveDate` to — so the bound value is what needs repeating, not
    // the cast. Each bound day is passed twice, once as the null test and once as
    // the comparison.
    .bind(account_id.hyphenated())
    .bind(from)
    .bind(from)
    .bind(to)
    .bind(to)
    .bind(limit)
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
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
    Query(query): Query<LimitQuery>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);

    let rows = sqlx::query(
        r#"
        SELECT id, amount_idr, order_id, status, created_at, settled_at
        FROM topups
        WHERE account_id = ?
        ORDER BY created_at DESC
        LIMIT ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let result: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| -> Result<serde_json::Value, AppError> {
            let id: Uuid = r.try_get::<Hyphenated, _>("id")?.into_uuid();
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

/// Rows returned when the caller asks for no explicit limit. Matches the
/// dashboard's "last N metered calls" and keeps the response small enough to
/// render without paging.
const RECENT_USAGE_DEFAULT_LIMIT: i64 = 20;

/// `GET /api/usage/recent?limit=` — the last N metered requests for this account.
///
/// docs/website/03-functional-spec.md, Dashboard table: "Recent requests — last N
/// metered calls". `usage_daily` is an aggregate with no per-request rows and no
/// model, so it cannot answer this; the source is `usage_events`, which the
/// settlement transaction writes one row to per billed request.
///
/// Only THIS account's rows are ever returned: the cookie is resolved first and
/// every query is bound to that account id, so a second account's requests are
/// unreachable even with a guessed id. Nothing here exposes a prompt, a
/// completion, or a key's plaintext — `usage_events` stores none of them.
pub async fn get_recent_usage(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
    Query(query): Query<LimitQuery>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;
    let limit = query
        .limit
        .unwrap_or(RECENT_USAGE_DEFAULT_LIMIT)
        .clamp(1, 100);

    // Newest first. The index `usage_events_account_idx (account_id, created_at
    // DESC)` serves exactly this ordering, so the sort is not a table scan.
    let rows = sqlx::query(
        r#"
        SELECT id, api_key_id, model,
               input_tokens, cache_read_tokens, output_tokens, cost_idr, created_at
        FROM usage_events
        WHERE account_id = ?
        ORDER BY created_at DESC, id DESC
        LIMIT ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(limit)
    .fetch_all(&pool)
    .await?;

    let events: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| -> Result<serde_json::Value, AppError> {
            let id: Uuid = r.try_get::<Hyphenated, _>("id")?.into_uuid();
            // A key deleted later nulls the column (ON DELETE SET NULL), so this
            // is genuinely optional; the UI shows the request without a key name.
            let api_key_id: Option<Uuid> = r
                .try_get::<Option<Hyphenated>, _>("api_key_id")?
                .map(Hyphenated::into_uuid);
            Ok(json!({
                "id": id,
                "api_key_id": api_key_id,
                "model": r.try_get::<String, _>("model")?,
                "input_tokens": r.try_get::<i64, _>("input_tokens")?,
                "cache_read_tokens": r.try_get::<i64, _>("cache_read_tokens")?,
                "output_tokens": r.try_get::<i64, _>("output_tokens")?,
                "cost_idr": r.try_get::<i64, _>("cost_idr")?,
                "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(Json(events))
}

/// `GET /api/export` - the customer's own data, as one JSON document.
///
/// docs/data-retention.md "Access and deletion requests": a customer can ask to
/// export their data. This is that export. The scope is fixed by that document's
/// "The export — what is IN and what is OUT" table, and the rule is **metadata,
/// not secrets**:
///
/// - IN: the account, wallet, ledger, top-ups, usage (daily and per-request) and
///   API-key METADATA - exactly what the customer can already see or act on.
/// - OUT: `key_hash`, `token_hash`, `pb_user_id`, `snap_token`, session rows,
///   IP hashes and the Telegram chat id. Hashes and internal ids are not the
///   customer's to hold, and handing them out is an attack surface for no benefit.
/// - OUT: `admin_audit`. Whether operator actions reach the customer is a
///   SEPARATE open decision (docs/admin-surface.md Open items); this does not
///   pre-empt it.
///
/// Every query is bound to the cookie-resolved account id, so a customer can only
/// export their OWN rows.
pub async fn export_account_data(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;
    let id = account_id.hyphenated().to_string();

    // Account: no pb_user_id (an internal link), no password (elsewhere).
    let account =
        sqlx::query("SELECT id, status, is_operator, created_at FROM accounts WHERE id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await?;

    let wallet = sqlx::query("SELECT balance_idr, updated_at FROM wallets WHERE account_id = ?")
        .bind(&id)
        .fetch_optional(&pool)
        .await?;

    let ledger = sqlx::query(
        "SELECT delta_idr, reason, ref, balance_after, created_at
         FROM ledger WHERE account_id = ? ORDER BY id",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;

    // snap_token is deliberately NOT selected: it is a Midtrans credential.
    let topups = sqlx::query(
        "SELECT id, amount_idr, order_id, status, rail, created_at, settled_at
         FROM topups WHERE account_id = ? ORDER BY created_at",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;

    let usage_daily = sqlx::query(
        "SELECT day, input_tokens, cache_read_tokens, output_tokens, cost_idr
         FROM usage_daily WHERE account_id = ? ORDER BY day",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;

    let usage_events = sqlx::query(
        "SELECT id, model, input_tokens, cache_read_tokens, output_tokens, cost_idr, created_at
         FROM usage_events WHERE account_id = ? ORDER BY created_at",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;

    // Key METADATA only. key_hash is not selected.
    let keys = sqlx::query(
        "SELECT prefix, label, models, spend_limit_idr, token_limit, rate_limit_rpm,
                expires_at, last_used_at, revoked_at, created_at
         FROM api_keys WHERE account_id = ? ORDER BY created_at",
    )
    .bind(&id)
    .fetch_all(&pool)
    .await?;

    // Link STATE only - never the chat id, which is the other party's identifier.
    let telegram_linked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM telegram_links WHERE account_id = ?")
            .bind(&id)
            .fetch_one(&pool)
            .await?;

    // Each table is mapped explicitly rather than generically: a generic row ->
    // JSON pass would have to guess a column's type, and getting that wrong on a
    // money field is worse than a few extra lines. Every field here is named in
    // the doc's IN table and nowhere else.
    let account_json = json!({
        "id": account.try_get::<String, _>("id")?,
        "status": account.try_get::<String, _>("status")?,
        "is_operator": account.try_get::<bool, _>("is_operator")?,
        "created_at": account.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
    });

    let wallet_json = match wallet {
        Some(w) => json!({
            "balance_idr": w.try_get::<i64, _>("balance_idr")?,
            "updated_at": w.try_get::<chrono::DateTime<Utc>, _>("updated_at")?,
        }),
        None => serde_json::Value::Null,
    };

    let mut ledger_json = Vec::with_capacity(ledger.len());
    for r in ledger {
        ledger_json.push(json!({
            "delta_idr": r.try_get::<i64, _>("delta_idr")?,
            "reason": r.try_get::<String, _>("reason")?,
            "ref": r.try_get::<Option<String>, _>("ref")?,
            "balance_after": r.try_get::<i64, _>("balance_after")?,
            "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
        }));
    }

    let mut topups_json = Vec::with_capacity(topups.len());
    for r in topups {
        topups_json.push(json!({
            "id": r.try_get::<String, _>("id")?,
            "amount_idr": r.try_get::<i64, _>("amount_idr")?,
            "order_id": r.try_get::<String, _>("order_id")?,
            "status": r.try_get::<String, _>("status")?,
            "rail": r.try_get::<String, _>("rail")?,
            "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
            "settled_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("settled_at")?,
        }));
    }

    let mut daily_json = Vec::with_capacity(usage_daily.len());
    for r in usage_daily {
        daily_json.push(json!({
            "day": r.try_get::<String, _>("day")?,
            "input_tokens": r.try_get::<i64, _>("input_tokens")?,
            "cache_read_tokens": r.try_get::<i64, _>("cache_read_tokens")?,
            "output_tokens": r.try_get::<i64, _>("output_tokens")?,
            "cost_idr": r.try_get::<i64, _>("cost_idr")?,
        }));
    }

    let mut events_json = Vec::with_capacity(usage_events.len());
    for r in usage_events {
        events_json.push(json!({
            "id": r.try_get::<String, _>("id")?,
            "model": r.try_get::<String, _>("model")?,
            "input_tokens": r.try_get::<i64, _>("input_tokens")?,
            "cache_read_tokens": r.try_get::<i64, _>("cache_read_tokens")?,
            "output_tokens": r.try_get::<i64, _>("output_tokens")?,
            "cost_idr": r.try_get::<i64, _>("cost_idr")?,
            "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
        }));
    }

    let mut keys_json = Vec::with_capacity(keys.len());
    for r in keys {
        // models is stored as JSON text. Parse it to a real array so the export is
        // machine-readable; a value that does not parse is passed through as text
        // rather than dropped — losing a key's allowlist would misstate the account.
        let models_raw: String = r.try_get("models")?;
        let models_value: serde_json::Value =
            serde_json::from_str(&models_raw).unwrap_or(serde_json::Value::String(models_raw));
        keys_json.push(json!({
            "prefix": r.try_get::<String, _>("prefix")?,
            "label": r.try_get::<Option<String>, _>("label")?,
            "models": models_value,
            "spend_limit_idr": r.try_get::<i64, _>("spend_limit_idr")?,
            "token_limit": r.try_get::<i64, _>("token_limit")?,
            "rate_limit_rpm": r.try_get::<i64, _>("rate_limit_rpm")?,
            "expires_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("expires_at")?,
            "last_used_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("last_used_at")?,
            "revoked_at": r.try_get::<Option<chrono::DateTime<Utc>>, _>("revoked_at")?,
            "created_at": r.try_get::<chrono::DateTime<Utc>, _>("created_at")?,
        }));
    }

    Ok(Json(json!({
        "exported_at": Utc::now(),
        "note": "Metadata, not secrets. Credential hashes, session/IP records, the Telegram chat id and internal ids are deliberately excluded - see docs/data-retention.md.",
        "account": account_json,
        "wallet": wallet_json,
        "ledger": ledger_json,
        "topups": topups_json,
        "usage_daily": daily_json,
        "usage_events": events_json,
        "api_keys": keys_json,
        "telegram_linked": telegram_linked > 0,
    })))
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

/// WalletConfig is not part of the router state (State<SqlitePool>), so the config
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

/// True only for an explicit "production". Anything else - including a typo -
/// is not production, which is what keeps a typo off the live host.
fn is_production(midtrans_env: Option<&str>) -> bool {
    matches!(
        midtrans_env.map(str::trim),
        Some(env) if env.eq_ignore_ascii_case("production")
    )
}

/// The Snap host for MIDTRANS_ENV. Anything other than an explicit
/// "production" stays on sandbox, so a typo can never move real money.
fn snap_endpoint(midtrans_env: Option<&str>) -> &'static str {
    if is_production(midtrans_env) {
        SNAP_PRODUCTION_URL
    } else {
        SNAP_SANDBOX_URL
    }
}

/// Which Midtrans environment a top-up session is created against, as it goes
/// on the wire: exactly "sandbox" or "production".
///
/// Deliberately the SAME predicate that picks the Snap host (`snap_endpoint`
/// above), not a second read of MIDTRANS_ENV: the response field and the host
/// the request is actually sent to are two views of one decision, so they
/// cannot drift into disagreeing with each other.
fn midtrans_environment(midtrans_env: Option<&str>) -> &'static str {
    if is_production(midtrans_env) {
        "production"
    } else {
        "sandbox"
    }
}

/// The Snap host this process will actually POST to.
///
/// `override_url` (from MIDTRANS_SNAP_URL) lets a live test point the handler at
/// a loopback stub instead of Midtrans, so the success path below the Snap call
/// is reachable without a network double in production code.
///
/// REFUSED IN PRODUCTION, and that is the whole point of the gate: this variable
/// redirects a call that creates a payment session, so a stray value must never
/// be able to send a customer's top-up to an attacker's host. When MIDTRANS_ENV
/// is an explicit "production" the override is ignored entirely - fail closed,
/// not "warn and continue".
///
/// Pure on purpose: both inputs are parameters, so the decision is testable
/// without a database, without a socket, and without racing a sibling test over
/// process-global env vars. The only env reads are at the call site.
fn resolve_snap_endpoint(
    midtrans_env: Option<&str>,
    override_url: Option<&str>,
) -> Cow<'static, str> {
    let default = snap_endpoint(midtrans_env);
    if is_production(midtrans_env) {
        return Cow::Borrowed(default);
    }

    match override_url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => Cow::Owned(url.to_string()),
        None => Cow::Borrowed(default),
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
        "SELECT count(*) AS count FROM topups WHERE account_id = ? AND status = 'settled'",
    )
    .bind(account_id.hyphenated())
    .fetch_one(&state.pool)
    .await?;

    let settled_count: i64 = past_settled.try_get("count")?;
    check_deposit_limit(payload.amount_idr, settled_count, wallet)?;

    let server_key = env::var("MIDTRANS_SERVER_KEY").map_err(|_| {
        AppError::Internal("MIDTRANS_SERVER_KEY environment variable is not configured".to_string())
    })?;
    // MIDTRANS_SNAP_URL is a test seam and is ignored in production (see
    // resolve_snap_endpoint); it is read here, at the call site, so the decision
    // function itself stays pure.
    // Read ONCE: both the Snap host and the environment reported to the browser
    // are derived from this same value, so the two can never disagree.
    let midtrans_env = env::var("MIDTRANS_ENV").ok();
    let endpoint = resolve_snap_endpoint(
        midtrans_env.as_deref(),
        env::var("MIDTRANS_SNAP_URL").ok().as_deref(),
    );
    let environment = midtrans_environment(midtrans_env.as_deref());

    let topup_id = Uuid::new_v4();
    let order_id = format!("topup_{}", topup_id);

    // accounts stores only the PocketBase record id; the email lives in PocketBase.
    let pb_user_id: String = sqlx::query("SELECT pb_user_id FROM accounts WHERE id = ?")
        .bind(account_id.hyphenated())
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
        create_snap_transaction(&snap_http, &server_key, &endpoint, &snap_payload).await?;

    // `created_at` has no default: the Postgres schema defaulted it to `now()`,
    // and that default was removed so that no SQL-side time can ever be written
    // in the other format (plan section 4.6, rule 2).
    //
    // `rail` is named explicitly and is `'midtrans'` because Midtrans QRIS is the
    // only rail implemented. The column is NOT NULL with no default precisely so
    // that a future second rail cannot inherit this literal by omission.
    //
    // THE CAP IS RE-CHECKED HERE, INSIDE THIS TRANSACTION, and that is the fix.
    //
    // The check at the top of this handler is a COUNT on the pool, and this INSERT
    // is the row it governs - with a Midtrans Snap call and a PocketBase lookup in
    // between. So concurrent callers all read the same count, all saw room, and all
    // created a top-up; abuse.rs measures that at fourteen rows against a cap of
    // five.
    //
    // The obvious fix - one transaction from the first check to the INSERT - is NOT
    // available, and deliberately not attempted: it would hold a SQLite WRITE LOCK
    // across a network round trip to a payment provider, and every other top-up in
    // the process would serialise behind it.
    //
    // So the cap is checked TWICE, and the two checks have different jobs. The early
    // one is a cheap gate that refuses an over-cap account BEFORE a Snap session is
    // created - which is the whole reason it is first. This one is authoritative
    // and atomic, because there is nothing between it and the INSERT but this
    // comment: SQLite serialises writers, so a second caller either sees the first
    // caller's row or is still waiting for the lock.
    //
    // WHAT THAT COSTS, stated rather than glossed: a caller that passes the early
    // check and then loses this one has already created a Snap transaction with no
    // matching row, so its payment would be orphaned. That is the same state a Snap
    // call that succeeded and whose INSERT then failed already produces, and the
    // window for it is a burst by an account already at its cap - the case the
    // early gate exists to stop in the first place. Trading an unbounded abuse
    // window for that is the right way round, but it is a trade and not a free win.
    //
    // ONE instant for both the cap boundary and the row's created_at, read inside
    // the transaction rather than beside it. They must agree: a row stamped slightly
    // after the check that admitted it could in principle fall on the far side of a
    // window edge, and then the count that refused the next caller and the row that
    // was admitted would disagree about what "in the window" means.
    let now = Utc::now();
    let mut tx = crate::db::begin_immediate(&state.pool).await?;

    crate::abuse::enforce_creation_cap_in(
        &mut tx,
        "topups",
        crate::abuse::topup_window(),
        state.config.limits.topup_per_hour,
        account_id,
        now,
    )
    .await?;

    sqlx::query(
        "INSERT INTO topups (id, account_id, amount_idr, order_id, status, snap_token, rail, created_at) VALUES (?, ?, ?, ?, 'pending', ?, 'midtrans', ?)",
    )
    .bind(topup_id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(payload.amount_idr)
    .bind(&order_id)
    .bind(&snap_token)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

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
            environment: environment.to_string(),
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

    /// The environment on the wire is the SAME decision that picks the Snap host.
    ///
    /// Not "both happen to agree today": the assertion is that the two move
    /// together across every input the endpoint test above pins, including the
    /// typos. A second, independent read of MIDTRANS_ENV is what this forbids.
    #[test]
    fn reported_environment_tracks_the_endpoint_that_was_chosen() {
        for raw in [
            None,
            Some("production"),
            Some(" production "),
            Some("Production"),
            Some("sandbox"),
            Some("prod"),
            Some(""),
        ] {
            let environment = midtrans_environment(raw);
            assert!(
                environment == "sandbox" || environment == "production",
                "{raw:?} produced {environment:?}; the field is exactly one of the \
                 two documented values"
            );

            let host_is_live = snap_endpoint(raw) == SNAP_PRODUCTION_URL;
            assert_eq!(
                environment == "production",
                host_is_live,
                "{raw:?} reported {environment:?} while the Snap host was \
                 {} - the browser would be told the wrong environment",
                snap_endpoint(raw)
            );
        }
    }

    /// The response SHAPE, serialized the way axum writes it.
    ///
    /// A response struct, so this is the whole contract: the three existing keys
    /// keep their names and types and `environment` is a string. Nothing else is
    /// asserted, because adding a field must not disturb the keys a deployed
    /// website already reads.
    #[test]
    fn create_topup_response_carries_the_environment_as_a_string() {
        let body = serde_json::to_value(CreateTopupResponse {
            topup_id: Uuid::nil(),
            order_id: "topup_x".to_string(),
            snap_token: Some("snap-token".to_string()),
            environment: midtrans_environment(Some("production")).to_string(),
        })
        .expect("the response must serialize");

        assert_eq!(
            body,
            json!({
                "topup_id": Uuid::nil(),
                "order_id": "topup_x",
                "snap_token": "snap-token",
                "environment": "production",
            })
        );
        assert!(
            body["environment"].is_string(),
            "environment must be a JSON string, not a bool or a null: {body}"
        );
    }

    /// The MIDTRANS_SNAP_URL gate, pure: both inputs are arguments, so this needs
    /// no database, no socket and no env mutation - and cannot race a sibling
    /// test over a process-global variable.
    #[test]
    fn snap_endpoint_override_applies_only_outside_production() {
        const STUB: &str = "http://127.0.0.1:9/snap/v1/transactions";

        // (a) Honoured whenever MIDTRANS_ENV is not an explicit production.
        assert_eq!(resolve_snap_endpoint(None, Some(STUB)), STUB);
        assert_eq!(resolve_snap_endpoint(Some("sandbox"), Some(STUB)), STUB);
        assert_eq!(
            resolve_snap_endpoint(Some(" staging "), Some(STUB)),
            STUB,
            "anything that is not explicitly production may be overridden"
        );
        assert_eq!(
            resolve_snap_endpoint(Some("prod"), Some(STUB)),
            STUB,
            "a typo is not production, so the override still applies - the same \
             rule that keeps a typo off the live host"
        );

        // (b) IGNORED in production. This is the safety half: the variable
        // redirects a payment-session call, so production must fail closed.
        assert_eq!(
            resolve_snap_endpoint(Some("production"), Some(STUB)),
            SNAP_PRODUCTION_URL,
            "a production process must never dial an overridden Snap host"
        );
        assert_eq!(
            resolve_snap_endpoint(Some(" production "), Some(STUB)),
            SNAP_PRODUCTION_URL,
            "the gate is trimmed and case-insensitive, like snap_endpoint"
        );
        assert_eq!(
            resolve_snap_endpoint(Some("Production"), Some(STUB)),
            SNAP_PRODUCTION_URL
        );

        // No override: unchanged behaviour, borrowed not allocated.
        assert_eq!(resolve_snap_endpoint(None, None), SNAP_SANDBOX_URL);
        assert_eq!(
            resolve_snap_endpoint(Some("sandbox"), Some("   ")),
            SNAP_SANDBOX_URL,
            "a blank override is not a host"
        );
        assert_eq!(
            resolve_snap_endpoint(Some("production"), None),
            SNAP_PRODUCTION_URL
        );
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

    /// The window parser, without a database. The live test covers the whole
    /// handler; this one keeps the malformed-date contract checked in the
    /// default suite, where no Postgres is running.
    #[test]
    fn usage_bounds_are_optional_dates_and_a_bad_one_names_its_field() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 1, 31).unwrap();

        // docs/server/api-spec.md:97 writes the params as `?from=&to=`: an
        // empty value means "not supplied", not "parse this".
        assert_eq!(parse_usage_day(None, "from").unwrap(), None);
        assert_eq!(parse_usage_day(Some(""), "from").unwrap(), None);
        assert_eq!(parse_usage_day(Some("  "), "to").unwrap(), None);
        assert_eq!(
            parse_usage_day(Some("2026-01-31"), "from").unwrap(),
            Some(day)
        );

        // docs/error-model.md rule 5: name the offending field.
        for (value, field) in [
            ("not-a-date", "from"),
            ("2026-02-30", "from"),
            ("31/01/2026", "to"),
        ] {
            let err = parse_usage_day(Some(value), field).unwrap_err();
            assert_eq!(
                err.status_code(),
                StatusCode::UNPROCESSABLE_ENTITY,
                "{value}"
            );
            assert_eq!(err.details(), Some(json!({ "field": field })), "{value}");
        }
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

    use crate::db::{credit_topup_transaction, debit_usage_transaction};
    use crate::routes::events::RealtimeHub;
    use crate::routes::test_env::{EnvGuard, EnvLock};
    use crate::test_support::{self, TestDb};
    use axum::body::to_bytes;
    use std::sync::Arc;

    /// The key both Snap-failure tests install: deliberately invalid, so the
    /// outcome cannot depend on whatever the developer shell exports.
    const INVALID_SERVER_KEY: &str = "SB-Mid-server-INVALID-LIVE-TEST-KEY";

    /// The reconciliation check from docs/observability.md, scoped to one
    /// account: wallets.balance_idr must equal SUM(ledger.delta_idr).
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

    /// An account with a live session cookie, built the way the login path
    /// builds one: an accounts row, a sessions row holding only the SHA-256 of
    /// the token, and the zero-balance wallets row.
    struct LiveAccount {
        account_id: Uuid,
        token: String,
    }

    async fn live_account(pool: &SqlitePool) -> LiveAccount {
        // Ported from the Postgres original, which leaned on column DEFAULTS for
        // `accounts.id`, `accounts.created_at` and `wallets.updated_at`. The strict
        // SQLite schema has none of them (plan section 4.1, correction 1), so every
        // NOT NULL column is bound from Rust - the defect the compiler cannot see,
        // because the INSERT still type-checks and only fails at runtime.
        let account_id = test_support::account(pool).await;
        test_support::wallet(pool, account_id).await;

        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(crate::routes::hash_token(&token))
        .bind(now + chrono::Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");

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
    async fn settle_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) -> Uuid {
        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        // Ported: `topups.id` is TEXT, so RETURNING decodes through `Hyphenated`
        // rather than a bare 16-byte `Uuid` (the failure is
        // `ParseByteLength { len: 36 }` - the hyphenated string arriving where a raw
        // uuid was expected).
        let topup_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at)
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(topup_id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(amount_idr)
        .bind(&order_id)
        .bind(Utc::now())
        .execute(pool)
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
    fn live_app_state(pool: SqlitePool) -> AppState {
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

    #[tokio::test]
    async fn live_get_me_returns_the_real_balance_and_the_documented_fields() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
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
        db.close().await;
        outcome.expect("the get_me assertions panicked");
    }

    async fn get_me_assertions(
        pool: SqlitePool,
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
        let stored: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
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
            [
                "account_id",
                "balance_idr",
                "is_operator",
                "status",
                "telegram_linked",
                "usage_today"
            ],
            "docs/server/api-spec.md GET /api/me: {body}"
        );
        assert_eq!(body["account_id"], json!(account_id));
        assert_eq!(body["status"], json!("active"));
        assert_eq!(body["telegram_linked"], json!(false));
        // The fixture accounts are ordinary customers, so the operator flag the
        // admin UI gates on must read false. Cross-checked against the row.
        let stored_operator: bool =
            sqlx::query_scalar("SELECT is_operator FROM accounts WHERE id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read the account row");
        assert_eq!(body["is_operator"], json!(stored_operator));

        let usage = body["usage_today"]
            .as_object()
            .expect("usage_today is an object");
        assert_eq!(usage.len(), 4, "usage_today: {body}");
        for field in [
            "input_tokens",
            "cache_read_tokens",
            "output_tokens",
            "cost_idr",
        ] {
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

    #[tokio::test]
    async fn live_get_usage_keeps_the_three_token_classes_separate() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(get_usage_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the get_usage assertions panicked");
    }

    async fn create_api_key(pool: &SqlitePool, account_id: Uuid) -> Uuid {
        // Ported: `api_keys.id` is TEXT, so it decodes through `Hyphenated`.
        let key_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO api_keys (id, account_id, key_hash, prefix, created_at)
             VALUES (?, ?, ?, 'apk_test', ?)",
        )
        .bind(key_id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(format!("test_hash_{}", Uuid::new_v4().simple()))
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("create api key");

        key_id
    }

    async fn insert_usage(
        pool: &SqlitePool,
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
            ) VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(account_id.hyphenated())
        .bind(key_id.hyphenated())
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
        pool: SqlitePool,
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

        insert_usage(
            &pool,
            account_id,
            key_a,
            today,
            (in_a, cache_a, out_a, cost_a),
        )
        .await;
        insert_usage(
            &pool,
            account_id,
            key_a,
            yesterday,
            (in_y, cache_y, out_y, cost_y),
        )
        .await;
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

        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(None, None),
        ))
        .await;
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
        for field in [
            "input_tokens",
            "cache_read_tokens",
            "output_tokens",
            "cost_idr",
        ] {
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
        assert_eq!(
            yesterday_row["input_tokens"],
            json!(in_y),
            "{yesterday_row}"
        );
        assert_eq!(
            yesterday_row["cache_read_tokens"],
            json!(cache_y),
            "{yesterday_row}"
        );
        assert_eq!(
            yesterday_row["output_tokens"],
            json!(out_y),
            "{yesterday_row}"
        );
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

    /// Builds the extractor the router hands get_usage from `?from=..&to=..`.
    fn usage_query(from: Option<&str>, to: Option<&str>) -> Query<UsageQuery> {
        Query(UsageQuery {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        })
    }

    // -----------------------------------------------------------------------
    // 2b. get_usage?from=&to= - docs/server/api-spec.md:97
    //
    // The gap: the spec advertises `?from=&to=` and the handler took no query
    // extractor at all, answering every request with the last 30 days. A client
    // that asked a bounded question got a plausible answer to a different one.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_get_usage_honours_the_documented_from_to_range() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(usage_range_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the usage range assertions panicked");
    }

    async fn usage_range_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        token: String,
        other_account_id: Uuid,
    ) {
        let today = Utc::now().date_naive();
        let key = create_api_key(&pool, account_id).await;
        let other_key = create_api_key(&pool, other_account_id).await;

        // Four consecutive days, each carrying a value only that day has, so a
        // row from OUTSIDE the asked-for window is identifiable by its numbers
        // and not merely by its date.
        let days: Vec<chrono::NaiveDate> =
            (0..4).map(|d| today - chrono::Duration::days(d)).collect();
        let values = [101i64, 202i64, 303i64, 404i64];
        for (day, value) in days.iter().zip(values) {
            insert_usage(&pool, account_id, key, *day, (value, value, value, value)).await;
        }

        // The SAME days on the other account, with numbers that cannot be
        // confused with the primary's: a lost account scope shows up here.
        for day in &days {
            insert_usage(
                &pool,
                other_account_id,
                other_key,
                *day,
                (9_000_001, 9_000_002, 9_000_003, 9_000_004),
            )
            .await;
        }

        // --- A bounded range returns ONLY the days inside it. ---
        // days[3] (oldest) ..= days[1]: days 3, 2 and 1. days[0] - the most
        // recent, and the row a `LIMIT 30` would return first - is OUTSIDE and
        // must not appear, which is precisely what the old handler got wrong.
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(Some(&days[3].to_string()), Some(&days[1].to_string())),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let rows = body.as_array().expect("daily buckets are an array");
        assert_eq!(
            rows.len(),
            3,
            "docs/server/api-spec.md:97 - a bounded range must return ONLY the days \
             inside it, so today ({}) is excluded by from={} to={}. body: {body}",
            days[0],
            days[3],
            days[1]
        );
        assert_eq!(rows[0]["day"], json!(days[1]), "day DESC: {body}");
        assert_eq!(rows[1]["day"], json!(days[2]), "day DESC: {body}");
        assert_eq!(rows[2]["day"], json!(days[3]), "day DESC: {body}");
        assert_eq!(rows[0]["input_tokens"], json!(202), "{body}");
        assert_eq!(rows[1]["input_tokens"], json!(303), "{body}");
        assert_eq!(rows[2]["input_tokens"], json!(404), "{body}");

        // The three token classes stay separate inside a range too.
        let mut keys: Vec<&str> = rows[0]
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
            "three token classes, never summed: {body}"
        );

        // TENANCY: a range must not widen the account scope.
        let rendered = body.to_string();
        for leaked in ["9000001", "9000002", "9000003", "9000004"] {
            assert!(
                !rendered.contains(leaked),
                "another account usage leaked into a ranged response: {body}"
            );
        }

        // --- BOTH BOUNDS ARE INCLUSIVE. ---
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(Some(&days[1].to_string()), Some(&days[1].to_string())),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let rows = body.as_array().expect("an array");
        assert_eq!(
            rows.len(),
            1,
            "from == to must include that one day on both ends: {body}"
        );
        assert_eq!(rows[0]["day"], json!(days[1]), "{body}");
        assert_eq!(rows[0]["input_tokens"], json!(202), "{body}");

        // --- A range with no rows is EMPTY, never a fallback. ---
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(Some("2001-01-01"), Some("2001-01-31")),
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "an empty range is a valid answer, not an error: {body}"
        );
        assert_eq!(
            body,
            json!([]),
            "a range excluding every row must return an EMPTY array - never an error \
             and never the silent last-30-days fallback this test exists to forbid: {body}"
        );

        // --- A range that starts after today is still empty, not a fallback. ---
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(
                Some(&(today + chrono::Duration::days(10)).to_string()),
                Some(&(today + chrono::Duration::days(20)).to_string()),
            ),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body, json!([]), "a future range is empty: {body}");

        // --- No params: the documented default window still holds. ---
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(None, None),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        let rows = body.as_array().expect("an array");
        assert_eq!(
            rows.len(),
            4,
            "the default window must still cover the last 30 days: {body}"
        );
    }

    /// A malformed date must be REJECTED, naming the field - not silently
    /// ignored, which is exactly how the original bug behaved.
    #[tokio::test]
    async fn live_get_usage_rejects_a_malformed_date_naming_the_field() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(usage_validation_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the usage validation assertions panicked");
    }

    async fn usage_validation_assertions(pool: SqlitePool, account_id: Uuid, token: String) {
        let key = create_api_key(&pool, account_id).await;
        insert_usage(
            &pool,
            account_id,
            key,
            Utc::now().date_naive(),
            (7, 8, 9, 10),
        )
        .await;

        for (from, to, field) in [
            (Some("not-a-date"), None, "from"),
            (None, Some("01/03/2026"), "to"),
            (Some("2026-02-30"), None, "from"),
        ] {
            let (status, body) = respond(get_usage(
                State(pool.clone()),
                cookie_header(&token),
                usage_query(from, to),
            ))
            .await;

            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "docs/error-model.md rule 5: from={from:?} to={to:?} is not a date and must be \
                 REJECTED, not silently ignored. body: {body}"
            );
            assert_eq!(body["error"]["code"], json!("validation_failed"), "{body}");
            assert_eq!(
                body["error"]["details"]["field"],
                json!(field),
                "the error must name the offending field: {body}"
            );
        }

        // The valid path is untouched by the refusals above.
        let (status, body) = respond(get_usage(
            State(pool.clone()),
            cookie_header(&token),
            usage_query(None, None),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body.as_array().map(Vec::len), Some(1), "{body}");
    }

    // -----------------------------------------------------------------------
    // 3. get_topups
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_get_topups_returns_only_this_accounts_topups() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let outcome = tokio::spawn(get_topups_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            other.account_id,
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the get_topups assertions panicked");
    }

    async fn get_topups_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        token: String,
        other_account_id: Uuid,
    ) {
        // One SETTLED top-up through the real money path, and one PENDING row
        // created a minute earlier so the ordering is deterministic.
        let settled_id = settle_topup(&pool, account_id, 50_000).await;

        let pending_order = format!("test_pending_{}", Uuid::new_v4().simple());
        // Ported: SQLite has no `interval` arithmetic and its `now()` emits the
        // space-separated form the schema GLOB CHECK refuses, so the earlier instant
        // is computed in Rust. `id` and `created_at` are bound because the strict
        // schema has no DEFAULT for either.
        let pending_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(pending_id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(25_000)
        .bind(&pending_order)
        .bind(Utc::now() - chrono::Duration::minutes(1))
        .execute(&pool)
        .await
        .expect("create a pending topup");

        let other_order = format!("test_other_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(other_account_id.hyphenated())
        .bind(1_234_567)
        .bind(&other_order)
        .bind(Utc::now())
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
        const DOCUMENTED: [&str; 6] = [
            "amount_idr",
            "created_at",
            "id",
            "order_id",
            "settled_at",
            "status",
        ];
        const ALLOWED_STATUS: [&str; 5] = ["pending", "settled", "denied", "expired", "refunded"];

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
    // 3b. get_recent_usage - the per-request history (usage_events)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_get_recent_usage_returns_only_this_accounts_requests() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;
        let other = live_account(&pool).await;

        let primary_key = test_support::api_key(&pool, primary.account_id).await;
        let other_key = test_support::api_key(&pool, other.account_id).await;

        let outcome = tokio::spawn(get_recent_usage_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
            primary_key,
            other.account_id,
            other_key,
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the get_recent_usage assertions panicked");
    }

    async fn get_recent_usage_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        token: String,
        key_id: Uuid,
        other_account_id: Uuid,
        other_key_id: Uuid,
    ) {
        // The requests are written through the REAL settlement path, so
        // `usage_events` is populated the way production populates it.
        test_support::fund(&pool, account_id, 1_000_000).await;
        debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            "flash",
            100,
            10,
            50,
            111,
            Some("r1"),
            0,
        )
        .await
        .expect("first settlement");

        // A second request by the OTHER account must never appear in this list.
        test_support::fund(&pool, other_account_id, 1_000_000).await;
        debit_usage_transaction(
            &pool,
            other_account_id,
            Some(other_key_id),
            "flash",
            999,
            0,
            999,
            222,
            Some("r2"),
            0,
        )
        .await
        .expect("other account settlement");

        let (status, body) = respond(get_recent_usage(
            State(pool.clone()),
            cookie_header(&token),
            Query(LimitQuery { limit: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let rows = body.as_array().expect("a recent-usage list is an array");
        assert_eq!(rows.len(), 1, "only this account's requests: {body}");
        let row = &rows[0];

        // The documented shape, exactly - no prompt, no completion, no key hash.
        let mut keys: Vec<&str> = row
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "api_key_id",
                "cache_read_tokens",
                "cost_idr",
                "created_at",
                "id",
                "input_tokens",
                "model",
                "output_tokens",
            ],
            "docs/server/api-spec.md GET /api/usage/recent: {body}"
        );
        assert_eq!(row["model"], json!("flash"));
        assert_eq!(row["input_tokens"], json!(100));
        assert_eq!(row["cache_read_tokens"], json!(10));
        assert_eq!(row["output_tokens"], json!(50));
        assert_eq!(row["cost_idr"], json!(111));
        assert_eq!(row["api_key_id"], json!(key_id));
        // The event is tied to the reservation the ledger rows carry.
        let text = body.to_string();
        for forbidden in ["token_hash", "key_hash", "apk_live", "prompt", "completion"] {
            assert!(
                !text.contains(forbidden),
                "the history leaked {forbidden}: {text}"
            );
        }

        // An unauthenticated call is refused before any row is read.
        let (status, _) = respond(get_recent_usage(
            State(pool.clone()),
            HeaderMap::new(),
            Query(LimitQuery { limit: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // 4. create_topup
    //
    // Midtrans Snap endpoint is hardcoded in snap_endpoint and there is no
    // local Snap fake, so a handler-level SUCCESS path cannot be driven here.
    // What IS asserted: the handler persists nothing when Snap cannot be
    // reached, and the client never fabricates a token.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_create_topup_persists_nothing_when_snap_cannot_be_reached() {
        // Held for the WHOLE test body. The assertions below run in a spawned
        // task and write MIDTRANS_SERVER_KEY, and this lock is the half that
        // stops that write from interleaving with another test's - in this
        // module or in routes::webhooks, which installs its own key.
        let _env = EnvLock::acquire();

        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(create_topup_failure_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the create_topup assertions panicked");
    }

    #[tokio::test]
    async fn live_create_topup_without_a_server_key_is_an_internal_failure_persisting_nothing() {
        // Held for the WHOLE test body: the spawned task REMOVES
        // MIDTRANS_SERVER_KEY, and this lock keeps that write from interleaving
        // with another test's - in this module or in routes::webhooks, which
        // installs its own key.
        let _env = EnvLock::acquire();

        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        // The same contract as an unreachable Snap, reached a different way:
        // the configuration itself is MISSING, which in production is a deploy
        // defect. The handler must still answer 500 internal_error, persist
        // nothing, and never leak the missing variable's name to the client.
        let outcome = tokio::spawn(async move {
            let _guard = EnvGuard::remove("MIDTRANS_SERVER_KEY");
            let state = live_app_state(pool.clone());
            let (status, body) = respond(create_topup(
                State(state),
                cookie_header(&primary.token),
                Json(CreateTopupRequest {
                    amount_idr: configured_wallet().min_first_deposit as i64,
                }),
            ))
            .await;

            assert_eq!(
                status,
                StatusCode::INTERNAL_SERVER_ERROR,
                "a top-up that cannot even find its server key is an internal \
                 failure, not a created top-up. body: {body}"
            );
            assert_eq!(body["error"]["code"], json!("internal_error"), "{body}");
            assert!(
                !serde_json::to_string(&body)
                    .expect("the body is json")
                    .contains("MIDTRANS_SERVER_KEY"),
                "the missing-variable name must not reach the client: {body}"
            );
        });

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the missing-server-key assertions panicked");
    }

    async fn create_topup_failure_assertions(pool: SqlitePool, account_id: Uuid, token: String) {
        let wallet = configured_wallet();
        let amount = wallet.min_first_deposit as i64;

        // An explicitly INVALID server key, so the outcome cannot depend on
        // whatever the developer shell exports. Snap answers 401 to it, and an
        // unreachable network fails the same way - either is a failure to
        // obtain a token, and neither may leave a row behind. The guard puts the
        // PREVIOUS key back when it drops (panic included), instead of leaking
        // this one into every later test in the process.
        let _server_key = EnvGuard::set("MIDTRANS_SERVER_KEY", INVALID_SERVER_KEY);

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
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count topups");
        assert_eq!(
            rows, 0,
            "a failed Snap call must not persist a topups row (no token, no pending row)"
        );

        let settled: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = ? AND status = 'settled'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count settled");
        assert_eq!(settled, 0, "nothing may be settled or paid");

        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
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

    #[tokio::test]
    async fn live_deposit_limits_follow_the_real_settled_history() {
        // Same reason as the Snap-failure test above: this one also reaches Snap
        // with an invalid key, so its write is serialised and restored.
        let _env = EnvLock::acquire();

        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(deposit_limit_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the deposit limit assertions panicked");
    }

    async fn deposit_limit_assertions(pool: SqlitePool, account_id: Uuid, token: String) {
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

        // The key is restored when this function returns, and the caller holds
        // the env lock across it, so nothing else can see it in between.
        let _server_key = EnvGuard::set("MIDTRANS_SERVER_KEY", INVALID_SERVER_KEY);

        // --- No settled history: the FIRST-deposit minimum applies. ---
        let history: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = ? AND status = 'settled'",
        )
        .bind(account_id.hyphenated())
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
            sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count topups");
        assert_eq!(after_refusal, 0, "a refused amount must not create a row");

        // --- ONE SETTLED TOP-UP: the lower minimum now applies. ---
        settle_topup(&pool, account_id, wallet.min_first_deposit as i64).await;

        let history: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM topups WHERE account_id = ? AND status = 'settled'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count settled history");
        assert_eq!(
            history, 1,
            "the fixture must have exactly one settled top-up"
        );

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
            sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = ?")
                .bind(account_id.hyphenated())
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

    /// A rejection whose body carries `status_message` (and no `error_messages`)
    /// must surface that detail, and a 2xx whose body is not JSON must be its
    /// own error rather than a parse panic. Covers account.rs:803-806 and
    /// account.rs:815-817 - the two arms the 401 test above never reaches.
    #[tokio::test]
    async fn snap_client_surfaces_status_message_and_refuses_a_non_json_success() {
        let payload = build_snap_payload("topup_x", 50_000, None);

        // status_message is the documented fallback when error_messages is absent.
        let endpoint = snap_stub(http_response(
            "402 Payment Required",
            r#"{"status_message": "declined by the fraud engine"}"#,
        ))
        .await;
        let err = create_snap_transaction(&snap_client(), "key", &endpoint, &payload)
            .await
            .expect_err("a 402 is not a token");
        let msg = format!("{err}");
        assert!(
            msg.contains("declined by the fraud engine"),
            "the status_message detail must surface in the error: {msg}"
        );

        // A 2xx whose body is not JSON is refused with its own message, never
        // turned into a token and never a panic.
        let endpoint = snap_stub(http_response("200 OK", "<html>not json</html>")).await;
        let err = create_snap_transaction(&snap_client(), "key", &endpoint, &payload)
            .await
            .expect_err("a non-JSON 2xx is not a token");
        let msg = format!("{err}");
        assert!(
            msg.contains("not valid JSON"),
            "a non-JSON 2xx must be its own error: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // account_email: best-effort BY CONTRACT, so every PocketBase failure mode
    // must degrade to None and a real address must come back trimmed. The stub
    // is the same one-shot listener the Snap tests use; account_email only ever
    // reads the ORIGIN, so the path suffix is stripped.
    // -----------------------------------------------------------------------

    /// A loopback PocketBase origin, derived from a `snap_stub` endpoint.
    async fn pb_stub(response: String) -> String {
        snap_stub(response)
            .await
            .trim_end_matches("/snap/v1/transactions")
            .to_string()
    }

    #[tokio::test]
    async fn account_email_reads_the_address_from_pocketbase_and_trims_it() {
        let _env = EnvLock::acquire();
        let origin = pb_stub(http_response(
            "200 OK",
            r#"{"email":"  ada@example.com  "}"#,
        ))
        .await;
        let _guard = EnvGuard::set("POCKETBASE_URL", &origin);

        let email = account_email(&snap_client(), "pb_user_1").await;
        assert_eq!(
            email.as_deref(),
            Some("ada@example.com"),
            "the address must be the trimmed value PocketBase returned"
        );
    }

    #[tokio::test]
    async fn account_email_degrades_to_none_on_every_pocketbase_failure_mode() {
        let _env = EnvLock::acquire();
        let client = snap_client();

        let refused = pb_stub(http_response("404 Not Found", r#"{"status":404}"#)).await;
        let not_json = pb_stub(http_response("200 OK", "<html>not json</html>")).await;
        let no_email_field = pb_stub(http_response("200 OK", r#"{"username":"ada"}"#)).await;

        let mut env_vars = EnvGuard::set("POCKETBASE_URL", &refused);
        assert_eq!(
            account_email(&client, "pb_user_1").await,
            None,
            "a non-2xx record response carries no address"
        );

        env_vars.also("POCKETBASE_URL", &not_json);
        assert_eq!(
            account_email(&client, "pb_user_1").await,
            None,
            "a non-JSON record response carries no address"
        );

        env_vars.also("POCKETBASE_URL", &no_email_field);
        assert_eq!(
            account_email(&client, "pb_user_1").await,
            None,
            "a record without an email field carries no address"
        );
    }

    /// An account with no wallets row yet exports its wallet as JSON null rather
    /// than failing: the export describes what EXISTS. Covers account.rs:511.
    #[tokio::test]
    async fn the_export_of_an_account_with_no_wallet_is_null_not_an_error() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        // The accounts row + a live session, WITHOUT the wallets row that
        // `live_account` adds - the whole point of this test.
        let account_id = test_support::account(&pool).await;
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(crate::routes::hash_token(&token))
        .bind(now + chrono::Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await
        .expect("create session");

        let (status, body) = respond(export_account_data(
            State(pool.clone()),
            cookie_header(&token),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert!(
            body["wallet"].is_null(),
            "a missing wallets row must export as null, not an error: {body}"
        );
        db.close().await;
    }

    /// The success path when Snap answers a token but NO redirect_url: the row
    /// is still created and the 201 still carries the token. A subscriber is
    /// installed so the `info!` field expressions (account.rs:936) are actually
    /// evaluated - tracing skips them when no subscriber is active, which left
    /// that line uncovered even though the success path itself ran.
    #[tokio::test]
    async fn live_create_topup_success_without_a_redirect_url_still_creates_the_row() {
        let _env = EnvLock::acquire();
        let _capture =
            tracing::subscriber::set_default(tracing_subscriber::fmt().with_ansi(false).finish());
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account = live_account(&pool).await;

        let endpoint = snap_stub(http_response(
            "200 OK",
            r#"{"token":"snap-token-no-redirect"}"#,
        ))
        .await;

        let mut env_vars = EnvGuard::set("MIDTRANS_ENV", "sandbox");
        env_vars.also("MIDTRANS_SNAP_URL", &endpoint);
        env_vars.also("MIDTRANS_SERVER_KEY", "SB-Mid-server-LIVE-TEST-SUCCESS");

        let state = live_app_state(pool.clone());
        let amount = configured_wallet().min_first_deposit as i64;

        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&account.token),
            Json(CreateTopupRequest { amount_idr: amount }),
        ))
        .await;

        drop(env_vars);

        assert_eq!(
            status,
            StatusCode::CREATED,
            "a token without a redirect_url is still a created top-up: {body}"
        );
        assert_eq!(
            body["snap_token"],
            json!("snap-token-no-redirect"),
            "{body}"
        );
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 7. create_topup: the in-handler 429, and the SUCCESS path
    //
    // Every create_topup test above drove the handler to a FAILURE, so the
    // branch that persists the 'pending' row and answers 201 CREATED had never
    // executed in this suite. The tests below close that, and the third pins the
    // row contract against the migrated schema.
    //
    // The cap test needs no Snap at all, and is green.
    //
    // The success test reaches the INSERT through the `MIDTRANS_SNAP_URL` seam:
    // `resolve_snap_endpoint` lets the process point at the `snap_stub` below,
    // which answers exactly as Snap does. No network is faked - create_topup
    // still runs its real reqwest client against a real socket; only the peer it
    // dials changes. The seam is refused when MIDTRANS_ENV is production, and
    // the pure test above pins that.
    // -----------------------------------------------------------------------

    /// Seeds `count` topups rows for the account, each stamped `created_at`
    /// inside the `topup_per_hour` window.
    ///
    /// Written directly rather than through create_topup: the subject is the
    /// handler's decision on EXISTING history, and manufacturing that history
    /// with the very code path under test would be circular. `created_at` is
    /// pinned 90s back so the rows sit unambiguously inside the rolling window.
    async fn seed_topups_inside_the_cap_window(pool: &SqlitePool, account_id: Uuid, count: i64) {
        let created_at = Utc::now() - chrono::Duration::seconds(90);
        for _ in 0..count {
            let order_id = format!("test_cap_{}", Uuid::new_v4().simple());
            sqlx::query(
                "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
                 VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(1_000_000_i64)
            .bind(&order_id)
            .bind(created_at)
            .execute(pool)
            .await
            .expect("seed a topup inside the cap window");
        }
    }

    #[tokio::test]
    async fn live_create_topup_refuses_with_429_once_the_handler_cap_is_hit() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(create_topup_cap_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the create_topup cap assertions panicked");
    }

    async fn create_topup_cap_assertions(pool: SqlitePool, account_id: Uuid, token: String) {
        let state = live_app_state(pool.clone());
        let cap = state.config.limits.topup_per_hour as i64;
        assert!(
            cap > 0,
            "a zero cap turns the guard off (abuse::cap_outcome), so this test \
             would assert nothing"
        );

        seed_topups_inside_the_cap_window(&pool, account_id, cap).await;

        // Deliberately BELOW every configured minimum. The abuse guard is the
        // FIRST thing create_topup does (account.rs:462, before the deposit
        // check at :483), so a full account is refused with 429 and never
        // reaches a 422. Reordering those two guards fails this test - which is
        // the documented order, not an incidental detail.
        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&token),
            Json(CreateTopupRequest { amount_idr: 1 }),
        ))
        .await;

        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "an account at its topup_per_hour cap must be refused by the handler \
             itself, before it can create another Midtrans session. body: {body}"
        );
        assert_eq!(body["error"]["code"], json!("rate_limited"), "{body}");

        // The refusal wrote nothing. The cap counts rows, so a refused request
        // that still inserted one would refill its own budget.
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM topups WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count topups");
        assert_eq!(
            rows, cap,
            "a refused request must not create a row: {rows} rows, expected {cap}"
        );

        // And no money moved on the way to the refusal.
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance, 0, "an abuse refusal must not touch the wallet");
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );
    }

    #[tokio::test]
    async fn live_create_topup_success_persists_a_pending_row_and_returns_201() {
        // Held for the WHOLE test body: the assertions below point
        // MIDTRANS_SNAP_URL at this test's loopback stub, and that value must
        // never be visible to the sibling test that asserts Snap FAILS.
        let _env = EnvLock::acquire();

        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(create_topup_success_assertions(
            pool.clone(),
            primary.account_id,
            primary.token.clone(),
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the create_topup success assertions panicked");
    }

    async fn create_topup_success_assertions(pool: SqlitePool, account_id: Uuid, token: String) {
        let wallet = configured_wallet();
        let amount = wallet.min_first_deposit as i64;

        // A loopback peer answering exactly as Midtrans Snap does. create_topup
        // runs its REAL reqwest client against a REAL socket - nothing about the
        // handler is stubbed, only the peer it dials.
        let endpoint = snap_stub(http_response(
            "200 OK",
            r#"{"token":"snap-token-from-midtrans","redirect_url":"https://app.sandbox.midtrans.com/snap/v3/redirection/abc"}"#,
        ))
        .await;

        // MIDTRANS_ENV is forced to a non-production value FIRST: the override is
        // deliberately ignored in production (resolve_snap_endpoint), so without
        // this a developer shell exporting MIDTRANS_ENV=production would send
        // this test's request to the REAL Midtrans host. That must be impossible,
        // not merely unlikely.
        // Saved and restored by the guard, which also holds the process-wide
        // env lock: save/restore alone is NOT enough here, because these tests
        // run on parallel threads in one process and a sibling could interleave
        // between the save and the restore.
        let mut env_vars = EnvGuard::set("MIDTRANS_ENV", "sandbox");
        env_vars.also("MIDTRANS_SNAP_URL", &endpoint);
        env_vars.also("MIDTRANS_SERVER_KEY", "SB-Mid-server-LIVE-TEST-SUCCESS");

        // Prove the seam is live before the handler runs, so a silently-ignored
        // override fails HERE (clearly) rather than as a 15s timeout against
        // Midtrans.
        assert_eq!(
            resolve_snap_endpoint(
                std::env::var("MIDTRANS_ENV").ok().as_deref(),
                std::env::var("MIDTRANS_SNAP_URL").ok().as_deref(),
            ),
            endpoint,
            "the Snap override must resolve to the loopback stub, not to a real host"
        );

        let state = live_app_state(pool.clone());

        // The cap must not be what refuses this request: a fresh fixture account
        // is under it. Asserted rather than assumed, so a future config change
        // cannot turn this test's 201 into a 429 for the wrong reason.
        assert!(
            crate::abuse::enforce_creation_cap(
                &pool,
                "topups",
                crate::abuse::topup_window(),
                state.config.limits.topup_per_hour,
                account_id,
                Utc::now(),
            )
            .await
            .is_ok(),
            "the fixture account starts under its top-up cap"
        );

        let (status, body) = respond(create_topup(
            State(state),
            cookie_header(&token),
            Json(CreateTopupRequest { amount_idr: amount }),
        ))
        .await;

        // Restore the host BEFORE asserting, so a failure here cannot leak the
        // loopback endpoint into whatever test runs next. Dropping the guard does
        // exactly that - and does it on the panic path too.
        drop(env_vars);

        assert_eq!(
            status,
            StatusCode::CREATED,
            "a Snap call that returns a token must create the top-up. body: {body}"
        );

        let topup_id = body["topup_id"].as_str().expect("topup_id is a string");
        let order_id = body["order_id"].as_str().expect("order_id is a string");
        assert_eq!(
            order_id,
            format!("topup_{topup_id}"),
            "the order id is derived from the topup id: {body}"
        );
        assert_eq!(
            body["snap_token"],
            json!("snap-token-from-midtrans"),
            "the response carries the token Snap returned, verbatim: {body}"
        );

        // The server's own environment rides on the 201, so the browser can
        // cross-check instead of guessing from the client key's prefix. This test
        // forced MIDTRANS_ENV=sandbox above, so this reads the real wire field
        // against the real variable - the pure test pins the production side.
        assert_eq!(
            body["environment"],
            json!("sandbox"),
            "the 201 must name the environment the Snap call actually used: {body}"
        );

        // The row the success INSERT wrote, read back column by column.
        let row = sqlx::query(
            "SELECT id, account_id, amount_idr, order_id, status, snap_token, settled_at \
             FROM topups WHERE order_id = ?",
        )
        .bind(order_id)
        .fetch_one(&pool)
        .await
        .expect("the success path must have inserted the topups row");

        // Ported: `topups.id` and `topups.account_id` are TEXT, so both decode
        // through `Hyphenated` rather than a bare `Uuid` (the failure is
        // `ParseByteLength { len: 36 }`).
        assert_eq!(
            row.get::<Hyphenated, _>("id").into_uuid(),
            Uuid::parse_str(topup_id).unwrap()
        );
        assert_eq!(
            row.get::<Hyphenated, _>("account_id").into_uuid(),
            account_id
        );
        assert_eq!(row.get::<i64, _>("amount_idr"), amount);
        assert_eq!(row.get::<String, _>("order_id"), order_id);
        assert_eq!(
            row.get::<String, _>("status"),
            "pending",
            "Midtrans first, settle later: the row starts pending"
        );
        assert_eq!(
            row.get::<Option<String>, _>("snap_token").as_deref(),
            Some("snap-token-from-midtrans"),
            "the token is persisted with the row, so the webhook can match it"
        );
        assert!(
            row.get::<Option<chrono::DateTime<Utc>>, _>("settled_at")
                .is_none(),
            "a pending top-up carries no settled_at"
        );

        // The customer sees it through the read path, as pending.
        let (status, history) = respond(get_topups(
            State(pool.clone()),
            cookie_header(&token),
            Query(LimitQuery { limit: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {history}");
        let listed = history
            .as_array()
            .expect("a top-up history is an array")
            .iter()
            .find(|r| r["id"] == json!(topup_id))
            .unwrap_or_else(|| panic!("the created top-up is missing from {history}"));
        assert_eq!(listed["status"], json!("pending"));
        assert_eq!(listed["amount_idr"], json!(amount));
        assert!(
            listed["settled_at"].is_null(),
            "pending means no settled_at: {listed}"
        );
        assert!(
            !history.to_string().contains("snap_token"),
            "the history must never expose the Midtrans token: {history}"
        );

        // The persisted row is real money: settle it through the documented
        // path (the webhook's own function) and check the ledger follows.
        let credited = credit_topup_transaction(&pool, order_id, amount)
            .await
            .expect("the pending row must be settleable");
        assert_eq!(
            credited,
            crate::db::TopupCreditResult::Settled {
                new_balance: amount
            },
            "the 201's row must settle to exactly its amount"
        );

        let settled = sqlx::query("SELECT status, settled_at FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_one(&pool)
            .await
            .expect("read the settled topup");
        assert_eq!(settled.get::<String, _>("status"), "settled");
        assert!(
            settled
                .get::<Option<chrono::DateTime<Utc>>, _>("settled_at")
                .is_some(),
            "a settled top-up carries its settled_at"
        );

        let ledger = sqlx::query("SELECT delta_idr, reason, ref FROM ledger WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_all(&pool)
            .await
            .expect("read the ledger");
        assert_eq!(ledger.len(), 1, "exactly one ledger row for one top-up");
        assert_eq!(ledger[0].get::<i64, _>("delta_idr"), amount);
        assert_eq!(ledger[0].get::<String, _>("reason"), "topup");
        assert_eq!(ledger[0].get::<String, _>("ref"), topup_id);

        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance, amount, "the wallet holds the top-up");

        // A replayed webhook must not pay twice.
        assert_eq!(
            credit_topup_transaction(&pool, order_id, amount)
                .await
                .expect("replay the settlement"),
            crate::db::TopupCreditResult::AlreadySettled,
            "a replayed settlement must be idempotent"
        );
        let after_replay: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(after_replay, amount, "a replay must not move money");

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr)"
        );
    }

    /// The success-path ROW CONTRACT, driven without a Snap token.
    ///
    /// This is NOT the handler and cannot prove create_topup reaches its INSERT.
    /// What it proves is that the exact tuple that INSERT writes is accepted by
    /// the migrated schema, and that the documented status vocabulary is the one
    /// the CHECK constraint enforces - so a schema change that would break the
    /// success path fails here instead of in production.
    #[tokio::test]
    async fn live_topups_row_accepts_exactly_what_create_topups_insert_writes() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let primary = live_account(&pool).await;

        let outcome = tokio::spawn(topups_row_contract_assertions(
            pool.clone(),
            primary.account_id,
        ));

        let outcome = outcome.await;
        db.close().await;
        outcome.expect("the topups row contract assertions panicked");
    }

    async fn topups_row_contract_assertions(pool: SqlitePool, account_id: Uuid) {
        // Column list transcribed from create_topup's INSERT (account.rs:516),
        // status literal included.
        let topup_id = Uuid::new_v4();
        let order_id = format!("topup_{topup_id}");
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, snap_token, rail, created_at) \
             VALUES (?, ?, ?, ?, 'pending', ?, 'midtrans', ?)",
        )
        .bind(topup_id.hyphenated())
        .bind(account_id.hyphenated())
        .bind(50_000_i64)
        .bind(&order_id)
        .bind("snap-token-from-midtrans")
        .bind(Utc::now())
        .bind(Utc::now())
        .execute(&pool)
        .await
        .expect("the schema must accept the tuple create_topup writes");

        let row = sqlx::query("SELECT status, snap_token, settled_at FROM topups WHERE id = ?")
            .bind(topup_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read back the row");
        assert_eq!(row.get::<String, _>("status"), "pending");
        assert_eq!(
            row.get::<Option<String>, _>("snap_token").as_deref(),
            Some("snap-token-from-midtrans")
        );
        assert!(row
            .get::<Option<chrono::DateTime<Utc>>, _>("settled_at")
            .is_none());

        // The documented vocabulary round-trips...
        for status in ["pending", "settled", "denied", "expired", "refunded"] {
            let order_id = format!("test_status_{}", Uuid::new_v4().simple());
            sqlx::query(
                "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
                 VALUES (?, ?, ?, ?, ?, 'midtrans', ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account_id.hyphenated())
            .bind(1_000_i64)
            .bind(&order_id)
            .bind(status)
            .bind(Utc::now())
            .execute(&pool)
            .await
            .unwrap_or_else(|e| panic!("{status} must be a documented status: {e}"));
        }

        // ...and anything outside it is refused, not silently stored.
        let bad_order = format!("test_status_bad_{}", Uuid::new_v4().simple());
        let refused = sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at) \
             VALUES (?, ?, ?, ?, 'partially_paid', 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(1_000_i64)
        .bind(&bad_order)
        .bind(Utc::now())
        .execute(&pool)
        .await;
        assert!(
            refused.is_err(),
            "an undocumented status must be refused by the CHECK constraint"
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM topups WHERE order_id = ?")
            .bind(&bad_order)
            .fetch_one(&pool)
            .await
            .expect("count the refused row");
        assert_eq!(rows, 0, "a refused status must leave no row");
    }

    // -----------------------------------------------------------------------
    // GET /api/export - the customer's own data.
    //
    // The two things that matter: it returns ONLY this account's rows (a scoping
    // bug leaks another customer's financial history), and it never contains a
    // credential or internal id (docs/data-retention.md "metadata, not secrets").
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn export_returns_only_this_account_and_no_secrets() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let mine = live_account(&pool).await;
        let other = live_account(&pool).await;

        let key_id = test_support::api_key(&pool, mine.account_id).await;
        test_support::fund(&pool, mine.account_id, 42_000).await;
        debit_usage_transaction(
            &pool,
            mine.account_id,
            Some(key_id),
            "flash",
            100,
            10,
            50,
            111,
            Some("exp_ref"),
            0,
        )
        .await
        .expect("a settlement");

        // The OTHER account carries a distinct balance, so a scoping bug is
        // visible rather than coincidentally equal.
        test_support::fund(&pool, other.account_id, 999_999).await;

        let (status, body) = respond(export_account_data(
            State(pool.clone()),
            cookie_header(&mine.token),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        for field in [
            "account",
            "wallet",
            "ledger",
            "topups",
            "usage_daily",
            "usage_events",
            "api_keys",
            "telegram_linked",
            "exported_at",
            "note",
        ] {
            assert!(
                body.get(field).is_some(),
                "export is missing {field}: {body}"
            );
        }

        assert_eq!(
            body["account"]["id"],
            json!(mine.account_id.hyphenated().to_string())
        );
        // The balance AFTER the settlement: 42,000 funded minus the 111 IDR the
        // request cost. Asserting the net (not the funding) is what proves the
        // export reads the wallet rather than echoing a fixture constant.
        assert_eq!(body["wallet"]["balance_idr"], json!(42_000 - 111));
        // The ledger carries the matching rows, so the export can reconcile.
        assert!(
            !body["ledger"].as_array().unwrap().is_empty(),
            "the ledger must be exported"
        );
        assert_eq!(
            body["usage_events"].as_array().unwrap().len(),
            1,
            "the request must be exported"
        );

        // A key's `models` is emitted as a real JSON array, not a string that
        // contains JSON — the export must be machine-readable.
        assert!(
            body["api_keys"][0]["models"].is_array(),
            "models must be an array, not a string: {}",
            body["api_keys"][0]["models"]
        );

        // No secret or internal identifier anywhere in the document, by name.
        let text = body.to_string();
        for forbidden in [
            "key_hash",
            "token_hash",
            "pb_user_id",
            "snap_token",
            "apk_live_",
            "password",
            "ip_hash",
        ] {
            assert!(
                !text.contains(forbidden),
                "the export leaked {forbidden}: {text}"
            );
        }

        // The other account's money never appears.
        assert!(
            !text.contains("999999"),
            "the export leaked another account's balance: {text}"
        );

        db.close().await;
    }

    /// An unauthenticated request is refused before any row is read.
    #[tokio::test]
    async fn export_requires_a_session() {
        let db = TestDb::new().await;
        let (status, _) = respond(export_account_data(
            State(db.pool.clone()),
            HeaderMap::new(),
        ))
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        db.close().await;
    }
}
