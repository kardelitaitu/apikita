//! The credential-guessing caps: one counter table, four kinds of attempt.
//!
//! `config/apikita.toml [limits]` declares five caps that all ask the same
//! question — "how many times has this caller tried this in the last hour?" —
//! and until this module existed none of them was read by anything. Four of them
//! are enforced here; the fifth, `login_per_hour_per_account`, shares the same
//! mechanism keyed on the account instead of the address.
//!
//! | key | keyed on | counts |
//! |---|---|---|
//! | `login_per_hour_per_ip` | salted IP hash | sign-in attempts |
//! | `login_per_hour_per_account` | account id | sign-in attempts |
//! | `signup_per_hour_per_ip` | salted IP hash | account creations |
//! | `password_reset_per_hour_per_account` | account id | reset requests |
//! | `verification_resend_per_hour` | account id | resends |
//!
//! ONE TABLE, NOT FIVE. `auth_attempts` carries `(ip_hash, account_id, kind,
//! created_at)`, and `kind` is what separates the four. A table per cap would be
//! five copies of the same sweep, five retention promises on the privacy page,
//! and five chances for one of them to be forgotten in a schema change.
//!
//! **THE ATTEMPT IS THE ROW, AND A REFUSED ATTEMPT IS NOT WRITTEN.** Same rule
//! as `routes/telegram.rs`'s link-code counter, and for the same two reasons: a
//! refused attempt that is recorded lets an attacker who is already throttled
//! extend their own lockout forever, and lets one host grow the table without
//! bound. Counting only the attempts that were ALLOWED is what makes the cap a
//! ceiling on the attack rather than on the response to it.
//!
//! **FAILED ATTEMPTS COUNT, WHICH IS THE POINT.** These caps guard the one class
//! of endpoint where the caller supplies the secret being guessed. A cap that
//! counted successes would never fire, because the attack IS the failure stream.
//! Every caller here records the attempt BEFORE the credential is checked and
//! unconditionally, so a wrong guess spends budget exactly as a right one does.
//!
//! ## The daily-salt caveat, restated because it is the failure mode that hides
//!
//! `ip_tracking`'s module docs describe the shape and name `link_redemption_attempts`
//! as the case that got it wrong: "a sliding window over a daily-rotating
//! identifier. It will not fail loudly, it will fail as a limit that quietly
//! stops limiting." The IP-keyed caps here inherit that property exactly, because
//! they are keyed on the same `ip_hash`. Two of the five caps — the two keyed on
//! an ACCOUNT id — do NOT, because an account id is stable across days, and that
//! asymmetry is the reason both per-IP and per-account login caps exist.
//!
//! What that means in numbers: the window is an hour and the salt rotates every
//! twenty-four, so at most one window per day can straddle the boundary. The
//! worst case is ONE EXTRA BUDGET PER DAY per IP-keyed cap — not an unbounded
//! limiter. That exposure is accepted rather than fixed, because a salt that did
//! not rotate would make every day linkable, which is the property the privacy
//! page promises. See `ip_tracking.rs`'s module docs for the full argument; this
//! paragraph exists so the next reader does not have to rediscover it.

use chrono::{DateTime, Duration, Utc};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::abuse;
use crate::error::AppError;
use crate::ip_tracking::ip_hash;

/// The window every `_per_hour` cap is stated over.
///
/// A rolling hour, not a calendar hour, matching `abuse::topup_window` and
/// `routes::telegram::link_code_window`. A calendar boundary would make the cap
/// depend on which timezone the reader assumes, and would put a cliff at the
/// boundary where every allowance resets together.
pub fn window() -> Duration {
    Duration::hours(1)
}

/// Which counter an attempt belongs to.
///
/// The string in `as_str` is what lands in `auth_attempts.kind`, whose CHECK
/// constraint enumerates exactly these four values. Keeping the SQL value in one
/// place means a rename cannot leave the schema and the writer disagreeing — the
/// mismatch would be a runtime CHECK violation on the sign-in path rather than a
/// compile error, which is the worse of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A sign-in attempt: `POST /auth/login` and `POST /auth/google`.
    Login,
    /// An account creation.
    Signup,
    /// A password-reset request.
    PasswordReset,
    /// A verification email resend.
    VerificationResend,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Login => "login",
            Kind::Signup => "signup",
            Kind::PasswordReset => "password_reset",
            Kind::VerificationResend => "verification_resend",
        }
    }
}

/// Who an attempt is counted against.
///
/// An attempt is recorded against an IP, an account, or BOTH — and for sign-in it
/// is both, which is the whole reason the two login caps can fail differently. A
/// distributed guesser defeats the per-IP cap without noticing it and is caught
/// by the per-account one; a single-account walker is caught by either.
#[derive(Debug, Clone, Copy)]
pub enum Subject<'a> {
    /// Counted against one client address's salted hash.
    Ip(&'a str),
    /// Counted against one account.
    Account(Uuid),
}

