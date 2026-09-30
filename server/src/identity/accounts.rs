//! The identity rows, and the linking rule that decides which account a sign-in
//! belongs to.
//!
//! ## What an "identity" is
//!
//! One row in `identities` is one way of proving you are someone: a password, or a
//! Google account. An ACCOUNT is the thing that owns a wallet, an API key set and a
//! session — and an account may have more than one identity attached, which is what
//! lets a person who signed up with a password later sign in with Google and land
//! in the same place.
//!
//! The schema already forbids the two shapes that would be nonsense:
//!
//! * `UNIQUE (provider, subject)` — one Google subject maps to one row.
//! * `CHECK ((provider = 'password') = (password_hash IS NOT NULL))` — a password
//!   identity has a hash, a Google identity does not.
//! * `CHECK (provider <> 'google' OR email_verified = 1)` — a Google identity is
//!   ALWAYS verified, because Google checked it.
//! * `UNIQUE (provider, email)` — one address per provider. NOTE THE SHAPE: it is
//!   `(provider, email)` and not `(email)`. That is deliberate and load-bearing —
//!   it is what allows an UNVERIFIED password identity and a Google identity to
//!   coexist on one address, which is the state the refusal branch below relies on.
//!   "Simplifying" it to `UNIQUE (email)` would turn the refusal branch into a
//!   constraint violation, which is a 500 on a user error.
//!
//! ## The four pre-hijacking vectors
//!
//! `docs/architecture/identity.md` names four ways an attacker with the victim's
//! email address (but not their mailbox) can end up owning the victim's account.
//! Each is answered by a predicate here:
//!
//! **R1 — the attacker registers with the victim's address first.** The victim then
//! signs in with Google. If that sign-in adopted the attacker's row, the attacker's
//! password would open the victim's account. So a Google sign-in may adopt an
//! existing password identity ONLY IF that identity's address was verified. When it
//! was not, a NEW account is created and the collision is logged.
//!
//! WHY THE UNVERIFIED CASE CREATES RATHER THAN REFUSES: refusing would hand an
//! attacker a denial of service on any address they like — register with it, never
//! verify, and the real owner can never use Google. Creating gives the attacker
//! nothing (their row is inert and unverified) and gives the victim a working
//! account. The cost is an orphan row, which is stated rather than hidden.
//!
//! **R2 — verification can be claimed rather than proven.** `email_verified` goes
//! to 1 only on proof of that exact address: a token delivered to it, a Google ID
//! token proving it, or a reset token for it. See the module docs on [`super`].
//!
//! **R3 — the late-verification retro-link.** This is why `verified_at` exists. A
//! bit says "verified" but not "verified WHEN", and the attack needs the ordering:
//! the attacker registers with the victim's address and leaves it unverified; the
//! victim signs in with Google, and the unverified row is correctly refused; the
//! victim then verifies their own address, and if the rule were re-evaluated on the
//! bit alone the link would now be authorised — by the victim's own proof, to the
//! attacker's row. So the rule compares `p.verified_at < g.created_at`: the
//! password identity's verification must PREDATE the Google identity's creation.
//!
//! **Vector 3 — a Google identity that is not verified.** Already unrepresentable
//! in the schema (`CHECK (provider <> 'google' OR email_verified = 1)`), but the
//! callback must still refuse an ID token whose `email_verified` is not true,
//! BEFORE touching the database. Otherwise the INSERT hits the raw CHECK and
//! surfaces as `AppError::Internal` — a 500 for what is the caller's problem.
//!
//! **Vector 4 — resetting a password onto a Google-only account.** DECIDED: allow
//! it, and it is not an escalation. Completing a reset already requires control of
//! the mailbox, and control of the mailbox is exactly what Google-ownership means,
//! so the reset proves nothing new. Blocking it would cost support load (the
//! legitimate "I signed up with Google and want a password now" case) and buy
//! nothing. What the reset must do is create the password identity with `subject`
//! set to the normalised address, not a random id: `UNIQUE (provider, subject)`
//! cannot collide on a random subject, so two password identities for one address
//! would become representable.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::AppError;

/// The two providers, as they appear in `identities.provider`.
pub const GOOGLE: &str = "google";
pub const PASSWORD: &str = "password";

