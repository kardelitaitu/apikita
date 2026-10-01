#![cfg_attr(
    not(test),
    // THE API-KEY SURFACE, fenced with the other money-adjacent modules.
    //
    // This is where a per-key spend limit and a token ceiling are COMPARED, so an
    // overflow here does not corrupt a total so much as stop a limit from biting.
    // That is the permissive direction, which is why the fold in this module now
    // saturates rather than wrapping, and why this fence exists at all: the
    // arithmetic that decides whether a key keeps being served is arithmetic.
    //
    // It is NOT free here, and the difference from the last three modules is worth
    // recording: this one has two real sites, and both carry a written argument.
    // The third was the fold, and that one was not an argument at all - it was a
    // wrong direction, so it became saturating_add instead of an allow.
    deny(clippy::arithmetic_side_effects)
)]

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::{DateTime, NaiveDate, Utc};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use uuid::fmt::Hyphenated;
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::hash_token;
use crate::routes::proxy::{invalidate_key_cache, AppState};

#[derive(Debug, Serialize)]
pub struct ApiKeyDto {
    pub id: Uuid,
    pub prefix: String,
    pub label: Option<String>,
    pub models: Value,
    pub spend_limit_idr: i64,
    /// The token ceiling this key was created with, 0 meaning unlimited.
    ///
    /// THIS FIELD WAS MISSING, and its absence silently destroyed the value it
    /// describes. `CreateKeyRequest` and `UpdateKeyRequest` both accept
    /// `token_limit`, and the dashboard's edit form sends the WHOLE field set on
    /// every save rather than a diff (`website/src/lib/dashboard-form.ts`, whose
    /// own doc argues that sending only changed fields would preserve a value the
    /// user deliberately cleared). That argument holds only if the form is
    /// populated from the stored value - and this field is what would have
    /// populated it. Without it the edit form opened with the token input blank,
    /// a blank limit reads as 0, and 0 means unlimited: renaming a key or
    /// toggling a model on it raised that key's token ceiling to no ceiling at
    /// all, with nothing on screen or in the response to show it had happened.
    pub token_limit: i64,
    pub spend_used_idr: i64,
    pub rate_limit_rpm: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct CreateKeyRequest {
    pub label: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub spend_limit_idr: i64,
    #[serde(default)]
    pub token_limit: i64,
    #[serde(default)]
    pub rate_limit_rpm: i32,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct CreateKeyResponse {
    pub id: Uuid,
    pub key: String,
    pub prefix: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateKeyRequest {
    pub label: Option<String>,
    pub models: Option<Vec<String>>,
    pub spend_limit_idr: Option<i64>,
    pub token_limit: Option<i64>,
    pub rate_limit_rpm: Option<i32>,
    pub expires_at: Option<DateTime<Utc>>,
}

/// Length of the rolling `spend_limit_idr` window, in days, inclusive of today.
///
/// docs/website/06-api-keys-and-limits.md fixes this at "the trailing 30 days"
/// (a rolling window, deliberately not a calendar month). No window length is
/// configured in config.rs, so there is nothing to read instead of this.
///
/// Public because the PROXY enforces this same window on the request path: one
/// constant, so the limit the proxy blocks on and the spend the dashboard shows
/// cannot drift apart.
pub const SPEND_WINDOW_DAYS: i64 = 30;

/// First day (inclusive) of the rolling spend window ending on `today`.
/// The window holds exactly 30 day-values, so it starts 29 days back.
///
/// SAFE, twice over. `SPEND_WINDOW_DAYS - 1` is `30 - 1` on a const, folded at
/// compile time with no runtime operand that could overflow. And `NaiveDate -
/// Duration` PANICS on an out-of-range result rather than wrapping, so even a
/// nonsense window length would fail loudly instead of silently dating rows to the
/// wrong side of the boundary. 29 days back from any real date is in range.
///
/// The allow is on the FUNCTION, not on the expression. An attribute in
/// tail-expression position is still unstable - "attributes on expressions are
/// experimental" - so the version that put it on the line below did not compile,
/// and the compiler said so rather than accepting it.
#[allow(clippy::arithmetic_side_effects)]
pub(crate) fn spend_window_start(today: NaiveDate) -> NaiveDate {
    today - chrono::Duration::days(SPEND_WINDOW_DAYS - 1)
}

/// Whether a `usage_daily.day` row falls inside the rolling spend window.
fn in_spend_window(day: NaiveDate, today: NaiveDate) -> bool {
    day >= spend_window_start(today)
}

/// Folds per-day usage rows into one window total per API key.
/// Rows outside the window are dropped; keys with no rows are absent (read as 0).
///
/// The summed value is `usage_daily.cost_idr`, which the settlement path already
/// stores as the money.rs `calculate_token_cost_idr` result — no second formula.
///
/// **THIS IS USAGE, NOT WHAT THE CUSTOMER WAS BILLED**, and for a clamped request
/// those differ. When a hold cannot be fully covered the debit is clamped to the
/// balance, so the wallet takes less than the service cost, while `usage_daily` records
/// the full cost because the service was genuinely consumed. This function sums the full
/// cost, so a customer whose balance ran out has their key SPEND limit advanced by money
/// they were never charged.
///
/// That is defensible — a limit expressed in IDR is most safely read as a ceiling on
/// consumption, and using revenue would let a customer with a tiny balance and a large
/// limit consume without ever reaching it. But the column is `spend_limit_idr`, the
/// documentation calls it a spend limit, and a customer who finds their key blocked for
/// spend that was never collected deserves to know why. The behaviour is right; the NAME
/// reads the other way, and that is what this note is for. The same divergence is
/// recorded under Overdraft in `docs/decisions.md`.
fn fold_spend_in_window(rows: &[(Uuid, NaiveDate, i64)], today: NaiveDate) -> HashMap<Uuid, i64> {
    let mut spend: HashMap<Uuid, i64> = HashMap::new();
    for &(key_id, day, cost_idr) in rows {
        if !in_spend_window(day, today) {
            continue;
        }
        // SATURATING, not wrapping, and the direction is the whole point.
        //
        // This total decides whether the per-key spend limit bites. A wrapped sum
        // that crosses i64::MAX comes back NEGATIVE, and a negative spend is
        // "nowhere near the limit" - so the one condition that stops a key being
        // served would be the one arithmetic that turned the limit off. That is
        // the exact failure shape this repository treats as worst: silent, and
        // pointed in the permissive direction.
        //
        // Saturating puts the failure on the other side: the total reads as
        // i64::MAX, which is above every spend_limit_idr the config can express, so
        // the limit keeps biting. Being wrong about how MUCH a key has spent is
        // recoverable; not enforcing a limit is not.
        //
        // Unreachable with real figures - 30 days of IDR would have to exceed
        // 9.2e18 - but the same sum IS implemented a second time in SQL by
        // `key_spend_used`, which is the read the PROXY enforces from, and the two
        // must not fail in opposite directions. This one now saturates.
        let total = spend.entry(key_id).or_insert(0);
        *total = total.saturating_add(cost_idr);
    }
    spend
}

/// 30-day spend already recorded against one key, in IDR.
///
/// Public because the proxy enforces the per-key limit with this exact read
/// (DEFECT 1): same table, same column, same window. A second formula on the
/// request path is how the dashboard number and the enforcement number start
/// disagreeing, and a key showing "limit reached" that still gets served is
/// precisely the defect this closes.
pub(crate) async fn key_spend_used(
    pool: &SqlitePool,
    account_id: Uuid,
    key_id: Uuid,
    today: NaiveDate,
) -> Result<i64, AppError> {
    let used: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT SUM(cost_idr)
        FROM usage_daily
        WHERE account_id = ? AND api_key_id = ? AND day >= ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(key_id.hyphenated())
    .bind(spend_window_start(today))
    .fetch_one(pool)
    .await?;

    Ok(used.unwrap_or(0))
}

/// 30-day token usage already recorded against one key, in tokens.
///
/// Public for the same reason as `key_spend_used`: the proxy enforces
/// `token_limit` with this exact read, over the same `usage_daily` rows and the
/// same `spend_window_start` boundary as the spend limit. Two limits, one
/// window, one source — so they cannot disagree with each other or with the
/// dashboard.
///
/// All three token classes are summed because `token_limit` counts tokens, not
/// money: cache-read tokens are ~50x cheaper than output tokens but they are
/// still tokens consumed, and `usage_daily` is the only place the request path
/// records them.
pub(crate) async fn key_tokens_used(
    pool: &SqlitePool,
    account_id: Uuid,
    key_id: Uuid,
    today: NaiveDate,
) -> Result<i64, AppError> {
    let used: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT SUM(input_tokens + cache_read_tokens + output_tokens)
        FROM usage_daily
        WHERE account_id = ? AND api_key_id = ? AND day >= ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(key_id.hyphenated())
    .bind(spend_window_start(today))
    .fetch_one(pool)
    .await?;

    Ok(used.unwrap_or(0))
}

/// Rejects a requested `spend_limit_idr` that cannot be enforced, reporting the
/// key's current window usage so the operator can see where it stands.
///
/// 0 means "no limit" (docs/website/06-api-keys-and-limits.md). A negative limit
/// has no meaning and would fail every request, so it is refused here rather than
/// stored and silently obeyed.
fn check_spend_limit(requested_idr: i64, spend_used_idr: i64) -> Result<(), AppError> {
    if requested_idr < 0 {
        return Err(AppError::KeyLimitExceeded {
            details: Some(json!({
                "reason": "invalid_spend_limit_idr",
                "spend_limit_idr": requested_idr,
                "spend_used_idr": spend_used_idr,
                "window_days": SPEND_WINDOW_DAYS,
            })),
        });
    }
    Ok(())
}

/// Rejects a requested `token_limit` that cannot be enforced.
///
/// 0 means "no limit", which the proxy honours by SKIPPING the check
/// (`proxy.rs`: `if token_limit > 0`). That is exactly why a negative value must
/// be refused here instead of stored: every non-positive limit reads as
/// "unlimited" on the request path, so a negative ceiling would silently become
/// no ceiling at all — the opposite of the budget the operator set. Same error
/// shape as `check_spend_limit`, with the field named in `details.reason`.
fn check_token_limit(requested: i64) -> Result<(), AppError> {
    if requested < 0 {
        return Err(AppError::KeyLimitExceeded {
            details: Some(json!({
                "reason": "invalid_token_limit",
                "token_limit": requested,
                "window_days": SPEND_WINDOW_DAYS,
            })),
        });
    }
    Ok(())
}

/// The account a request's session cookie resolves to.
///
/// The local resolver that used to sit here is DELETED, and its comment claimed it
/// was "equivalent to" `crate::routes`'s because both take a `SqlitePool`. That
/// was true of the hash and false of the RULE: the copy filtered revocation and
/// absolute expiry in SQL and stopped, so it never applied the idle bound and
/// never recorded activity. The one place the session rule lives is the only one
/// now.
use crate::routes::resolve_account_from_cookie;

/// The same refusal for `rate_limit_rpm`.
///
/// `proxy.rs`: "Setting `rate_limit_rpm` to 0 disables the check entirely." A
/// negative value therefore does not throttle harder — it disables throttling,
/// which is the opposite of what the operator asked for. Refused here, with the
/// field named, rather than stored and silently obeyed.
fn check_rate_limit(requested: i32) -> Result<(), AppError> {
    if requested < 0 {
        return Err(AppError::KeyLimitExceeded {
            details: Some(json!({
                "reason": "invalid_rate_limit_rpm",
                "rate_limit_rpm": requested,
            })),
        });
    }
    Ok(())
}

pub async fn list_keys(
    State(pool): State<SqlitePool>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&pool, &headers).await?;

    let keys = sqlx::query(
        r#"
        SELECT
            id, prefix, label, models, spend_limit_idr, token_limit, rate_limit_rpm,
            expires_at, last_used_at, revoked_at
        FROM api_keys
        WHERE account_id = ?
        ORDER BY created_at DESC
        "#,
    )
    .bind(account_id.hyphenated())
    .fetch_all(&pool)
    .await?;

    // Real 30-day spend per key. usage_daily.cost_idr is the settled cost of the
    // key's usage; a key with no rows in the window reads as 0.
    let today = crate::ip_tracking::today_utc();
    let usage_rows = sqlx::query(
        r#"
        SELECT api_key_id, day, cost_idr
        FROM usage_daily
        WHERE account_id = ? AND api_key_id IS NOT NULL AND day >= ?
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(spend_window_start(today))
    .fetch_all(&pool)
    .await?;

    let usage: Vec<(Uuid, NaiveDate, i64)> = usage_rows
        .into_iter()
        .map(|r| {
            let key_id: Uuid = r.get::<Hyphenated, _>("api_key_id").into_uuid();
            let day: NaiveDate = r.get("day");
            let cost_idr: i64 = r.get("cost_idr");
            (key_id, day, cost_idr)
        })
        .collect();
    let spend_by_key = fold_spend_in_window(&usage, today);

    let response: Vec<ApiKeyDto> = keys
        .into_iter()
        .map(|k| {
            let id: Uuid = k.get::<Hyphenated, _>("id").into_uuid();
            ApiKeyDto {
                id,
                prefix: k.get("prefix"),
                label: k.get("label"),
                models: k.get("models"),
                spend_limit_idr: k.get("spend_limit_idr"),
                token_limit: k.get("token_limit"),
                spend_used_idr: spend_by_key.get(&id).copied().unwrap_or(0),
                rate_limit_rpm: k.get("rate_limit_rpm"),
                expires_at: k.get("expires_at"),
                last_used_at: k.get("last_used_at"),
                revoked_at: k.get("revoked_at"),
            }
        })
        .collect();

    Ok(Json(response))
}

pub async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // Abuse guard: `limits.key_creation_per_day` (docs/decisions.md) exists to
    // blunt limit circumvention by key-spam. Enforced from the `api_keys` rows,
    // so a revoked key still counts - revoking one to mint another is exactly
    // the circumvention the cap is for. The cap comes from the config the app
    // already owns.
    //
    // HELD OPEN ACROSS THE INSERT, and that is the fix. The cap is a COUNT, and a
    // count taken before the insert it guards is a count of the past: concurrent
    // callers all read the same number and all insert, so the cap held against a
    // sequential script and not at all against a parallel one. SQLite serialises
    // writers, so one transaction spanning the COUNT and the INSERT makes the two
    // atomic - a second caller either sees the first caller's row or is still
    // waiting for the lock.
    //
    // This path can afford that because everything between them is LOCAL: key
    // generation, hashing and three validations, all microseconds of CPU. The
    // top-up path cannot - its insert comes after a Midtrans Snap call, and a
    // transaction spanning a network round trip to a payment provider would
    // serialise every other top-up in the process behind it. That path keeps the
    // non-transactional variant and its race is characterised in abuse.rs.
    let mut tx = crate::db::begin_immediate(&state.pool).await?;
    crate::abuse::enforce_creation_cap_in(
        &mut tx,
        "api_keys",
        crate::abuse::key_creation_window(),
        state.config.limits.key_creation_per_day,
        account_id,
        Utc::now(),
    )
    .await?;

    // The alphabet indexing is provably in bounds: `% 62` yields 0..=61 and the
    // array is exactly 62 bytes, so this can never panic. That is precisely why it
    // is written with `get()` and a fallback anyway — the proof depends on two
    // literals staying in step, and a future edit that adds or removes one
    // alphabet character would turn a panic-free line into a panic WITHOUT any
    // visible change here. The fallback is unreachable and is the point.
    const KEY_ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let random_bytes: String = (0..43)
        .map(|_| {
            // SAFE: a remainder is strictly less than its divisor, and the divisor
            // is a non-zero constant, so the modulo cannot overflow or divide by
            // zero. The narrowing casts are also bounded - the value is already
            // < 62 and the target types are wider than that.
            //
            // What the modulo DOES cost is a slight bias, and it is left in place
            // deliberately: 2^32 is not a multiple of 62, so the first
            // (2^32 mod 62) alphabet characters are very slightly more likely than
            // the rest. The alphabet is 62 characters over a 43-character key, so
            // the bias is ~1 in 10^7 per position and 43 positions do not make it
            // attackable. Correcting it would mean rejection sampling, which is
            // real code for a defect this key length cannot express.
            #[allow(clippy::arithmetic_side_effects)]
            let idx = (rand_core::OsRng.next_u32() % KEY_ALPHABET.len() as u32) as usize;
            KEY_ALPHABET.get(idx).copied().unwrap_or(b'0') as char
        })
        .collect();

    let full_key = format!("apk_live_{}", random_bytes);
    // The display prefix is the first 4 characters, and this is a RANGE slice
    // rather than an index — the same "the proof depends on a literal" argument
    // as the alphabet above. `random_bytes` is 43 characters by construction, so
    // this cannot panic today; `get(..4)` keeps it that way if the length ever
    // changes, at the cost of a fallback that is unreachable and therefore free.
    let prefix = format!(
        "apk_live_{}",
        random_bytes.get(..4).unwrap_or(random_bytes.as_str())
    );
    let key_hash = hash_token(&full_key);

    let models_json = serde_json::to_value(&payload.models).unwrap_or(json!([]));

    // A key starts with no usage, so its window spend is 0; validate the
    // requested limit against that before storing it.
    check_spend_limit(payload.spend_limit_idr, 0)?;
    // The other two ceilings are validated here too, BEFORE the INSERT: a
    // negative value is stored as-is and then read as "no limit" by the proxy,
    // so accepting it would answer 201 and enforce nothing.
    check_token_limit(payload.token_limit)?;
    check_rate_limit(payload.rate_limit_rpm)?;

    // `id` and `created_at` are bound, not defaulted: both had Postgres defaults
    // (`gen_random_uuid()`, `now()`) which the SQLite schema deliberately removed
    // (plan section 4.6). Returning `id` rather than echoing the generated value
    // keeps this honest if the insert ever gains an upsert clause.
    let key_record = sqlx::query(
        r#"
        INSERT INTO api_keys (
            id, account_id, key_hash, prefix, label, models,
            spend_limit_idr, token_limit, rate_limit_rpm, expires_at, created_at
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        RETURNING id
        "#,
    )
    .bind(Uuid::new_v4().hyphenated())
    .bind(account_id.hyphenated())
    .bind(key_hash)
    .bind(&prefix)
    .bind(payload.label)
    .bind(models_json)
    .bind(payload.spend_limit_idr)
    .bind(payload.token_limit)
    .bind(payload.rate_limit_rpm)
    .bind(payload.expires_at)
    .bind(Utc::now())
    .fetch_one(&mut *tx)
    .await?;

    let id: Uuid = key_record.get::<Hyphenated, _>("id").into_uuid();

    // Commit BEFORE the response is built, so a caller that sees a 201 knows the
    // key is durable. The write lock is held for the whole of the above: the
    // generate/hash/validate sequence is local, but it is still a write lock, and
    // that is the price of a cap that actually caps. See the note at the
    // transaction's opening.
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(CreateKeyResponse {
            id,
            key: full_key,
            prefix,
        }),
    ))
}

pub async fn update_key(
    // The whole state, not just the pool: the invalidation below reaches the
    // proxy's process-wide cache, which is built from `config`.
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Json(payload): Json<UpdateKeyRequest>,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // Lowering a limit below what the key has already spent in the window leaves
    // it immediately over limit. Validate against the real spend so the operator
    // sees the number rather than a silently ineffective limit.
    if let Some(requested) = payload.spend_limit_idr {
        let spend_used_idr =
            key_spend_used(&state.pool, account_id, id, crate::ip_tracking::today_utc()).await?;
        check_spend_limit(requested, spend_used_idr)?;
    }
    // A patch that is absent leaves the column alone (COALESCE below), so only a
    // present value is validated — but a present negative one must be refused
    // before the UPDATE runs, or it replaces a working ceiling with "unlimited".
    if let Some(requested) = payload.token_limit {
        check_token_limit(requested)?;
    }
    if let Some(requested) = payload.rate_limit_rpm {
        check_rate_limit(requested)?;
    }

    // NOT `.unwrap()`: this is the key-UPDATE handler, and a panic here is a 500
    // on a money-adjacent endpoint rather than a refused request. Serialising a
    // `Vec<String>` cannot realistically fail, but "cannot realistically fail" is
    // exactly the reasoning that makes an unwrap survive review and then fire in
    // production. The create path above (line ~375) already uses the safe form;
    // this now matches it.
    let models_json = payload
        .models
        .map(|m| serde_json::to_value(m).unwrap_or_else(|_| json!([])));

    let res: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE api_keys
        SET
            label = COALESCE(?, label),
            models = COALESCE(?, models),
            spend_limit_idr = COALESCE(?, spend_limit_idr),
            token_limit = COALESCE(?, token_limit),
            rate_limit_rpm = COALESCE(?, rate_limit_rpm),
            expires_at = COALESCE(?, expires_at)
        WHERE id = ? AND account_id = ? AND revoked_at IS NULL
        RETURNING key_hash
        "#,
    )
    .bind(payload.label)
    .bind(models_json)
    .bind(payload.spend_limit_idr)
    .bind(payload.token_limit)
    .bind(payload.rate_limit_rpm)
    .bind(payload.expires_at)
    .bind(id.hyphenated())
    .bind(account_id.hyphenated())
    .fetch_optional(&state.pool)
    .await?;