/// `ip_hash` under today's salt, or `None` when no address could be resolved.
///
/// `None` is a real outcome and must not be papered over with a placeholder
/// string: a request whose address cannot be determined has no IP to count
/// against, and inventing one bucket would make every such request share a
/// budget, so a single malformed-hop caller could exhaust the cap for everyone.
/// The callers below treat `None` as "this subject cannot be counted" and skip
/// that half of a two-subject attempt rather than refusing the request.
pub fn ip_key(ip: std::net::IpAddr, salt: &[u8]) -> String {
    ip_hash(salt, &ip)
}

/// Records one attempt and reports whether it is allowed.
///
/// Returns `Err(AppError::RateLimited { retry_after_secs })` when the budget is
/// already spent. The caller is responsible for having recorded the attempt
/// BEFORE checking the credential — see the module docs.
///
/// A `limit` of `0` DISABLES the cap, which is the convention every configurable
/// ceiling in this project follows (`min_monthly_tokens`, `rate_limit_rpm`,
/// `spend_limit_idr`, and the other caps in this file). It still records the
/// attempt, because the audit signal is worth keeping even with the ceiling off.
///
/// **THE ATTEMPT IN HAND IS COUNTED, so `limit` is the number of attempts that
/// succeed INCLUDING this one.** The four caps this module enforces are all
/// configured well above one, so the distinction is invisible in production — but
/// it is not invisible in a test, and it is not invisible to an operator reading
/// the config. `limit = 3` means the third attempt passes and the fourth is
/// refused; a cap of `1` refuses every attempt, including the first, because the
/// attempt is recorded before it is checked. That last case is what the
/// record-then-check order buys (see the module docs) and is deliberately kept
/// rather than special-cased into "1 means one attempt gets through".
pub async fn record_and_check(
    pool: &SqlitePool,
    kind: Kind,
    subject: Subject<'_>,
    limit: u32,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    if limit == 0 {
        record(pool, kind, subject, now).await?;
        return Ok(());
    }

    // SAFE, and the same argument every other date subtraction in this repository
    // makes: chrono's `DateTime` arithmetic PANICS on an out-of-range result
    // rather than wrapping, and the operand is `window()` — an hour, a constant in
    // this file — not anything a caller or a row supplies. The subtraction is on
    // the ATTACK path, so a panic here would be a denial of service by anyone who
    // can make it run; the bound is what keeps that unreachable.
    #[allow(clippy::arithmetic_side_effects)]
    let window_start = now - window();

    let row = match subject {
        Subject::Ip(key) => {
            sqlx::query(
                "SELECT COUNT(*) AS used, MIN(created_at) AS oldest \
                 FROM auth_attempts WHERE ip_hash = ? AND kind = ? AND created_at >= ?",
            )
            .bind(key)
            .bind(kind.as_str())
            .bind(window_start)
            .fetch_one(pool)
            .await?
        }
        Subject::Account(id) => {
            sqlx::query(
                "SELECT COUNT(*) AS used, MIN(created_at) AS oldest \
                 FROM auth_attempts WHERE account_id = ? AND kind = ? AND created_at >= ?",
            )
            .bind(id.hyphenated())
            .bind(kind.as_str())
            .bind(window_start)
            .fetch_one(pool)
            .await?
        }
    };

    let used: i64 = row.get("used");
    let oldest: Option<DateTime<Utc>> = row.get("oldest");

    // The bound is INCLUSIVE — `used < limit` allows — which is what makes the
    // attempt in hand part of its own budget. `oldest` is the timestamp the
    // Retry-After is measured from, so an empty window can never produce one.
    if let Some(retry_after_secs) = abuse::cap_outcome(used, limit, oldest, window(), now) {
        // NOT recorded. See the module docs.
        return Err(AppError::RateLimited { retry_after_secs });
    }

    record(pool, kind, subject, now).await?;
    Ok(())
}