/// What a Google sign-in resolved to.
#[derive(Debug)]
pub enum GoogleSignIn {
    /// An identity with this subject already existed; this is an ordinary sign-in.
    Existing(Uuid),
    /// The address matched a VERIFIED password identity, which was adopted. This is
    /// the legitimate "I signed up with a password, now I am using Google" path.
    ///
    /// **It carries only the account.** It used to also carry `identity_id` — "the
    /// password identity that was adopted, for the audit line" — and there was no audit
    /// line: the only production match binds it with `..` and logs nothing. The
    /// COLLISION case below is the one that logs, which is right, because an adoption
    /// is the ordinary path and a collision is the defended one.
    Linked { account_id: Uuid },
    /// The address matched a password identity that was NOT verified (or not
    /// verified before this Google identity was created). R1/R3 say do not adopt
    /// it; a fresh account was created instead.
    CollisionCreated {
        account_id: Uuid,
        /// Logged, never shown to the caller.
        colliding_identity_id: Uuid,
    },
    /// Nothing matched; a fresh account was created.
    Created(Uuid),
}

/// Normalise an address for storage and comparison.
///
/// LOWERCASED AND OTHERWISE VERBATIM, and this is a deliberate conservative choice
/// rather than a finished answer. The full RFC 5321 local-part rules make two
/// addresses equal in ways that vary by provider (dots in Gmail, `+` tags
/// everywhere, case almost nowhere), and applying a provider-specific rule here
/// would mean that two spellings the SYSTEM thinks are one address can disagree
/// with what the identity providers think.
///
/// Lowercasing is the one transformation that is safe across every provider in
/// practice and that both of ours already perform, so it is the whole rule. The
/// risk this leaves is stated in the pre-hijacking spec: two identities that a
/// human reads as the same mailbox can exist as two rows. That is a support case,
/// not a security hole — the linking rule never infers account identity from an
/// address that was not proven.
pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// The identity a Google sign-in belongs to, creating or linking as the rules say.
///
/// `google_subject` is the `sub` claim, which is the provider's IMMUTABLE id for
/// the account. Resolving on it — not on the email — is what makes a stable
/// sign-in: a Google user can change their address, and `sub` is what survives.
///
/// This runs inside ONE write transaction because the decision it makes is a
/// read-then-write over rows that a second request could be racing.
pub async fn resolve_google_sign_in(
    pool: &SqlitePool,
    google_subject: &str,
    email: &str,
    now: DateTime<Utc>,
) -> Result<GoogleSignIn, AppError> {
    let normalized = normalize_email(email);
    let mut tx = crate::db::begin_immediate(pool).await?;

    // 1. The subject is the identity, when it is already known. This is the
    //    ordinary repeat-sign-in path and it must come FIRST: it is the only step
    //    that is not about the address at all.
    let existing: Option<String> =
        sqlx::query_scalar("SELECT account_id FROM identities WHERE provider = ? AND subject = ?")
            .bind(GOOGLE)
            .bind(google_subject)
            .fetch_optional(&mut *tx)
            .await?;

    if let Some(account_id) = existing {
        let account_id = parse_uuid(&account_id, "identities.account_id")?;
        tx.commit().await?;
        return Ok(GoogleSignIn::Existing(account_id));
    }

    // 2. R1 + R3, in one query. The password identity on this address is adoptable
    //    only when it was verified BEFORE this Google identity is being created.
    //
    //    `verified_at IS NOT NULL AND verified_at < ?` rather than `email_verified
    //    = 1`: the bit says the address was proven at some point, and the attack
    //    (R3) is precisely the case where it was proven LATER. Comparing against
    //    `now` — the moment this Google identity comes into existence — is what
    //    makes the ordering the rule cares about.
    let candidate = sqlx::query(
        "SELECT id, account_id, verified_at FROM identities \
         WHERE provider = ? AND email = ? LIMIT 1",
    )
    .bind(PASSWORD)
    .bind(&normalized)
    .fetch_optional(&mut *tx)
    .await?;

    // The Google row is written either way, so it is built once here.
    let google_identity_id = Uuid::new_v4();

    let outcome = match candidate {
        Some(row) => {
            let identity_id = parse_uuid(&row.get::<String, _>("id"), "identities.id")?;
            let candidate_account =
                parse_uuid(&row.get::<String, _>("account_id"), "identities.account_id")?;
            let verified_at: Option<DateTime<Utc>> = row.get("verified_at");

            let adoptable = verified_at.is_some_and(|verified| verified < now);

            if adoptable {
                sqlx::query(
                    "INSERT INTO identities \
                     (id, account_id, provider, subject, email, email_verified, verified_at, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
                )
                .bind(google_identity_id.hyphenated())
                .bind(candidate_account.hyphenated())
                .bind(GOOGLE)
                .bind(google_subject)
                .bind(&normalized)
                .bind(now)
                .bind(now)
                .bind(now)
                .execute(&mut *tx)
                .await?;

                GoogleSignIn::Linked {
                    account_id: candidate_account,
                }
            } else {
                // R1: the colliding row exists but proves nothing about this
                // caller, so a NEW account is created rather than refusing. See
                // the module docs for why creating beats refusing here.
                let account_id = Uuid::new_v4();
                create_account_row(&mut tx, account_id, now).await?;
                insert_google_identity(
                    &mut tx,
                    google_identity_id,
                    account_id,
                    google_subject,
                    &normalized,
                    now,
                )
                .await?;

                GoogleSignIn::CollisionCreated {
                    account_id,
                    colliding_identity_id: identity_id,
                }
            }
        }
        None => {
            let account_id = Uuid::new_v4();
            create_account_row(&mut tx, account_id, now).await?;
            insert_google_identity(
                &mut tx,
                google_identity_id,
                account_id,
                google_subject,
                &normalized,
                now,
            )
            .await?;
            GoogleSignIn::Created(account_id)
        }
    };

    tx.commit().await?;
    Ok(outcome)
}

