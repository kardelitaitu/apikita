//
// FENCED, and the allow is at the TOP of the module rather than on each site,
// because there is one production site here and it is a constant-bounded date
// subtraction already argued in full above it. A per-site allow for a single site
// would be the same thing with more ceremony.
#![cfg_attr(
    not(test),
    // THE HIGHEST-RISK ENDPOINT IN THE PRODUCT, fenced for that reason rather than
    // for the arithmetic.
    //
    // docs/architecture/identity.md calls this "the highest-risk endpoint in the
    // Telegram surface", and the reason is money: a 6-digit code is 10^6
    // possibilities, and a successful guess attaches an attacker's Telegram to a
    // FUNDED WALLET. Two of this module's three caps are what stand between the
    // public internet and somebody else's balance - the per-IP redemption cap on
    // guessing, and the per-account issuance cap on how many codes exist to guess.
    //
    // The third cap is the reason this file is worth fencing at all. The issuance
    // cap counted link_codes, a table this very handler DELETEs from, so it could
    // not fire at all; a cap that cannot fire is indistinguishable from a cap that is
    // not being hit, and the suite was green throughout. It now counts
    // link_code_issues. A module whose job is enforcing limits should not be the
    // one place where a limit is quietly inert.
    deny(clippy::arithmetic_side_effects)
)]

//! Telegram link-code redemption: the binding of a chat to a funded account.
//!
//! `docs/architecture/identity.md` names this the **highest-risk endpoint in the
//! Telegram surface**, and the reason is money rather than privacy: "a 6-digit
//! code is brute-forceable, and a successful guess attaches an attacker's
//! Telegram to a **funded wallet**". A 6-digit code is 10^6 possibilities, which
//! is nothing to a script, so the security of this endpoint does not rest on the
//! code being secret — it rests on **how few guesses are permitted** and on
//! **what a guesser can learn from a refusal**.
//!
//! Two independent caps, because they defend against two different attacks:
//!
//! - **per ACCOUNT** (`limits.link_code_issuance_per_hour`) — bounds how many codes
//!   one account can have in flight, reusing `abuse::enforce_creation_cap` over
//!   `link_codes (account_id, created_at)`. This is the same guard `topups` and
//!   `api_keys` use, not a second copy of the boundary rule.
//! - **per IP** (`limits.link_redemption_per_hour`) — bounds guessing from one
//!   host. A single attacker must not be able to walk the code space by cycling
//!   accounts, which the per-account cap would not notice at all.
//!
//! **A FAILED GUESS COUNTS AGAINST BOTH.** This is the entire point: the attack
//! IS the stream of failures, so a counter that only advanced on success would
//! never fire. The attempt is recorded before the code is even looked up, and the
//! tests below refuse a *correct* code once the caps are exhausted, which is the
//! only way to prove the counter is on the failure path.
//!
//! **Every refusal is the same refusal.** Wrong code, expired code, already-used
//! code and a code that never existed all return one identical body. Telling them
//! apart would turn the endpoint into an oracle: an attacker who learns "this code
//! exists but has expired" has reduced a blind 10^6 search to a walk over the
//! handful of codes that are live at any moment.
//!
//! Nothing here is reachable from `/v1/*`, and no path in this module can move
//! money: the only write is the binding row.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
};
use chrono::{DateTime, Duration, Utc};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Row, SqlitePool};
use uuid::fmt::Hyphenated;
use uuid::Uuid;

use crate::abuse;
use crate::error::AppError;
use crate::ip_tracking::{ip_hash, resolve_client_ip};
use crate::routes::proxy::AppState;

/// How long an issued code stays redeemable.
///
/// `docs/server/api-spec.md`: "Issues a 6-digit code: single-use, 5-minute TTL".
/// Five minutes is short on purpose — it is long enough for a human to copy the
/// code into a chat, and short enough that the window an attacker has to guess a
/// given code is small. The TTL is NOT configurable: it is a security parameter
/// stated in the contract, and a config knob would let a deployment widen it by
/// accident.
pub const LINK_CODE_TTL_MINUTES: i64 = 5;

/// How long a link code outlives its own usefulness: a code the customer already
/// redeemed, or one that hit `LINK_CODE_TTL_MINUTES` unused, is kept one more day
/// and then deleted.
///
/// The number is a TRANSCRIPTION, not a discovery. `website/src/lib/privacy.ts` and
/// `docs/data-retention.md:77` have both stated "Until used or expired + 24h" since
/// before any code did it, and this constant exists so the two can be compared
/// rather than trusted - the same reasoning as `ip_tracking`'s
/// `LINK_CODE_ISSUE_RETENTION_DAYS`, which was named for the same purpose.
///
/// WHY +24h AND NOT "immediately", which would be simpler to implement and is what
/// `identity_tokens` does: a link code is read by a HUMAN who may be holding it open
/// in a chat window, and the page says a day. Deleting at expiry would also be a
/// defensible policy, but it would make the published sentence false, and the
/// published sentence is the thing this repository treats as the contract - the
/// whole point of the retention sweep is that the document is what the code
/// executes.
///
/// The grace applies to BOTH terminal states, and `used_at` is what tells them
/// apart: an UNUSED code is stale once `expires_at` passed, a USED one once
/// `used_at` did. Comparing only `expires_at` would retain a redeemed code for the
/// whole grace period even though nothing can redeem it twice - which is precisely
/// the window `issue_link_code`'s "issuing a code deletes this account's previous
/// codes" already refuses to honour.
pub const LINK_CODE_RETENTION_GRACE_DAYS: i64 = 1;

/// How many digits a link code carries.
const CODE_DIGITS: u32 = 6;

/// The uniform refusal for every unsuccessful redemption.
///
/// One constant, returned for wrong/expired/used/unknown codes alike, so the
/// endpoint cannot be used to tell a non-existent code from a dead one. The body
/// is deliberately the same shape as the success path's failure case would be if
/// it had one: `{"status":"invalid_code"}`, carrying nothing about which check
/// failed, how many guesses remain, or whether the code ever existed.
pub const INVALID_CODE_STATUS: &str = "invalid_code";

/// The body of a refused redemption. See `INVALID_CODE_STATUS`.
#[derive(Debug, Serialize)]
pub struct InvalidCodeResponse {
    pub status: &'static str,
}

/// What a successful redemption returns.
#[derive(Debug, Serialize)]
pub struct LinkRedemptionResponse {
    pub status: &'static str,
    pub account_id: Uuid,
}

/// The redemption endpoint's WHOLE response surface, in one type.
///
/// An enum rather than two `impl IntoResponse` arms because the handler has two
/// successful-but-different outcomes (linked / invalid) and Rust will not unify
/// two distinct `Json<T>` types in one `ReturnType`. Modelling it this way also
/// makes the no-oracle property structural: there is exactly ONE variant for
/// every unsuccessful redemption, so a future edit cannot add a second failure
/// shape without this enum changing — and the test that pins the refusal bodies as
/// byte-identical would then fail immediately.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum RedeemResponse {
    Linked(LinkRedemptionResponse),
    Invalid(InvalidCodeResponse),
}

