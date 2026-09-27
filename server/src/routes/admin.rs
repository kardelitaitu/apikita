//! The operator surface: a read-only account lookup, suspend, and resume.
//!
//! The gap this closes: suspending an account used to be a status flag and
//! nothing else, so a suspended account kept every credential it already held -
//! live sessions for up to sessions.absolute_days and live API keys for as long
//! as they lived - and could keep spending money while appearing suspended
//! (docs/admin-surface.md:126-128, docs/launch-checklist.md Gate 3).
//!
//! Everything here is deliberately built from the SAME pieces the customer
//! surface uses, so there is no second code path to drift:
//!
//! - the actor is identified by resolve_account_from_cookie (routes/mod.rs:61),
//!   the one session-cookie parser and resolver - never a query parameter, never
//!   a header, never a separate admin credential (docs/server/api-spec.md:421);
//! - revocation reuses the same predicate logout_all uses for sessions
//!   (auth.rs:309) - `revoked_at IS NULL` - and stamps the keys the way
//!   revoke_key does;
//! - a revoked key is evicted from the proxy's process-wide metadata cache with
//!   proxy::invalidate_key_cache, exactly as revoke_key does - without it a
//!   revoked key stays usable for up to limits.key_metadata_cache_seconds, which
//!   would leave the suspension half-done on the money path;
//! - the audit row is written in the SAME transaction as the effect
//!   (docs/server/api-spec.md:440), so a rolled-back action cannot leave a trail
//!   claiming it happened.
//!
//! Rollout is read-only + suspend/restore (docs/admin-surface.md:179-185); the
//! money actions are deliberately absent.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::error::{ApiErrorBody, ApiErrorResponse, AppError};
use crate::routes::proxy::{invalidate_key_cache, AppState};
use crate::routes::resolve_account_from_cookie;

/// admin_audit.action values written by this module.
const ACTION_SUSPEND: &str = "suspend";
const ACTION_RESUME: &str = "resume";

/// admin_audit.target_type for every action here.
const TARGET_TYPE_ACCOUNT: &str = "account";

// ---------------------------------------------------------------------------
// The operator guard
// ---------------------------------------------------------------------------

/// Why an admin request was refused.
///
/// The two variants exist because the crate's AppError has no 403 that is honest
/// here: ModelNotAllowed is about a model allowlist and WrongCredentialType is
/// documented as RESERVED AND NEVER EMITTED (docs/error-model.md:44, :67-71), so
/// emitting either would make a documented promise false. Forbidden therefore
/// renders the crate's own error envelope (the same ApiErrorResponse /
/// ApiErrorBody structs AppError serialises) with the new stable code
/// "forbidden" - which docs/error-model.md:186 rule 4 permits ("code values are
/// permanent. Adding is fine; changing meaning is not").
///
/// The status is 403 and not 401/404 on purpose. The caller IS authenticated:
/// the cookie resolved to a real account, we know exactly who they are, and they
/// may not do this (docs/error-model.md:52-59, the 401-vs-403 table). A 404
/// would be a lie about a resource the operator surface exists to administer,
/// and a 401 would tell an operator with a perfectly good session to re-login
/// for a permissions problem.
pub enum AdminError {
    /// Any failure the rest of the crate already knows how to render: 401 for a
    /// dead cookie, 404 for an absent target, 409 for a state conflict, 500 for
    /// a database failure.
    App(AppError),
    /// The actor is authenticated but not an operator, or is acting on
    /// themselves. The message says which, because it is for an operator reading
    /// their own dashboard; the CODE stays one value.
    Forbidden(&'static str),
}

impl From<AppError> for AdminError {
    fn from(err: AppError) -> Self {
        Self::App(err)
    }
}

/// A raw sqlx failure on the request path is an AppError::Database, exactly as
/// it is everywhere else in the crate (error.rs:84). Without this, every
/// question-mark on a query inside this module would have to be spelled
/// map_err(AppError::from) by hand.
impl From<sqlx::Error> for AdminError {
    fn from(err: sqlx::Error) -> Self {
        Self::App(AppError::Database(err))
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::App(err) => err.into_response(),
            Self::Forbidden(message) => forbidden_response(message),
        }
    }
}

/// The 403 envelope. Built from the crate's own response structs, so the shape
/// (code/message/request_id/details) cannot drift from every other error the API
/// returns.
fn forbidden_response(message: &str) -> Response {
    let body = ApiErrorResponse {
        error: ApiErrorBody {
            code: "forbidden".to_string(),
            message: message.to_string(),
            request_id: format!("req_{}", Uuid::new_v4().simple()),
            details: None,
        },
    };
    (StatusCode::FORBIDDEN, Json(body)).into_response()
}

/// The ONE operator guard. Every admin handler starts here, so the order of the
/// checks - and therefore what an attacker learns at each step - is defined
/// once.
///
/// Order, and why each step is where it is:
///
/// 1. **Resolve the actor from the session cookie.** A missing, unknown, revoked
///    or expired cookie is 401 unauthenticated, the same answer every other
///    cookie endpoint gives. Nothing else about the request is read first.
/// 2. **Require accounts.is_operator = true.** A real, live session on a
///    non-operator account is 403 forbidden.
/// 3. **Refuse self-action** (see refuse_self_action).
/// 4. **Only then touch the target.**
///
/// Steps 1-3 run BEFORE any target lookup, which is what stops a non-operator
/// from probing the account space: they get the same 403 for an id that exists
/// and an id that does not, so the response cannot be used to enumerate account
/// ids. An operator - already trusted with every account - gets an honest 404
/// for an absent target, because hiding it from them would only make the surface
/// harder to use.
async fn require_operator(state: &AppState, headers: &HeaderMap) -> Result<Uuid, AdminError> {
    let actor = resolve_account_from_cookie(&state.pool, headers).await?;

    let is_operator: Option<bool> =
        sqlx::query_scalar("SELECT is_operator FROM accounts WHERE id = ?")
            .bind(actor.hyphenated())
            .fetch_optional(&state.pool)
            .await?;

    match is_operator {
        Some(true) => Ok(actor),
        Some(false) => Err(AdminError::Forbidden("Operator access required.")),
        // A live session whose account row is gone cannot be authorised: the FK
        // makes this unreachable, and if it ever is reached the safe answer is
        // the same 401 a dead session gets.
        None => Err(AdminError::App(AppError::Unauthenticated)),
    }
}