/// The identity row a password sign-in should be checked against.
///
/// Returns the account id, the stored PHC string and whether the address is
/// verified. `None` means there is no password identity for this address — and the
/// caller must treat that identically to a wrong password, including spending the
/// same CPU, or the endpoint becomes an oracle for which addresses are registered.
pub async fn password_identity(
    pool: &SqlitePool,
    email: &str,
) -> Result<Option<PasswordIdentity>, AppError> {
    let normalized = normalize_email(email);

    let row = sqlx::query(
        "SELECT id, account_id, password_hash, email_verified FROM identities \
         WHERE provider = ? AND email = ? LIMIT 1",
    )
    .bind(PASSWORD)
    .bind(&normalized)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let password_hash: Option<String> = row.get("password_hash");
    let Some(password_hash) = password_hash else {
        // The CHECK constraint makes this impossible for a password identity, so a
        // row in this state is corrupt rather than a client error.
        return Err(AppError::Internal(
            "a password identity has no password hash; the CHECK constraint should make this unrepresentable"
                .into(),
        ));
    };

    Ok(Some(PasswordIdentity {
        identity_id: parse_uuid(&row.get::<String, _>("id"), "identities.id")?,
        account_id: parse_uuid(&row.get::<String, _>("account_id"), "identities.account_id")?,
        password_hash,
        email_verified: row.get::<i64, _>("email_verified") == 1,
    }))
}

#[derive(Debug)]
pub struct PasswordIdentity {
    pub identity_id: Uuid,
    pub account_id: Uuid,
    pub password_hash: String,
    pub email_verified: bool,
}

/// The password identity belonging to `account_id`, when it has one.
///
/// The account-addressed sibling of [`password_identity`], which is addressed by
/// email. A password change already knows its account from the session and must NOT
/// take an address from the request body — a caller who could name the address
/// could name one on a different account.
///
/// `ORDER BY created_at ASC LIMIT 1` because an account can in principle hold more
/// than one password identity (a reset on a Google-only account creates one
/// alongside nothing, but a future merge could leave two). The oldest is the one
/// signup or the first reset created, which is the one the account holder
/// recognises; picking arbitrarily would make the answer depend on row order.
pub async fn first_password_identity(
    pool: &SqlitePool,
    account_id: Uuid,
) -> Result<Option<PasswordIdentity>, AppError> {
    let row = sqlx::query(
        "SELECT id, account_id, password_hash, email_verified FROM identities \
         WHERE provider = ? AND account_id = ? ORDER BY created_at ASC LIMIT 1",
    )
    .bind(PASSWORD)
    .bind(account_id.hyphenated())
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    let password_hash: Option<String> = row.get("password_hash");
    let Some(password_hash) = password_hash else {
        // The CHECK constraint makes this impossible for a password identity, so a
        // row in this state is corrupt rather than a client error.
        return Err(AppError::Internal(
            "a password identity has no password_hash".into(),
        ));
    };

    Ok(Some(PasswordIdentity {
        identity_id: parse_uuid(&row.get::<String, _>("id"), "identities.id")?,
        account_id: parse_uuid(&row.get::<String, _>("account_id"), "identities.account_id")?,
        password_hash,
        email_verified: row.get::<i64, _>("email_verified") != 0,
    }))
}