    // A narrowed limit, allowlist or expiry is an enforcement input too: the
    // cached record still holds the old values, so without this the proxy would
    // keep honouring the previous (looser) limit for up to the cache TTL.
    // Dropped after the update commits, and only in THIS process.
    let Some(key_hash) = res else {
        return Err(AppError::NotFound("Key not found or revoked".into()));
    };
    invalidate_key_cache(&state.config, &key_hash);

    Ok(StatusCode::OK)
}

pub async fn revoke_key(
    // The whole state, not just the pool: the proxy's key-metadata cache is a
    // process-wide singleton built from `config`, so reaching the same instance
    // the request path uses needs the config it was built from.
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = resolve_account_from_cookie(&state.pool, &headers).await?;

    // RETURNING the hash is what makes the invalidation below possible: the
    // proxy cache is keyed by the SHA-256 of the presented token, and the
    // plaintext key exists nowhere in the database (it was shown once, at
    // creation). The hash is also all `invalidate_key_cache` will ever take, so
    // the plaintext never has to be reconstructed to evict an entry.
    let revoked_hash: Option<String> = sqlx::query_scalar(
        r#"
        UPDATE api_keys SET revoked_at = ?
        WHERE id = ? AND account_id = ? AND revoked_at IS NULL
        RETURNING key_hash
        "#,
    )
    .bind(Utc::now())
    .bind(id.hyphenated())
    .bind(account_id.hyphenated())
    .fetch_optional(&state.pool)
    .await?;

    // The update committed, so the revocation is durable; dropping the cached
    // record is what makes it take effect NOW instead of at the TTL boundary
    // (DEFECT 2: a revoked key used to stay usable for up to
    // `limits.key_metadata_cache_seconds`). A second revoke matches no row and
    // returns None — there is nothing left to invalidate, and the response is
    // the same 204 either way, so the call stays idempotent.
    //
    // Only THIS process's cache is dropped. Instances behind the load balancer
    // keep their own copy until its TTL expires; that residual window is the
    // documented cost of the cache and is not closed by this call.
    if let Some(key_hash) = revoked_hash {
        invalidate_key_cache(&state.config, &key_hash);
    }

    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn test_spend_window_boundary() {
        let today = day(2026, 5, 20);
        let start = spend_window_start(today);
        assert_eq!(start, day(2026, 4, 21));
        assert_eq!((today - start).num_days() + 1, SPEND_WINDOW_DAYS);
        assert!(in_spend_window(start, today));
        assert!(in_spend_window(today, today));
        assert!(!in_spend_window(start - chrono::Duration::days(1), today));
        assert!(!in_spend_window(day(2026, 4, 20), today));
        assert_eq!(spend_window_start(day(2026, 3, 1)), day(2026, 1, 31));
    }

    /// THE DIRECTION OF THE OVERFLOW, which is the whole reason the fold saturates.
    ///
    /// The window total decides whether the per-key spend limit bites, so the two
    /// ways to be wrong are not equally bad. A wrapping sum that crosses i64::MAX
    /// comes back NEGATIVE, and a negative spend is "nowhere near the limit" - the
    /// limit stops enforcing, silently, and only for a key that has somehow run up an
    /// absurd total. A saturating sum reads as i64::MAX, which is above every
    /// spend_limit_idr the schema can hold, so the limit keeps biting.
    ///
    /// This is asserted on the FUNCTION rather than in prose, with figures no
    /// database would ever produce, because the property is exactly that it holds
    /// for figures no database would ever produce. A debug build panics on the
    /// wrapping version, so the two implementations are distinguishable by this
    /// test alone.
    ///
    /// The two rows are the minimum that crosses the boundary: i64::MAX alone sums
    /// to itself, and only the second row pushes the running total past the edge.
    #[test]
    fn a_window_total_that_overflows_reads_as_over_the_limit_and_never_below_it() {
        let today = day(2026, 5, 20);
        let start = spend_window_start(today);
        let key = Uuid::new_v4();

        let rows = vec![(key, start, i64::MAX), (key, today, 1)];
        let spend = fold_spend_in_window(&rows, today);

        let total = spend
            .get(&key)
            .copied()
            .expect("a key with rows in the window must appear in the fold");
        assert_eq!(
            total,
            i64::MAX,
            "an over-large window total must SATURATE, not wrap to a negative that \
             reads as 'under the limit'"
        );

        // The property that actually matters, stated without reference to i64: the
        // total is above every limit, so a limit comparison refuses. A spend limit
        // is an i64 column, so i64::MAX is the largest one that can exist.
        assert!(
            total > 0,
            "a saturated spend must never read as zero or negative: that is the \
             direction in which a limit silently stops biting"
        );

        // The control, or the assertion above could pass because nothing was summed
        // at all. A single row must still be reported exactly.
        let one = vec![(key, today, 4_242)];
        assert_eq!(
            fold_spend_in_window(&one, today).get(&key).copied(),
            Some(4_242),
            "an ordinary total must be reported unchanged - saturation must not \
             perturb the normal case"
        );
    }

    #[test]
    fn test_fold_spend_in_window() {
        let today = day(2026, 5, 20);
        let start = spend_window_start(today);
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let rows = vec![
            (a, today, 700),
            (a, start, 300),
            (a, start - chrono::Duration::days(1), 9_999),
            (b, start - chrono::Duration::days(1), 500),
        ];
        let spend = fold_spend_in_window(&rows, today);
        assert_eq!(spend.get(&a).copied(), Some(1_000));
        assert_eq!(spend.get(&b).copied(), None);
        assert_eq!(spend.len(), 1);
        assert_eq!(spend.values().sum::<i64>(), 1_000);
    }

    #[test]
    fn test_check_spend_limit() {
        assert!(check_spend_limit(0, 0).is_ok());
        assert!(check_spend_limit(50_000, 12_340).is_ok());
        let err = check_spend_limit(-1, 12_340).unwrap_err();
        assert!(matches!(&err, AppError::KeyLimitExceeded { .. }));
        assert_eq!(err.code(), "key_limit_exceeded");
        assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
        let details = err.details().unwrap();
        assert_eq!(details["spend_limit_idr"], -1);
        assert_eq!(details["spend_used_idr"], 12_340);
        assert_eq!(details["window_days"], 30);
    }

    /// docs/website/06-api-keys-and-limits.md line 45: "A limit of `0` or `null`
    /// means 'no limit of this kind'." A NEGATIVE value is not that: the proxy
    /// skips any limit it reads as non-positive (`proxy.rs`: `if token_limit > 0`,
    /// and `limit_reached` is `limit > 0 && used >= limit`), so a negative ceiling
    /// would be stored and then honoured as UNLIMITED - the exact opposite of the
    /// ceiling the operator asked for. Same refusal, same error shape as
    /// `check_spend_limit`; only the reason and the field name differ.
    #[test]
    fn test_check_token_limit() {
        assert!(check_token_limit(0).is_ok(), "0 is the documented no-limit");
        assert!(check_token_limit(1_000_000).is_ok());
        let err = check_token_limit(-1).unwrap_err();
        assert!(matches!(&err, AppError::KeyLimitExceeded { .. }));
        assert_eq!(err.code(), "key_limit_exceeded");
        assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
        let details = err.details().unwrap();
        assert_eq!(details["reason"], "invalid_token_limit");
        assert_eq!(details["token_limit"], -1);
        assert_eq!(details["window_days"], 30);
    }

    /// The same rule for the per-minute ceiling. 0 disables the check entirely
    /// (`proxy.rs`: "Setting `rate_limit_rpm` to 0 disables the check"), which is
    /// exactly why a negative one must never reach the row: it would disable the
    /// check the operator believed they had just set.
    #[test]
    fn test_check_rate_limit() {
        assert!(check_rate_limit(0).is_ok(), "0 is the documented no-limit");
        assert!(check_rate_limit(60).is_ok());
        let err = check_rate_limit(-5).unwrap_err();
        assert!(matches!(&err, AppError::KeyLimitExceeded { .. }));
        assert_eq!(err.code(), "key_limit_exceeded");
        assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
        let details = err.details().unwrap();
        assert_eq!(details["reason"], "invalid_rate_limit_rpm");
        assert_eq!(details["rate_limit_rpm"], -5);
    }

    // -----------------------------------------------------------------------
    // Live Postgres. Ignored rather than silently skipped, exactly like the
    // settlement tests in db.rs and the cap tests in abuse.rs: a test that
    // asserts nothing is worse than no test. Every handler in THIS module was
    // previously covered only by the pure tests above, so not one DB-backed key
    // handler had ever actually been executed.
    //
    //   DATABASE_URL=... cargo test --lib routes::keys:: -- --ignored
    // -----------------------------------------------------------------------

    use crate::config::AppConfig;
    use crate::db::{
        credit_topup_transaction, debit_usage_transaction, TopupCreditResult, UsageSettlement,
        SHIPPED_CREDIT_EXPIRY_MONTHS,
    };
    // Postgres keeps timestamptz at microsecond resolution, so the live tests
    // truncate a computed instant before comparing it to the stored value.
    use crate::ip_tracking::{parse_cidrs, DailySalt, IpCidr};
    use crate::routes::events::RealtimeHub;
    use chrono::SubsecRound;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// The ONE process-wide lock over the proxy's key-metadata cache.
    ///
    /// `proxy.rs`'s `KEY_CACHE` is a process-global singleton shared by every
    /// test in this binary AND by real request handling, but the cache tests in
    /// this module warm it and then invalidate it, each assuming it is the ONLY
    /// writer. Run in parallel, one test's `update_key`/`revoke_key` (which
    /// invalidates) lands between the other's out-of-band write and its
    /// "still stale" assertion - and both fail, intermittently, with
    /// `key_limit_exceeded` where the test expects `insufficient_balance`.
    ///
    /// This is the same defect class as the MIDTRANS_* environment leak already
    /// fixed in `routes::account` / `routes::webhooks`, and it gets the same
    /// remedy: a process-wide lock held for the WHOLE body of every test that
    /// touches the shared global, so no two can interleave. It serialises; it
    /// does not bypass - every staleness assertion below still has to hold on
    /// its own merits.
    static KEY_CACHE_LOCK: Mutex<()> = Mutex::new(());

    /// Held for the whole body of a test that warms or invalidates `KEY_CACHE`.
    struct CacheLock {
        _lock: MutexGuard<'static, ()>,
    }

    impl CacheLock {
        fn acquire() -> Self {
            // A panicking test must not poison the lock for every later test:
            // the cache entry it left behind is exactly what its own out-of-band
            // write and `invalidate_key_cache` would have replaced anyway.
            Self {
                _lock: KEY_CACHE_LOCK.lock().unwrap_or_else(|err| err.into_inner()),
            }
        }
    }

    fn live_config() -> Arc<AppConfig> {
        for path in ["../config/apikita.toml", "config/apikita.toml"] {
            if std::path::Path::new(path).exists() {
                return Arc::new(AppConfig::load_from_file(path).expect("parse apikita.toml"));
            }
        }
        panic!("could not find apikita.toml for testing");
    }

    /// The real application state, built the way main.rs builds it, so the
    /// handlers run against the same cache instance and the same config the
    /// process serves with.
    fn test_state(pool: SqlitePool) -> AppState {
        let config = live_config();
        let events = Arc::new(RealtimeHub::new(&config.realtime));
        let trusted_proxies: Arc<[IpCidr]> = Arc::from(
            parse_cidrs(&config.network.trusted_proxy_cidrs)
                .expect("config CIDRs parse")
                .into_boxed_slice(),
        );
        AppState {
            pool,
            config,
            http_client: reqwest::Client::new(),
            events,
            ip_salt: Arc::new(DailySalt::new()),
            trusted_proxies,
        }
    }

    async fn create_account(pool: &SqlitePool) -> Uuid {
        test_support::account(pool).await
    }

    /// A real sessions row and the cookie that resolves to it, so every handler
    /// below is reached through the production authentication path
    /// (resolve_account_from_cookie) rather than a hand-passed account id.
    async fn session_cookie(pool: &SqlitePool, account_id: Uuid) -> HeaderMap {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(hash_token(&token))
        .bind(now + chrono::Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");

        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// Money enters a wallet ONLY through credit_topup_transaction, which writes
    /// the matching + ledger row in the same transaction. Writing
    /// wallets.balance_idr directly manufactures exactly the reconciliation drift
    /// the drift_rows assertion at the end of every test looks for.
    async fn open_wallet(pool: &SqlitePool, account_id: Uuid, opening_idr: i64) {
        // Ported: `wallets.updated_at` is NOT NULL with no DEFAULT in the strict
        // SQLite schema, so the Postgres shape fails at runtime.
        test_support::wallet(pool, account_id).await;

        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO topups (id, account_id, amount_idr, order_id, status, rail, created_at)
             VALUES (?, ?, ?, ?, 'pending', 'midtrans', ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(opening_idr)
        .bind(&order_id)
        .bind(Utc::now())
        .execute(pool)
        .await
        .expect("create topup");

        assert_eq!(
            credit_topup_transaction(pool, &order_id, opening_idr, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("credit the opening balance"),
            TopupCreditResult::Settled {
                new_balance: opening_idr
            },
            "the fixture must open the wallet through the real top-up path"
        );
    }

    /// One usage_daily row on a chosen day.
    ///
    /// Written directly rather than through debit_usage_transaction because the
    /// window boundary needs an arbitrary day and the settlement path can only
    /// ever write today's. This touches neither wallets nor ledger, so it is
    /// drift-neutral: the reconciliation invariant is unaffected by it.
    #[allow(clippy::too_many_arguments)]
    async fn insert_usage(
        pool: &SqlitePool,
        account_id: Uuid,
        key_id: Uuid,
        day: NaiveDate,
        input_tokens: i64,
        cache_read_tokens: i64,
        output_tokens: i64,
        cost_idr: i64,
    ) {
        sqlx::query(
            "INSERT INTO usage_daily (
                 account_id, api_key_id, day,
                 input_tokens, cache_read_tokens, output_tokens, cost_idr
             ) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(account_id.hyphenated())
        .bind(key_id.hyphenated())
        .bind(day)
        .bind(input_tokens)
        .bind(cache_read_tokens)
        .bind(output_tokens)
        .bind(cost_idr)
        .execute(pool)
        .await
        .expect("insert usage_daily row");
    }

    /// The reconciliation check from docs/observability.md, scoped to THIS
    /// fixture's account: wallets.balance_idr must equal SUM(ledger.delta_idr).
    /// It must return 0 rows.
    ///
    /// FULL OUTER JOIN, matching `tools/reconcile/reconcile.sh`, the gate this
    /// transcribes. A LEFT JOIN driven from `wallets` asks a weaker question: it sees
    /// only accounts that HAVE a wallet row, while `ledger.account_id` references
    /// `accounts(id)` and not `wallets`, so a ledger row with no wallet is permitted by
    /// the schema and was invisible to the old form. Measured: 5000 IDR of ledger with
    /// no wallet row reports drift=1 here and reported drift=0 before, so an assertion
    /// built on this helper could pass on an account the shipped gate fails. It is a
    /// sibling of `ledger_drift_rows` in `db.rs`, `routes/auth.rs` and
    /// `routes/account.rs`, which carry the same note.
    ///
    /// NOT PINNED BY A TEST OF ITS OWN, and that is measured rather than assumed:
    /// reverting this SQL to the weak LEFT JOIN it used to be survives the ENTIRE suite.
    /// `admin.rs` and `routes/account.rs` carry guards that do catch their own copies
    /// (`the_admin_drift_helper_sees_ledger_money_with_no_wallet_row` and its sibling);
    /// this one and the copies in `keys.rs`, `proxy.rs` and `webhooks.rs` have none, so
    /// nothing would fail if this file silently regressed to the weaker rule.
    ///
    /// Adding a fourth identical test would close the symptom and leave four copies of one
    /// rule, which is the condition that produced the defect. If this helper is touched
    /// again the real fix is to delete the copies and call one shared definition - the
    /// duplication has now cost two rounds.
    async fn drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
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

    async fn json_body<T: serde::de::DeserializeOwned>(res: axum::response::Response) -> T {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read the response body");
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "response body was not the expected JSON: {err}: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    /// create_key driven through the handler. Returns the id, the ONE-TIME
    /// plaintext, and the display prefix.
    async fn create_key_via_handler(
        state: &AppState,
        headers: &HeaderMap,
        label: &str,
        models: Vec<String>,
        spend_limit_idr: i64,
    ) -> (Uuid, String, String) {
        let res = create_key(
            State(state.clone()),
            headers.clone(),
            Json(CreateKeyRequest {
                label: Some(label.to_string()),
                models,
                spend_limit_idr,
                token_limit: 0,
                rate_limit_rpm: 0,
                expires_at: None,
            }),
        )
        .await
        .expect("create_key must succeed")
        .into_response();

        assert_eq!(
            res.status(),
            StatusCode::CREATED,
            "creation must answer 201"
        );
        let body: Value = json_body(res).await;
        (
            serde_json::from_value(body["id"].clone()).expect("id is a UUID"),
            body["key"].as_str().expect("key is a string").to_string(),
            body["prefix"]
                .as_str()
                .expect("prefix is a string")
                .to_string(),
        )
    }

    /// Create a key with a NON-ZERO token ceiling, via the same handler the
    /// dashboard calls. The other helper hard-codes `token_limit: 0`, which is
    /// exactly the value a defect of this shape is invisible behind.
    async fn create_key_with_limit_via_handler(
        state: &AppState,
        headers: &HeaderMap,
        label: &str,
        token_limit: i64,
    ) -> Uuid {
        let res = create_key(
            State(state.clone()),
            headers.clone(),
            Json(CreateKeyRequest {
                label: Some(label.to_string()),
                models: vec!["flash".into()],
                spend_limit_idr: 0,
                token_limit,
                rate_limit_rpm: 0,
                expires_at: None,
            }),
        )
        .await
        .expect("create_key must succeed")
        .into_response();

        assert_eq!(
            res.status(),
            StatusCode::CREATED,
            "creation must answer 201"
        );
        let body: Value = json_body(res).await;
        serde_json::from_value(body["id"].clone()).expect("id is a UUID")
    }

    /// The whole application router, reached the way a caller reaches it: a real
    /// request through create_router, with the peer address mocked so
    /// ConnectInfo resolves. This is the only way to exercise the proxy's
    /// documented enforcement order, which is where a spend limit actually
    /// blocks.
    fn proxy_app(state: AppState) -> axum::Router {
        use axum::extract::connect_info::MockConnectInfo;
        use std::net::SocketAddr;
        crate::routes::create_router(state)
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
    }

    /// POST /v1/chat/completions with a bearer key, returning the status and the
    /// parsed error/JSON body.
    async fn call_proxy(app: &axum::Router, key: &str, model: &str) -> (StatusCode, Value) {
        use axum::body::Body;
        use axum::http::{header, Request};
        use tower::ServiceExt;

        let req = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(
                r#"{{"model":"{model}","stream":true}}"#
            )))
            .expect("build the request");

        let res = app
            .clone()
            .oneshot(req)
            .await
            .expect("the router must respond");
        let status = res.status();
        (status, json_body(res).await)
    }

    /// CREATION stores a HASHED key, never the plaintext.
    ///
    /// docs/website/06-api-keys-and-limits.md: the stored form is a hash of the
    /// full key, and the plaintext is shown exactly once. That is the property
    /// that makes a database leak survivable, so it is asserted against the row
    /// itself, not against the response.
    #[tokio::test]
    async fn creating_a_key_stores_only_a_sha256_digest_never_the_plaintext() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let (key_id, plaintext, prefix) =
            create_key_via_handler(&state, &headers, "hashed", vec!["flash".into()], 0).await;

        // The documented format: apk_live_ + 43 base62 chars. The prefix is the
        // display head of that key - docs/website/06-api-keys-and-limits.md:84
        // shows it as `apk_live_a1b2`, i.e. the 9-char marker plus the first 4
        // body chars - and is explicitly not secret.
        assert!(plaintext.starts_with("apk_live_"), "got {plaintext}");
        assert_eq!(plaintext.len(), "apk_live_".len() + 43);
        assert_eq!(
            prefix,
            &plaintext[..13],
            "prefix is the display head of the key"
        );
        assert_eq!(prefix.len(), 13);

        // The stored form is the SHA-256 digest, hex - never the key itself.
        let stored: String = sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE id = ?")
            .bind(key_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read key_hash");
        assert_eq!(
            stored,
            hash_token(&plaintext),
            "key_hash must be the SHA-256 of the returned plaintext"
        );
        assert_eq!(stored.len(), 64, "a SHA-256 digest in hex is 64 chars");
        assert!(
            stored
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "the digest must be lowercase hex: {stored}"
        );
        assert_ne!(
            stored, plaintext,
            "the plaintext must not be the stored value"
        );

        // And nowhere in the row: a dump must not yield a usable credential.
        // Ported: `row_to_json` is Postgres-only. The subject is the WHOLE row, so
        // the SQLite equivalent concatenates every column explicitly - spelling them
        // out is what makes a column added later fail this test loudly instead of
        // being silently skipped by a `SELECT *`.
        let row_text: String = sqlx::query_scalar(
            "SELECT id || '|' || account_id || '|' || key_hash || '|' || prefix || '|' || \
             COALESCE(label, '') || '|' || COALESCE(models, '') || '|' || \
             spend_limit_idr || '|' || token_limit || '|' || rate_limit_rpm || '|' || \
             COALESCE(expires_at, '') || '|' || COALESCE(last_used_at, '') || '|' || \
             COALESCE(revoked_at, '') || '|' || created_at
             FROM api_keys WHERE id = ?",
        )
        .bind(key_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("read the whole row");
        assert!(
            !row_text.contains(&plaintext),
            "the plaintext key appears in its own row: {row_text}"
        );
        assert!(
            !row_text.contains(&plaintext[9..]),
            "the secret body of the key appears in its own row: {row_text}"
        );

        // docs/website/06: key_hash is never returned by the Rust API handler.
        let listed = list_keys(State(pool.clone()), headers.clone())
            .await
            .expect("list keys")
            .into_response();
        let listed_bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .expect("read the list body");
        let listed_text = String::from_utf8(listed_bytes.to_vec()).expect("list body is UTF-8");
        assert!(
            !listed_text.contains(&stored),
            "GET /api/keys leaked key_hash: {listed_text}"
        );
        assert!(
            !listed_text.contains(&plaintext),
            "GET /api/keys leaked the plaintext key: {listed_text}"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// REVOCATION ACTUALLY REVOKES: after revoke_key, resolving that key through
    /// the REAL lookup path must refuse it. A key that still works after
    /// revocation is the failure this test exists to prevent.
    #[tokio::test]
    async fn revoking_a_key_makes_the_real_lookup_path_refuse_it() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;
        let app = proxy_app(state.clone());

        // The control: a live key with an empty allowlist authenticates and is
        // then refused for its model - 403, not 401. That proves the credential
        // itself was accepted by the lookup path.
        let (_, live_key, _) = create_key_via_handler(&state, &headers, "live", vec![], 0).await;
        let (status, body) = call_proxy(&app, &live_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a live key must authenticate and then be refused for its model: {body}"
        );
        assert_eq!(body["error"]["code"], "model_not_allowed");

        // The key under test, revoked before it is ever presented.
        let (revoked_id, revoked_key, _) =
            create_key_via_handler(&state, &headers, "revoked", vec!["flash".into()], 0).await;

        let res = revoke_key(State(state.clone()), Path(revoked_id), headers.clone())
            .await
            .expect("revoke must succeed")
            .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let (status, body) = call_proxy(&app, &revoked_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a revoked key must be refused by the lookup path: {body}"
        );
        assert_eq!(body["error"]["code"], "key_revoked");

        // Durable, not merely a cache miss: the row itself says so.
        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = ?")
                .bind(revoked_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read revoked_at");
        assert!(
            revoked_at.is_some(),
            "revocation must be durable in the database, not only in the cache"
        );

        // Idempotent: a second revoke matches no row and is still 204.
        let res = revoke_key(State(state.clone()), Path(revoked_id), headers.clone())
            .await
            .expect("a second revoke is not an error")
            .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// CACHE INVALIDATION: the proxy caches key metadata for a TTL, so a
    /// revocation that does not reach that cache stays honoured for up to
    /// limits.key_metadata_cache_seconds. The revoke path is supposed to call
    /// invalidate_key_cache; this proves the effect through the request path
    /// rather than by poking at proxy.rs internals, which keys.rs cannot reach.
    #[tokio::test]
    async fn revoking_a_key_invalidates_the_proxy_cache_instead_of_leaving_it_honoured_for_the_ttl()
    {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;
        let app = proxy_app(state.clone());

        let ttl = state.config.limits.key_metadata_cache_seconds;
        assert!(
            ttl > 0,
            "this test pins the behaviour of an ENABLED key-metadata cache;              key_metadata_cache_seconds is 0, which disables the cache entirely"
        );

        // PART A - the cache is real, and this is the documented tradeoff
        // (docs/website/06-api-keys-and-limits.md, Caching): a revocation that
        // does NOT go through the revoke handler is not seen until the TTL
        // expires. Without this half, Part B could pass for the wrong reason.
        let (stale_id, stale_key, _) =
            create_key_via_handler(&state, &headers, "stale", vec![], 0).await;
        let (status, body) = call_proxy(&app, &stale_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the warm-up request must authenticate: {body}"
        );
        assert_eq!(body["error"]["code"], "model_not_allowed");

        // Ported: SQLite has no `now()`. The instant is bound, which is also what
        // the schema's GLOB CHECK requires - SQLite's own `now()` emits the
        // space-separated form that the CHECK refuses outright.
        sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE id = ?")
            .bind(Utc::now())
            .bind(stale_id.hyphenated())
            .execute(&pool)
            .await
            .expect("revoke out of band, deliberately bypassing invalidate_key_cache");

        let (status, body) = call_proxy(&app, &stale_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a cache-warm key revoked out of band is served until the TTL expires              (that is the documented staleness); a 401 here means the entry was              never cached and Part B would prove nothing: {body}"
        );

        // PART B - the revoke path drops the cached record, so the very next
        // request sees the revocation instead of the pre-revocation row.
        let (id, key, _) = create_key_via_handler(&state, &headers, "invalidated", vec![], 0).await;
        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the warm-up request must authenticate and be cached: {body}"
        );

        let res = revoke_key(State(state.clone()), Path(id), headers.clone())
            .await
            .expect("revoke must succeed")
            .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "revoke_key must invalidate the cached metadata; a 403 here means the              pre-revocation row is still being honoured from the cache: {body}"
        );
        assert_eq!(body["error"]["code"], "key_revoked");

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// LIST REPORTS REAL SPEND: list_keys returns spend_used_idr from
    /// usage_daily, not a hardcoded 0.
    #[tokio::test]
    async fn list_keys_reports_real_spend_from_usage_daily_not_a_hardcoded_zero() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let (used_id, _, _) =
            create_key_via_handler(&state, &headers, "used", vec!["flash".into()], 0).await;
        let (unused_id, _, _) =
            create_key_via_handler(&state, &headers, "unused", vec!["flash".into()], 0).await;

        // Usage for ONE key only.
        let cost_idr = 12_345;
        insert_usage(
            &pool,
            account_id,
            used_id,
            crate::ip_tracking::today_utc(),
            100,
            20,
            50,
            cost_idr,
        )
        .await;

        let res = list_keys(State(pool.clone()), headers.clone())
            .await
            .expect("list keys")
            .into_response();
        let body: Value = json_body(res).await;
        let keys = body.as_array().expect("a JSON array of keys");
        assert_eq!(keys.len(), 2, "both of this account's keys must be listed");

        let spend_of = |id: Uuid| -> i64 {
            let wanted = serde_json::to_value(id).expect("id serialises");
            let row = keys
                .iter()
                .find(|k| k["id"] == wanted)
                .unwrap_or_else(|| panic!("key {id} is missing from list_keys"));
            row["spend_used_idr"]
                .as_i64()
                .expect("spend_used_idr is an integer")
        };

        assert_eq!(
            spend_of(used_id),
            cost_idr,
            "the key with usage must report its real 30-day spend"
        );
        assert_eq!(
            spend_of(unused_id),
            0,
            "a key with no usage in the window must report exactly 0"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// LIST REPORTS THE TOKEN CEILING: every key the list returns carries the
    /// `token_limit` it was created with, because the dashboard's edit form
    /// submits the WHOLE field set on every save rather than a diff. A field
    /// missing from this response is not merely absent from the UI - it is
    /// submitted as its blank default, and the blank default for a limit is 0,
    /// which means unlimited. So the omission this test pins did not hide a
    /// value; it destroyed one, on the next unrelated save.
    #[tokio::test]
    async fn list_keys_publishes_the_token_ceiling_that_a_save_would_otherwise_clear() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let key_id = create_key_with_limit_via_handler(&state, &headers, "capped", 1_000).await;

        let res = list_keys(State(pool.clone()), headers.clone())
            .await
            .expect("list keys")
            .into_response();
        let body: Value = json_body(res).await;
        let row = body
            .as_array()
            .expect("a JSON array of keys")
            .iter()
            .find(|k| k["id"] == serde_json::to_value(key_id).expect("id serialises"))
            .expect("the key just created must be listed");

        assert_eq!(
            row["token_limit"].as_i64(),
            Some(1_000),
            "the key was created with a ceiling of 1000 tokens and the list must say so. A \
             missing or zero token_limit here is not a cosmetic gap: the dashboard reads \
             this field to fill its edit form, and a form that opens blank submits 0 - \
             unlimited - so the next rename would silently raise this key's ceiling to no \
             ceiling at all."
        );

        // The field must be a number in the payload, not a string or null: the
        // island types it `token_limit: number` and assigns it to a number input.
        assert!(
            row["token_limit"].is_i64(),
            "token_limit must serialise as an integer, got {}",
            row["token_limit"]
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// THE 30-DAY WINDOW IS REAL: the window is the trailing 30 days inclusive,
    /// so a row exactly at its first day counts and one a single day earlier does
    /// not. Asserted as an exact total, so any widening OR narrowing of the
    /// window fails this test rather than passing it.
    #[tokio::test]
    async fn the_thirty_day_window_is_real_and_only_in_window_usage_contributes() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let (key_id, _, _) =
            create_key_via_handler(&state, &headers, "window", vec!["flash".into()], 0).await;

        let today = crate::ip_tracking::today_utc();

        // The offsets are LITERALS, deliberately not derived from
        // SPEND_WINDOW_DAYS: a fixture computed from the constant under test
        // moves with it, so widening the window would drag the "outside" row
        // back inside and the test would still pass. Pinning them to the
        // DOCUMENTED contract instead - docs/website/06-api-keys-and-limits.md
        // fixes the window at "the trailing 30 days" - is what makes a changed
        // window fail here.
        //
        // The window holds exactly 30 day-values, so it starts 29 days back:
        // `inside` is its first day (inclusive), `outside` is one day earlier.
        let inside = today - chrono::Duration::days(29);
        let outside = today - chrono::Duration::days(30);
        insert_usage(&pool, account_id, key_id, inside, 10, 0, 10, 1_000).await;
        insert_usage(&pool, account_id, key_id, outside, 10, 0, 10, 5_000).await;

        // The proxy's enforcement read.
        assert_eq!(
            key_spend_used(&pool, account_id, key_id, today)
                .await
                .expect("key_spend_used"),
            1_000,
            "the out-of-window row must NOT contribute: the window is the trailing              {SPEND_WINDOW_DAYS} days inclusive"
        );

        // The dashboard read, which must apply the same window.
        let res = list_keys(State(pool.clone()), headers.clone())
            .await
            .expect("list keys")
            .into_response();
        let body: Value = json_body(res).await;
        let wanted = serde_json::to_value(key_id).expect("id serialises");
        let row = body
            .as_array()
            .expect("array")
            .iter()
            .find(|k| k["id"] == wanted)
            .expect("the key is listed");
        assert_eq!(
            row["spend_used_idr"].as_i64(),
            Some(1_000),
            "list_keys must apply the same window as the enforcement read"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// THE LIMIT ACTUALLY BLOCKS. The pure check_spend_limit test above only
    /// exercises the argument validation; the integration - real usage rows, read
    /// by the real enforcement path, refusing the request - is what it cannot see.
    #[tokio::test]
    async fn real_usage_at_the_spend_limit_makes_the_proxy_refuse_the_request() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;
        let app = proxy_app(state.clone());

        // The wallet is funded through the real top-up path and the usage is
        // recorded through the real settlement path, so the row the proxy reads
        // is the row production would have written.
        let opening_idr = 5_000;
        open_wallet(&pool, account_id, opening_idr).await;

        let (at_limit_id, at_limit_key, _) = create_key_via_handler(
            &state,
            &headers,
            "at-limit",
            vec!["flash".into()],
            opening_idr,
        )
        .await;
        let (_, under_limit_key, _) = create_key_via_handler(
            &state,
            &headers,
            "under-limit",
            vec!["flash".into()],
            opening_idr + 1_000,
        )
        .await;

        let settled = debit_usage_transaction(
            &pool,
            account_id,
            Some(at_limit_id),
            "flash",
            10,
            0,
            10,
            opening_idr,
            Some("test_at_limit"),
            0,
        )
        .await
        .expect("record real usage");
        assert_eq!(
            settled,
            UsageSettlement::Settled { new_balance: 0 },
            "the real settlement path must record the usage and debit the wallet"
        );

        // The key is exactly AT its ceiling: the window total equals the limit.
        assert_eq!(
            key_spend_used(
                &pool,
                account_id,
                at_limit_id,
                crate::ip_tracking::today_utc()
            )
            .await
            .expect("key_spend_used"),
            opening_idr
        );

        let (status, body) = call_proxy(&app, &at_limit_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "a key at its spend limit must be refused before anything is spent: {body}"
        );
        assert_eq!(body["error"]["code"], "key_limit_exceeded");
        assert_eq!(
            body["error"]["details"]["reason"],
            "spend_limit_idr_reached"
        );
        assert_eq!(
            body["error"]["details"]["spend_used_idr"].as_i64(),
            Some(opening_idr),
            "the refusal must report the real window spend"
        );
        assert_eq!(
            body["error"]["details"]["window_days"].as_i64(),
            Some(SPEND_WINDOW_DAYS)
        );

        // The control: the SAME account, the SAME wallet, the SAME body, a limit
        // one rupiah higher. It is not the spend limit that stops it - it reaches
        // the NEXT step of the documented enforcement order and fails there.
        let (status, body) = call_proxy(&app, &under_limit_key, "flash").await;
        assert_eq!(
            body["error"]["code"], "insufficient_balance",
            "a key UNDER its limit must get past the spend check and reach the wallet              check instead (status {status}): {body}"
        );
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// CROSS-ACCOUNT ISOLATION: a key of account A must never appear in B's
    /// list_keys, and B must not be able to revoke or update A's key. A tenancy
    /// property a pure test cannot see.
    #[tokio::test]
    async fn a_key_of_one_account_is_invisible_and_unrevokable_to_another() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_a = create_account(&pool).await;
        let account_b = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers_a = session_cookie(&pool, account_a).await;
        let headers_b = session_cookie(&pool, account_b).await;

        let (a_key_id, _, _) =
            create_key_via_handler(&state, &headers_a, "a", vec!["flash".into()], 0).await;
        let (b_key_id, _, _) =
            create_key_via_handler(&state, &headers_b, "b", vec!["flash".into()], 0).await;

        let listed_ids = |body: Value| -> Vec<Uuid> {
            body.as_array()
                .expect("a JSON array of keys")
                .iter()
                .map(|k| serde_json::from_value(k["id"].clone()).expect("id is a UUID"))
                .collect()
        };

        let listed_a = json_body(
            list_keys(State(pool.clone()), headers_a.clone())
                .await
                .expect("list keys as A")
                .into_response(),
        )
        .await;
        assert_eq!(
            listed_ids(listed_a),
            vec![a_key_id],
            "A's list must hold A's key and nothing else"
        );

        let listed_b = json_body(
            list_keys(State(pool.clone()), headers_b.clone())
                .await
                .expect("list keys as B")
                .into_response(),
        )
        .await;
        assert_eq!(
            listed_ids(listed_b),
            vec![b_key_id],
            "B's list must hold B's key and nothing else"
        );

        // B cannot revoke A's key: the UPDATE is scoped by account_id, so it
        // matches no row. docs/server/api-spec.md:163-166 fixes the response for
        // this endpoint at "Sets revoked_at. Idempotent. 204" - a foreign key is
        // a no-op that still answers 204, deliberately indistinguishable from a
        // successful revoke so the response cannot be used to probe which key ids
        // exist. The tenancy property is therefore not the status code but the
        // EFFECT: A's key must still be active.
        let res = revoke_key(State(state.clone()), Path(a_key_id), headers_b.clone())
            .await
            .expect("the documented revoke response is 204, including for a no-op")
            .into_response();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);

        // ...and A's key is untouched.
        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = ?")
                .bind(a_key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read revoked_at");
        assert!(
            revoked_at.is_none(),
            "B's failed revoke must not have revoked A's key"
        );

        // The same scoping protects the update path. Every enforcement input is
        // in the payload, because a no-op that only skips the label but writes
        // the allowlist would be a cross-account privilege escalation.
        let err = match update_key(
            State(state.clone()),
            Path(a_key_id),
            headers_b.clone(),
            Json(UpdateKeyRequest {
                label: Some("hijacked".into()),
                models: Some(vec![]),
                spend_limit_idr: Some(0),
                token_limit: None,
                rate_limit_rpm: None,
                expires_at: None,
            }),
        )
        .await
        {
            Ok(_) => panic!("B must not be able to update A's key"),
            Err(err) => err,
        };
        assert_eq!(err.status_code(), StatusCode::NOT_FOUND);

        let label: Option<String> = sqlx::query_scalar("SELECT label FROM api_keys WHERE id = ?")
            .bind(a_key_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read label");
        assert_eq!(
            label.as_deref(),
            Some("a"),
            "B's failed update must not have relabelled A's key"
        );

        // ...and B's payload did not widen A's key into an empty allowlist: the
        // no-op is complete, not partial.
        let a_models: Value = sqlx::query_scalar("SELECT models FROM api_keys WHERE id = ?")
            .bind(a_key_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read models");
        assert_eq!(
            a_models,
            json!(["flash"]),
            "B's failed update must not have touched A's allowlist"
        );

        assert_eq!(
            drift_rows(&pool, account_a).await,
            0,
            "fixture A must not drift"
        );
        assert_eq!(
            drift_rows(&pool, account_b).await,
            0,
            "fixture B must not drift"
        );
        db.close().await;
    }

    /// A SUCCESSFUL UPDATE PERSISTS. The 200 is not the property - the ROW is.
    /// Every patched column is read back from `api_keys`: a handler that
    /// answered 200 without writing (or wrote only some of the columns) leaves
    /// the key enforcing its OLD limits while the operator believes otherwise,
    /// which is a silent security hole, not a cosmetic bug.
    #[tokio::test]
    async fn updating_a_key_persists_every_patched_column_to_the_row() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let (key_id, _, _) =
            create_key_via_handler(&state, &headers, "before", vec!["flash".into()], 1_000).await;

        // Truncated to microseconds: Postgres stores timestamptz at microsecond
        // resolution, so a nanosecond-precision instant would not round-trip and
        // the assertion below would fail for a reason that is not the handler.
        let new_expiry = (Utc::now() + chrono::Duration::days(7)).trunc_subsecs(6);

        let res = update_key(
            State(state.clone()),
            Path(key_id),
            headers.clone(),
            Json(UpdateKeyRequest {
                label: Some("after".into()),
                models: Some(vec!["flash".into(), "pro".into()]),
                spend_limit_idr: Some(5_000),
                token_limit: Some(12_345),
                rate_limit_rpm: Some(7),
                expires_at: Some(new_expiry),
            }),
        )
        .await
        .expect("a valid update must succeed")
        .into_response();
        assert_eq!(
            res.status(),
            StatusCode::OK,
            "the documented update response"
        );

        let row = sqlx::query(
            "SELECT label, models, spend_limit_idr, token_limit, rate_limit_rpm, expires_at
             FROM api_keys WHERE id = ?",
        )
        .bind(key_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("read the updated row back");

        let label: Option<String> = row.get("label");
        let models: Value = row.get("models");
        let spend_limit_idr: i64 = row.get("spend_limit_idr");
        let token_limit: i64 = row.get("token_limit");
        let rate_limit_rpm: i32 = row.get("rate_limit_rpm");
        let expires_at: Option<DateTime<Utc>> = row.get("expires_at");

        assert_eq!(label.as_deref(), Some("after"), "label must persist");
        assert_eq!(
            models,
            json!(["flash", "pro"]),
            "models must persist, not stay at the pre-update allowlist"
        );
        assert_eq!(spend_limit_idr, 5_000, "spend_limit_idr must persist");
        assert_eq!(token_limit, 12_345, "token_limit must persist");
        assert_eq!(rate_limit_rpm, 7, "rate_limit_rpm must persist");
        assert_eq!(expires_at, Some(new_expiry), "expires_at must persist");

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// UPDATE INVALIDATES THE PROXY CACHE. The proxy caches key metadata for a
    /// TTL, so a limit narrowed through update_key that does not reach that cache
    /// keeps the OLD limit honoured for up to
    /// limits.key_metadata_cache_seconds. update_key is supposed to call
    /// invalidate_key_cache; this proves the effect through the REAL request path
    /// rather than by poking at proxy.rs internals, which keys.rs cannot reach.
    ///
    /// Two parts, exactly like the revocation test above: Part A shows the cache
    /// is genuinely warm and that a change bypassing the handler is not seen, so
    /// Part B cannot pass merely because the entry was never cached.
    #[tokio::test]
    async fn updating_a_key_invalidates_the_proxy_cache_instead_of_leaving_the_old_limit_honoured_for_the_ttl(
    ) {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;
        let app = proxy_app(state.clone());

        let ttl = state.config.limits.key_metadata_cache_seconds;
        assert!(
            ttl > 0,
            "this test pins the behaviour of an ENABLED key-metadata cache;              key_metadata_cache_seconds is 0, which disables the cache entirely"
        );

        // 5 IDR of real window spend on each key under test, recorded through the
        // drift-neutral usage_daily path: a key narrowed to a 5 IDR ceiling is
        // then exactly AT its limit, so a fresh read refuses it while a stale
        // record (limit 0 = unlimited) lets it through to the wallet check.
        let spend_idr = 5;

        // PART A - the cache is real, and this is the documented tradeoff
        // (docs/website/06-api-keys-and-limits.md, Caching): a limit change that
        // does NOT go through update_key is not seen until the TTL expires.
        let (stale_id, stale_key, _) =
            create_key_via_handler(&state, &headers, "stale", vec!["flash".into()], 0).await;
        insert_usage(
            &pool,
            account_id,
            stale_id,
            crate::ip_tracking::today_utc(),
            100,
            20,
            50,
            spend_idr,
        )
        .await;

        let (status, body) = call_proxy(&app, &stale_key, "flash").await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "the warm-up request must authenticate and be cached: {body}"
        );
        assert_eq!(
            body["error"]["code"], "insufficient_balance",
            "an UNLIMITED key must get past the spend check and reach the wallet check: {body}"
        );

        sqlx::query("UPDATE api_keys SET spend_limit_idr = ? WHERE id = ?")
            .bind(stale_id)
            .bind(spend_idr)
            .execute(&pool)
            .await
            .expect("lower the limit out of band, deliberately bypassing invalidate_key_cache");

        let (status, body) = call_proxy(&app, &stale_key, "flash").await;
        assert_eq!(
            body["error"]["code"], "insufficient_balance",
            "a cache-warm key whose limit was lowered out of band keeps its OLD              (unlimited) limit until the TTL expires - that is the documented              staleness; a key_limit_exceeded here means the entry was never              cached and Part B would prove nothing: {body}"
        );
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);

        // PART B - the update path drops the cached record, so the very next
        // request enforces the NEW limit instead of the pre-update row.
        let (id, key, _) =
            create_key_via_handler(&state, &headers, "invalidated", vec!["flash".into()], 0).await;
        insert_usage(
            &pool,
            account_id,
            id,
            crate::ip_tracking::today_utc(),
            100,
            20,
            50,
            spend_idr,
        )
        .await;

        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "the warm-up request must authenticate and be cached: {body}"
        );
        assert_eq!(body["error"]["code"], "insufficient_balance");

        let res = update_key(
            State(state.clone()),
            Path(id),
            headers.clone(),
            Json(UpdateKeyRequest {
                label: None,
                models: None,
                spend_limit_idr: Some(spend_idr),
                token_limit: None,
                rate_limit_rpm: None,
                expires_at: None,
            }),
        )
        .await
        .expect("lowering the limit to the spend already recorded is a valid update")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "the narrowed limit must refuse the next request: {body}"
        );
        assert_eq!(
            body["error"]["code"], "key_limit_exceeded",
            "update_key must invalidate the cached metadata; an              insufficient_balance here means the PRE-update record (limit 0) is              still being honoured from the cache: {body}"
        );
        assert_eq!(
            body["error"]["details"]["reason"], "spend_limit_idr_reached",
            "the refusal must name the limit that was just narrowed: {body}"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// DENY BY DEFAULT, after an update. docs/website/06-api-keys-and-limits.md:41:
    /// "A key with an empty allowlist can call nothing - deny by default." The
    /// failure mode is the opposite reading - an empty list taken as "every
    /// model" - which would turn narrowing a key into granting it everything.
    /// Asserted through the real request path, and against the row.
    #[tokio::test]
    async fn an_empty_model_allowlist_after_an_update_denies_every_model() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;
        let app = proxy_app(state.clone());

        let (id, key, _) =
            create_key_via_handler(&state, &headers, "narrowed", vec!["flash".into()], 0).await;

        // The control: BEFORE the update the allowlist admits flash, so the key
        // gets past step 2 of the enforcement order and fails at the wallet.
        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "the allowlisted model must reach the wallet check: {body}"
        );
        assert_eq!(body["error"]["code"], "insufficient_balance");

        let res = update_key(
            State(state.clone()),
            Path(id),
            headers.clone(),
            Json(UpdateKeyRequest {
                label: None,
                models: Some(vec![]),
                spend_limit_idr: None,
                token_limit: None,
                rate_limit_rpm: None,
                expires_at: None,
            }),
        )
        .await
        .expect("an empty allowlist is a valid update")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK);

        let (status, body) = call_proxy(&app, &key, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a key with an empty allowlist can call nothing: {body}"
        );
        assert_eq!(body["error"]["code"], "model_not_allowed");

        // And the row really holds the empty list: the refusal above must come
        // from the stored value, not from a stray cache entry.
        let models: Value = sqlx::query_scalar("SELECT models FROM api_keys WHERE id = ?")
            .bind(id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read models");
        assert_eq!(
            models,
            json!([]),
            "the empty allowlist must be what was persisted"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// THE KEY CAP NOW HOLDS UNDER CONCURRENCY, and this is the test that says so.
    ///
    /// The cap was a COUNT taken on the pool, with the INSERT that it guards
    /// happening afterwards and outside any transaction - so concurrent callers all
    /// read the same number and all created a key. The fix holds one write
    /// transaction open across the COUNT and the INSERT, and SQLite serialises
    /// writers, so a second caller either sees the first caller's row or is still
    /// waiting for the lock.
    ///
    /// The assertion is the strict one, and deliberately so: the total must be
    /// EXACTLY the cap, not "about the cap". A test that allowed a small overshoot
    /// would pass against the old code on a fast machine, which is the whole problem
    /// the old code had.
    ///
    /// It is driven through the real handler rather than a helper, so the
    /// transaction the fix introduced is the one under test and not a stand-in for
    /// it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_key_creation_cap_holds_under_concurrent_requests() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = test_support::account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let limit = state.config.limits.key_creation_per_day;
        assert!(limit > 0, "the fixture assumes a configured daily cap");

        // Seed to ONE UNDER, so exactly one more caller is entitled to a key.
        for i in 0..limit - 1 {
            create_key_via_handler(
                &state,
                &headers,
                &format!("seed-{i}"),
                vec!["flash".into()],
                0,
            )
            .await;
        }

        // Sixteen at once, of which at most one may succeed.
        let mut handles = Vec::new();
        for i in 0..16 {
            let state = state.clone();
            let headers = headers.clone();
            handles.push(tokio::spawn(async move {
                create_key(
                    State(state),
                    headers,
                    Json(CreateKeyRequest {
                        label: Some(format!("burst-{i}")),
                        models: vec!["flash".into()],
                        spend_limit_idr: 0,
                        token_limit: 0,
                        rate_limit_rpm: 0,
                        expires_at: None,
                    }),
                )
                .await
                .map(|response| response.into_response().status())
            }));
        }

        let mut created = 0usize;
        let mut refused = 0usize;
        for handle in handles {
            match handle.await.expect("the create task must not panic") {
                Ok(status) => {
                    assert_eq!(
                        status,
                        StatusCode::CREATED,
                        "a permitted key must answer 201, not {status}"
                    );
                    created += 1;
                }
                Err(AppError::RateLimited { .. }) => refused += 1,
                Err(other) => panic!("unexpected error from a burst request: {other:?}"),
            }
        }

        let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count the keys");

        assert_eq!(
            total,
            i64::from(limit),
            "{total} keys for an account whose cap is {limit} ({created} created and \
             {refused} refused across a burst of sixteen). The cap must hold \
             EXACTLY: an assertion that allowed a small overshoot would pass against \
             the old, non-transactional code on a fast machine"
        );
    }

    /// A NEGATIVE CEILING IS REFUSED, AND NOTHING IS WRITTEN.
    ///
    /// The pure tests above only prove the check itself. This one proves the
    /// HANDLERS call it: the proxy skips every limit it reads as non-positive
    /// (`if rate_limit_rpm > 0`, `limit_reached` = `limit > 0 && used >= limit`),
    /// so a negative value that reached the row would silently turn the ceiling
    /// into "unlimited" - the operator sets a budget and gets none. The row
    /// count is asserted too: a handler that refuses with an error but has
    /// already INSERTed (or one that stores the value and errors afterwards)
    /// would leave exactly the key this defect is about behind.
    #[tokio::test]
    async fn creating_a_key_with_a_negative_limit_is_refused_and_writes_no_row() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        // The control: 0 is the documented "no limit of this kind" and MUST stay
        // accepted, so the refusal below cannot come from rejecting non-positive
        // values wholesale.
        create_key_via_handler(&state, &headers, "unlimited", vec!["flash".into()], 0).await;

        for (token_limit, rate_limit_rpm, reason) in [
            (-1_i64, 0_i32, "invalid_token_limit"),
            (0, -1, "invalid_rate_limit_rpm"),
            (-1, -1, "invalid_token_limit"),
        ] {
            let err = match create_key(
                State(state.clone()),
                headers.clone(),
                Json(CreateKeyRequest {
                    label: Some("negative".into()),
                    models: vec!["flash".into()],
                    spend_limit_idr: 0,
                    token_limit,
                    rate_limit_rpm,
                    expires_at: None,
                }),
            )
            .await
            {
                Ok(_) => panic!(
                    "a negative limit must be refused: token_limit={token_limit}, \
                     rate_limit_rpm={rate_limit_rpm}"
                ),
                Err(err) => err,
            };

            // The SAME shape `check_spend_limit` uses: the shared
            // `key_limit_exceeded` code, a 402, and `details.reason` naming the
            // offending field so the dashboard can point at it.
            assert_eq!(err.code(), "key_limit_exceeded");
            assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
            let details = err.details().unwrap();
            assert_eq!(details["reason"], reason);
        }

        // Only the control key exists. A refused create must write NOTHING.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_keys WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("count the account's keys");
        assert_eq!(
            rows, 1,
            "the three refused creates must not have inserted a row"
        );

        let negative_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys
             WHERE account_id = ? AND (token_limit < 0 OR rate_limit_rpm < 0)",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count negative limits");
        assert_eq!(negative_rows, 0, "no negative limit may reach the row");

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// The same rule on the UPDATE path, where it matters most: the row already
    /// exists and is enforcing. A negative PATCH that lands replaces a working
    /// ceiling with "unlimited", so the assertion is not just the 402 - it is
    /// that the stored value is UNCHANGED.
    #[tokio::test]
    async fn updating_a_key_with_a_negative_limit_is_refused_and_leaves_the_row_alone() {
        let _cache = CacheLock::acquire();
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let account_id = create_account(&pool).await;
        let state = test_state(pool.clone());
        let headers = session_cookie(&pool, account_id).await;

        let (key_id, _, _) =
            create_key_via_handler(&state, &headers, "limited", vec!["flash".into()], 0).await;

        // Give the key real, non-zero ceilings so "unchanged" is a meaningful
        // assertion rather than 0 == 0 by accident.
        let res = update_key(
            State(state.clone()),
            Path(key_id),
            headers.clone(),
            Json(UpdateKeyRequest {
                label: None,
                models: None,
                spend_limit_idr: None,
                token_limit: Some(500),
                rate_limit_rpm: Some(60),
                expires_at: None,
            }),
        )
        .await
        .expect("a positive update must succeed")
        .into_response();
        assert_eq!(res.status(), StatusCode::OK, "the control update must land");

        for (token_limit, rate_limit_rpm, reason) in [
            (Some(-1_i64), None, "invalid_token_limit"),
            (None, Some(-1_i32), "invalid_rate_limit_rpm"),
        ] {
            let err = match update_key(
                State(state.clone()),
                Path(key_id),
                headers.clone(),
                Json(UpdateKeyRequest {
                    label: None,
                    models: None,
                    spend_limit_idr: None,
                    token_limit,
                    rate_limit_rpm,
                    expires_at: None,
                }),
            )
            .await
            {
                Ok(_) => panic!("a negative limit must be refused on update: {reason}"),
                Err(err) => err,
            };
            assert_eq!(err.code(), "key_limit_exceeded");
            assert_eq!(err.status_code(), StatusCode::PAYMENT_REQUIRED);
            assert_eq!(err.details().unwrap()["reason"], reason);
        }

        let (token_limit, rate_limit_rpm): (i64, i32) =
            sqlx::query_as("SELECT token_limit, rate_limit_rpm FROM api_keys WHERE id = ?")
                .bind(key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read the row back");
        assert_eq!(
            (token_limit, rate_limit_rpm),
            (500, 60),
            "a refused update must leave the enforced limits untouched"
        );

        assert_eq!(
            drift_rows(&pool, account_id).await,
            0,
            "fixture must not drift"
        );
        db.close().await;
    }

    /// The cookie path that falls through every `session=` piece without a match
    /// (wrong token, revoked, or expired) must resolve to `Unauthenticated`, never
    /// to a panic or a default account. Covers the final `Err` arm at keys.rs:253
    /// and the inner `if let Some(s)` fall-through at keys.rs:249.
    #[tokio::test]
    async fn a_session_cookie_with_no_matching_row_falls_through_to_unauthenticated() {
        let db = TestDb::new().await;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            axum::http::HeaderValue::from_static("session=no-such-token"),
        );
        let result = resolve_account_from_cookie(&db.pool, &headers).await;
        assert!(
            matches!(result, Err(AppError::Unauthenticated)),
            "an unmatched session cookie must be refused as unauthenticated: {result:?}"
        );
        db.close().await;
    }
}