impl IntoResponse for RedeemResponse {
    fn into_response(self) -> axum::response::Response {
        // 200 for both. A refusal is not an HTTP error: the request was
        // well-formed and the bot authenticated, the code simply did not redeem.
        // Returning 4xx would also invite the bot's transport to retry a guess,
        // which is the opposite of what the cap is for.
        (StatusCode::OK, Json(self)).into_response()
    }
}

/// Body of `POST /api/bot/link`: what the bot sends when a user types `/link 123456`.
#[derive(Debug, Deserialize)]
pub struct RedeemLinkCodeRequest {
    pub code: String,
    pub telegram_id: String,
}

/// A 6-digit code, drawn from a CSPRNG.
///
/// `rand_core::RngCore` is already a dependency of this crate (`keys.rs` uses it
/// for key material), so this reuses the same source rather than adding one.
/// `next_u32() % 1_000_000` is a modulo bias of ~2^-12 over a 2^32 space, which is
/// far below the resolution of the 10^6 search space an attacker can actually
/// walk — but the code is zero-padded so `000042` is a valid, well-formed code
/// rather than a 2-digit one, which matters because the redemption side compares
/// strings of fixed width.
fn generate_code<R: RngCore>(rng: &mut R) -> String {
    format!(
        "{:0width$}",
        rng.next_u32() % 1_000_000,
        width = CODE_DIGITS as usize
    )
}

/// Whether a submitted string is shaped like a code at all.
///
/// Checked BEFORE the database is touched, so a malformed code costs no query and
/// cannot be used to probe the table. The refusal is the same as any other
/// (`invalid_code`), because "malformed" and "unknown" must stay
/// indistinguishable to the caller.
fn is_well_formed(code: &str) -> bool {
    code.len() == CODE_DIGITS as usize && code.bytes().all(|b| b.is_ascii_digit())
}

/// The per-IP redemption cap's counter key.
///
/// Hashed, never raw: Gate 4 of `docs/launch-checklist.md` and
/// `docs/data-retention.md` promise that no raw IP address is persisted, only a
/// salted hash. `ip_hash` is the same HMAC used for API-key IP tracking, so the
/// two cannot drift into different notions of "the same client".
fn client_ip_key(salt: &[u8], ip: std::net::IpAddr) -> String {
    ip_hash(salt, &ip)
}

/// The window both caps are stated over.
///
/// One hour for both, matching the `_per_hour` names of every other cap in
/// `[limits]`. A rolling window, not a calendar hour, for the same reason
/// `abuse::key_creation_window` is a rolling day: a calendar boundary makes the
/// cap depend on which timezone the reader assumes, and creates a cliff at the
/// boundary where the allowance resets.
pub fn link_code_window() -> Duration {
    Duration::hours(1)
}

/// Records one redemption ATTEMPT from `ip_key` and reports whether it is allowed.
///
/// **Called BEFORE the code is looked up, and unconditionally.** The attempt is
/// what is counted, so a wrong guess consumes budget exactly as a right one does;
/// counting only success would make this cap unfireable, because the attack is
/// the failure stream. This is the single most important property in the module.
///
/// The counter is a table row rather than in-memory state, for the reason
/// `abuse.rs` gives: an in-process counter resets on restart, is not shared across
/// instances, and cannot be audited. `link_redemption_attempts` carries
/// `(ip_hash, attempted_at)`, and the raw IP is never stored - only the salted
/// HMAC (`docs/data-retention.md`).
///
/// The boundary arithmetic is `abuse::cap_outcome`, the same pure function the
/// per-account caps use, so "exactly at the cap is refused", "floor the
/// Retry-After at 1" and "0 disables" cannot drift between the two.
///
/// **WHAT THIS ACTUALLY LIMITS IS N PER UTC DAY, NOT N PER WINDOW — and that is a
/// consequence of the salt, not a bug in the window.** The counter is keyed on
/// `ip_hash`, which is an HMAC under a salt that `ip_tracking` replaces at the UTC day
/// boundary. At midnight the same caller hashes to a different value, so the counter
/// starts empty and every attempt before midnight becomes invisible. A sliding
/// `link_code_window` is therefore really a daily allowance that resets on the hour,
/// and an attacker who times it gets the full budget at 23:59 and again at 00:00.
///
/// **THE EXPOSURE IS BOUNDED, and it is small: one extra budget per DAY.** The window
/// here is an hour and the salt rotates every twenty-four, so a salt boundary can fall
/// inside a window at most once a day. The worst case is one additional hour of
/// attempts, once, at midnight.
///
/// I first wrote this as a limit that quietly stops limiting, which reads as though the
/// cap were unbounded. It is not, and the difference is the whole point: an operator
/// needs the worst case, not the mood. A rule that sounds alarming is a rule people
/// disable, and this one is a price worth paying - one budget a day for history that is
/// unlinkable across days, which is the property the privacy page promises.
///
/// It is written down rather than fixed because the two properties are in genuine
/// conflict: a salt that does not rotate makes the counter work across days, and it also
/// makes every day linkable, which is the whole privacy design in ip-tracking.md and the
/// reason this is documented on a public page. Restoring the cross-day count would mean
/// keeping a stable per-caller identifier, which is precisely what that document says is
/// not kept.
///
/// So the honest statement of the control is: it stops an unbounded attempt stream from
/// one caller within a day. It does not stop a caller who is willing to spend two
/// budgets across midnight, and anyone reading `review_per_hour`-style numbers here
/// should know that is the shape of what they have.
async fn record_and_check_attempt(
    pool: &SqlitePool,
    ip_key: &str,
    limit: u32,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    if limit == 0 {
        // Disabled. Recorded anyway so the audit signal still exists, but never
        // refused - the convention every other ceiling follows.
        record_attempt(pool, ip_key, now).await?;
        return Ok(());
    }

    let window: i64 = link_code_window().num_hours();
    // SAFE, and the same argument as every other date subtraction in this
    // repository: chrono's DateTime arithmetic PANICS on an out-of-range result
    // rather than wrapping, and the operand is `link_code_window()` - an hour, a
    // constant in abuse.rs - rather than anything a caller supplies. The subtraction
    // is on the ATTACK path, so a panic here would be a denial of service by
    // anyone who can make this run; the bound is what keeps that unreachable.
    #[allow(clippy::arithmetic_side_effects)]
    let window_start = now - Duration::hours(window);

    let row = sqlx::query(
        "SELECT COUNT(*) AS used, MIN(attempted_at) AS oldest \
         FROM link_redemption_attempts WHERE ip_hash = ? AND attempted_at >= ?",
    )
    .bind(ip_key)
    .bind(window_start)
    .fetch_one(pool)
    .await?;

    let used: i64 = row.get("used");
    let oldest: Option<DateTime<Utc>> = row.get("oldest");

    // The two-argument shape of `cap_outcome`: it counts what is ALREADY on the
    // books, so the attempt in hand is the one that would exceed a full budget.
    if let Some(retry_after_secs) = abuse::cap_outcome(used, limit, oldest, link_code_window(), now)
    {
        // A REFUSED attempt is NOT recorded. Recording it would let an attacker
        // who is already being throttled extend their own lockout indefinitely
        // and, worse, grow an unbounded table from a single host.
        return Err(AppError::RateLimited { retry_after_secs });
    }

    record_attempt(pool, ip_key, now).await?;
    Ok(())
}