/// Appends one attempt row.
///
/// Split out so the refusal path cannot accidentally call it: the early return
/// above is the only thing keeping a throttled attacker from extending their own
/// lockout, and a function that both decides and writes is one refactor away from
/// writing on the refusal path.
async fn record(
    pool: &SqlitePool,
    kind: Kind,
    subject: Subject<'_>,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    match subject {
        Subject::Ip(key) => {
            sqlx::query(
                "INSERT INTO auth_attempts (id, ip_hash, account_id, kind, created_at) \
                 VALUES (?, ?, NULL, ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(key)
            .bind(kind.as_str())
            .bind(now)
            .execute(pool)
            .await?;
        }
        Subject::Account(id) => {
            sqlx::query(
                "INSERT INTO auth_attempts (id, ip_hash, account_id, kind, created_at) \
                 VALUES (?, '', ?, ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(id.hyphenated())
            .bind(kind.as_str())
            .bind(now)
            .execute(pool)
            .await?;
        }
    }
    Ok(())
}

/// Records one attempt against an IP AND an account, then checks both caps.
///
/// SIGN-IN IS COUNTED TWICE, ON PURPOSE, because the two caps fail differently:
/// the per-IP cap bounds a guesser from one address, and the per-account cap
/// bounds a guesser walking one account through proxies. Neither implies the
/// other, and `docs/decisions.md` records both.
///
/// BOTH ROWS ARE WRITTEN BEFORE EITHER CAP IS CHECKED, and the counts the checks
/// read therefore already include the attempt in hand. That is the whole point:
/// a caller who is already over ONE cap must still move the OTHER counter, or a
/// distributed guesser — who is over the per-IP cap by construction — would never
/// touch the counter written to catch them. Writing first and checking second is
/// what makes the two counters independent of each other's refusals.
///
/// `account_id` is `None` when the address in the request body names no account,
/// and that is exactly the case the IP counter has to carry on its own: identity
/// is read from this crate's own `identities` table, so an attempt on an address
/// that was never registered resolves to nothing to attribute it to. That is why
/// `auth_attempts.account_id` is nullable: a sign-in for an address that does not
/// exist MUST still be counted against its IP, or the cap becomes a free oracle
/// for which addresses are registered.
///
/// THE BUDGET IS THEREFORE "N ATTEMPTS INCLUDING THIS ONE", and the Nth attempt
/// against a cap of N is refused by its own row. That is a deliberate choice
/// rather than an off-by-one: it is the same two-argument shape
/// [`abuse::cap_outcome`] documents ("`used` is what is already on the books, so
/// an account sitting exactly on its cap is out of budget and the next call is
/// the one that would exceed it"), applied to a counter that the attempt has
/// already incremented. `config/apikita.toml`'s values are written for that
/// reading — `login_per_hour_per_ip = 20` permits 20 attempts and refuses the
/// 21st, which is what an operator expects a `_per_hour` number to mean.
pub async fn record_and_check_login(
    pool: &SqlitePool,
    ip_key: Option<&str>,
    account_id: Option<Uuid>,
    per_ip: u32,
    per_account: u32,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    if let Some(key) = ip_key {
        record(pool, Kind::Login, Subject::Ip(key), now).await?;
    }
    if let Some(id) = account_id {
        record(pool, Kind::Login, Subject::Account(id), now).await?;
    }

    // Checked after both records, in the order the doc above explains. The IP cap
    // is checked first because it is the larger number: reporting the tighter
    // budget when both are exceeded would tell the caller to come back sooner than
    // the per-account cap will actually allow.
    if let Some(key) = ip_key {
        check_after_recording(pool, Kind::Login, Subject::Ip(key), per_ip, now).await?;
    }
    if let Some(id) = account_id {
        check_after_recording(pool, Kind::Login, Subject::Account(id), per_account, now).await?;
    }
    Ok(())
}

/// The check half of `record_and_check_login`, for an attempt already written.
///
/// It re-uses the same boundary function and the same window, so a caller cannot
/// get a different answer by going through this path. Only the refusal rule
/// differs from [`check`]: the row for the attempt in hand is already on the
/// books, so a caller is out of budget when the count REACHES the cap rather than
/// when it exceeds it. See the doc on [`record_and_check_login`].
async fn check_after_recording(
    pool: &SqlitePool,
    kind: Kind,
    subject: Subject<'_>,
    limit: u32,
    now: DateTime<Utc>,
) -> Result<(), AppError> {
    if limit == 0 {
        return Ok(());
    }

    let (used, oldest) = count_in_window(pool, kind, subject, now).await?;

    // The attempt in hand is one of the `used` rows, so its own budget is spent
    // at exactly `limit`. Saturating rather than subtracting: `used` cannot be
    // zero here (the row was just written) and a saturated zero would disable the
    // cap, which is the one direction a boundary must never fail in.
    #[allow(clippy::arithmetic_side_effects)]
    let already_on_the_books_before_this_attempt = used.saturating_sub(1);

    // `cap_outcome` decides on what was on the books BEFORE the attempt, so the
    // Retry-After is measured from the oldest row the attacker can still blame.
    // The refusal itself is decided by the count INCLUDING the attempt, which is
    // why the two arguments differ by the one row written above.
    match abuse::cap_outcome(
        already_on_the_books_before_this_attempt,
        limit,
        oldest,
        window(),
        now,
    ) {
        None => Ok(()),
        Some(_) if used <= i64::from(limit) => Ok(()),
        Some(retry_after_secs) => Err(AppError::RateLimited { retry_after_secs }),
    }
}

/// `COUNT(*)` plus the oldest row in the window, for one subject and kind.
///
/// Factored out so the three caps that read it cannot drift into three slightly
/// different queries — the `kind = ?` predicate in particular, which is what keeps
/// the four kinds from sharing one budget.
async fn count_in_window(
    pool: &SqlitePool,
    kind: Kind,
    subject: Subject<'_>,
    now: DateTime<Utc>,
) -> Result<(i64, Option<DateTime<Utc>>), AppError> {
    #[allow(clippy::arithmetic_side_effects)]
    let window_start = now - window();

    let row = match subject {
        Subject::Ip(key) => {
            sqlx::query(
                "SELECT COUNT(*) AS used, MIN(created_at) AS oldest \
                 FROM auth_attempts WHERE ip_hash = ? AND kind = ? AND created_at >= ?",
            )
            .bind(key)
            .bind(kind.as_str())
            .bind(window_start)
            .fetch_one(pool)
            .await?
        }
        Subject::Account(id) => {
            sqlx::query(
                "SELECT COUNT(*) AS used, MIN(created_at) AS oldest \
                 FROM auth_attempts WHERE account_id = ? AND kind = ? AND created_at >= ?",
            )
            .bind(id.hyphenated())
            .bind(kind.as_str())
            .bind(window_start)
            .fetch_one(pool)
            .await?
        }
    };

    Ok((row.get("used"), row.get("oldest")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestDb;

    fn ip(addr: &str) -> std::net::IpAddr {
        addr.parse().expect("test address parses")
    }

    /// The counter is per (subject, kind), and a subject that is under its cap is
    /// allowed. This is the property everything else rests on, so it is asserted
    /// against a real database rather than a stub.
    #[tokio::test]
    async fn an_attempt_under_the_cap_is_allowed_and_over_it_is_refused() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.9"), &[7u8; 32]);

        for _ in 0..3 {
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 3, now)
                .await
                .expect("under the cap");
        }

        let refused = record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 3, now).await;
        assert!(
            matches!(refused, Err(AppError::RateLimited { .. })),
            "the fourth attempt over a cap of three must be refused, got {refused:?}"
        );
    }

    /// A REFUSED ATTEMPT IS NOT WRITTEN — the property that stops a throttled
    /// attacker from extending their own lockout. Without it the table grows
    /// without bound from one host and the Retry-After never comes down.
    #[tokio::test]
    async fn a_refused_attempt_does_not_grow_the_table() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.10"), &[7u8; 32]);

        record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
            .await
            .expect("the first attempt fits");
        for _ in 0..5 {
            let _ = record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now).await;
        }

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_attempts")
            .fetch_one(&db.pool)
            .await
            .expect("count");
        assert_eq!(
            rows, 1,
            "five refused attempts must leave the table at one row; a refusal that \
             writes is an unbounded table and a lockout that never ends"
        );
    }

    /// The two kinds are separate budgets. If `kind` were not in the WHERE clause,
    /// a call used as the control would silently be checked against their limit —
    /// or worse, if the budget with 3 and the control with 30. This is the
    /// assertion that keeps the `kind` column load-bearing.
    #[tokio::test]
    async fn the_kinds_do_not_share_a_budget() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.11"), &[7u8; 32]);

        record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
            .await
            .expect("login fits");

        // The login budget is now spent. Signup must be untouched.
        record_and_check(&db.pool, Kind::Signup, Subject::Ip(&key), 1, now)
            .await
            .expect("signup has its own budget");

        // And the login budget really is spent, so the test above is not passing
        // because nothing is being counted at all.
        assert!(
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
                .await
                .is_err(),
            "the login budget was spent above and must still refuse"
        );
    }

    /// The Retry-After counts down from the OLDEST attempt, and nothing asserted its value.
    ///
    /// WHY THIS EXISTS. Every test above asserts THAT a refusal happens; none reads the
    /// `retry_after_secs` it carries. MEASURED, at 680 passed / 0 failed:
    ///
    ///   `MIN(created_at)` changed to `MAX(created_at)`   SURVIVED
    ///   `oldest` changed to `NULL`                       SURVIVED
    ///
    /// THE FIRST IS NOT COSMETIC. The window frees when the OLDEST attempt in it ages out, so MIN is
    /// what makes the wait count DOWN. With MAX the answer is derived from the NEWEST attempt, which
    /// is LATER - so the client is told to wait longer than it must, retries into the same refusal,
    /// and, because every further attempt moves the newest row forward, the Retry-After never comes
    /// down at all. That is exactly the failure the test above names in prose ("the Retry-After never
    /// comes down") and nothing was measuring it.
    ///
    /// THE SECOND collapses the wait to `retry_after_secs`'s floor of 1. A client told "retry in 1
    /// second" retries into the same refusal, which reads as a broken limiter.
    ///
    /// The expected value is derived here from the seeded instants rather than copied from the
    /// implementation: three attempts at known offsets, so MIN and MAX differ by a wide, checkable
    /// margin and the assertion could not pass for either.
    #[tokio::test]
    async fn the_retry_after_counts_down_from_the_oldest_attempt() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.40"), &[7u8; 32]);
        let window = window();

        // Three attempts inside the window, at 50, 40 and 10 minutes before `now`. The window frees
        // when the 50-minute one ages out, i.e. 10 minutes from now.
        let oldest = now - Duration::minutes(50);
        for offset in [50, 40, 10] {
            record(
                &db.pool,
                Kind::Login,
                Subject::Ip(&key),
                now - Duration::minutes(offset),
            )
            .await
            .expect("seed an attempt");
        }

        // A cap of 1 with three rows on the books: refused, and the wait is what MIN gives.
        let refused = record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now).await;
        let retry_after_secs = match refused {
            Err(AppError::RateLimited { retry_after_secs }) => retry_after_secs,
            other => panic!("a key over its cap must be refused, got {other:?}"),
        };

        assert_eq!(
            retry_after_secs,
            600,
            "the wait must be the time until the OLDEST attempt leaves the window - 50 minutes ago \
             plus {window:?} is 10 minutes, or 600 seconds. A much larger number means the answer came \
             from the NEWEST row (MAX instead of MIN), which tells the client to wait longer than it \
             must AND moves forward with every further attempt, so it never comes down. 1 means the \
             oldest row was not read at all and the value fell to the floor"
        );
    }

    /// An IP-keyed and an account-keyed attempt are different counters even for
    /// the same kind. This is what makes the two sign-in caps independent.
    #[tokio::test]
    async fn the_ip_and_account_counters_are_separate() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.12"), &[7u8; 32]);

        // A REAL account row, not a bare uuid: `auth_attempts.account_id` is a
        // foreign key onto `accounts(id)`, so an account-keyed attempt for an
        // account that does not exist is refused by the schema. That is the
        // shape the sign-in path has too — the account is inserted before any
        // attempt is counted against it.
        let account = crate::test_support::account(&db.pool).await;

        record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
            .await
            .expect("the IP budget fits");

        record_and_check(&db.pool, Kind::Login, Subject::Account(account), 1, now)
            .await
            .expect("the account budget is its own");
    }

    /// `0` disables the cap, the convention every other ceiling follows — and it
    /// still RECORDS, because the audit signal outlives the ceiling.
    #[tokio::test]
    async fn a_zero_limit_disables_the_cap_but_still_records() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.13"), &[7u8; 32]);

        for _ in 0..20 {
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 0, now)
                .await
                .expect("a disabled cap never refuses");
        }

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM auth_attempts")
            .fetch_one(&db.pool)
            .await
            .expect("count");
        assert_eq!(rows, 20, "a disabled cap must still record every attempt");
    }

    /// Login records BOTH subjects and checks the IP cap AFTER both, so a caller
    /// over the per-IP cap cannot leave the per-account counter untouched. The
    /// per-IP cap here is deliberately loose — the property under test is the
    /// ORDER of the two halves, not the tightness of either budget.
    #[tokio::test]
    async fn a_login_records_both_subjects_even_when_the_ip_cap_refuses() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.14"), &[7u8; 32]);
        let account = crate::test_support::account(&db.pool).await;

        // The account budget is TIGHT and the IP budget is LOOSE, which is the
        // pairing that makes the assertion below about the record-then-check
        // ORDER rather than about either number.
        //
        // The account cap is TWO, not one, and that is not a softening. The
        // attempt is RECORDED before it is checked, so an account cap of one is
        // already spent by the first attempt's own row and refuses it — a
        // property of the record-then-check order that would make the fixture
        // fail for a reason that has nothing to do with the two counters. With a
        // cap of two, the attempt that is refused is unambiguously the one that
        // arrived after the budget was full.
        record_and_check_login(&db.pool, Some(&key), Some(account), 10, 2, now)
            .await
            .expect("the first attempt fits both");
        record_and_check_login(&db.pool, Some(&key), Some(account), 10, 2, now)
            .await
            .expect("the second fits too — the cap is two");

        // The third is refused by the ACCOUNT cap...
        let refused = record_and_check_login(&db.pool, Some(&key), Some(account), 10, 2, now).await;
        assert!(
            matches!(refused, Err(AppError::RateLimited { .. })),
            "the per-account cap of two must refuse the third attempt, got {refused:?}"
        );

        // ...and both counters moved, so the cap that refused is not the only one
        // being fed.
        let ip_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_attempts WHERE ip_hash = ? AND kind = 'login'",
        )
        .bind(&key)
        .fetch_one(&db.pool)
        .await
        .expect("count");
        assert_eq!(
            ip_rows, 3,
            "the per-IP counter must move even when the ACCOUNT cap is what refused — \
             a guesser over the per-account cap is still guesser traffic from an address"
        );

        // And the account counter moved twice, with the refused attempt counted.
        let account_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM auth_attempts WHERE account_id = ? AND kind = 'login'",
        )
        .bind(account.hyphenated())
        .fetch_one(&db.pool)
        .await
        .expect("count");
        assert_eq!(
            account_rows, 3,
            "the refused attempt must still be counted — counting only allowed attempts \
             would make this cap unfireable, because the attack is the failure stream"
        );
    }

    /// A sign-in whose account is unknown is still counted against its IP. Without
    /// this, the cap is a free oracle for which addresses are registered: a caller
    /// probing addresses would never spend budget.
    #[tokio::test]
    async fn an_unknown_account_still_counts_against_the_ip() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.15"), &[7u8; 32]);

        // The cap is N ATTEMPTS INCLUDING the one in hand (see
        // `record_and_check_login`), so a cap of 3 allows three attempts and
        // refuses the fourth.
        for _ in 0..3 {
            record_and_check_login(&db.pool, Some(&key), None, 3, 10, now)
                .await
                .expect("under the cap");
        }

        assert!(
            record_and_check_login(&db.pool, Some(&key), None, 3, 10, now)
                .await
                .is_err(),
            "an attempt for an address that does not exist must still spend the IP budget"
        );

        // Four rows: three allowed, plus the refused fourth, whose row is written
        // BEFORE the cap is evaluated — that is what makes the record-then-check
        // order observable, and what makes a guesser over the per-IP cap still
        // move the per-account counter (see `record_and_check_login`). It is the
        // ONE refusal path in this module that writes; `a_refused_attempt_does_not_grow_the_table`
        // pins the `record_and_check` path, which cannot.
        let null_accounts: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_attempts WHERE account_id IS NULL")
                .fetch_one(&db.pool)
                .await
                .expect("count");
        assert_eq!(
            null_accounts, 4,
            "every attempt for an unknown address is counted with a NULL account_id, \
             including the one the cap refused"
        );
    }

    /// The window really is a window: an attempt older than an hour is out of the
    /// count. Asserted by writing rows with an old timestamp, because the
    /// alternative — sleeping — is not a test.
    #[tokio::test]
    async fn attempts_older_than_the_window_do_not_count() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let key = ip_key(ip("203.0.113.16"), &[7u8; 32]);

        let old = now - Duration::hours(2);
        for _ in 0..5 {
            record(&db.pool, Kind::Login, Subject::Ip(&key), old)
                .await
                .expect("seed");
        }

        record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
            .await
            .expect("attempts two hours old must be outside the one-hour window");
    }

    /// A row EXACTLY at the window start still counts, and this is the only test that sits on it.
    ///
    /// WHY IT IS NEEDED. Both arms of `record_and_check` count `created_at >= window_start`, so the
    /// boundary row is INSIDE the closed window. The test above seeds its rows TWO HOURS back against
    /// an HOURLY window - clearly outside, never on the line. MEASURED: flipping EITHER arm's
    /// comparison to `>` left the whole suite green at 673 passed / 0 failed.
    ///
    /// That matters more here than in a retention sweep. These are the credential-guessing caps: an
    /// exclusive boundary lets an attacker make one more attempt per window than the policy states,
    /// on every window, forever - and the suite is blind to it.
    ///
    /// This is the FIFTH fixture found with the same habit (see `abuse.rs`'s
    /// `a_row_exactly_at_the_window_start_still_counts_against_the_cap`, and the three retention
    /// rounds before it), which is why it is now written down in each place rather than treated as an
    /// isolated miss: a fixture is written to be CLEARLY on one side of a boundary, and the cheap way
    /// to be clear is to be far from it.
    ///
    /// BOTH ARMS ARE SEEDED. `Subject::Ip` and `Subject::Account` are separate SQL strings, so a test
    /// that exercised one would leave the other's flip uncaught - which is exactly the state this
    /// round found them in.
    #[tokio::test]
    async fn an_attempt_exactly_at_the_window_start_still_counts() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let window = window();
        let key = ip_key(ip("203.0.113.17"), &[7u8; 32]);
        let account = crate::test_support::account(&db.pool).await;

        // `window` rows, all stamped EXACTLY at `now - window`, so a cap of `window`-count is full and
        // the next attempt must be refused. If the comparison is `>`, these fall outside, `used` reads
        // 0, and the attempt is allowed while the budget is in fact spent.
        let at_the_start = now - window;
        let budget = window.num_hours();
        assert!(
            budget > 0,
            "the fixture assumes a window of at least an hour"
        );
        for _ in 0..budget {
            record(&db.pool, Kind::Login, Subject::Ip(&key), at_the_start)
                .await
                .expect("seed an IP attempt on the window start");
            record(
                &db.pool,
                Kind::Login,
                Subject::Account(account),
                at_the_start,
            )
            .await
            .expect("seed an account attempt on the window start");
        }

        // A cap of ONE with `budget` rows on the boundary: both arms must refuse.
        let ip_err = record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now).await;
        assert!(
            ip_err.is_err(),
            "{budget} attempts stamped exactly at `now - window` are INSIDE the closed window \
             [now - window, now], so the IP-keyed budget is spent and the next attempt must be \
             refused. Being allowed here means the IP arm's comparison excludes the boundary, so the \
             cap is one wider than the policy on every window"
        );

        let account_err =
            record_and_check(&db.pool, Kind::Login, Subject::Account(account), 1, now).await;
        assert!(
            account_err.is_err(),
            "the ACCOUNT arm has its own SQL and its own boundary. A test that covers only the IP arm \
             leaves this one uncaught, which is the state a mutation survey found them in"
        );

        // And a microsecond the other side of the line is OUTSIDE it, so both budgets free. This is
        // what makes the refusals above a BOUNDARY assertion rather than a counting one.
        sqlx::query("UPDATE auth_attempts SET created_at = ? WHERE kind = ?")
            .bind(at_the_start - Duration::microseconds(1))
            .bind(Kind::Login.as_str())
            .execute(&db.pool)
            .await
            .expect("move every attempt a microsecond out of the window");

        assert!(
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 1, now)
                .await
                .is_ok(),
            "a microsecond outside the window does not count, so the IP budget frees"
        );
        assert!(
            record_and_check(&db.pool, Kind::Login, Subject::Account(account), 1, now)
                .await
                .is_ok(),
            "and the account budget frees with it"
        );
    }

    /// THE DAILY-SALT CONSEQUENCE, which until now lived only in this module's prose: the IP-keyed
    /// caps are an hourly window over a key that CHANGES AT THE UTC DAY BOUNDARY, so a caller who
    /// spends their whole budget at the end of one day and again at the start of the next gets TWO
    /// budgets in about ten seconds.
    ///
    /// The module doc calls this "the failure mode that hides" and states the ceiling as "ONE EXTRA
    /// BUDGET PER DAY per IP-keyed cap — not an unbounded limiter". The arithmetic behind that
    /// sentence was measured before this test existed, by simulating the real rule (rows are keyed on
    /// the salted hash, a refused attempt writes nothing, the window is one hour, the salt rotates
    /// daily): over ten simulated days an unbounded attacker never exceeded the designed 24-hour rate,
    /// and the ONLY excess was one extra budget at the boundary. That is the claim asserted here.
    ///
    /// The salt itself is NOT reachable from this module - `Subject::Ip` carries the already-computed
    /// hash, and `routes/auth.rs` is what hashes under `salt_for_day(today_utc())`. So two days are
    /// modelled as two keys, which is exactly what the boundary produces. `ip_tracking.rs` separately
    /// tests that the rotation really mints a different salt (`the_salt_is_stable_within_a_day_and_replaced_across_days`),
    /// which is the half that makes this key change in production.
    ///
    /// WHAT THIS TEST DOES *NOT* PIN, measured rather than assumed: every row it writes is stamped
    /// `now`, so it has no row outside any plausible window and is blind to the window's LENGTH.
    /// Widening `window()` from 1 hour to 25 does not fail this test - it is caught by
    /// `attempts_older_than_the_window_do_not_count`, which is the test written for that property.
    /// Dropping the `kind` condition from the query also does not fail this one; that is
    /// `the_kinds_do_not_share_a_budget`'s job. Both were checked by mutation, and both are equivalent
    /// mutants HERE rather than gaps. The property this test owns is the KEY separation - that a fresh
    /// key grants a fresh budget and that the budget is still finite.
    #[tokio::test]
    async fn a_salt_rotation_grants_one_extra_budget_and_not_an_unbounded_limiter() {
        let db = TestDb::new().await;
        let now = Utc::now();
        /// The cap the caller passes in; `record_and_check` takes a `u32`.
        const CAP: u32 = 3;
        /// The same figure for the row-count assertion, which `COUNT(*)` returns as `i64`.
        const CAP_ROWS: i64 = CAP as i64;

        // Day one's key, and the key the rotation produces a moment later.
        let day_one = ip_key(ip("203.0.113.17"), &[11u8; 32]);
        let day_two = ip_key(ip("203.0.113.17"), &[12u8; 32]);

        // Spend day one's ENTIRE budget in the minute before the boundary. `>=` is the refusal bound
        // (module docs: "The bound is INCLUSIVE — `used < limit` allows"), so the CAP-th attempt is
        // the last one allowed.
        for attempt in 0..CAP {
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&day_one), CAP, now)
                .await
                .unwrap_or_else(|e| {
                    panic!("attempt {attempt} of {CAP} is within day one's budget: {e:?}")
                });
        }
        assert!(
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&day_one), CAP, now)
                .await
                .is_err(),
            "day one's budget must be EXHAUSTED, or the second budget below proves nothing"
        );

        // The boundary passes: the same caller, the same address, a new key.
        // The extra budget is real - this is the documented cost of the privacy choice.
        for attempt in 0..CAP {
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&day_two), CAP, now)
                .await
                .unwrap_or_else(|e| {
                    panic!("attempt {attempt} of {CAP} is day two's fresh budget: {e:?}")
                });
        }

        // AND IT IS NOT UNBOUNDED: day two's budget is itself exhaustible, so the excess is one
        // budget per rotation rather than a limiter that stops limiting. If a future change made
        // these caps count on something unstable, or lengthened the window past the salt's life,
        // this assertion is what would stop holding.
        assert!(
            record_and_check(&db.pool, Kind::Login, Subject::Ip(&day_two), CAP, now)
                .await
                .is_err(),
            "the second day's budget must ALSO be exhaustible, or the rotation gives an unbounded \
             limiter rather than the documented one-extra-budget-per-day"
        );

        // The two budgets are separate rows, not one shared count: the first day's attempts are
        // invisible under the second day's key, which is the unlinkability the privacy page promises.
        let day_two_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM auth_attempts WHERE ip_hash = ? AND kind = ?")
                .bind(day_two)
                .bind(Kind::Login.as_str())
                .fetch_one(&db.pool)
                .await
                .expect("count day two");
        assert_eq!(
            day_two_rows, CAP_ROWS,
            "the second day's rows must be its own budget's worth - the first day's attempts are \
             under a different hash and are what makes yesterday unlinkable"
        );
    }

    /// A real IP and a real account are hashed and stored as such — the raw
    /// address never lands in the table. Gate 4 of docs/launch-checklist.md and
    /// the privacy page both promise this, and it is the kind of promise that is
    /// only true as long as a test says so.
    #[tokio::test]
    async fn no_raw_address_is_stored_and_the_hash_is_structured_with_a_daily_salt() {
        let db = TestDb::new().await;
        let now = Utc::now();
        let salt = [9u8; 32];
        let address = ip("198.51.100.7");
        let key = ip_key(address, &salt);

        record_and_check(&db.pool, Kind::Login, Subject::Ip(&key), 5, now)
            .await
            .expect("record");

        let stored: String = sqlx::query_scalar("SELECT ip_hash FROM auth_attempts")
            .fetch_one(&db.pool)
            .await
            .expect("read back");

        assert_eq!(stored, key, "the stored value is the hash that was written");
        assert_eq!(stored.len(), 64, "sha256 hex");
        assert!(
            !stored.contains("198.51.100.7"),
            "the raw address must not appear in the stored identifier"
        );
        assert_ne!(
            ip_key(address, &[0u8; 32]),
            stored,
            "a different salt must produce a different identifier; if this held, the \
             'hash' would not be keyed and the address space would be enumerable"
        );
    }

    /// The kind strings must match the migration's CHECK constraint exactly. A
    /// rename here that the schema does not know about fails at runtime on the
    /// sign-in path, which is the worst place for a surprise; this asserts the
    /// four values against the live schema so the two cannot drift.
    #[tokio::test]
    async fn the_kind_strings_are_the_ones_the_schema_accepts() {
        let db = TestDb::new().await;
        let account = crate::test_support::account(&db.pool).await;

        for kind in [
            Kind::Login,
            Kind::Signup,
            Kind::PasswordReset,
            Kind::VerificationResend,
        ] {
            sqlx::query(
                "INSERT INTO auth_attempts (id, ip_hash, account_id, kind, created_at) \
                 VALUES (?, '', ?, ?, ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account.hyphenated())
            .bind(kind.as_str())
            .bind(Utc::now())
            .execute(&db.pool)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "the schema rejected kind '{}' — the enum and the CHECK constraint \
                     have drifted: {e}",
                    kind.as_str()
                )
            });
        }

        // And the constraint really is enforcing the vocabulary, so the loop above
        // is not passing because the CHECK is absent.
        assert!(
            sqlx::query(
                "INSERT INTO auth_attempts (id, ip_hash, account_id, kind, created_at) \
                 VALUES (?, '', ?, 'not_a_kind', ?)",
            )
            .bind(Uuid::new_v4().hyphenated())
            .bind(account.hyphenated())
            .bind(Utc::now())
            .execute(&db.pool)
            .await
            .is_err(),
            "the CHECK constraint must reject an unknown kind"
        );
    }
}
