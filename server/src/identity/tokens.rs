//! The single-use email tokens: verification links and password-reset links.
//!
//! One table, two purposes. `identity_tokens` distinguishes them with a `purpose`
//! column rather than a second table, because everything about the two is the
//! same — issue, mail, redeem once, expire — and the ways they differ (the TTL,
//! the copy, what redemption does next) live in the config and in the caller.
//!
//! ## Why the token is 32 random bytes and not a UUID
//!
//! `routes/auth.rs` mints session tokens as `apk_sess_{uuid}`, and that is right
//! for a session: it is delivered over TLS to one client that asked for it. A link
//! in this module is not. It travels through a mail provider, a mail client, an
//! inbox, possibly a chat window when someone forwards it, and a browser history —
//! so it must be unguessable by an observer who never sees the request. 122 bits
//! of UUIDv4 is a fine identifier; 256 bits of OS entropy is what a bearer
//! credential that will be pasted around deserves.
//!
//! ## Why the hash is plain SHA-256
//!
//! `hash_token` is the same function the session path uses, and it is unsalted and
//! unstretched ON PURPOSE. Stretching exists to make a low-entropy secret
//! expensive to enumerate; the input here is 256 uniform bits from `OsRng`, so
//! there is nothing to enumerate and no dictionary to run. The value of the hash
//! is that a read of this table does not hand over working links — not that it
//! resists brute force, which it has nothing to resist.
//!
//! ## Single use is enforced in SQL, and issuing replaces
//!
//! `consume` marks `consumed_at` and refuses when it is already set. Doing it as a
//! conditional UPDATE rather than a SELECT-then-UPDATE is what makes two
//! simultaneous redemptions of one link resolve to exactly one winner: SQLite
//! serialises the two writes, and the second one's WHERE clause no longer matches.
//! A read followed by a write would let both see `consumed_at IS NULL`.
//!
//! `issue` deletes the account's outstanding tokens of the same purpose first. That
//! is a resend invalidating the previous link, which is what "resend" has to mean —
//! otherwise every resend leaves another working credential in every mailbox the
//! older one reached, and the count of live links per account is unbounded.

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::AppError;
use crate::routes::hash_token;

/// What a token is for. The string in `as_str` is `identity_tokens.purpose`, whose
/// CHECK constraint enumerates exactly these two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// Proves control of an email address. Redeeming it sets `email_verified = 1`
    /// and stamps `verified_at`.
    Verification,
    /// Lets the holder choose a new password without knowing the old one.
    Reset,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Verification => "verification",
            Purpose::Reset => "reset",
        }
    }
}

/// A freshly minted token: the value that goes in the link, and the row's id.
///
/// The raw value is returned ONCE, here, and never stored — the table holds only
/// its hash. A caller that loses this value cannot recover it; it re-issues.
#[derive(Debug)]
pub struct IssuedToken {
    /// The token as it should appear in the emailed URL. Never logged.
    pub raw: String,
    /// The `identity_tokens.id`, for a log line that can name the row without
    /// naming the secret.
    pub token_id: Uuid,
    pub expires_at: DateTime<Utc>,
}

/// The fields a redemption needs from the token's row.
#[derive(Debug)]
pub struct RedeemedToken {
    pub account_id: Uuid,
    /// The address on the identity this token authorises, so a caller can compare
    /// it against the address the token was issued for. See the module docs in
    /// `prehijack-spec.md` on why address normalisation is load-bearing.
    pub purpose: Purpose,
}

/// Mint, store and return a token for `account_id`.
///
/// Replaces any outstanding token of the same purpose for the account — see the
/// module docs. The window is the caller's, because the two purposes have
/// different TTLs in `[auth]` and neither is this module's decision.
pub async fn issue(
    pool: &SqlitePool,
    account_id: Uuid,
    purpose: Purpose,
    ttl: Duration,
    now: DateTime<Utc>,
) -> Result<IssuedToken, AppError> {
    // 32 bytes from the OS entropy source, hex-encoded. Lowercase hex to match
    // every other token this crate mints (`routes/mod.rs::hash_token` produces
    // lowercase hex, so the stored hash and the display form agree).
    let mut bytes = [0u8; 32];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut bytes);
    let raw = hex::encode(bytes);

    // SAFE, and the same argument every date arithmetic in this repository makes:
    // chrono PANICS rather than wrapping on an out-of-range result, and the
    // operand is a TTL that `config.rs::validate` has already bounded with
    // `checked_add_signed`. A panic here would be a 500 on a sign-in path.
    #[allow(clippy::arithmetic_side_effects)]
    let expires_at = now + ttl;

    let token_id = Uuid::new_v4();

    // One transaction: the DELETE that invalidates the predecessor and the INSERT
    // that replaces it must not be separable, or a crash between them leaves the
    // account with no live token and a user who never got a mail. BEGIN IMMEDIATE
    // rather than a deferred BEGIN for the reason `db.rs` gives.
    let mut tx = crate::db::begin_immediate(pool).await?;

    sqlx::query("DELETE FROM identity_tokens WHERE account_id = ? AND purpose = ?")
        .bind(account_id.hyphenated())
        .bind(purpose.as_str())
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        "INSERT INTO identity_tokens (id, account_id, purpose, token_hash, expires_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(token_id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(purpose.as_str())
    .bind(hash_token(&raw))
    .bind(expires_at)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IssuedToken {
        raw,
        token_id,
        expires_at,
    })
}