/// Appends one attempt row. Split out so the refusal path cannot accidentally
/// call it - see `record_and_check_attempt`.
async fn record_attempt(
    pool: &SqlitePool,
    ip_key: &str,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO link_redemption_attempts (ip_hash, attempted_at) VALUES (?, ?)")
        .bind(ip_key)
        .bind(now)
        .execute(pool)
        .await?;
    Ok(())
}

/// `POST /api/telegram/link-code` — issue a code for the signed-in account.
///
/// Invalidates any previous code for the account by REPLACING it: the old row is
/// deleted rather than left to expire, so at most one code per account is ever
/// live. Two live codes would double an attacker's chance per guess and make
/// "invalidated by a newer code" (`api-spec.md`) only nominally true.
///
/// The plaintext code is returned exactly once, like an API key: it is the only
/// time it exists outside the chat the customer is about to paste it into.
pub async fn issue_link_code(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = crate::routes::resolve_account_from_cookie(&state.pool, &headers).await?;

    let now = Utc::now();
    // SAFE: LINK_CODE_TTL_MINUTES is a compile-time constant, so the operand cannot be
    // anything but a few minutes and the addition cannot leave the representable
    // range. Worth saying because chrono's DateTime arithmetic PANICS rather than
    // wrapping, and this is the code that mints a link code - a panic here is a
    // refusal of a legitimate customer, not a wrong number.
    #[allow(clippy::arithmetic_side_effects)]
    let expires_at = now + Duration::minutes(LINK_CODE_TTL_MINUTES);

    let mut tx = crate::db::begin_immediate(&state.pool).await?;

    // Abuse guard: how many codes this ACCOUNT has issued this hour. Note this is
    // the account cap, not the guessing cap - it bounds issuance VOLUME, and
    // the per-IP redemption cap is what stops the brute force.
    //
    // COUNTS link_code_issues, NOT link_codes, and that is the fix rather than a
    // detail. It used to count link_codes - which 20260926000000 records as
    // deliberate - and that was true when written and stopped being true when the
    // DELETE below was added for the one-live-code guarantee. The DELETE makes the
    // row count always 0 or 1, so a cap of 10 could never be approached: the guard
    // parsed, ran, and did nothing. Issuances are recorded in a table that survives
    // the delete, so the count means what the cap says it means.
    //
    // INSIDE THE TRANSACTION, which is NECESSARY BUT NOT SUFFICIENT here - and the
    // second half of that sentence is the more important half.
    //
    // Necessary: this was a COUNT on the pool with the INSERT it governs inside a
    // separate transaction, so concurrent callers would each read the same count.
    // The transaction was already open for the DELETE below, so the COUNT joins it
    // for free.
    //
    // NOT SUFFICIENT, and writing it up as a fix would be a false claim. This cap
    // CANNOT FIRE AT ALL. It counts rows in link_codes for the account - and the
    // DELETE immediately below removes every one of them before the INSERT, so the
    // count is always 0 or 1. Against a cap of 10 the comparison is true forever.
    // The guard is documented in docs/server/api-spec.md and docs/decisions.md as
    // bounding how many codes an account may issue in an hour, and it bounds
    // nothing. MEASURED by the test below: eighteen concurrent issues against a cap
    // of ten produce eighteen successes and zero refusals.
    //
    // Nothing here makes the cap fire, and pretending otherwise is the easy
    // mistake. Counting ISSUES rather than LIVE ROWS needs a record that survives
    // the delete - a counter table, or superseding old rows instead of removing
    // them - and superseding would break the one-live-code-per-account guarantee
    // that doubles an attacker's chance per guess. So the fix is a schema and
    // behaviour change, and this transaction move is the part that can be done now:
    // correct, and necessary once the counting is.
    abuse::enforce_creation_cap_in(
        &mut tx,
        "link_code_issues",
        link_code_window(),
        state.config.limits.link_code_issuance_per_hour,
        account_id,
        now,
    )
    .await?;

    // The ISSUANCE RECORD, in this same transaction and stamped with the same
    // `now` as the cap check above.
    //
    // It has to be here and not after the commit: a record written outside the
    // transaction would be a second failure mode, in which the code exists but the
    // issuance was never counted and the next caller is admitted too - the exact
    // defect this table exists to remove, reintroduced one line down. Together with
    // the cap check, the count and the record commit as one fact.
    sqlx::query("INSERT INTO link_code_issues (account_id, created_at) VALUES (?, ?)")
        .bind(account_id.hyphenated())
        .bind(now)
        .execute(&mut *tx)
        .await?;

    // One live code per account. Deleting the predecessor inside the same
    // transaction as the insert means a crash cannot leave two live codes.
    sqlx::query("DELETE FROM link_codes WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .execute(&mut *tx)
        .await?;

    let code = generate_code(&mut rand_core::OsRng);
    sqlx::query(
        "INSERT INTO link_codes (code, account_id, expires_at, created_at) VALUES (?, ?, ?, ?)",
    )
    .bind(&code)
    .bind(account_id.hyphenated())
    .bind(expires_at)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok((
        StatusCode::OK,
        Json(json!({
            "code": code,
            "expires_at": expires_at,
            "ttl_minutes": LINK_CODE_TTL_MINUTES,
        })),
    ))
}

/// `POST /api/bot/link` — the bot redeems a code typed in chat.
///
/// Authenticated by the BOT token, not a cookie: the caller is the bot process,
/// and the account is whichever one the code belongs to. See `api-spec.md`
/// ("The bot calls a separate internal endpoint to redeem codes").
///
/// **Order is load-bearing and must not be rearranged:**
///
/// 1. **reject a malformed code** before any query, so it costs nothing and cannot
///    probe the table;
/// 2. **count the attempt against the caller's IP** before looking anything up -
///    the attempt is what is capped, and a guess that misses must still be counted
///    or the cap only fires on success, i.e. never;
/// 3. **claim the code with ONE conditional UPDATE**, which is what makes it
///    single-use under concurrency: two simultaneous redemptions cannot both take
///    the row, because `used_at IS NULL` is part of the UPDATE's own predicate;
/// 4. **bind the telegram id**, replacing any previous binding for that chat.
///
/// Every failure from step 3 onward returns the SAME body (see
/// `INVALID_CODE_STATUS`): the caller must not be able to distinguish "wrong" from
/// "expired" from "already used" from "never existed".
pub async fn redeem_link_code(
    State(state): State<AppState>,
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(payload): Json<RedeemLinkCodeRequest>,
) -> Result<impl IntoResponse, AppError> {
    // The caller must be the bot before anything else is read: a cookie or an
    // absent header is not a bot credential (docs/server/api-spec.md - bot token
    // endpoints). Checked before the code's shape so an unauthenticated caller
    // learns nothing at all, not even whether their input was well formed.
    require_bot_token(&headers)?;

    let now = Utc::now();

    // 1. Shape check first: no query for input that cannot be a code.
    if !is_well_formed(payload.code.trim()) {
        return Ok(invalid_code_response());
    }

    // 2. Count the attempt. The IP comes from the same resolver the proxy uses, so
    //    a forged X-Forwarded-For is ignored unless the peer is a trusted relay.
    let client_ip = resolve_client_ip(peer.ip(), &headers, &state.trusted_proxies);
    let salt = state.ip_salt.salt_for_day(crate::ip_tracking::today_utc());
    let ip_key = client_ip_key(&salt, client_ip);

    record_and_check_attempt(
        &state.pool,
        &ip_key,
        state.config.limits.link_redemption_per_hour,
        now,
    )
    .await?;

    // 3. Claim the code. The predicate IS the guard: expired, used and unknown all
    //    fail to produce a row, and `RETURNING account_id` gives the binder its
    //    target in the same statement that consumed the code.
    let mut tx = crate::db::begin_immediate(&state.pool).await?;

    let claimed = sqlx::query(
        "UPDATE link_codes SET used_at = ? \
         WHERE code = ? AND used_at IS NULL AND expires_at > ? \
         RETURNING account_id",
    )
    .bind(now)
    .bind(payload.code.trim())
    .bind(now)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = claimed else {
        // Nothing to roll back - the UPDATE matched no row - but the transaction
        // is closed explicitly rather than dropped, so the write lock is released
        // before the response is built.
        tx.rollback().await?;
        return Ok(invalid_code_response());
    };

    let account_id: Uuid = row.get::<Hyphenated, _>("account_id").into_uuid();

    // 4. Bind the chat. `telegram_links` is keyed by telegram_id, so re-linking the
    //    same chat REPLACES its account rather than violating the primary key. That
    //    is the intended semantics: a chat belongs to whoever most recently proved
    //    control of an account, and the alternative (refusing when already linked)
    //    would strand a user whose account changed.
    sqlx::query(
        "INSERT INTO telegram_links (telegram_id, account_id, linked_at) \
         VALUES (?, ?, ?) \
         ON CONFLICT (telegram_id) DO UPDATE \
             SET account_id = excluded.account_id, linked_at = excluded.linked_at",
    )
    .bind(&payload.telegram_id)
    .bind(account_id.hyphenated())
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(RedeemResponse::Linked(LinkRedemptionResponse {
        status: "linked",
        account_id,
    }))
}

/// Removes link codes that are past their usefulness plus the published grace
/// period, for the retention sweep.
///
/// `now` is an argument rather than read from the clock so the window can be
/// tested at its boundaries - the same shape as `identity::tokens::purge_expired`,
/// and the SQL is written once because `db::purge_expired_usage` calls THIS rather
/// than restating the predicate.
///
/// THE `COALESCE` IS THE WHOLE RULE. A code reaches a terminal state two ways, and
/// which way it went decides when its day starts:
/// - redeemed -> `used_at` is set, and the day runs from there;
/// - never redeemed -> `used_at` is NULL and the day runs from `expires_at`.
///
/// So the governing instant is `COALESCE(used_at, expires_at)` plus the grace, and
/// it is compared against `now` rather than a midnight cutoff. A midnight cutoff
/// would be wrong here for the same reason it would be wrong for `identity_tokens`:
/// the boundary is the code's OWN lifetime, not a nightly policy boundary, so a code
/// that became stale at 23:50 would get a grace a day short or a day long depending
/// on what hour the container happens to restart at.
///
/// NOTHING ELSE DELETES FROM THIS TABLE except the one code being superseded
/// (`DELETE FROM link_codes WHERE account_id = ?`, which reaches only that account's
/// current attempt). An unused code a customer requested and never redeemed had no
/// delete path at all: every row ever issued stayed on disk, and expired codes
/// accumulated behind a page that said twenty-four hours. That is the third time a
/// published retention window has had no code behind it, which is why the
/// maintenance entrypoint's table list is now compared to Rust's in both directions.
pub async fn purge_terminal(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64, AppError> {
    // SAFE: `LINK_CODE_RETENTION_GRACE_DAYS` is a compile-time constant (1), and
    // chrono panics rather than wraps on an out-of-range date - see the note on
    // `db::oldest_row_past_window` for the full argument, which is the same shape.
    #[allow(clippy::arithmetic_side_effects)]
    let cutoff = now - Duration::days(LINK_CODE_RETENTION_GRACE_DAYS);

    let result = sqlx::query("DELETE FROM link_codes WHERE COALESCE(used_at, expires_at) <= ?")
        .bind(cutoff)
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

/// `DELETE /api/telegram` — unlink the signed-in account's chat.
///
/// Deletes rows from `telegram_links` ONLY. `api-spec.md`: "**Removes one row —
/// never the account or the wallet.**" The wallet and its balance are untouched,
/// and the FK is `ON DELETE CASCADE` from accounts, so nothing here can reach
/// either.
pub async fn unlink_telegram(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let account_id = crate::routes::resolve_account_from_cookie(&state.pool, &headers).await?;

    let removed = sqlx::query("DELETE FROM telegram_links WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .execute(&state.pool)
        .await?;

    Ok((
        StatusCode::OK,
        Json(json!({ "status": "unlinked", "removed": removed.rows_affected() })),
    ))
}

/// The bot shared secret, from the environment, read once per call.
///
/// `TELEGRAM_BOT_TOKEN` is the name `.env.example` already declares. It is read at
/// call time rather than cached in a `OnceLock` so a test can set it per case; the
/// cost is one environment read on an endpoint that is not on any hot path.
///
/// **A missing token REFUSES rather than allows.** An endpoint that guards a
/// wallet binding must fail closed: with no configured secret there is no way to
/// tell the bot from an attacker, and "no token configured" silently becoming "any
/// caller is the bot" would hand out exactly the binding this module exists to
/// protect.
fn bot_token() -> Result<String, AppError> {
    std::env::var("TELEGRAM_BOT_TOKEN")
        .ok()
        .filter(|token| !token.trim().is_empty())
        .ok_or(AppError::Unauthenticated)
}

/// Checks the `Authorization: Bearer <token>` header against the bot secret.
///
/// Compared in CONSTANT TIME via `subtle`, the same primitive the Midtrans
/// signature uses: a byte-by-byte early-exit comparison leaks the secret's prefix
/// through timing, and this secret authorises a wallet binding.
fn require_bot_token(headers: &HeaderMap) -> Result<(), AppError> {
    let expected = bot_token()?;

    let presented = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or(AppError::Unauthenticated)?;

    use subtle::ConstantTimeEq;
    if presented.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err(AppError::Unauthenticated)
    }
}

/// The one refusal every unsuccessful redemption returns.
fn invalid_code_response() -> RedeemResponse {
    RedeemResponse::Invalid(InvalidCodeResponse {
        status: INVALID_CODE_STATUS,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};

    /// A deterministic RNG so a test can assert on the SHAPE of a generated code
    /// without depending on the CSPRNG's output. Not used for real codes.
    struct StubRng(u32);

    impl RngCore for StubRng {
        fn next_u32(&mut self) -> u32 {
            self.0
        }
        fn next_u64(&mut self) -> u64 {
            u64::from(self.0)
        }
        fn fill_bytes(&mut self, dest: &mut [u8]) {
            for b in dest.iter_mut() {
                *b = 0;
            }
        }
        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    #[test]
    fn a_generated_code_is_always_six_ascii_digits() {
        // The boundary values matter: 0 must render as "000000" (not "0"), and
        // 999_999 as itself. A short code would be rejected by the redemption
        // side's own shape check, i.e. the issuer would mint codes it refuses.
        for raw in [0u32, 1, 42, 999_998, 999_999, u32::MAX] {
            let code = generate_code(&mut StubRng(raw));
            assert_eq!(code.len(), CODE_DIGITS as usize, "raw {raw} -> {code}");
            assert!(
                code.bytes().all(|b| b.is_ascii_digit()),
                "raw {raw} -> {code} must be all digits"
            );
            assert!(
                is_well_formed(&code),
                "a code the issuer mints must pass the redemption side's own shape check: {code}"
            );
        }
    }

    /// `record_and_check_attempt` with limit 0 is the DISABLED ceiling: it records
    /// the attempt (the audit signal) but never refuses, whatever the prior
    /// volume. This is the `limit == 0` arm at telegram.rs:192-196, which the live
    /// handler tests never reach because the deployed config sets a positive cap.
    #[tokio::test]
    async fn a_disabled_link_attempt_ceiling_records_but_never_refuses() {
        let db = TestDb::new().await;
        let now = chrono::Utc::now();
        let result = record_and_check_attempt(&db.pool, "ip-test-disabled", 0, now).await;
        assert!(result.is_ok(), "limit 0 must never refuse: {result:?}");
        // The attempt was recorded despite the refusal being disabled.
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM link_redemption_attempts WHERE ip_hash = ?")
                .bind("ip-test-disabled")
                .fetch_one(&db.pool)
                .await
                .expect("count attempts");
        assert_eq!(count, 1, "the attempt must be recorded even when disabled");
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // Live tests: the properties that make this endpoint safe.
    //
    // Each drives the REAL handler against a REAL migrated SQLite file. The
    // security of this endpoint is not a property of a pure function - it is a
    // property of the ORDER the handler does things in (count before lookup,
    // claim within one conditional UPDATE) - so a test that stubbed those steps
    // would prove nothing at all.
    // -----------------------------------------------------------------------

    /// The AppState the handlers run on: real config file, real pool.
    fn state_for(pool: SqlitePool) -> AppState {
        let config = std::sync::Arc::new(
            crate::config::AppConfig::load_from_file("../config/apikita.toml")
                .expect("the shipped config parses"),
        );
        let events = std::sync::Arc::new(crate::routes::events::RealtimeHub::new(&config.realtime));
        AppState {
            pool,
            config,
            http_client: reqwest::Client::new(),
            events,
            ip_salt: std::sync::Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies: std::sync::Arc::from(Vec::new().into_boxed_slice()),
        }
    }

    /// Inserts a code row directly, so a test controls expiry and used state
    /// exactly. The issuer is exercised by its own test.
    async fn seed_code(
        pool: &SqlitePool,
        account_id: Uuid,
        code: &str,
        expires_in_minutes: i64,
        used: bool,
    ) {
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO link_codes (code, account_id, expires_at, used_at, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(code)
        .bind(account_id.hyphenated())
        .bind(now + Duration::minutes(expires_in_minutes))
        .bind(used.then_some(now))
        .bind(now)
        .execute(pool)
        .await
        .expect("seed link code");
    }

    /// The account bound to a chat, if any.
    async fn linked_account(pool: &SqlitePool, telegram_id: &str) -> Option<Uuid> {
        let row = sqlx::query("SELECT account_id FROM telegram_links WHERE telegram_id = ?")
            .bind(telegram_id)
            .fetch_optional(pool)
            .await
            .expect("read telegram_links");
        row.map(|r| r.get::<Hyphenated, _>("account_id").into_uuid())
    }

    /// Attempts recorded across ALL hash keys.
    ///
    /// The handler derives its own salt from AppState.ip_salt (a DailySalt seeded
    /// per state), so a test cannot recompute the hash the handler will write.
    /// Counting every row asserts the property under test - "an attempt was
    /// recorded" - without reimplementing the very thing being tested.
    async fn any_attempt_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM link_redemption_attempts")
            .fetch_one(pool)
            .await
            .expect("count attempts")
    }

    /// The fixed client address the tests drive through ConnectInfo.
    const TEST_IP: &str = "203.0.113.7";

    fn peer() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
        axum::extract::ConnectInfo(format!("{TEST_IP}:5555").parse().unwrap())
    }

    fn bot_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        headers
    }

    /// The session cookie AMONG OTHERS, which is what a browser actually sends.
    ///
    /// This helper used to build a bare "session={token}". Four other route modules
    /// build "a=1; session={token}; b=2", and the difference is not cosmetic: a bare
    /// header never exercises the parser's scan past another cookie's "=", so a
    /// regression in `session_token_from_cookie_header` that broke multi-cookie
    /// parsing would have left every test in this file green.
    ///
    /// The header a customer's browser sends is never a lone cookie, so the fixture
    /// that should be impossible is the one this file was using.
    fn cookie_headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("a=1; session={token}; b=2").parse().unwrap(),
        );
        headers
    }

    /// A session for the account, returning its plaintext token.
    async fn session_for(pool: &SqlitePool, account_id: Uuid) -> String {
        let token = format!("apk_sess_{}", Uuid::new_v4().simple());
        let now = Utc::now();
        sqlx::query(
            "INSERT INTO sessions (id, account_id, token_hash, expires_at, last_seen_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().hyphenated())
        .bind(account_id.hyphenated())
        .bind(crate::routes::hash_token(&token))
        .bind(now + Duration::days(30))
        .bind(now)
        .bind(now)
        .execute(pool)
        .await
        .expect("create session");
        token
    }

    /// The bot token every redemption test installs.
    const BOT_TOKEN: &str = "test-bot-token-0123456789";

    /// The response body as text, for assertions on the JSON the caller sees.
    ///
    /// Going through the real `IntoResponse` rather than inspecting a struct is
    /// deliberate: the anti-oracle test compares the BYTES a caller receives, so
    /// serialisation differences would matter and must be included.
    async fn body_text(response: impl IntoResponse) -> String {
        let bytes = axum::body::to_bytes(response.into_response().into_body(), 8192)
            .await
            .expect("read the response body");
        String::from_utf8(bytes.to_vec()).expect("the body is utf-8")
    }

    // -----------------------------------------------------------------------
    // Live tests: the properties that make this endpoint safe.
    // -----------------------------------------------------------------------

    /// A FAILED guess is counted, and the cap eventually refuses a CORRECT code.
    ///
    /// This is the single most important test in the module. The attack on a
    /// 6-digit code IS a stream of failures, so a counter that only advanced on a
    /// successful redemption would never fire once - and the endpoint would hand
    /// out a funded wallet to a script. Refusing a code that is CORRECT, purely
    /// because too many guesses preceded it, is the only assertion that proves the
    /// counter sits on the failure path.
    #[tokio::test]
    async fn failed_guesses_are_counted_until_the_cap_refuses_even_a_correct_code() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());
        let limit = state.config.limits.link_redemption_per_hour;
        assert!(limit > 0, "the shipped config must have the cap ON");

        // A real, live, correct code. It must NOT redeem once the budget is spent.
        seed_code(&db.pool, account, "424242", 5, false).await;

        // Burn the budget with guesses that are all WRONG.
        for i in 0..limit {
            redeem_link_code(
                State(state.clone()),
                peer(),
                bot_headers(BOT_TOKEN),
                Json(RedeemLinkCodeRequest {
                    code: format!("{:06}", 900_000 + i),
                    telegram_id: "5550001".into(),
                }),
            )
            .await
            .expect("a wrong guess is a refusal body, not an error");
        }

        // THE PROOF: the correct code is refused now. Nothing about the code
        // changed - only how many times this client has guessed.
        let response = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers(BOT_TOKEN),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550001".into(),
            }),
        )
        .await;

        match response {
            Err(AppError::RateLimited { retry_after_secs }) => {
                assert!(
                    retry_after_secs >= 1,
                    "Retry-After must never be 0: a zero invites an immediate retry that is refused again"
                );
            }
            Ok(_) => panic!(
                "a correct code MUST be refused once the guess budget is spent - otherwise the cap never fires on the attack it exists for"
            ),
            Err(other) => panic!("expected a rate-limit refusal, got {other:?}"),
        }

        // The refusal is not cosmetic: no binding was written.
        assert_eq!(
            linked_account(&db.pool, "5550001").await,
            None,
            "a refused redemption must not have written a binding"
        );

        // A refused attempt is deliberately NOT stored, so the count is exactly the
        // budget that was allowed through - a throttled attacker cannot grow the
        // table without bound.
        assert_eq!(any_attempt_count(&db.pool).await, i64::from(limit));

        db.close().await;
    }

    /// POSITIVE CONTROL for the test above: under the cap, a correct code DOES
    /// redeem and DOES write the binding.
    ///
    /// Without this, "the cap refuses" would also pass on a handler that refuses
    /// everything.
    #[tokio::test]
    async fn a_correct_code_under_the_cap_redeems_and_writes_the_binding() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        seed_code(&db.pool, account, "424242", 5, false).await;

        let response = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers(BOT_TOKEN),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550002".into(),
            }),
        )
        .await
        .expect("a correct code under the cap must succeed");

        let text = body_text(response).await;
        assert!(text.contains("linked"), "expected a linked status: {text}");

        assert_eq!(
            linked_account(&db.pool, "5550002").await,
            Some(account),
            "the redemption must bind the chat to the account the code belonged to"
        );

        // The code is consumed, not deleted: data-retention.md retains the row
        // until used or expired + 24h, and the purge owns deletion.
        let used_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT used_at FROM link_codes WHERE code = ?")
                .bind("424242")
                .fetch_one(&db.pool)
                .await
                .expect("read the consumed code");
        assert!(
            used_at.is_some(),
            "a redeemed code must be stamped used_at, not left looking unused"
        );

        db.close().await;
    }

    /// A code is SINGLE-USE: the second redemption of the same code is refused.
    #[tokio::test]
    async fn a_code_is_single_use() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        seed_code(&db.pool, account, "424242", 5, false).await;

        let first = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers(BOT_TOKEN),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550003".into(),
            }),
        )
        .await
        .expect("first redemption succeeds");
        assert!(
            body_text(first).await.contains("linked"),
            "first use must link"
        );

        // Second use of the SAME code, from a different chat.
        let second = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers(BOT_TOKEN),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550004".into(),
            }),
        )
        .await
        .expect("a replayed code is a refusal body, not an error");
        assert!(
            body_text(second).await.contains(INVALID_CODE_STATUS),
            "a spent code must be refused"
        );

        assert_eq!(
            linked_account(&db.pool, "5550004").await,
            None,
            "the second chat must not be bound by a spent code"
        );

        db.close().await;
    }

    /// An EXPIRED code and an UNKNOWN code both refuse, and neither binds.
    #[tokio::test]
    async fn expired_and_unknown_codes_are_refused() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        seed_code(&db.pool, account, "111111", -1, false).await; // expired 1 min ago

        for code in ["111111", "222222"] {
            let response = redeem_link_code(
                State(state.clone()),
                peer(),
                bot_headers(BOT_TOKEN),
                Json(RedeemLinkCodeRequest {
                    code: code.into(),
                    telegram_id: "5550005".into(),
                }),
            )
            .await
            .expect("refusal is a body, not an error");
            assert!(
                body_text(response).await.contains(INVALID_CODE_STATUS),
                "code {code} must be refused"
            );
        }

        assert_eq!(linked_account(&db.pool, "5550005").await, None);
        db.close().await;
    }

    /// THE ANTI-ORACLE PROPERTY: wrong, expired, used and malformed codes all
    /// produce a BYTE-IDENTICAL refusal.
    ///
    /// If the endpoint distinguished them, an attacker would not need to guess a
    /// live code: learning "this one existed but expired" reduces a blind 10^6
    /// search to a walk over the few hundred codes live at any moment.
    #[tokio::test]
    async fn every_kind_of_bad_code_produces_the_same_refusal() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        seed_code(&db.pool, account, "111111", -1, false).await; // expired
        seed_code(&db.pool, account, "333333", 5, true).await; // already used

        let mut bodies: Vec<(String, String)> = Vec::new();
        for (code, label) in [
            ("999999", "unknown"),
            ("111111", "expired"),
            ("333333", "used"),
            ("12345", "malformed"),
        ] {
            let response = redeem_link_code(
                State(state.clone()),
                peer(),
                bot_headers(BOT_TOKEN),
                Json(RedeemLinkCodeRequest {
                    code: code.into(),
                    telegram_id: "5550006".into(),
                }),
            )
            .await
            .expect("refusal is a body, not an error");
            bodies.push((label.into(), body_text(response).await));
        }

        for (label, body) in &bodies[1..] {
            assert_eq!(
                *body, bodies[0].1,
                "the refusal for a {label} code must be IDENTICAL to the one for a {} code - any difference turns this endpoint into an oracle that reveals which codes are live",
                bodies[0].0
            );
        }

        db.close().await;
    }

    /// The bot credential is required, and a cookie is not one.
    #[tokio::test]
    async fn redemption_requires_the_bot_token() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());
        seed_code(&db.pool, account, "424242", 5, false).await;

        // No Authorization header at all.
        let no_auth = redeem_link_code(
            State(state.clone()),
            peer(),
            HeaderMap::new(),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550007".into(),
            }),
        )
        .await;
        assert!(matches!(no_auth, Err(AppError::Unauthenticated)));

        // A WRONG token.
        let wrong = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers("not-the-bot-token"),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550007".into(),
            }),
        )
        .await;
        assert!(matches!(wrong, Err(AppError::Unauthenticated)));

        // A COOKIE is a human credential, not a bot one.
        let session = session_for(&db.pool, account).await;
        let cookie = redeem_link_code(
            State(state.clone()),
            peer(),
            cookie_headers(&session),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550007".into(),
            }),
        )
        .await;
        assert!(
            matches!(cookie, Err(AppError::Unauthenticated)),
            "a cookie must never authenticate the bot endpoint"
        );

        // None of the three consumed the code or wrote a binding.
        assert_eq!(linked_account(&db.pool, "5550007").await, None);
        let used_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT used_at FROM link_codes WHERE code = ?")
                .bind("424242")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert!(
            used_at.is_none(),
            "an unauthenticated request must not consume a live code"
        );

        db.close().await;
    }

    /// A MISSING bot token refuses rather than allows: the fail-closed rule.
    #[tokio::test]
    async fn a_missing_bot_token_refuses_rather_than_allows() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        std::env::remove_var("TELEGRAM_BOT_TOKEN");

        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());
        seed_code(&db.pool, account, "424242", 5, false).await;

        let response = redeem_link_code(
            State(state.clone()),
            peer(),
            bot_headers(""),
            Json(RedeemLinkCodeRequest {
                code: "424242".into(),
                telegram_id: "5550008".into(),
            }),
        )
        .await;

        assert!(
            matches!(response, Err(AppError::Unauthenticated)),
            "with no configured secret there is no way to tell the bot from an attacker, so this must fail CLOSED"
        );
        assert_eq!(linked_account(&db.pool, "5550008").await, None);
        db.close().await;
    }

    /// THE ISSUANCE CAP NOW FIRES, and under concurrency.
    ///
    /// This guard was DEAD, and the way that was found is the part worth keeping.
    /// `link_code_issuance_per_hour` is documented in docs/server/api-spec.md and
    /// docs/decisions.md as bounding how many link codes an ACCOUNT may issue in an
    /// hour, and migration 20260926000000 records the design: it counts `link_codes`
    /// rows, "which the existing table already carries (account_id, created_at), so
    /// it needs nothing new and reuses abuse::enforce_creation_cap unchanged."
    ///
    /// That was true when written and stopped being true when issue_link_code gained
    /// the DELETE that keeps one live code per account. The DELETE makes the row
    /// count always 0 or 1, so a cap of 10 was never approached: the guard parsed,
    /// ran, and did nothing.
    ///
    /// A test written to assert the OPPOSITE is what surfaced it, failing with "1
    /// codes issued for an account whose cap is 10 (18 answered 200, 0 refused)". The
    /// 1 was the giveaway rather than the problem - the table can only ever hold one
    /// row for the account, so the row count CANNOT measure the cap whatever it
    /// does. The number that measures it is how many requests were ADMITTED, and
    /// every one of them was.
    ///
    /// The fix is a table that survives the delete: issuances are recorded in
    /// `link_code_issues`, the cap counts that, and the count and the record commit
    /// in the same transaction as the code itself. Superseding the old rows instead
    /// would have fixed the count too, but the DELETE is what expresses the
    /// one-live-code guarantee, and a second mechanism for it is a second thing to get
    /// wrong.
    ///
    /// The assertion is EXACT rather than approximate: a burst of cap+8 produces
    /// exactly `cap` admissions, the rest are refused, exactly one issuance row is
    /// written per admission, and exactly one code survives.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_link_code_issuance_cap_fires_and_holds_under_concurrent_requests() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());
        let session = session_for(&db.pool, account).await;

        let limit = state.config.limits.link_code_issuance_per_hour;
        assert!(limit > 0, "the fixture assumes a configured hourly cap");

        let mut handles = Vec::new();
        for _ in 0..(limit + 8) {
            let state = state.clone();
            let headers = cookie_headers(&session);
            handles.push(tokio::spawn(async move {
                issue_link_code(State(state), headers)
                    .await
                    .map(|response| response.into_response().status())
            }));
        }

        let mut issued = 0usize;
        let mut refused = 0usize;
        for handle in handles {
            match handle.await.expect("the issue task must not panic") {
                Ok(status) => {
                    assert_eq!(status, StatusCode::OK, "a permitted issue answers 200");
                    issued += 1;
                }
                Err(AppError::RateLimited { .. }) => refused += 1,
                Err(other) => panic!("unexpected error from a burst request: {other:?}"),
            }
        }

        // The cap must hold EXACTLY, under a burst, against a table that survives
        // the delete. A test allowing a small overshoot would pass against the old
        // link_codes counting on a fast machine - which is the whole problem the old
        // counting had, since it could not fire at all.
        let burst = limit as usize + 8;
        assert_eq!(
            issued, limit as usize,
            "{issued} of {burst} concurrent issues were admitted against a cap of \
             {limit}, with {refused} refused. The count and the record commit in one \
             transaction, so the cap must hold EXACTLY"
        );
        assert_eq!(
            refused,
            burst - limit as usize,
            "every request past the cap must be refused, and refused with 429 rather \
             than silently succeeding"
        );

        // The issuance record is what makes the cap countable, so it is asserted
        // directly: exactly one row per ADMITTED issue, not per surviving code.
        let recorded: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM link_code_issues WHERE account_id = ?")
                .bind(account.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("count the issuance record");
        assert_eq!(
            recorded,
            i64::from(limit),
            "one issuance row per admitted issue: a refused request must not leave a \
             record, or the cap would ratchet on every attempt and refuse an account \
             that never actually issued anything"
        );

        // And the one-live-code guarantee is untouched by any of this.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM link_codes WHERE account_id = ?")
            .bind(account.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count the surviving rows");
        assert_eq!(
            rows, 1,
            "only one code may survive per account, whatever the cap does - the \
             guarantee the DELETE exists for, and the reason the cap needed a \
             separate table rather than a change to this one"
        );

        db.close().await;
    }

    /// Issuing requires a session, writes a live code, and REPLACES any prior one.
    #[tokio::test]
    async fn issuing_replaces_the_previous_code_and_requires_a_session() {
        let db = TestDb::new().await;
        let account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        // No cookie -> unauthenticated, and nothing written.
        let anonymous = issue_link_code(State(state.clone()), HeaderMap::new()).await;
        assert!(matches!(anonymous, Err(AppError::Unauthenticated)));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM link_codes")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert_eq!(count, 0, "an unauthenticated issue must write nothing");

        let session = session_for(&db.pool, account).await;

        let first = issue_link_code(State(state.clone()), cookie_headers(&session))
            .await
            .expect("an authenticated issue succeeds");
        assert!(body_text(first).await.contains("ttl_minutes"));

        issue_link_code(State(state.clone()), cookie_headers(&session))
            .await
            .expect("a second issue succeeds");

        let live: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM link_codes WHERE account_id = ? AND used_at IS NULL AND expires_at > ?",
        )
        .bind(account.hyphenated())
        .bind(Utc::now())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            live, 1,
            "at most ONE code may be live per account: two double an attacker chance per guess"
        );

        db.close().await;
    }

    /// Unlinking removes the binding and NOTHING else.
    #[tokio::test]
    async fn unlinking_removes_only_the_binding_row() {
        let db = TestDb::new().await;
        let account = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, account, 50_000).await;
        let state = state_for(db.pool.clone());

        sqlx::query(
            "INSERT INTO telegram_links (telegram_id, account_id, linked_at) VALUES (?, ?, ?)",
        )
        .bind("5550009")
        .bind(account.hyphenated())
        .bind(Utc::now())
        .execute(&db.pool)
        .await
        .expect("bind a chat");

        let balance_before = test_support::balance(&db.pool, account).await;
        let ledger_before = test_support::ledger_sum(&db.pool, account).await;

        let session = session_for(&db.pool, account).await;
        unlink_telegram(State(state.clone()), cookie_headers(&session))
            .await
            .expect("unlink succeeds");

        assert_eq!(linked_account(&db.pool, "5550009").await, None);

        // The point: api-spec.md says "Removes one row - never the account or the
        // wallet".
        assert_eq!(
            test_support::balance(&db.pool, account).await,
            balance_before,
            "unlinking must not touch the wallet"
        );
        assert_eq!(
            test_support::ledger_sum(&db.pool, account).await,
            ledger_before,
            "unlinking must not move the ledger"
        );

        db.close().await;
    }

    /// Re-linking the SAME chat rebinds it instead of failing on the primary key.
    #[tokio::test]
    async fn relinking_the_same_chat_rebinds_it() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let first_account = test_support::account(&db.pool).await;
        let second_account = test_support::account(&db.pool).await;
        let state = state_for(db.pool.clone());

        seed_code(&db.pool, first_account, "111111", 5, false).await;
        seed_code(&db.pool, second_account, "222222", 5, false).await;

        for (code, expected) in [("111111", first_account), ("222222", second_account)] {
            let response = redeem_link_code(
                State(state.clone()),
                peer(),
                bot_headers(BOT_TOKEN),
                Json(RedeemLinkCodeRequest {
                    code: code.into(),
                    telegram_id: "5550010".into(),
                }),
            )
            .await
            .expect("each redemption succeeds");
            assert!(body_text(response).await.contains("linked"), "code {code}");
            assert_eq!(
                linked_account(&db.pool, "5550010").await,
                Some(expected),
                "the chat must follow the most recent successful redemption"
            );
        }

        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM telegram_links WHERE telegram_id = ?")
                .bind("5550010")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(rows, 1, "a rebind must replace, not accumulate");

        db.close().await;
    }

    /// The per-IP counter sees guesses that reference NO account at all, and
    /// stores only a salted hash.
    ///
    /// The two caps defend different attacks: the account cap bounds codes in
    /// flight for one account, while the IP cap bounds guessing from one host. An
    /// attacker cycling the code space touches no account, so this is the test
    /// that proves the IP counter exists.
    #[tokio::test]
    async fn the_per_ip_cap_records_guesses_that_match_no_account_and_stores_only_a_hash() {
        let _lock = crate::routes::test_env::EnvLock::acquire();
        let _token = crate::routes::test_env::EnvGuard::set("TELEGRAM_BOT_TOKEN", BOT_TOKEN);

        let db = TestDb::new().await;
        let state = state_for(db.pool.clone());

        assert_eq!(any_attempt_count(&db.pool).await, 0);

        for i in 0..3 {
            redeem_link_code(
                State(state.clone()),
                peer(),
                bot_headers(BOT_TOKEN),
                Json(RedeemLinkCodeRequest {
                    code: format!("{:06}", 800_000 + i),
                    telegram_id: "5550011".into(),
                }),
            )
            .await
            .expect("a guess is a refusal body, not an error");
        }

        assert_eq!(
            any_attempt_count(&db.pool).await,
            3,
            "every guess must be recorded against the client IP hash, even when no code and no account matched"
        );

        let stored: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT ip_hash FROM link_redemption_attempts")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert_eq!(stored.len(), 1, "one client, one hash");
        assert!(
            !stored[0].contains(TEST_IP),
            "the raw IP must NEVER be stored - only a salted hash: got {}",
            stored[0]
        );

        db.close().await;
    }
    #[test]
    fn only_six_ascii_digits_are_well_formed() {
        assert!(is_well_formed("000000"));
        assert!(is_well_formed("123456"));
        assert!(is_well_formed("999999"));

        // Too short, too long, empty.
        assert!(!is_well_formed("12345"));
        assert!(!is_well_formed("1234567"));
        assert!(!is_well_formed(""));

        // Non-digits, including the shapes a naive parser accepts.
        assert!(!is_well_formed("12345a"));
        assert!(!is_well_formed("12 456"));
        assert!(!is_well_formed("-12345"));
        assert!(!is_well_formed("+12345"));
        // Unicode digits are NOT ASCII digits: '٣' is not '3'.
        assert!(!is_well_formed("١٢٣٤٥٦"));
        // A leading/trailing newline is a classic bypass of a length-only check.
        assert!(!is_well_formed("123456\n"));
    }
}