/// A password identity attached to `account_id`, creating one when absent.
///
/// Used by the reset path (vector 4) and by signup. It does NOT decide whether the
/// address is verified — the caller does, and `verified_at` is only stamped by the
/// redemption of a delivered token. Passing `verified = false` here is the honest
/// default for a fresh signup.
///
/// `subject` is the normalised address, not a random id. `UNIQUE (provider,
/// subject)` is the constraint that stops one address having two password
/// identities, and a random subject would sail past it.
pub async fn upsert_password_identity(
    pool: &SqlitePool,
    account_id: Uuid,
    email: &str,
    password_hash: &str,
    verified: bool,
    now: DateTime<Utc>,
) -> Result<Uuid, AppError> {
    let normalized = normalize_email(email);
    let identity_id = Uuid::new_v4();

    // `verified_at` is set to `now` ONLY when the caller says this write is itself
    // the proof. The linking rule reads that column, so defaulting it to `now`
    // would silently authorise every future link.
    let verified_at = if verified { Some(now) } else { None };
    let verified_bit: i64 = if verified { 1 } else { 0 };

    sqlx::query(
        "INSERT INTO identities \
         (id, account_id, provider, subject, email, email_verified, verified_at, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (provider, email) DO UPDATE SET \
           password_hash = excluded.password_hash, \
           updated_at = excluded.updated_at",
    )
    .bind(identity_id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(PASSWORD)
    .bind(&normalized)
    .bind(&normalized)
    .bind(verified_bit)
    .bind(verified_at)
    .bind(password_hash)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;

    // The upsert may have updated an existing row rather than inserting this id, so
    // the id is read back rather than assumed.
    let stored: String =
        sqlx::query_scalar("SELECT id FROM identities WHERE provider = ? AND email = ? LIMIT 1")
            .bind(PASSWORD)
            .bind(&normalized)
            .fetch_one(pool)
            .await?;

    parse_uuid(&stored, "identities.id")
}

/// Marks an address as PROVEN, stamping when.
///
/// The only writers are the three in [`super`]'s rule 4. Idempotent in the sense
/// that re-marking keeps the EARLIEST `verified_at`: an address proven once stays
/// proven, and moving the stamp forward would invalidate links that were authorised
/// by the earlier proof.
pub async fn mark_verified(
    pool: &SqlitePool,
    identity_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE identities SET email_verified = 1, \
           verified_at = COALESCE(verified_at, ?), updated_at = ? \
         WHERE id = ?",
    )
    .bind(now)
    .bind(now)
    .bind(identity_id.hyphenated())
    .execute(pool)
    .await?;

    Ok(())
}

/// The identity behind a reset token, if the address on it still exists.
///
/// Vector 4: a reset may land on a Google-only account, in which case there is no
/// password identity to update and one must be created with the address as its
/// subject. Returns the account so the caller can do that.
///
/// ## One address, TWO accounts — and why an ORDER BY is not optional here
///
/// `identities_provider_email_uniq` is `UNIQUE (provider, email)`, NOT
/// `UNIQUE (email)`. That shape is deliberate (see the module docs and R1): an
/// UNVERIFIED password identity and a Google identity are allowed to coexist on one
/// address, because a Google sign-in over an unverified colliding row creates a
/// SECOND account rather than adopting the attacker's. So "one address resolves to
/// two accounts" is a state this system is DESIGNED to reach, not a corruption.
///
/// This function used to be `SELECT account_id ... WHERE email = ? LIMIT 1` with no
/// ordering, which answers that state with whichever row SQLite happens to visit
/// first — rowid order, not a decision. Two things went wrong, and neither was
/// visible in a test, because every test had one identity on the address:
///
///   * `request_password_reset` mails the reset link to one arbitrary account of the
///     two, and spends the per-account rate-limit budget on that one, so the other
///     account's cap is never drawn down.
///   * `signup` treats "found" as "this address is already registered", so the
///     address became unregisterable even when the account holding it is a
///     Google-only one the password user cannot sign in to.
///
/// The ordering below makes the answer a rule rather than a scan artifact: a
/// GOOGLE identity wins, because a Google identity is always verified
/// (`CHECK (provider <> 'google' OR email_verified = 1)`) while the password row that
/// collides with it proves nothing about the caller — it is the row an attacker can
/// create for any address they like without ever proving it. When both are Google,
/// or both password, the oldest row wins so the answer cannot change under a
/// re-scan, an `UPDATE` that moves a row, or a `VACUUM`.
pub async fn account_for_email(pool: &SqlitePool, email: &str) -> Result<Option<Uuid>, AppError> {
    let normalized = normalize_email(email);

    let found: Option<String> = sqlx::query_scalar(
        "SELECT account_id FROM identities WHERE email = ? \
         ORDER BY (provider = 'google') DESC, created_at ASC, id ASC LIMIT 1",
    )
    .bind(&normalized)
    .fetch_optional(pool)
    .await?;

    match found {
        None => Ok(None),
        Some(id) => Ok(Some(parse_uuid(&id, "identities.account_id")?)),
    }
}