/// An operator must not act on their own account (docs/admin-surface.md:147,
/// docs/server/api-spec.md:437, docs/launch-checklist.md Gate 3).
///
/// This is checked BEFORE the target is read, so it costs nothing and applies to
/// every action in this module, including the read-only one. Refusing here is
/// what stops an operator from self-suspending, from muddying their own audit
/// trail, or - once money actions exist - from self-crediting.
fn refuse_self_action(actor: Uuid, target: Uuid) -> Result<(), AdminError> {
    if actor == target {
        return Err(AdminError::Forbidden(
            "An operator cannot act on their own account.",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

/// Read-only account view. Carries counts, never credentials: no key_hash, no
/// token_hash, no plaintext of either (docs/admin-surface.md:158-166 - the admin
/// surface must not be able to read a key).
#[derive(Debug, Serialize)]
pub struct AdminAccountResponse {
    pub account_id: Uuid,
    pub status: String,
    pub is_operator: bool,
    pub created_at: DateTime<Utc>,
    pub balance_idr: i64,
    /// Sessions that are usable right now: unrevoked AND unexpired.
    pub live_sessions: i64,
    /// Keys that are usable right now: unrevoked.
    pub live_keys: i64,
}

/// The outcome of a suspend or resume, including the counts of what it revoked.
#[derive(Debug, Serialize)]
pub struct AdminActionResult {
    pub account_id: Uuid,
    pub status: String,
    pub sessions_revoked: i64,
    pub keys_revoked: i64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// GET /api/admin/accounts/{id} - read-only.
///
/// The self-action refusal applies here too, deliberately: an operator reading
/// their own account through the ADMIN surface would be the one exception to a
/// rule that is otherwise total, and their own account is already readable at
/// GET /api/me. One rule, no special case to reason about.
pub async fn get_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let operator_id = require_operator(&state, &headers).await?;
    refuse_self_action(operator_id, id)?;

    let row = sqlx::query(
        r#"
        SELECT
            a.status,
            a.is_operator,
            a.created_at,
            COALESCE(w.balance_idr, 0) AS balance_idr,
            (SELECT COUNT(*) FROM sessions s
              WHERE s.account_id = a.id AND s.revoked_at IS NULL AND s.expires_at > ?)
                AS live_sessions,
            (SELECT COUNT(*) FROM api_keys k
              WHERE k.account_id = a.id AND k.revoked_at IS NULL)
                AS live_keys
        FROM accounts a
        LEFT JOIN wallets w ON w.account_id = a.id
        WHERE a.id = ?
        "#,
    )
    // The expiry bound is BOUND FROM RUST, never SQLite's now(): its
    // space-separated output sorts before the RFC3339 values every timestamp
    // column holds, so a SQL now() here would count expired sessions as live.
    .bind(Utc::now())
    .bind(id.hyphenated())
    .fetch_optional(&state.pool)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound("Account not found".into()).into());
    };

    Ok(Json(AdminAccountResponse {
        account_id: id,
        status: row.try_get("status")?,
        is_operator: row.try_get("is_operator")?,
        created_at: row.try_get("created_at")?,
        balance_idr: row.try_get("balance_idr")?,
        live_sessions: row.try_get("live_sessions")?,
        live_keys: row.try_get("live_keys")?,
    })
    .into_response())
}

/// Query parameters for `GET /api/admin/accounts`.
///
/// Deliberately raw strings rather than a typed int: a malformed value must
/// produce a JSON 422 naming the field, not axum's plain-text extractor
/// rejection (docs/error-model.md requires every response to be JSON), which is
/// the same reasoning `account.rs`'s UsageQuery documents.
#[derive(Debug, Deserialize)]
pub struct AdminListQuery {
    /// Free-text filter, matched case-insensitively against the account id and
    /// the PocketBase id. Nothing else is searchable: email lives in PocketBase,
    /// not here, and the admin surface must not become a second identity store.
    pub q: Option<String>,
    /// Optional status filter. A value outside the schema's vocabulary simply
    /// matches nothing rather than erroring: a filter with no rows is a normal
    /// outcome, not a bad request.
    pub status: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// The listing row. Carries the same counts as the single-account view and
/// NOTHING that could read a credential - no token, no key hash, no email
/// (docs/admin-surface.md:158-166).
#[derive(Debug, Serialize)]
pub struct AdminAccountSummary {
    pub account_id: Uuid,
    pub status: String,
    pub is_operator: bool,
    pub created_at: DateTime<Utc>,
    pub balance_idr: i64,
    pub live_sessions: i64,
    pub live_keys: i64,
}

/// `GET /api/admin/accounts` - a bounded, filterable account listing.
///
/// Why this exists: the single-account route requires the caller to already know
/// a UUID, so an operator had no way to FIND an account. That made the console
/// usable only for accounts whose id had been captured elsewhere, which is not
/// what "administer accounts" means. This is the index that lookup-by-id assumes.
///
/// Safety, in the same order as every sibling handler:
///
/// 1. `require_operator` runs FIRST, before any row is read, so a non-operator
///    gets a 403 that reveals nothing about whether any account exists.
/// 2. `refuse_self_action` deliberately does NOT apply. Reading a LIST is not
///    acting on one account, and the operator's own row is legitimately one row
///    among many - excluding it would make the total count out by one and hide
///    the operator from their own inventory. The self-action rule protects
///    against ACTING on oneself; there is no action here.
/// 3. Only then does the query run, bounded by `limit` so a large table cannot
///    be pulled in one request.
pub async fn list_accounts(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AdminListQuery>,
) -> Result<Response, AdminError> {
    // Step 1: authorize before touching the account space, like every sibling.
    require_operator(&state, &headers).await?;

    let limit = query.limit.unwrap_or(25).clamp(1, 100);
    let offset = query.offset.unwrap_or(0).max(0);

    // An absent or empty filter is "no filter". A non-empty one becomes a LIKE
    // pattern; '%' and '_' are escaped with an explicit ESCAPE clause so a
    // literal percent sign in the input does not silently become a wildcard.
    let needle = query.q.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let pattern = needle.map(|s| {
        let escaped = s
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{escaped}%")
    });
    let status = query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Every placeholder is positional and bound in order. The two NULL tests let
    // one statement serve the filtered and unfiltered cases, so there is no
    // second query to drift from this one.
    let rows = sqlx::query(
        r#"
        SELECT
            a.id,
            a.status,
            a.is_operator,
            a.created_at,
            COALESCE(w.balance_idr, 0) AS balance_idr,
            (SELECT COUNT(*) FROM sessions s
              WHERE s.account_id = a.id AND s.revoked_at IS NULL AND s.expires_at > ?)
                AS live_sessions,
            (SELECT COUNT(*) FROM api_keys k
              WHERE k.account_id = a.id AND k.revoked_at IS NULL)
                AS live_keys
        FROM accounts a
        LEFT JOIN wallets w ON w.account_id = a.id
        WHERE (? IS NULL OR a.id LIKE ? ESCAPE '\' OR a.pb_user_id LIKE ? ESCAPE '\')
          AND (? IS NULL OR a.status = ?)
        ORDER BY a.created_at DESC
        LIMIT ? OFFSET ?
        "#,
    )
    // The session-expiry bound is BOUND FROM RUST, never SQLite's now(): its
    // space-separated output sorts before the RFC3339 values every timestamp
    // column holds, so a SQL now() here would count expired sessions as live.
    .bind(Utc::now())
    .bind(pattern.as_deref())
    .bind(pattern.as_deref())
    .bind(pattern.as_deref())
    .bind(status)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let mut accounts: Vec<AdminAccountSummary> = Vec::with_capacity(rows.len());
    for row in rows {
        // id is TEXT in the schema; decode it the way the rest of the crate does.
        let id_raw: String = row.try_get("id")?;
        let account_id = Uuid::parse_str(&id_raw).map_err(|e| {
            AdminError::App(AppError::Internal(format!(
                "accounts.id is not a uuid: {e}"
            )))
        })?;
        accounts.push(AdminAccountSummary {
            account_id,
            status: row.try_get("status")?,
            is_operator: row.try_get("is_operator")?,
            created_at: row.try_get("created_at")?,
            balance_idr: row.try_get("balance_idr")?,
            live_sessions: row.try_get("live_sessions")?,
            live_keys: row.try_get("live_keys")?,
        });
    }

    Ok(Json(json!({
        "accounts": accounts,
        "limit": limit,
        "offset": offset,
    }))
    .into_response())
}

/// POST /api/admin/accounts/{id}/suspend - the security-critical action.
///
/// ONE transaction does all three things, and the audit row is written inside it
/// so a rollback cannot leave a trail for an action that did not happen:
///
/// 1. accounts.status = 'suspended';
/// 2. revoke EVERY live session for the account;
/// 3. revoke EVERY live API key for the account.
///
/// The transaction takes SQLite's write lock UP FRONT (BEGIN IMMEDIATE), so two
/// concurrent suspends of the same account serialise and the second sees
/// 'suspended' and refuses, instead of both reporting a successful revocation.
/// The Postgres original relied on `SELECT ... FOR UPDATE` here; SQLite has no row
/// locks and rejects that clause as a syntax error.
///
/// Only an 'active' account can be suspended. Anything else (already suspended,
/// or closed) is a 409 and writes NO audit row: there is nothing to do, and a
/// non-active account cannot acquire new credentials in the meantime - the login
/// path refuses a non-active account outright (auth.rs:204), so it cannot mint a
/// session, and the only way to mint a key is with one.
///
/// The cache eviction is deliberately AFTER the commit: evicting before it would
/// let a rollback empty the cache for no reason, and the eviction is a
/// process-local best-effort step, never part of the durability guarantee. The
/// revocation itself is durable in api_keys.revoked_at, which is what the
/// request path reads on a cache miss.
pub async fn suspend_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let operator_id = require_operator(&state, &headers).await?;
    refuse_self_action(operator_id, id)?;

    // BEGIN IMMEDIATE, not a deferred BEGIN. SQLite has no row locks and
    // rejects FOR UPDATE as a syntax error (measured), so the write lock taken up
    // front is what serialises two concurrent suspends of one account: the second
    // then sees 'suspended' and refuses, instead of both reporting a successful
    // revocation. See db.rs::begin_immediate for the full reasoning.
    let mut tx = crate::db::begin_immediate(&state.pool).await?;

    let target = sqlx::query("SELECT status FROM accounts WHERE id = ?")
        .bind(id.hyphenated())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(target) = target else {
        return Err(AppError::NotFound("Account not found".into()).into());
    };
    let status_from: String = target.try_get("status")?;
    if status_from != "active" {
        return Err(AppError::Conflict(format!(
            "Account is '{status_from}', not 'active'; nothing to suspend."
        ))
        .into());
    }

    sqlx::query("UPDATE accounts SET status = 'suspended', updated_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(id.hyphenated())
        .execute(&mut *tx)
        .await?;

    // The exact statement logout_all already uses, so "revoked" means the same
    // thing here as it does everywhere else. "revoked_at IS NULL" is the whole
    // predicate: an already-revoked row is not touched, which is what makes the
    // count an honest count of what this call revoked.
    let sessions_revoked = sqlx::query(
        "UPDATE sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL",
    )
    .bind(Utc::now())
    .bind(id.hyphenated())
    .execute(&mut *tx)
    .await?
    .rows_affected() as i64;

    // RETURNING the hash is what makes the cache eviction possible: the plaintext
    // key exists nowhere (it was shown once, at creation), so the hash is the
    // only handle the process-wide cache can be addressed by.
    let revoked_key_hashes: Vec<String> = sqlx::query_scalar(
        "UPDATE api_keys SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL \
         RETURNING key_hash",
    )
    .bind(Utc::now())
    .bind(id.hyphenated())
    .fetch_all(&mut *tx)
    .await?;
    let keys_revoked = revoked_key_hashes.len() as i64;

    let detail = json!({
        "sessions_revoked": sessions_revoked,
        "keys_revoked": keys_revoked,
        "status_from": status_from,
        "status_to": "suspended",
    });

    // created_at is bound from Rust: the SQLite schema has no DEFAULT for it
    // (plan section 4.1, correction 1 - the Postgres now() default was
    // deliberately removed), so omitting it fails the NOT NULL constraint at
    // runtime. This is the defect a compiler cannot see.
    sqlx::query(
        "INSERT INTO admin_audit
             (operator_id, action, target_type, target_id, detail, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(operator_id.hyphenated())
    .bind(ACTION_SUSPEND)
    .bind(TARGET_TYPE_ACCOUNT)
    .bind(id.to_string())
    .bind(&detail)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    // Only THIS process's cache is dropped; an instance behind the load balancer
    // keeps its copy until its TTL expires. That residual window is the
    // documented cost of the cache (proxy.rs:571-576) and is not closed here.
    for key_hash in &revoked_key_hashes {
        invalidate_key_cache(&state.config, key_hash);
    }

    Ok(Json(AdminActionResult {
        account_id: id,
        status: "suspended".to_string(),
        sessions_revoked,
        keys_revoked,
    })
    .into_response())
}

/// POST /api/admin/accounts/{id}/restore (also mounted at /resume).
///
/// Sets the status back to 'active' and audits it. It does NOT resurrect the
/// revoked sessions or keys, and that is the correct security posture, not an
/// oversight:
///
/// - the credentials revoked by a suspension were revoked because the account
///   was abusive (or compromised). Restoring the STATUS is a statement about the
///   account's standing, not about the trustworthiness of credentials that were
///   already in the wild at the moment of suspension;
/// - "unrevoke" would mean resurrecting a session token the customer may not
///   even hold any more, and re-enabling a key whose plaintext only ever existed
///   once at creation - so the account could not be made whole by it anyway;
/// - the customer re-authenticates (which mints a fresh session) and issues a
///   fresh key, which is one deliberate step and leaves the audit trail clean.
///
/// docs/admin-surface.md:120 states the same contract: "Does **not** restore
/// keys; the customer reissues".
pub async fn resume_account(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let operator_id = require_operator(&state, &headers).await?;
    refuse_self_action(operator_id, id)?;

    let mut tx = crate::db::begin_immediate(&state.pool).await?;

    let target = sqlx::query("SELECT status FROM accounts WHERE id = ?")
        .bind(id.hyphenated())
        .fetch_optional(&mut *tx)
        .await?;
    let Some(target) = target else {
        return Err(AppError::NotFound("Account not found".into()).into());
    };
    let status_from: String = target.try_get("status")?;
    if status_from != "suspended" {
        return Err(AppError::Conflict(format!(
            "Account is '{status_from}', not 'suspended'; nothing to resume."
        ))
        .into());
    }

    sqlx::query("UPDATE accounts SET status = 'active', updated_at = ? WHERE id = ?")
        .bind(Utc::now())
        .bind(id.hyphenated())
        .execute(&mut *tx)
        .await?;

    let detail = json!({
        "status_from": status_from,
        "status_to": "active",
        "sessions_revoked": 0,
        "keys_revoked": 0,
    });

    // created_at is bound from Rust for the same reason as the suspend path
    // above: the SQLite schema gives the column no DEFAULT.
    sqlx::query(
        "INSERT INTO admin_audit
             (operator_id, action, target_type, target_id, detail, created_at)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(operator_id.hyphenated())
    .bind(ACTION_RESUME)
    .bind(TARGET_TYPE_ACCOUNT)
    .bind(id.to_string())
    .bind(&detail)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(Json(AdminActionResult {
        account_id: id,
        status: "active".to_string(),
        // Reported as zero rather than omitted: the caller must be able to see
        // that resuming did not hand the credentials back.
        sessions_revoked: 0,
        keys_revoked: 0,
    })
    .into_response())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::Router;
    use tower::ServiceExt;

    use crate::config::AppConfig;
    use crate::ip_tracking::{parse_cidrs, DailySalt, IpCidr};
    use crate::routes::auth::{exchange_token, AuthExchangeRequest};
    use crate::routes::events::RealtimeHub;
    use crate::routes::hash_token;
    use crate::routes::keys::{create_key, CreateKeyRequest};
    use crate::test_support::{self, TestDb};
    use serde_json::Value;
    use sqlx::SqlitePool;

    // -----------------------------------------------------------------------
    // A loopback PocketBase, for the login-after-suspend test.
    //
    // The test this replaces was `#[ignore]`d with the note "faking that would
    // test the fake". That is true of faking the LOGIC, but the PEER can be a
    // stub: `verify_pb_token` builds its URL from `pocketbase_base_url()`, which
    // reads `POCKETBASE_URL` on every call. Pointing that variable at a loopback
    // listener drives the REAL client, the REAL status handling and the REAL
    // `exchange_token` handler - no mock, no trait object, no production change.
    //
    // This is the same technique that took `routes/auth.rs` from 58.6% to 98.2%,
    // applied to the last remaining `#[ignore]` in the crate.
    // -----------------------------------------------------------------------

    /// A one-shot local HTTP stub standing in for PocketBase auth-refresh.
    async fn pocketbase_stub(status_line: &str, body: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the PocketBase stub");
        let addr = listener.local_addr().expect("stub address");

        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{addr}")
    }

    /// A successful auth-refresh body, shaped as PocketBase sends it.
    fn auth_refresh_body(record_id: &str) -> String {
        format!(
            r#"{{"token":"new.jwt.value","record":{{"id":"{record_id}","email":"u@example.com"}}}}"#
        )
    }

    /// Point POCKETBASE_URL at the stub for the duration of the test. Takes the
    /// shared env lock, so it cannot interleave with another test reading the
    /// same process-global.
    fn point_pocketbase_at(
        base: &str,
    ) -> (
        crate::routes::test_env::EnvLock,
        crate::routes::test_env::EnvGuard,
    ) {
        let lock = crate::routes::test_env::EnvLock::acquire();
        let guard = crate::routes::test_env::EnvGuard::set("POCKETBASE_URL", base);
        (lock, guard)
    }
    // -----------------------------------------------------------------------
    // Fixtures. Same style as keys.rs/account.rs: a real, migrated database, the
    // real handlers, and the real money path - never a hand-written balance.
    // Each test builds its OWN SQLite database in a temp directory (TestDb), so
    // the suite runs with no live server and no row-by-row teardown.
    // -----------------------------------------------------------------------

    fn live_config() -> Arc<AppConfig> {
        for path in ["../config/apikita.toml", "config/apikita.toml"] {
            if std::path::Path::new(path).exists() {
                return Arc::new(AppConfig::load_from_file(path).expect("parse apikita.toml"));
            }
        }
        panic!("could not find apikita.toml for testing");
    }

    /// The real application state, built the way main.rs builds it, so the
    /// handler evicts from the same process-wide key cache the proxy reads.
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

    /// The whole router, reached the way a caller reaches it.
    fn app(state: AppState) -> Router {
        use axum::extract::connect_info::MockConnectInfo;
        use std::net::SocketAddr;
        crate::routes::create_router(state)
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
    }

    /// An account row. The SQLite schema has no DEFAULT for id, created_at or
    /// updated_at (plan section 4.1, correction 1), so the Postgres
    /// RETURNING id shape would fail at runtime rather than here.
    async fn create_account(pool: &SqlitePool) -> Uuid {
        test_support::account(pool).await
    }

    /// An account with the operator flag. The flag is a column, not a table
    /// (docs/decisions.md:92), so this is the whole fixture.
    async fn create_operator(pool: &SqlitePool) -> Uuid {
        let id = Uuid::new_v4();
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO accounts (id, pb_user_id, is_operator, created_at, updated_at)
             VALUES (?, ?, 1, ?, ?)",
        )
        .bind(id.hyphenated())
        .bind(format!("test_op_{}", id.simple()))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create operator account");
        id
    }

    /// A real sessions row, plus the token whose cookie resolves to it, so every
    /// handler below is reached through the production authentication path.
    async fn issue_session(pool: &SqlitePool, account_id: Uuid) -> String {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        // Every NOT NULL column is bound from Rust: the SQLite schema has no
        // DEFAULT for id, last_seen_at or created_at, and now() + interval has no
        // SQLite spelling that keeps the RFC3339-offset form the timestamp GLOB
        // CHECK requires.
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
        token
    }

    fn cookie_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// Money enters a wallet ONLY through credit_topup_transaction, which writes
    /// the matching ledger row in the same transaction. Writing
    /// wallets.balance_idr directly manufactures exactly the drift the
    /// reconciliation assertion at the end of every test looks for.
    async fn open_wallet(pool: &SqlitePool, account_id: Uuid, opening_idr: i64) {
        test_support::wallet(pool, account_id).await;
        test_support::fund(pool, account_id, opening_idr).await;
    }

    /// A key created through the REAL handler, returning (id, plaintext). The
    /// plaintext is needed to present the key to the proxy; the id to read the
    /// row back.
    async fn create_key_via_handler(
        state: &AppState,
        headers: &HeaderMap,
        label: &str,
        models: Vec<String>,
    ) -> (Uuid, String) {
        let res = create_key(
            State(state.clone()),
            headers.clone(),
            Json(CreateKeyRequest {
                label: Some(label.to_string()),
                models,
                spend_limit_idr: 0,
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
        )
    }

    /// Live = usable right now. Both bounds matter for a session: an
    /// expired-but-unrevoked row is not a credential.
    async fn live_sessions(pool: &SqlitePool, account_id: Uuid) -> i64 {
        // The expiry bound is bound from Rust, not SQLite's now(): the
        // space-separated form it emits sorts before every RFC3339 value the
        // column holds, so a SQL now() would count expired sessions as live.
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions
              WHERE account_id = ? AND revoked_at IS NULL AND expires_at > ?",
        )
        .bind(account_id.hyphenated())
        .bind(Utc::now())
        .fetch_one(pool)
        .await
        .expect("count live sessions")
    }

    async fn live_keys(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM api_keys WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("count live keys")
    }

    async fn account_status(pool: &SqlitePool, account_id: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read status")
    }

    /// Every audit row aimed at one target, as (operator_id, action, target_type,
    /// detail). Read straight from the table: a response is not evidence.
    async fn audit_rows(
        pool: &SqlitePool,
        target_id: Uuid,
    ) -> Vec<(Uuid, String, String, Option<Value>)> {
        let rows = sqlx::query(
            "SELECT operator_id, action, target_type, detail FROM admin_audit
              WHERE target_id = ? ORDER BY id",
        )
        .bind(target_id.to_string())
        .fetch_all(pool)
        .await
        .expect("read admin_audit");

        rows.iter()
            .map(|row| {
                (
                    row.try_get::<uuid::fmt::Hyphenated, _>("operator_id")
                        .expect("operator_id")
                        .into_uuid(),
                    row.try_get("action").expect("action"),
                    row.try_get("target_type").expect("target_type"),
                    row.try_get("detail").expect("detail"),
                )
            })
            .collect()
    }

    /// docs/observability.md's reconciliation, scoped to THIS fixture's account:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr). Must return 0.
    async fn drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
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

    /// Every branch of every test ends here: the fixture must not have drifted.
    async fn assert_no_drift(pool: &SqlitePool, account_ids: &[Uuid]) {
        for account_id in account_ids {
            assert_eq!(
                drift_rows(pool, *account_id).await,
                0,
                "wallets.balance_idr must equal SUM(ledger.delta_idr) for {account_id}"
            );
        }
    }

    async fn json_body(res: Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read the response body");
        if bytes.is_empty() {
            return Value::Null;
        }
        serde_json::from_slice(&bytes).unwrap_or_else(|err| {
            panic!(
                "response body was not JSON: {err}: {}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    /// Renders a handler result the way axum would, so an error variant is
    /// asserted through the SAME envelope a caller sees.
    ///
    /// Takes the FUTURE, not the finished Result: a handler call and its await
    /// are then one expression at every call site, which is what keeps the
    /// assertion reading like the handler invocation it is testing.
    async fn render<F>(result: F) -> (StatusCode, Value)
    where
        F: std::future::Future<Output = Result<Response, AdminError>>,
    {
        let res = match result.await {
            Ok(ok) => ok,
            Err(err) => err.into_response(),
        };
        let status = res.status();
        (status, json_body(res).await)
    }

    /// Renders an AppError-returning handler the way axum would.
    async fn render_axum<T: IntoResponse>(result: Result<T, AppError>) -> (StatusCode, Value) {
        let res = match result {
            Ok(ok) => ok.into_response(),
            Err(err) => err.into_response(),
        };
        let status = res.status();
        (status, json_body(res).await)
    }

    /// A request through the whole router, so the real extractors and the real
    /// dispatch order run.
    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
        let res = app
            .clone()
            .oneshot(req)
            .await
            .expect("the router must respond");
        let status = res.status();
        (status, json_body(res).await)
    }

    async fn get_me(app: &Router, token: &str) -> (StatusCode, Value) {
        call(
            app,
            Request::builder()
                .method("GET")
                .uri("/api/me")
                .header(header::COOKIE, format!("session={token}"))
                .body(Body::empty())
                .expect("build the request"),
        )
        .await
    }

    async fn call_proxy(app: &Router, key: &str, model: &str) -> (StatusCode, Value) {
        call(
            app,
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::AUTHORIZATION, format!("Bearer {key}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"model":"{model}","stream":true}}"#
                )))
                .expect("build the request"),
        )
        .await
    }
    // -----------------------------------------------------------------------
    // (a) + (d) + (i): suspend revokes EVERY live session and EVERY live key,
    // counted from the tables, and writes exactly ONE audit row.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn suspend_revokes_every_live_session_and_every_live_key() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        open_wallet(&pool, victim, 50_000).await;

        // Two live sessions - one of which is the victim's own dashboard
        // session, used below to create the victim's keys - plus one
        // EXPIRED-but-unrevoked row: the revocation predicate is
        // "revoked_at IS NULL" (the statement logout_all uses), so the expired
        // row is revoked too, and the LIVE count is what must reach zero.
        let victim_headers = cookie_headers(&issue_session(&pool, victim).await);
        issue_session(&pool, victim).await;
        // The expired row carries every NOT NULL column from Rust, and its
        // expiry is computed in Rust: now() - interval has no SQLite spelling
        // that keeps the RFC3339-offset form the GLOB CHECK requires.
        let expired_now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(victim.hyphenated())
        .bind(hash_token(&format!("apk_sess_{}", Uuid::new_v4().simple())))
        .bind(expired_now - chrono::Duration::days(1))
        .bind(expired_now)
        .bind(expired_now)
        .execute(&pool)
        .await
        .expect("create an expired session row");

        // Three keys, all the VICTIM's (created through the real handler with
        // the victim's own session): one already revoked out of band (so it must
        // NOT be counted or re-stamped), two live.
        let (_, _) = create_key_via_handler(&state, &victim_headers, "warm", vec![]).await;
        let (_, _) = create_key_via_handler(&state, &victim_headers, "warm2", vec![]).await;
        let (dead_key_id, _) =
            create_key_via_handler(&state, &victim_headers, "dead", vec![]).await;
        sqlx::query("UPDATE api_keys SET revoked_at = ? WHERE id = ?")
            .bind(Utc::now())
            .bind(dead_key_id.hyphenated())
            .execute(&pool)
            .await
            .expect("pre-revoke one key");

        assert_eq!(
            live_sessions(&pool, victim).await,
            2,
            "fixture: exactly two live sessions before the suspend"
        );
        assert_eq!(
            live_keys(&pool, victim).await,
            2,
            "fixture: exactly two live keys before the suspend"
        );
        assert_eq!(
            audit_rows(&pool, victim).await.len(),
            0,
            "fixture: no audit rows before the suspend"
        );

        let (status, body) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers.clone(),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        // Read the TABLES, not the response.
        assert_eq!(
            account_status(&pool, victim).await,
            "suspended",
            "the status must be durable"
        );
        assert_eq!(
            live_sessions(&pool, victim).await,
            0,
            "EVERY live session must be revoked by the suspend"
        );
        assert_eq!(
            live_keys(&pool, victim).await,
            0,
            "EVERY live key must be revoked by the suspend"
        );

        // Not one unrevoked session row is left, expired ones included.
        let unrevoked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sessions WHERE account_id = ? AND revoked_at IS NULL",
        )
        .bind(victim.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count unrevoked sessions");
        assert_eq!(
            unrevoked, 0,
            "the suspend must revoke on revoked_at IS NULL, the predicate logout_all uses"
        );

        // The response's own counts must agree with the tables. The session
        // count is 3, not 2: the revocation predicate is "revoked_at IS NULL",
        // so the expired-but-unrevoked row is revoked as well. That is the
        // honest count of rows this call revoked, and the LIVE count above is
        // what proves the credentials are gone.
        assert_eq!(body["sessions_revoked"], json!(3), "body: {body}");
        assert_eq!(body["keys_revoked"], json!(2), "body: {body}");
        assert_eq!(body["status"], json!("suspended"), "body: {body}");

        // The already-revoked key stays revoked (it is not re-stamped).
        let dead_revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = ?")
                .bind(dead_key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read the pre-revoked key");
        assert!(
            dead_revoked_at.is_some(),
            "a pre-revoked key stays revoked and is not re-counted"
        );

        // (d) exactly ONE audit row, with the right operator/action/target and a
        // non-null detail carrying the counts.
        let audit = audit_rows(&pool, victim).await;
        assert_eq!(
            audit.len(),
            1,
            "exactly one audit row per suspend: {audit:?}"
        );
        let (audit_operator, action, target_type, detail) = &audit[0];
        assert_eq!(*audit_operator, operator, "the OPERATOR must be recorded");
        assert_eq!(action, "suspend");
        assert_eq!(target_type, "account");
        let detail = detail.as_ref().expect("detail must not be null");
        assert_eq!(detail["sessions_revoked"], json!(3), "detail: {detail}");
        assert_eq!(detail["keys_revoked"], json!(2), "detail: {detail}");
        assert_eq!(detail["status_from"], json!("active"), "detail: {detail}");

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }
    /// (d), second half: a FAILED suspend writes no audit row - and changes
    /// nothing else either.

    #[tokio::test]
    async fn a_failed_suspend_writes_no_audit_row() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        let (status, body) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers.clone(),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(audit_rows(&pool, victim).await.len(), 1);

        // Suspending an account that is not 'active' is refused...
        let (status, body) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers.clone(),
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "a second suspend has nothing to do and must be refused: {body}"
        );
        assert_eq!(body["error"]["code"], json!("conflict"));

        // ...and writes NO second audit row: an audit row for an action that did
        // not happen is as bad as no audit row (docs/admin-surface.md:139-140).
        assert_eq!(
            audit_rows(&pool, victim).await.len(),
            1,
            "a refused suspend must not add an audit row"
        );

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }

    /// (b) A session revoked by the suspend can no longer resolve - driven
    /// through the REAL request path, not by reading revoked_at.

    #[tokio::test]
    async fn a_session_revoked_by_suspend_can_no_longer_resolve() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);
        let victim_token = issue_session(&pool, victim).await;
        let app = app(state.clone());

        open_wallet(&pool, victim, 1_000).await;

        // Control: the victim's session is live and authenticates.
        let (status, body) = get_me(&app, &victim_token).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the fixture session must authenticate before the suspend: {body}"
        );

        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        // The SAME cookie, through the SAME route, is now refused.
        let (status, body) = get_me(&app, &victim_token).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a session revoked by suspend must stop resolving immediately: {body}"
        );
        assert_eq!(body["error"]["code"], json!("unauthenticated"));

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }

    /// (c) A key revoked by the suspend is refused by the real proxy path - and
    /// refused even when the proxy's metadata cache was already warm, which is
    /// the half a status-flag-only suspend would miss.

    #[tokio::test]
    async fn a_key_revoked_by_suspend_is_refused_by_the_proxy_path() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);
        let app = app(state.clone());

        open_wallet(&pool, victim, 1_000).await;

        // An empty allowlist so the request is refused for its model (403) after
        // authenticating - which WARMS the metadata cache without ever reaching
        // an upstream. The key belongs to the VICTIM: it is the victim's
        // credential the suspend must kill.
        let victim_headers = cookie_headers(&issue_session(&pool, victim).await);
        let (key_id, plaintext) =
            create_key_via_handler(&state, &victim_headers, "victim", vec![]).await;

        let (status, body) = call_proxy(&app, &plaintext, "flash").await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the control request must authenticate the key and then be refused for its model: {body}"
        );
        assert_eq!(body["error"]["code"], json!("model_not_allowed"));

        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        // The SAME key, through the SAME route, is now refused as revoked - not
        // 403 model_not_allowed, which is what a stale cache entry would give.
        let (status, body) = call_proxy(&app, &plaintext, "flash").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a key revoked by suspend must be refused by the proxy path: {body}"
        );
        assert_eq!(body["error"]["code"], json!("key_revoked"));

        let revoked_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = ?")
                .bind(key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read revoked_at");
        assert!(
            revoked_at.is_some(),
            "the revocation must be durable in the database, not only in the cache"
        );

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }
    /// (e) A non-operator is refused, and NOTHING happens to the target.

    #[tokio::test]
    async fn a_non_operator_is_refused_and_the_target_is_unchanged() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let nobody = create_account(&pool).await;
        let victim = create_account(&pool).await;
        let nobody_headers = cookie_headers(&issue_session(&pool, nobody).await);
        let app = app(state.clone());

        open_wallet(&pool, victim, 1_000).await;
        let victim_token = issue_session(&pool, victim).await;
        let victim_headers = cookie_headers(&victim_token);
        let (_, _) = create_key_via_handler(&state, &victim_headers, "live", vec![]).await;

        // 403, not 401 (the session is real) and not 404 (the target is real).
        let (status, body) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            nobody_headers.clone(),
        ))
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a non-operator must be refused with 403: {body}"
        );
        assert_eq!(body["error"]["code"], json!("forbidden"));

        // The same refusal for an id that does NOT exist: a non-operator cannot
        // tell the two apart, so the response cannot enumerate account ids.
        let (absent_status, absent_body) = render(suspend_account(
            State(state.clone()),
            Path(Uuid::new_v4()),
            nobody_headers.clone(),
        ))
        .await;
        assert_eq!(
            absent_status,
            StatusCode::FORBIDDEN,
            "an absent target must look exactly like a real one to a non-operator: {absent_body}"
        );

        // And nothing happened.
        assert_eq!(account_status(&pool, victim).await, "active");
        assert_eq!(live_sessions(&pool, victim).await, 1);
        assert_eq!(live_keys(&pool, victim).await, 1);
        assert_eq!(
            audit_rows(&pool, victim).await.len(),
            0,
            "a refused suspend must write no audit row"
        );
        // The read-only lookup is guarded too.
        let (status, _) = render(get_account(
            State(state.clone()),
            Path(victim),
            nobody_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // The victim's own session still works.
        let (status, _) = get_me(&app, &victim_token).await;
        assert_eq!(status, StatusCode::OK);

        assert_no_drift(&pool, &[nobody, victim]).await;
        db.close().await;
    }

    /// (f) An operator cannot act on themselves - and their own credentials are
    /// untouched.

    #[tokio::test]
    async fn an_operator_cannot_act_on_themselves() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let self_headers = cookie_headers(&issue_session(&pool, operator).await);
        let victim = create_account(&pool).await;

        open_wallet(&pool, operator, 1_000).await;
        let (_, _) = create_key_via_handler(&state, &self_headers, "own", vec![]).await;

        for (status, body) in [
            render(suspend_account(
                State(state.clone()),
                Path(operator),
                self_headers.clone(),
            ))
            .await,
            render(resume_account(
                State(state.clone()),
                Path(operator),
                self_headers.clone(),
            ))
            .await,
            render(get_account(
                State(state.clone()),
                Path(operator),
                self_headers.clone(),
            ))
            .await,
        ] {
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "an operator acting on themselves must be refused: {body}"
            );
            assert_eq!(body["error"]["code"], json!("forbidden"));
        }

        assert_eq!(
            account_status(&pool, operator).await,
            "active",
            "the operator's own account must be untouched"
        );
        assert_eq!(
            live_sessions(&pool, operator).await,
            1,
            "the operator's own session must survive"
        );
        assert_eq!(
            live_keys(&pool, operator).await,
            1,
            "the operator's own key must survive"
        );
        assert_eq!(
            audit_rows(&pool, operator).await.len(),
            0,
            "a refused self-action must write no audit row"
        );

        // Sanity: the same operator CAN act on someone else.
        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            self_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }
    /// (g) Resume sets the status back to active, revokes nothing, and audits.

    #[tokio::test]
    async fn resume_sets_status_active_and_revokes_nothing() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;

        open_wallet(&pool, victim, 1_000).await;
        let victim_token = issue_session(&pool, victim).await;
        let victim_headers = cookie_headers(&victim_token);
        let (key_id, _) = create_key_via_handler(&state, &victim_headers, "live", vec![]).await;

        // Suspended by hand: the point of this test is what RESUME does, and a
        // live session + live key is exactly the state resume must leave alone.
        sqlx::query("UPDATE accounts SET status = 'suspended' WHERE id = ?")
            .bind(victim.hyphenated())
            .execute(&pool)
            .await
            .expect("suspend the fixture out of band");
        assert_eq!(live_sessions(&pool, victim).await, 1);
        assert_eq!(live_keys(&pool, victim).await, 1);

        let (status, body) = render(resume_account(
            State(state.clone()),
            Path(victim),
            cookie_headers(&issue_session(&pool, operator).await),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["sessions_revoked"], json!(0), "body: {body}");
        assert_eq!(body["keys_revoked"], json!(0), "body: {body}");

        assert_eq!(account_status(&pool, victim).await, "active");
        assert_eq!(
            live_sessions(&pool, victim).await,
            1,
            "resume must NOT revoke sessions"
        );
        assert_eq!(
            live_keys(&pool, victim).await,
            1,
            "resume must NOT revoke keys"
        );
        let key_revoked: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT revoked_at FROM api_keys WHERE id = ?")
                .bind(key_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read revoked_at");
        assert!(
            key_revoked.is_none(),
            "resume must neither resurrect nor revoke: the key's revoked_at stays null"
        );

        let audit = audit_rows(&pool, victim).await;
        assert_eq!(audit.len(), 1, "resume must audit: {audit:?}");
        assert_eq!(audit[0].0, operator);
        assert_eq!(audit[0].1, "resume");
        assert_eq!(audit[0].2, "account");
        assert!(
            audit[0].3.as_ref().is_some_and(|d| !d.is_null()),
            "the audit detail must not be null: {:?}",
            audit[0].3
        );

        // A session revoked by a REAL suspend is NOT handed back by resume.
        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            cookie_headers(&issue_session(&pool, operator).await),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(live_sessions(&pool, victim).await, 0);
        let (status, _) = render(resume_account(
            State(state.clone()),
            Path(victim),
            cookie_headers(&issue_session(&pool, operator).await),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            live_sessions(&pool, victim).await,
            0,
            "resume must not resurrect a session revoked by the suspend"
        );
        let (status, _) = get_me(&app(state.clone()), &victim_token).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "the old cookie must still be dead after a resume"
        );

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }

    /// The read-only lookup answers with counts and NEVER a credential hash.

    #[tokio::test]
    async fn the_read_only_lookup_exposes_no_credential_hash() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let victim = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        open_wallet(&pool, victim, 7_500).await;
        let victim_token = issue_session(&pool, victim).await;
        let (_, _) =
            create_key_via_handler(&state, &cookie_headers(&victim_token), "live", vec![]).await;

        let (status, body) = render(get_account(
            State(state.clone()),
            Path(victim),
            operator_headers.clone(),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["account_id"], json!(victim.to_string()));
        assert_eq!(body["status"], json!("active"));
        assert_eq!(body["is_operator"], json!(false));
        assert_eq!(body["balance_idr"], json!(7_500));
        assert_eq!(body["live_sessions"], json!(1));
        assert_eq!(body["live_keys"], json!(1));
        assert!(body["created_at"].is_string(), "body: {body}");

        // Neither hash of anything real appears in the response.
        let key_hash: String =
            sqlx::query_scalar("SELECT key_hash FROM api_keys WHERE account_id = ?")
                .bind(victim.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read key_hash");
        let session_hash: String =
            sqlx::query_scalar("SELECT token_hash FROM sessions WHERE account_id = ?")
                .bind(victim.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read token_hash");
        let text = body.to_string();
        assert!(
            !text.contains(&key_hash),
            "the lookup leaked key_hash: {text}"
        );
        assert!(
            !text.contains(&session_hash),
            "the lookup leaked token_hash: {text}"
        );
        assert!(
            !text.contains(&victim_token),
            "the lookup leaked the session token"
        );

        // An absent target is an honest 404 for an operator.
        let (status, _) = render(get_account(
            State(state.clone()),
            Path(Uuid::new_v4()),
            operator_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }
    /// (h) The login path still refuses a suspended account - the behaviour
    /// auth.rs:204 already had, now asserted next to the suspension that makes it
    /// matter.
    ///
    /// Driven against the REAL PocketBase, because verify_pb_token has no seam to
    /// fake and mocking the network would test the mock. The identity is a real
    /// PocketBase record; the account row is created here with that record's id
    /// as its pb_user_id, exactly as the first login would.
    // The database half of this fixture is now a per-test SQLite file, but the
    // LOGIN half is not portable: exchange_token verifies its token against a
    // real PocketBase over the network, and faking that would test the fake. So
    // this is the one test here that still needs a live external service
    // (POCKETBASE_URL); nothing about it needs Postgres.
    /// THE LAST `#[ignore]` IN THE CRATE, now running by default.
    ///
    /// It proves the second half of Gate 3's suspension requirement: after an
    /// operator suspends an account, that account cannot log back in, the refusal
    /// is a 401 `unauthenticated`, and the refused login MINTS NOTHING (no session
    /// row) and leaves no ledger drift. `auth.rs` refuses a non-active status at
    /// exchange time; this is the test that proves the refusal holds through the
    /// real handler end to end - and until now it had never run in CI.
    ///
    /// The identity is served by a loopback stub rather than a live PocketBase,
    /// which is what makes it runnable. The code under test is unchanged.
    // -----------------------------------------------------------------------
    // The documented status contract for suspend/resume.
    //
    // docs/server/api-spec.md fixes these responses and nothing asserted them:
    // an absent target is 404 `not_found`, and a target in the wrong state is 409
    // `conflict`. They are user-visible contract, not internals - an operator
    // tooling against this API branches on them.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn suspending_an_absent_account_is_404_not_found() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let headers = cookie_headers(&issue_session(&pool, operator).await);

        let (status, body) = render(suspend_account(
            State(state.clone()),
            Path(Uuid::new_v4()),
            headers,
        ))
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], json!("not_found"));

        db.close().await;
    }

    #[tokio::test]
    async fn resuming_an_absent_account_is_404_not_found() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let headers = cookie_headers(&issue_session(&pool, operator).await);

        let (status, body) = render(resume_account(
            State(state.clone()),
            Path(Uuid::new_v4()),
            headers,
        ))
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], json!("not_found"));

        db.close().await;
    }

    /// Resuming an account that is NOT suspended is a 409, not a silent success.
    ///
    /// The distinction matters: a resume that quietly "succeeded" on an active
    /// account would write an `admin_audit` row for a state change that never
    /// happened, and the audit trail is only worth having if it records what
    /// actually occurred.
    #[tokio::test]
    async fn resuming_an_active_account_is_409_conflict_and_writes_no_audit_row() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let headers = cookie_headers(&issue_session(&pool, operator).await);

        // A fresh, ACTIVE account that was never suspended.
        let victim = test_support::account(&pool).await;

        let (status, body) =
            render(resume_account(State(state.clone()), Path(victim), headers)).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], json!("conflict"));

        // No audit row for a state change that did not happen.
        let audits: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM admin_audit WHERE target_id = ?")
                .bind(victim.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count audit rows");
        assert_eq!(
            audits, 0,
            "a refused resume must write no audit row - an audit trail that records non-events is worse than none"
        );

        db.close().await;
    }

    /// Suspending an account that is ALREADY suspended is a 409 too, and changes
    /// nothing.
    #[tokio::test]
    async fn suspending_an_already_suspended_account_is_409_conflict() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let headers = cookie_headers(&issue_session(&pool, operator).await);

        let victim = test_support::account(&pool).await;
        sqlx::query("UPDATE accounts SET status = 'suspended' WHERE id = ?")
            .bind(victim.hyphenated())
            .execute(&pool)
            .await
            .expect("park the account in the suspended state");

        let (status, body) =
            render(suspend_account(State(state.clone()), Path(victim), headers)).await;

        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"]["code"], json!("conflict"));

        db.close().await;
    }

    #[tokio::test]
    async fn a_new_login_after_suspend_is_still_refused() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        let record_id = format!("pb{}", Uuid::new_v4().simple());
        let base = pocketbase_stub("200 OK", &auth_refresh_body(&record_id)).await;
        let (_lock, _guard) = point_pocketbase_at(&base);
        // The stub answers ANY token with this record, so the token value is not
        // what is under test here - the ACCOUNT STATUS is.
        let pb_token = "a-valid-token".to_string();

        // Every NOT NULL column is bound from Rust: the SQLite schema has no
        // DEFAULT for id, created_at or updated_at.
        let victim = Uuid::new_v4();
        let victim_now = Utc::now();
        sqlx::query(
            "INSERT INTO accounts (id, pb_user_id, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(victim.hyphenated())
        .bind(&record_id)
        .bind(victim_now)
        .bind(victim_now)
        .execute(&pool)
        .await
        .expect("create the account the login would have created");

        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(victim),
            operator_headers,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = render_axum(
            exchange_token(
                State(pool.clone()),
                HeaderMap::new(),
                Json(AuthExchangeRequest {
                    pb_token: pb_token.clone(),
                }),
            )
            .await,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a suspended account must not be able to log in: {body}"
        );
        assert_eq!(body["error"]["code"], json!("unauthenticated"));

        // And it minted nothing on the way out.
        let sessions: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sessions WHERE account_id = ?")
                .bind(victim.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count sessions");
        assert_eq!(
            sessions, 0,
            "a refused login must not leave a session row behind"
        );

        assert_no_drift(&pool, &[operator, victim]).await;
        db.close().await;
    }

    // The live-PocketBase fixtures (pb_collection_url, pocketbase_identity,
    // delete_pocketbase_identity and their base-url helper) stood here. They are
    // DELETED with the test that needed them: the loopback stub above replaces the
    // approach, and a fixture that creates REAL PocketBase records would now imply
    // this suite still needs an external service.

    // -----------------------------------------------------------------------
    // GET /api/admin/accounts - the listing.
    //
    // Why these matter: the listing is the ONE admin route that returns many
    // accounts at once, so it is the only place a bad WHERE clause could leak the
    // whole account space to a non-operator, and the only place a filter bug
    // could return a page that looks complete but is not. Each test attacks one
    // of those two.
    // -----------------------------------------------------------------------

    /// A non-operator must not be able to list. The 403 is the same answer an
    /// unknown id gets, so the listing cannot be used to probe the account space.
    #[tokio::test]
    async fn listing_requires_an_operator() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let outsider = create_account(&pool).await;

        // No cookie at all -> 401, before anything is read.
        let (status, _) = render(list_accounts(
            State(state.clone()),
            HeaderMap::new(),
            Query(AdminListQuery { q: None, status: None, limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A real, live session on a NON-operator account -> 403.
        let outsider_headers = cookie_headers(&issue_session(&pool, outsider).await);
        let (status, body) = render(list_accounts(
            State(state.clone()),
            outsider_headers,
            Query(AdminListQuery { q: None, status: None, limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
        assert_eq!(body["error"]["code"], json!("forbidden"));

        // And the operator's own listing works, proving the same call succeeds
        // for the right caller - the 403 above is authorization, not a broken route.
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);
        let (status, _) = render(list_accounts(
            State(state.clone()),
            operator_headers,
            Query(AdminListQuery { q: None, status: None, limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        assert_no_drift(&pool, &[operator, outsider]).await;
        db.close().await;
    }

    /// The listing carries the same safe fields as the single view and NOTHING
    /// that could read a credential.
    #[tokio::test]
    async fn listing_rows_never_expose_a_credential() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let customer = create_account(&pool).await;
        open_wallet(&pool, customer, 7_000).await;

        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);
        let (status, body) = render(list_accounts(
            State(state.clone()),
            operator_headers,
            Query(AdminListQuery { q: Some(customer.hyphenated().to_string()), status: None, limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");

        let accounts = body["accounts"].as_array().expect("accounts is an array");
        assert_eq!(accounts.len(), 1, "the id filter must select exactly one row");
        let row = &accounts[0];

        // The exact field set. A new field here would have to be justified
        // against docs/admin-surface.md:158-166.
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "account_id",
                "balance_idr",
                "created_at",
                "is_operator",
                "live_keys",
                "live_sessions",
                "status",
            ],
        );
        assert_eq!(row["account_id"], json!(customer));
        assert_eq!(row["balance_idr"], json!(7_000));

        // Belt and braces: the rendered body contains none of the secret column
        // names, so a future SELECT * could not sneak one through unnoticed.
        let text = body.to_string();
        for forbidden in ["token_hash", "key_hash", "pb_user_id", "snap_token"] {
            assert!(!text.contains(forbidden), "the listing leaked {forbidden}: {text}");
        }

        assert_no_drift(&pool, &[operator, customer]).await;
        db.close().await;
    }

    /// The filter narrows to matching accounts and excludes the rest, and the
    /// status filter composes with it.
    #[tokio::test]
    async fn listing_filters_by_text_and_status() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        // Two customers; one will be suspended, so the status filter has a
        // difference to find. A filter that returned everything would still
        // satisfy a count of "at least one", which is why the count is exact.
        let active = create_account(&pool).await;
        let suspended = create_account(&pool).await;

        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        // Suspend one through the real handler so its status is genuine.
        let (status, _) = render(suspend_account(
            State(state.clone()),
            Path(suspended),
            operator_headers.clone(),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);

        // No filter: every account, including the operator, appears.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: None, limit: Some(100), offset: None }),
        ))
        .await;
        let all = body["accounts"].as_array().unwrap();
        assert_eq!(all.len(), 3, "operator + two customers: {body}");

        // status=suspended selects exactly the suspended one.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: Some("suspended".into()), limit: None, offset: None }),
        ))
        .await;
        let only = body["accounts"].as_array().unwrap();
        assert_eq!(only.len(), 1, "only one account is suspended: {body}");
        assert_eq!(only[0]["account_id"], json!(suspended));

        // A text filter on the active account's id selects just it - proving the
        // LIKE clause narrows rather than matching all.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: Some(active.hyphenated().to_string()), status: None, limit: None, offset: None }),
        ))
        .await;
        let matched = body["accounts"].as_array().unwrap();
        assert_eq!(matched.len(), 1, "the id filter selects one account: {body}");
        assert_eq!(matched[0]["account_id"], json!(active));

        // A status value outside the vocabulary matches nothing, and is not an
        // error: an empty page is a valid answer.
        let (status, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: Some("nonsense".into()), limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["accounts"].as_array().unwrap().len(), 0);

        assert_no_drift(&pool, &[operator, active, suspended]).await;
        db.close().await;
    }

    /// A literal '%' in the search box must not become a wildcard. Without the
    /// ESCAPE clause it would match every account, turning a narrow search into a
    /// full dump.
    #[tokio::test]
    async fn listing_treats_a_percent_sign_literally() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        let customer = create_account(&pool).await;
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        let (status, body) = render(list_accounts(
            State(state.clone()),
            operator_headers,
            Query(AdminListQuery { q: Some("%".into()), status: None, limit: None, offset: None }),
        ))
        .await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["accounts"].as_array().unwrap().len(),
            0,
            "a literal '%' must match nothing here, not every account: {body}"
        );

        assert_no_drift(&pool, &[operator, customer]).await;
        db.close().await;
    }

    /// The page size is bounded and echoed, so a caller can page deterministically.
    #[tokio::test]
    async fn listing_bounds_and_echoes_the_page() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        for _ in 0..3 {
            create_account(&pool).await;
        }
        let operator_headers = cookie_headers(&issue_session(&pool, operator).await);

        // limit=0 is raised to the minimum (1), never left at 0 (which SQLite
        // reads as "no limit").
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: None, limit: Some(0), offset: None }),
        ))
        .await;
        assert_eq!(body["limit"], json!(1), "limit is clamped up: {body}");
        assert_eq!(body["accounts"].as_array().unwrap().len(), 1);

        // A request for more than the cap is clamped to the cap.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: None, limit: Some(10_000), offset: None }),
        ))
        .await;
        assert_eq!(body["limit"], json!(100), "limit is clamped down: {body}");

        // offset is echoed and skips rows.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers.clone(),
            Query(AdminListQuery { q: None, status: None, limit: Some(100), offset: Some(1) }),
        ))
        .await;
        assert_eq!(body["offset"], json!(1));
        assert_eq!(body["accounts"].as_array().unwrap().len(), 3);

        // A negative offset is treated as zero, not passed to SQLite as-is.
        let (_, body) = render(list_accounts(
            State(state.clone()),
            operator_headers,
            Query(AdminListQuery { q: None, status: None, limit: Some(100), offset: Some(-5) }),
        ))
        .await;
        assert_eq!(body["offset"], json!(0));

        assert_no_drift(&pool, &[operator]).await;
        db.close().await;
    }

    /// The router mounts the listing at `GET /api/admin/accounts`, and it is
    /// reached through the production stack - not only by calling the handler.
    #[tokio::test]
    async fn listing_route_is_mounted_and_requires_a_cookie() {
        let db = TestDb::new().await;
        let pool = db.pool.clone();
        let state = test_state(pool.clone());
        let operator = create_operator(&pool).await;
        create_account(&pool).await;
        let app = app(state.clone());

        // No cookie -> 401 through the router.
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin/accounts")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // With the operator cookie -> 200 and a JSON body with the documented keys.
        let token = issue_session(&pool, operator).await;
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/admin/accounts?limit=5")
                    .header(header::COOKIE, format!("session={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = json_body(res).await;
        assert!(body["accounts"].is_array(), "body: {body}");
        assert_eq!(body["limit"], json!(5));

        assert_no_drift(&pool, &[operator]).await;
        db.close().await;
    }

    #[test]
    fn a_sqlx_error_maps_to_the_app_database_variant() {
        // The `?` operator on every admin query funnels sqlx failures through
        // this conversion; if it ever wrapped the wrong variant, operators
        // would see a misleading error code for a database outage.
        assert!(matches!(
            AdminError::from(sqlx::Error::RowNotFound),
            AdminError::App(AppError::Database(_))
        ));
    }
}