/// Redeem a token, exactly once.
///
/// Every failure — no such token, already consumed, expired, wrong purpose — is
/// `AppError::Unauthenticated`, the same collapse `resolve_account_from_cookie`
/// performs and for the same reason: a caller holding a bad link must not learn
/// which way it is bad. "Expired" versus "already used" would tell an attacker who
/// found a stale link that it was once real.
///
/// THE MARK AND THE CHECK ARE ONE STATEMENT. `UPDATE ... WHERE consumed_at IS NULL
/// AND expires_at > ?` returning the row means two simultaneous redemptions cannot
/// both succeed, and a token that expired a millisecond ago cannot be redeemed by
/// a caller whose clock check ran a millisecond earlier.
pub async fn consume(
    pool: &SqlitePool,
    raw: &str,
    purpose: Purpose,
    now: DateTime<Utc>,
) -> Result<RedeemedToken, AppError> {
    let row = sqlx::query(
        "UPDATE identity_tokens SET consumed_at = ? \
         WHERE token_hash = ? AND purpose = ? AND consumed_at IS NULL AND expires_at > ? \
         RETURNING account_id",
    )
    .bind(now)
    .bind(hash_token(raw))
    .bind(purpose.as_str())
    .bind(now)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        // No distinction is drawn between the reasons. See the doc above.
        return Err(AppError::Unauthenticated);
    };

    let account_id: String = row.get("account_id");
    let account_id = Uuid::parse_str(&account_id).map_err(|e| {
        AppError::Internal(format!("identity_tokens.account_id is unreadable: {e}"))
    })?;

    Ok(RedeemedToken {
        account_id,
        purpose,
    })
}

/// Deletes every token for an account, whatever its purpose.
///
/// Called when an account is done with the flow — a completed reset, a confirmed
/// address — so a link that was already used cannot be replayed from a mailbox
/// after the fact. `consume` already makes replay impossible for the token that
/// was redeemed; this covers the ones that were issued and never opened.
pub async fn clear_for_account(pool: &SqlitePool, account_id: Uuid) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM identity_tokens WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