/// Create an account with a password identity, in one transaction.
///
/// The account row, its wallet and its identity are one unit: an account without
/// a wallet is a state every other read would have to defend against, and an
/// account whose identity insert failed is an account nobody can sign in to.
/// `BEGIN IMMEDIATE` for the reason `db.rs` gives.
///
/// The identity starts UNVERIFIED. Nothing in this function proves the address -
/// only the redemption of a token delivered to it does (module rule 4).
pub async fn create_password_account(
    pool: &SqlitePool,
    email: &str,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<Uuid, AppError> {
    let normalized = normalize_email(email);
    let account_id = Uuid::new_v4();
    let identity_id = Uuid::new_v4();

    let mut tx = crate::db::begin_immediate(pool).await?;

    create_account_row(&mut tx, account_id, now).await?;

    sqlx::query(
        "INSERT INTO identities \
         (id, account_id, provider, subject, email, email_verified, password_hash, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 0, ?, ?, ?)",
    )
    .bind(identity_id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(PASSWORD)
    .bind(&normalized)
    .bind(&normalized)
    .bind(password_hash)
    .bind(now)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(account_id)
}

/// Replace the stored hash of an existing password identity.
///
/// Used by the reset path, where the identity is already known to belong to the
/// account the token authorised. It does NOT touch `email_verified`: a reset
/// proves the mailbox was reachable at that moment, but R2 makes the verified
/// transition its own claim, and a reset is not one of the three writers rule 4
/// allows. Silently upgrading here would let a reset launder an unverified
/// address into a linkable one.
pub async fn set_password(
    pool: &SqlitePool,
    identity_id: Uuid,
    password_hash: &str,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query("UPDATE identities SET password_hash = ?, updated_at = ? WHERE id = ?")
        .bind(password_hash)
        .bind(now)
        .bind(identity_id.hyphenated())
        .execute(pool)
        .await?;

    Ok(())
}

async fn create_account_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    account_id: Uuid,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    // A wallet as well, because an account without one is a state the rest of the
    // system does not expect and would have to defend against at every read. The
    // balance starts at zero; money only ever arrives through `db.rs`'s credit path.
    sqlx::query("INSERT INTO accounts (id, created_at, updated_at) VALUES (?, ?, ?)")
        .bind(account_id.hyphenated())
        .bind(now)
        .bind(now)
        .execute(&mut **tx)
        .await?;

    sqlx::query("INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?, 0, ?)")
        .bind(account_id.hyphenated())
        .bind(now)
        .execute(&mut **tx)
        .await?;

    Ok(())
}

async fn insert_google_identity(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    identity_id: Uuid,
    account_id: Uuid,
    google_subject: &str,
    email: &str,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO identities \
         (id, account_id, provider, subject, email, email_verified, verified_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 1, ?, ?, ?)",
    )
    .bind(identity_id.hyphenated())
    .bind(account_id.hyphenated())
    .bind(GOOGLE)
    .bind(google_subject)
    .bind(email)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

fn parse_uuid(raw: &str, column: &str) -> Result<Uuid, AppError> {
    Uuid::parse_str(raw).map_err(|e| AppError::Internal(format!("{column} is not a uuid: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDb;
    use chrono::Duration;

    /// R1's refusal branch: an UNVERIFIED password identity with the same address
    /// must not be adopted. The Google sign-in gets its own account, and the
    /// attacker's row is left where it was.
    #[tokio::test]
    async fn an_unverified_collision_creates_a_fresh_account() {
        let db = TestDb::new().await;
        let now = Utc::now();

        // The attacker: a password identity on the victim's address, never verified.
        let attacker_account = crate::test_support::account(&db.pool).await;
        let attacker_identity = upsert_password_identity(
            &db.pool,
            attacker_account,
            "victim@example.com",
            "$argon2id$fake",
            false,
            now,
        )
        .await
        .expect("the attacker registers");

        let outcome =
            resolve_google_sign_in(&db.pool, "google-subject-1", "Victim@Example.com", now)
                .await
                .expect("the google sign-in resolves");

        match outcome {
            GoogleSignIn::CollisionCreated {
                account_id,
                colliding_identity_id,
            } => {
                assert_eq!(
                    colliding_identity_id, attacker_identity,
                    "the collision must name the row it refused to adopt"
                );
                assert_ne!(
                    account_id, attacker_account,
                    "the victim must NOT land in the account the attacker created"
                );
            }
            other => panic!("expected CollisionCreated, got {other:?}"),
        }

        // And the attacker's row is untouched: still unverified, still theirs.
        let verified: i64 =
            sqlx::query_scalar("SELECT email_verified FROM identities WHERE id = ?")
                .bind(attacker_identity.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("read back");
        assert_eq!(
            verified, 0,
            "the refusal must not verify the attacker's row"
        );
    }

    /// R1's adopt branch: a VERIFIED password identity is the legitimate case, and
    /// the Google identity joins that account.
    ///
    /// The verification is stamped BEFORE the sign-in, and that is not incidental:
    /// R3's ordering rule is strict (`verified_at < now`), so a fixture that used
    /// the same instant for both would be asserting the boundary case rather than
    /// the ordinary one, and would fail for the right reason.
    #[tokio::test]
    async fn a_verified_collision_is_linked() {
        let db = TestDb::new().await;
        let now = Utc::now();

        let account = crate::test_support::account(&db.pool).await;
        let identity = upsert_password_identity(
            &db.pool,
            account,
            "person@example.com",
            "$argon2id$fake",
            true,
            now - Duration::minutes(5),
        )
        .await
        .expect("the person signed up and verified");

        let outcome =
            resolve_google_sign_in(&db.pool, "google-subject-2", "person@example.com", now)
                .await
                .expect("resolve");

        match outcome {
            GoogleSignIn::Linked { account_id } => {
                assert_eq!(
                    account_id, account,
                    "the sign-in must land in the same account"
                );
                // The adopted identity is no longer returned on this variant, because
                // nothing read it. Its presence is asserted where it matters instead:
                // `identity` is still the password identity the account can use, and
                // the assertions below in this module drive that path.
                assert_eq!(
                    password_identity(&db.pool, "person@example.com")
                        .await
                        .expect("lookup")
                        .map(|i| i.identity_id),
                    Some(identity),
                    "the password identity the Google sign-in adopted must still be the \
                     account's password identity"
                );
            }
            other => panic!("expected Linked, got {other:?}"),
        }
    }

    /// R3: the ORDER is the rule. A password identity verified AFTER the Google
    /// identity exists must not retro-authorise the link.
    #[tokio::test]
    async fn a_verification_that_postdates_the_google_identity_does_not_authorise_a_link() {
        let db = TestDb::new().await;
        let now = Utc::now();

        let attacker_account = crate::test_support::account(&db.pool).await;
        // Registered BEFORE the Google sign-in, but its verification is stamped as
        // happening AFTER it. That is exactly the late-verification attack.
        upsert_password_identity(
            &db.pool,
            attacker_account,
            "victim2@example.com",
            "$argon2id$fake",
            true,
            now + Duration::minutes(5),
        )
        .await
        .expect("register");

        let outcome =
            resolve_google_sign_in(&db.pool, "google-subject-3", "victim2@example.com", now)
                .await
                .expect("resolve");

        assert!(
            matches!(outcome, GoogleSignIn::CollisionCreated { .. }),
            "a verification stamped after the google identity must not authorise the link, got {outcome:?}"
        );
    }

    /// The ordinary repeat sign-in: same subject, second time. It must resolve to
    /// the existing identity and NOT re-run the address logic at all.
    #[tokio::test]
    async fn a_repeat_sign_in_resolves_on_the_subject() {
        let db = TestDb::new().await;
        let now = Utc::now();

        let first = resolve_google_sign_in(&db.pool, "google-subject-4", "repeat@example.com", now)
            .await
            .expect("first");
        let account_id = match first {
            GoogleSignIn::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        // The address CHANGED at Google - which is exactly why resolution is on the
        // subject. The second sign-in must still reach the same account.
        let second =
            resolve_google_sign_in(&db.pool, "google-subject-4", "renamed@example.com", now)
                .await
                .expect("second");

        match second {
            GoogleSignIn::Existing(id) => assert_eq!(
                id, account_id,
                "a changed address must not move the person to a new account"
            ),
            other => panic!("expected Existing, got {other:?}"),
        }
    }

    /// A brand new address creates an account with a wallet, because the rest of
    /// the system assumes every account has one.
    #[tokio::test]
    async fn a_new_sign_in_creates_an_account_with_a_wallet() {
        let db = TestDb::new().await;
        let now = Utc::now();

        let outcome = resolve_google_sign_in(&db.pool, "google-subject-5", "new@example.com", now)
            .await
            .expect("resolve");

        let account_id = match outcome {
            GoogleSignIn::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("a wallet must exist");
        assert_eq!(balance, 0, "a new account starts empty");
    }

    /// The Google identity is stored verified, because Google proved the address —
    /// and the schema would reject it otherwise.
    #[tokio::test]
    async fn a_google_identity_is_stored_verified() {
        let db = TestDb::new().await;
        let now = Utc::now();

        resolve_google_sign_in(&db.pool, "google-subject-6", "proven@example.com", now)
            .await
            .expect("resolve");

        let (bit, verified_at): (i64, Option<DateTime<Utc>>) = {
            let row = sqlx::query(
                "SELECT email_verified, verified_at FROM identities WHERE provider = ? AND subject = ?",
            )
            .bind(GOOGLE)
            .bind("google-subject-6")
            .fetch_one(&db.pool)
            .await
            .expect("read back");
            (row.get("email_verified"), row.get("verified_at"))
        };

        assert_eq!(bit, 1, "a google identity is always verified");
        assert!(
            verified_at.is_some(),
            "the stamp must be set, or a later link decision cannot order against it"
        );
    }

    /// The normalisation rule, pinned: lowercase, trimmed, nothing else. A test
    /// that asserted more would encode a provider rule this code does not have.
    #[test]
    fn an_address_is_lowercased_and_trimmed_and_left_otherwise_alone() {
        assert_eq!(
            normalize_email("  MixedCase@Example.COM "),
            "mixedcase@example.com"
        );
        assert_eq!(
            normalize_email("dots.are.kept@example.com"),
            "dots.are.kept@example.com",
            "dot-stripping is a Gmail rule, not a rule of this system"
        );
        assert_eq!(
            normalize_email("tag+kept@example.com"),
            "tag+kept@example.com",
            "a plus tag is part of the address as the provider sees it"
        );
    }

    #[tokio::test]
    async fn an_account_is_findable_by_address_for_a_google_only_signup() {
        let db = TestDb::new().await;
        let now = Utc::now();

        let outcome =
            resolve_google_sign_in(&db.pool, "google-subject-7", "googleonly@example.com", now)
                .await
                .expect("resolve");
        let account_id = match outcome {
            GoogleSignIn::Created(id) => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let found = account_for_email(&db.pool, "GOOGLEONLY@Example.com")
            .await
            .expect("lookup");
        assert_eq!(
            found,
            Some(account_id),
            "a reset for a google-only account must find it, or vector 4's decision is unimplementable"
        );

        assert_eq!(
            account_for_email(&db.pool, "nobody@example.com")
                .await
                .expect("lookup"),
            None,
            "and an unknown address finds nothing"
        );
    }

    /// One address on TWO accounts is a state this schema is DESIGNED to allow
    /// (`UNIQUE (provider, email)`, not `UNIQUE (email)`), and the lookup must answer
    /// it by a RULE rather than by whichever row SQLite scans first.
    ///
    /// The defect this pins: `account_for_email` was `... WHERE email = ? LIMIT 1`
    /// with no ORDER BY, so the answer depended on rowid order - which changed with
    /// insertion order, and could change again under a VACUUM. Both callers act on
    /// the answer: password-reset mails a link to one of the two accounts and bills
    /// the rate limit to it, and signup reads "found" as "already registered".
    ///
    /// WHY NO EXISTING TEST COULD SEE IT: every test in this file seeded ONE identity
    /// on the address, and the fixture that built the collision case built it through
    /// `resolve_google_sign_in`, which reads the same address back. A fixture that
    /// supplies one row cannot observe a defect about which of two rows is chosen.
    ///
    /// A GOOGLE identity must win, because it is always verified while the colliding
    /// password row proves nothing about the caller - it is the row anybody can
    /// create for any address without ever proving it (R1). The test asserts the
    /// answer both ways round, because the pre-fix code returned the FIRST-INSERTED
    /// row: seeding the password row first is what made the wrong answer look right.
    #[tokio::test]
    async fn an_address_on_two_accounts_resolves_to_the_verified_identity() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let email = "both-providers@example.com";

        // Seed the PASSWORD row first, so a scan-order answer would pick it.
        let password_account = crate::test_support::account(&db.pool).await;
        upsert_password_identity(
            &db.pool,
            password_account,
            email,
            "$argon2id$colliding",
            false,
            now,
        )
        .await
        .expect("password identity");

        // An UNVERIFIED colliding row: Google creates a SECOND account (R1) rather
        // than adopting, so this really does leave two accounts on one address.
        let google_account = match resolve_google_sign_in(&db.pool, "collide-sub", email, now)
            .await
            .expect("google")
        {
            GoogleSignIn::CollisionCreated { account_id, .. } => account_id,
            other => panic!("expected CollisionCreated, got {other:?}"),
        };
        assert_ne!(
            google_account, password_account,
            "the collision must produce a second account, or this test is not seeding the state"
        );

        // The rows really do share the address: this is the state, not a mock.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM identities WHERE email = ?")
            .bind(email)
            .fetch_one(&db.pool)
            .await
            .expect("count");
        assert_eq!(rows, 2, "two identities on one address is the premise");

        assert_eq!(
            account_for_email(&db.pool, email).await.expect("lookup"),
            Some(google_account),
            "the GOOGLE identity must win: it is verified by CHECK constraint, while \
             the colliding password row proves nothing about whoever is asking"
        );

        // And it must be the same answer when asked again, rather than a scan artifact.
        for _ in 0..3 {
            assert_eq!(
                account_for_email(&db.pool, email).await.expect("lookup"),
                Some(google_account),
                "the answer must not wobble between calls"
            );
        }
    }

    /// The password identity's subject is the address, so the UNIQUE constraint on
    /// (provider, subject) actually stops a second password identity for one
    /// address. A random subject would silently allow it.
    #[tokio::test]
    async fn a_second_password_identity_for_one_address_reuses_the_row() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let account = crate::test_support::account(&db.pool).await;

        let first = upsert_password_identity(
            &db.pool,
            account,
            "one@example.com",
            "$argon2id$first",
            false,
            now,
        )
        .await
        .expect("first");
        let second = upsert_password_identity(
            &db.pool,
            account,
            "ONE@example.com",
            "$argon2id$second",
            false,
            now,
        )
        .await
        .expect("second");

        assert_eq!(first, second, "the two spellings are one identity");

        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM identities WHERE provider = ? AND email = ?")
                .bind(PASSWORD)
                .bind("one@example.com")
                .fetch_one(&db.pool)
                .await
                .expect("count");
        assert_eq!(count, 1, "one address, one password identity");
    }

    /// `mark_verified` keeps the EARLIEST stamp. Moving it forward would invalidate
    /// links that the earlier proof authorised.
    #[tokio::test]
    async fn marking_verified_keeps_the_earliest_stamp() {
        let db = TestDb::new().await;
        let earlier = Utc::now();
        let later = earlier + Duration::hours(2);

        let account = crate::test_support::account(&db.pool).await;
        let identity = upsert_password_identity(
            &db.pool,
            account,
            "stamp@example.com",
            "$argon2id$fake",
            false,
            earlier,
        )
        .await
        .expect("create");

        mark_verified(&db.pool, identity, later)
            .await
            .expect("mark");
        mark_verified(&db.pool, identity, later + Duration::hours(1))
            .await
            .expect("mark again");

        let verified_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT verified_at FROM identities WHERE id = ?")
                .bind(identity.hyphenated())
                .fetch_one(&db.pool)
                .await
                .expect("read back");

        assert_eq!(
            verified_at,
            Some(later),
            "the second mark must not move the stamp forward"
        );
    }

    /// A password sign-in lookup returns the hash and the verified bit, and an
    /// unknown address returns None rather than an error.
    #[tokio::test]
    async fn a_password_identity_is_found_by_address() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let account = crate::test_support::account(&db.pool).await;

        upsert_password_identity(
            &db.pool,
            account,
            "signin@example.com",
            "$argon2id$stored",
            true,
            now,
        )
        .await
        .expect("create");

        let found = password_identity(&db.pool, "SignIn@Example.com")
            .await
            .expect("lookup")
            .expect("the identity exists");
        assert_eq!(found.account_id, account);
        assert_eq!(found.password_hash, "$argon2id$stored");
        assert!(found.email_verified);

        assert!(
            password_identity(&db.pool, "missing@example.com")
                .await
                .expect("lookup")
                .is_none(),
            "an address with no password identity must be None, not an error"
        );
    }
}