/// Removes rows that are past their life, for the retention sweep.
///
/// Returns the number of rows deleted. The distinction from `consume` is that this
/// deletes WITHOUT redeeming: an expired link is not a revoked one, and the reason
/// this runs at all is that the privacy page promises expired links do not persist.
pub async fn purge_expired(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM identity_tokens WHERE expires_at <= ?")
        .bind(now)
        .execute(pool)
        .await?;

    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDb;

    fn hour() -> Duration {
        Duration::hours(1)
    }

    /// The round trip, with the negative control that makes it mean something: a
    /// different token must not redeem.
    #[tokio::test]
    async fn a_token_redeems_and_a_different_one_does_not() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let issued = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("issue");

        let other = issue(&db.pool, account, Purpose::Reset, hour(), now)
            .await
            .expect("issue the other purpose");

        // The RESET token must not redeem as a VERIFICATION token. If purpose were
        // not in the WHERE clause this would succeed, and a reset link would
        // double as proof of the address.
        assert!(
            matches!(
                consume(&db.pool, &other.raw, Purpose::Verification, now).await,
                Err(AppError::Unauthenticated)
            ),
            "a reset token must not redeem as a verification token"
        );

        let redeemed = consume(&db.pool, &issued.raw, Purpose::Verification, now)
            .await
            .expect("the token redeems");
        assert_eq!(redeemed.account_id, account);
        assert_eq!(redeemed.purpose, Purpose::Verification);
    }

    /// Single use. The second redemption of one link fails, which is the whole
    /// reason redemption is an UPDATE and not a SELECT.
    #[tokio::test]
    async fn a_token_cannot_be_redeemed_twice() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let issued = issue(&db.pool, account, Purpose::Reset, hour(), now)
            .await
            .expect("issue");

        assert!(consume(&db.pool, &issued.raw, Purpose::Reset, now)
            .await
            .is_ok());
        assert!(
            matches!(
                consume(&db.pool, &issued.raw, Purpose::Reset, now).await,
                Err(AppError::Unauthenticated)
            ),
            "the second redemption of one link must fail"
        );
    }

    /// Only the hash is stored. A read of the table must not hand over working
    /// links, which is the one promise this module makes about its own storage.
    #[tokio::test]
    async fn the_raw_token_is_never_stored() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let issued = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("issue");

        let stored: String = sqlx::query_scalar("SELECT token_hash FROM identity_tokens")
            .fetch_one(&db.pool)
            .await
            .expect("read back");

        assert_ne!(stored, issued.raw, "the raw token must not be in the table");
        assert_eq!(
            stored,
            hash_token(&issued.raw),
            "the stored value is its hash"
        );
        assert_eq!(stored.len(), 64, "sha256 hex");
        assert_eq!(issued.raw.len(), 64, "32 bytes hex-encoded");
    }

    /// Issuing replaces, so a resend leaves exactly one live link and the previous
    /// one is dead. Without this, every resend would add another working
    /// credential to every mailbox the older link reached.
    #[tokio::test]
    async fn issuing_replaces_the_previous_token_for_the_same_purpose() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let first = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("issue");
        let second = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("reissue");

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identity_tokens")
            .fetch_one(&db.pool)
            .await
            .expect("count");
        assert_eq!(rows, 1, "a resend must leave one token, not two");

        assert!(
            matches!(
                consume(&db.pool, &first.raw, Purpose::Verification, now).await,
                Err(AppError::Unauthenticated)
            ),
            "the replaced token must be dead - that is what makes a resend a resend"
        );
        assert!(
            consume(&db.pool, &second.raw, Purpose::Verification, now)
                .await
                .is_ok(),
            "the replacement must work"
        );
    }

    /// The two purposes do not replace each other: a reset does not invalidate a
    /// pending verification link.
    #[tokio::test]
    async fn the_purposes_do_not_replace_each_other() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let verification = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("issue");
        issue(&db.pool, account, Purpose::Reset, hour(), now)
            .await
            .expect("issue the other purpose");

        assert!(
            consume(&db.pool, &verification.raw, Purpose::Verification, now)
                .await
                .is_ok(),
            "a reset request must not cancel a pending verification link"
        );
    }

    /// An expired token does not redeem, asserted by issuing one that is already
    /// stale rather than by sleeping.
    #[tokio::test]
    async fn an_expired_token_does_not_redeem() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        // Issued an hour ago with a one-minute life, so it expired 59 minutes ago.
        let issued = issue(
            &db.pool,
            account,
            Purpose::Reset,
            Duration::minutes(1),
            now - Duration::hours(1),
        )
        .await
        .expect("issue");

        assert!(
            matches!(
                consume(&db.pool, &issued.raw, Purpose::Reset, now).await,
                Err(AppError::Unauthenticated)
            ),
            "a token past its expiry must not redeem, and must not say why"
        );
    }

    /// The address on the row is the account's, so a token cannot be issued for one
    /// account and redeemed against another.
    #[tokio::test]
    async fn a_token_belongs_to_the_account_it_was_issued_for() {
        let db = TestDb::new().await;
        let first = crate::test_support::account(&db.pool).await;
        let second = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let issued = issue(&db.pool, first, Purpose::Reset, hour(), now)
            .await
            .expect("issue");

        let redeemed = consume(&db.pool, &issued.raw, Purpose::Reset, now)
            .await
            .expect("redeem");
        assert_eq!(redeemed.account_id, first);
        assert_ne!(
            redeemed.account_id, second,
            "the token must resolve to its own account"
        );
    }

    /// `clear_for_account` kills the outstanding links, so a completed flow leaves
    /// nothing in a mailbox that still works.
    #[tokio::test]
    async fn clearing_an_account_removes_its_outstanding_tokens() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let other = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let issued = issue(&db.pool, account, Purpose::Reset, hour(), now)
            .await
            .expect("issue");
        let kept = issue(&db.pool, other, Purpose::Reset, hour(), now)
            .await
            .expect("issue");

        let removed = clear_for_account(&db.pool, account).await.expect("clear");
        assert_eq!(removed, 1, "one token belonged to the account");

        assert!(
            matches!(
                consume(&db.pool, &issued.raw, Purpose::Reset, now).await,
                Err(AppError::Unauthenticated)
            ),
            "a cleared token must not redeem"
        );
        assert!(
            consume(&db.pool, &kept.raw, Purpose::Reset, now)
                .await
                .is_ok(),
            "another account's token must be untouched"
        );
    }

    /// `purge_expired` deletes the stale rows and leaves the live ones, which is
    /// what the retention sweep needs.
    #[tokio::test]
    async fn purging_removes_only_the_expired_rows() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;
        let now = Utc::now();

        let stale = issue(
            &db.pool,
            account,
            Purpose::Reset,
            Duration::minutes(1),
            now - Duration::hours(2),
        )
        .await
        .expect("issue the stale one");
        let live = issue(&db.pool, account, Purpose::Verification, hour(), now)
            .await
            .expect("issue the live one");

        let purged = purge_expired(&db.pool, now).await.expect("purge");
        assert_eq!(purged, 1, "one row was past its life");

        assert!(
            matches!(
                consume(&db.pool, &stale.raw, Purpose::Reset, now).await,
                Err(AppError::Unauthenticated)
            ),
            "the purged row is gone"
        );
        assert!(
            consume(&db.pool, &live.raw, Purpose::Verification, now)
                .await
                .is_ok(),
            "the live row must survive the purge"
        );
    }
}
