#![cfg_attr(
    not(test),
    // FENCED, and free. This binary's only arithmetic outside its tests is date
    // arithmetic over the same 900-second bound the whole tool is about, and the one
    // site clippy reports here is inside `mod tests` - so the non-test scoping
    // leaves nothing to justify. It is added anyway so that a future figure added
    // to this file is held to the same standard as the library, where an overflow
    // would be a wrong balance rather than a wrong console line.
    deny(clippy::arithmetic_side_effects)
)]

//! Stranded-hold sweep: the stated bound that makes an unpaired reservation an
//! incident instead of invisible money.
//!
//! The proxy takes a hold before it forwards a request and writes a NEGATIVE
//! `-reserved` ledger row under `ref = 'reserve_<uuid>'` (reason `usage`). A
//! healthy reservation is paired in `debit_usage_transaction` (the release lands
//! in the SAME transaction, a POSITIVE row under the SAME ref) or by
//! `release_reservation_transaction`; `ReservationGuard` covers the paths that
//! return early. A reserve ref with a negative row and NO positive row under the
//! same ref is money that left a wallet and came back nowhere.
//!
//! `db::unpaired_hold_rows` finds those rows but nothing ran it on a schedule and
//! nothing said how long a hold may stay unpaired - audit finding F6,
//! `docs/plans/proxy-hot-path-audit.md` section 5.6. This binary is that
//! schedule, and it carries the bound.
//!
//! ## The bound
//!
//! `--max-hold-age-seconds` (default 900 = 15 minutes). A hold lives exactly as
//! long as one request, so the longest a legitimate hold can stay unpaired is the
//! longest a request can stay in flight, plus the moment it takes the release to
//! commit. From `config/apikita.toml`:
//!
//!   * `request_timeout_seconds = 120` - the upstream client's per-request
//!     timeout, so the proxy abandons a request after 2 minutes at the most.
//!   * `reserve_settlement_cycles = 1` - the reservation is sized to cover one
//!     settlement, not a retry ladder, so there is no multiplier on top.
//!   * `max_stream_seconds = 1800` - the SSE stream is the one path that can
//!     outlive the request timeout. That cap is 30 minutes, but it is the ceiling
//!     for a *connection*, not for a settlement: the stream only ends a hold when
//!     it settles, and settlement happens at stream end.
//!
//! 900s is ~7.5x the request timeout and half the stream ceiling. Anything under
//! it is a release that is merely late (a slow commit, a dropped fire-and-forget
//! task still retrying) and alerting on it would be noise; anything over it is a
//! release that is never coming. A longer bound trades detection latency for a
//! quieter alarm - raise it if the stream path proves slow to settle, never
//! silently.
//!
//! ## Report-only
//!
//! The sweep NEVER moves money. Releasing a hold writes a positive ledger row,
//! which is an operator action with an audit trail
//! (`docs/admin-surface.md`), not something a cron job may do unseen. The
//! `--release` flag exists to do it deliberately and is opt-in; it logs every row
//! it touches with account, ref and amount at info level. Off by default, on
//! purpose: silently correcting a stranded hold is the same invisible-money
//! anti-pattern this sweep exists to catch.
//!
//! ## Exit codes
//!
//!   0  no hold exceeded the bound (or none exist at all)
//!   1  at least one hold exceeded it - alert; or the sweep could not run
//!   2  bad invocation
//!
//! Idempotent: it reads, and unless `--release` is passed it writes nothing, so
//! running it twice reports the same holds twice. Scheduled alongside `ip-purge`.

use std::env;

use apikita_server::db;
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

/// Default bound: 15 minutes. See the module comment for the derivation from
/// `request_timeout_seconds = 120` / `max_stream_seconds = 1800`.
const DEFAULT_MAX_HOLD_AGE_SECONDS: i64 = 900;

const USAGE: &str = "\
usage: hold-sweep [--max-hold-age-seconds <SECONDS>] [--release]

Sweeps for stranded reservation holds - a ledger row with a reserve_<uuid> ref
and no matching positive release under the same ref - and exits non-zero when any
of them is older than the bound.

  --max-hold-age-seconds <SECONDS>  How long a hold may stay unpaired before it is
                                    an incident. Default 900 (15 min).
  --release                         Opt-in operator action: credit each
                                    over-bound hold back to its wallet. Logs
                                    every row touched (account, ref, amount).
  -h, --help                        Print this.

Report-only by default. A sweep that moves money on its own is the defect, not
the fix.";

/// One stranded hold, as reported.
struct StrandedHold {
    account_id: Uuid,
    /// The address on the account, or `None` when it holds no identity row.
    ///
    /// This field used to carry `accounts.pb_user_id`, which was the only handle
    /// an operator had on the account. That column is gone (the identity port
    /// retired it), and a bare account uuid is not something a human can check
    /// against a support ticket, so the address takes its place. `Option` because
    /// an account really can exist with no identity yet: the port creates the row
    /// before the confirmation mail is delivered, and an interrupted signup leaves
    /// exactly that. Reporting "no identity" is honest; inventing an address or
    /// skipping the row would hide a stranded hold from the operator.
    email: Option<String>,
    reservation_ref: String,
    amount_idr: i64,
    held_at: DateTime<Utc>,
    age_seconds: i64,
    row_count: i64,
}

impl StrandedHold {
    fn over_bound(&self, bound_seconds: i64) -> bool {
        self.age_seconds > bound_seconds
    }
}

#[derive(Debug)]
struct Options {
    max_hold_age_seconds: i64,
    release: bool,
}

fn parse_args() -> Result<Options, String> {
    parse_args_from(env::args().skip(1))
}

/// Parses the CLI arguments from an explicit iterator so the rules can be
/// pinned by a unit test without reaching into the process's `argv`.
fn parse_args_from<I>(args: I) -> Result<Options, String>
where
    I: Iterator<Item = String>,
{
    let mut options = Options {
        max_hold_age_seconds: DEFAULT_MAX_HOLD_AGE_SECONDS,
        release: false,
    };

    let mut args = args;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max-hold-age-seconds" => {
                let raw = args
                    .next()
                    .ok_or_else(|| "--max-hold-age-seconds needs a value".to_string())?;
                let seconds: i64 = raw.parse().map_err(|_| {
                    format!("--max-hold-age-seconds expects an integer, got {raw:?}")
                })?;
                if seconds <= 0 {
                    return Err("--max-hold-age-seconds must be positive".into());
                }
                options.max_hold_age_seconds = seconds;
            }
            "--release" => options.release = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }

    Ok(options)
}

/// Every stranded hold in the ledger, with the age of its oldest unpaired
/// negative row.
///
/// The predicate is deliberately the one `db::unpaired_hold_rows` documents: a
/// `reserve_%` ref whose negative rows have NO positive row under the same ref.
/// A different predicate here would make the binary and the library disagree
/// about what "stranded" means, which is how a detector stops being trusted.
async fn stranded_holds(pool: &SqlitePool) -> Result<Vec<StrandedHold>, sqlx::Error> {
    let rows = sqlx::query(
        r#"
        SELECT
            l.account_id,
            (SELECT i.email FROM identities i
              WHERE i.account_id = l.account_id
              ORDER BY i.email_verified DESC, i.created_at ASC LIMIT 1) AS email,
            l.ref AS reservation_ref,
            -- SUM over BIGINT is NUMERIC; cast back so it decodes as i64.
            CAST(SUM(l.delta_idr) AS INTEGER) AS amount_idr,
            MIN(l.created_at) AS held_at,
            -- SQLite has no now()/EXTRACT: the schema stores RFC3339 TEXT, so the
            -- age is strftime seconds between the oldest hold and the bound instant
            -- the caller binds. See db.rs for the timestamp contract.
            CAST(strftime('%s', ?) - strftime('%s', MIN(l.created_at)) AS INTEGER) AS age_seconds,
            COUNT(*) AS row_count
        FROM ledger l
        JOIN accounts a ON a.id = l.account_id
        WHERE l.ref LIKE 'reserve_%'
          AND l.delta_idr < 0
        GROUP BY l.account_id, l.ref
        HAVING NOT EXISTS (
            SELECT 1 FROM ledger m
            WHERE m.account_id = l.account_id
              AND m.ref = l.ref
              AND m.delta_idr > 0
        )
        ORDER BY held_at ASC
        "#,
    )
    .bind(Utc::now().to_rfc3339())
    .fetch_all(pool)
    .await?;

    Ok(rows
        .iter()
        .map(|row| StrandedHold {
            account_id: row.get("account_id"),
            email: row.get("email"),
            reservation_ref: row.get("reservation_ref"),
            amount_idr: row.get("amount_idr"),
            held_at: row.get("held_at"),
            age_seconds: row.get("age_seconds"),
            row_count: row.get("row_count"),
        })
        .collect())
}

/// Credits one stranded hold back to its wallet and pairs it under the same ref,
/// so the row stops being stranded.
///
/// Opt-in only (`--release`). The ledger is append-only: this writes a POSITIVE
/// `adjustment` row under the SAME ref rather than touching the negative one.
/// The ref is passed through verbatim so the pairing is exact - rewriting it
/// would hide the very trace an operator needs afterwards.
async fn release_hold(pool: &SqlitePool, hold: &StrandedHold) -> Result<i64, sqlx::Error> {
    let amount = hold.amount_idr.abs();
    // The schema stores RFC3339 TEXT and refuses SQLite's own now() form, so every
    // instant is computed in Rust and bound. See db.rs for the timestamp contract.
    let now = Utc::now().to_rfc3339();

    // Same shape as reserve_balance_transaction: lock the wallet row, credit it,
    // and read back the resulting balance in one transaction so balance_after is
    // the true post-credit balance rather than a snapshot taken outside the lock.
    //
    // `account_id` IS BOUND HYPHENATED, like the other 196 binds in this crate and unlike the
    // two that used to be here. `accounts.id`/`wallets.account_id` are TEXT holding the
    // hyphenated form (`db.rs` reads them back via `uuid::fmt::Hyphenated`), so a raw `Uuid`
    // is a different string and matches no row: the UPDATE returned RowNotFound and `--release`
    // could never credit a hold. The unit test below is what found it - this function had no
    // coverage at all before, and the mutation that dropped `.abs()` survived too.
    let account_id = hold.account_id.hyphenated();

    let mut tx = pool.begin().await?;
    let new_balance: i64 = sqlx::query_scalar(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? \
         WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(amount)
    .bind(&now)
    .bind(account_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) \
         VALUES (?, ?, 'adjustment', ?, ?, ?)",
    )
    .bind(account_id)
    .bind(amount)
    .bind(&hold.reservation_ref)
    .bind(new_balance)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(new_balance)
}

fn print_summary(
    out: &mut impl std::io::Write,
    holds: &[StrandedHold],
    bound_seconds: i64,
    release: bool,
    released: &[StrandedHold],
) {
    // Plain stdout: the output is meant to be pasted into an incident verbatim,
    // so it must not depend on a log filter being set.
    writeln!(out, "stranded-hold sweep").ok();
    writeln!(
        out,
        "bound: {}s ({}) - a hold older than this is an incident, not a late release",
        bound_seconds,
        format_age(bound_seconds)
    )
    .ok();
    writeln!(
        out,
        "mode:  {}",
        if release {
            "release (opt-in)"
        } else {
            "report-only"
        }
    )
    .ok();
    writeln!(out, "holds: {}", holds.len()).ok();

    if holds.is_empty() {
        writeln!(
            out,
            "result: OK - no reservation hold is stranded; zero rows is the invariant"
        )
        .ok();
        return;
    }

    writeln!(out).ok();
    writeln!(
        out,
        "{:<8} {:<38} {:<44} {:>14} {:>10} {:>5}",
        "AGE", "ACCOUNT", "RESERVATION REF", "AMOUNT_IDR", "HELD_AT", "ROWS"
    )
    .ok();
    for hold in holds {
        writeln!(
            out,
            "{:<8} {:<38} {:<44} {:>14} {:>10} {:>5}",
            format_age(hold.age_seconds),
            hold.account_id,
            hold.reservation_ref,
            hold.amount_idr,
            hold.held_at.format("%Y-%m-%dT%H:%M:%SZ"),
            hold.row_count
        )
        .ok();
        writeln!(
            out,
            "         email={}",
            hold.email.as_deref().unwrap_or("(no identity)")
        )
        .ok();
    }

    let over: Vec<&StrandedHold> = holds
        .iter()
        .filter(|h| h.over_bound(bound_seconds))
        .collect();
    let over_total: i64 = over.iter().map(|h| h.amount_idr.abs()).sum();
    let held_total: i64 = holds.iter().map(|h| h.amount_idr.abs()).sum();

    writeln!(out).ok();
    writeln!(out, "total stranded: {} IDR", held_total).ok();
    writeln!(
        out,
        "over bound:     {} of {} ({} IDR)",
        over.len(),
        holds.len(),
        over_total
    )
    .ok();

    if release {
        writeln!(out, "released:       {}", released.len()).ok();
    }
    if !over.is_empty() {
        writeln!(
            out,
            "result: ALERT - {} hold(s) exceeded the {} bound; money is stranded",
            over.len(),
            format_age(bound_seconds)
        )
        .ok();
    }
}

fn format_age(seconds: i64) -> String {
    if seconds < 0 {
        return format!("{seconds}s");
    }
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, secs) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m{secs}s")
    } else {
        format!("{secs}s")
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,apikita_server=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let options = match parse_args() {
        Ok(options) => options,
        Err(err) => {
            eprintln!("hold-sweep: {err}");
            eprintln!();
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    let database_url = env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let pool = match db::init_pool(&database_url).await {
        Ok(pool) => pool,
        Err(err) => {
            error!("Could not connect to the database (SQLite): {err}");
            // Non-zero, so a scheduler notices a sweep that did not run. A sweep
            // that silently stops running is how a detector stops detecting.
            std::process::exit(1);
        }
    };

    let holds = match stranded_holds(&pool).await {
        Ok(holds) => holds,
        Err(err) => {
            error!("Could not read stranded holds: {err}");
            std::process::exit(1);
        }
    };

    let mut released = Vec::new();
    if options.release {
        for hold in holds
            .iter()
            .filter(|h| h.over_bound(options.max_hold_age_seconds))
        {
            // Every row touched is logged with account, ref and amount: the
            // operator action has to be reconstructable from the log alone.
            info!(
                account_id = %hold.account_id,
                email = hold.email.as_deref().unwrap_or("(no identity)"),
                reservation_ref = %hold.reservation_ref,
                amount_idr = hold.amount_idr.abs(),
                age_seconds = hold.age_seconds,
                "releasing stranded hold back to the wallet"
            );
            match release_hold(&pool, hold).await {
                Ok(new_balance) => {
                    info!(
                        account_id = %hold.account_id,
                        reservation_ref = %hold.reservation_ref,
                        amount_idr = hold.amount_idr.abs(),
                        new_balance,
                        "stranded hold released"
                    );
                    released.push(StrandedHold {
                        account_id: hold.account_id,
                        email: hold.email.clone(),
                        reservation_ref: hold.reservation_ref.clone(),
                        amount_idr: hold.amount_idr,
                        held_at: hold.held_at,
                        age_seconds: hold.age_seconds,
                        row_count: hold.row_count,
                    });
                }
                Err(err) => {
                    error!(
                        account_id = %hold.account_id,
                        reservation_ref = %hold.reservation_ref,
                        amount_idr = hold.amount_idr.abs(),
                        "could not release stranded hold: {err}"
                    );
                }
            }
        }
    }

    print_summary(
        &mut std::io::stdout(),
        &holds,
        options.max_hold_age_seconds,
        options.release,
        &released,
    );

    let over = holds
        .iter()
        .filter(|h| h.over_bound(options.max_hold_age_seconds))
        .count();

    if over > 0 {
        // Non-zero so a scheduler or CI can alert, exactly as ip-purge does for a
        // sweep that could not run.
        std::process::exit(1);
    }

    if options.release && !released.is_empty() {
        info!(released = released.len(), "stranded holds released");
    } else if holds.is_empty() {
        info!(
            bound_seconds = options.max_hold_age_seconds,
            "no stranded reservation holds"
        );
    } else {
        warn!(
            holds = holds.len(),
            bound_seconds = options.max_hold_age_seconds,
            "stranded holds present but all within the bound"
        );
    }

    // Drain cleanly so the pool's connections are not left to the process teardown.
    pool.close().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------------------
    // release_hold: the only function in this binary that MOVES MONEY
    // ---------------------------------------------------------------------------------------
    //
    // Everything else here is parsing, formatting and reporting, and those are tested. `release_hold`
    // had NO coverage of any kind, which was measured rather than assumed: dropping the `.abs()` -
    // which inverts the direction of the credit - and deleting the ledger insert BOTH left the whole
    // binary green at 15 passed.
    //
    // It was unreachable by the fixture style used above, because it needs a real database and
    // `test_support` is `#[cfg(test)]` and private to the lib. The harness below is the same shape
    // usage-purge.rs uses for the same reason.

    use sqlx::sqlite::SqlitePoolOptions;
    use std::str::FromStr;

    /// A migrated on-disk SQLite URL in the system temp directory.
    async fn migrated_temp_db() -> (String, std::path::PathBuf) {
        let path =
            std::env::temp_dir().join(format!("apikita-hold-sweep-test-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}", path.to_str().unwrap().replace('\\', "/"));
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(&url)
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("open temp db");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("apply migrations");
        pool.close().await;
        (url, path)
    }

    /// Builds an account funded with `funding_idr` and holding `held_idr`, returning (account, hold)
    /// with the hold shaped exactly as `stranded_holds` would report it.
    ///
    /// THE RESERVATION GOES THROUGH THE REAL PRODUCTION PATH (`db::reserve_balance_transaction`)
    /// rather than a hand-written ledger row. Two reasons, and the first is why the first version of
    /// this fixture failed: the ledger's `reason` is constrained to ('topup','usage','adjustment',
    /// 'refund'), so inventing `'reserve'` is rejected by the schema - a hold is identified by
    /// `ref LIKE 'reserve_%' AND delta_idr < 0`, not by its reason. The second is that a fixture
    /// which writes its own row can drift from what the proxy actually produces.
    ///
    /// The hold's `amount_idr` is NEGATIVE, because that is what the real reporting query produces:
    /// `stranded_holds` aggregates `SUM(l.delta_idr)`, and a reservation is a negative ledger row.
    /// A fixture using a positive amount would make `release_hold`'s `.abs()` a no-op, and the test
    /// would then pass against a build that debits the customer instead of refunding them.
    async fn account_holding(
        pool: &SqlitePool,
        funding_idr: i64,
        held_idr: i64,
    ) -> (Uuid, StrandedHold) {
        let account = Uuid::new_v4();
        let now = Utc::now();

        sqlx::query("INSERT INTO accounts (id, created_at, updated_at) VALUES (?, ?, ?)")
            .bind(account.hyphenated())
            .bind(now)
            .bind(now)
            .execute(pool)
            .await
            .expect("create the account");
        sqlx::query("INSERT INTO wallets (account_id, balance_idr, updated_at) VALUES (?, ?, ?)")
            .bind(account.hyphenated())
            .bind(0_i64)
            .bind(now)
            .execute(pool)
            .await
            .expect("create the wallet");

        // Fund it the way a deposit settles, so the wallet's history is realistic.
        sqlx::query("UPDATE wallets SET balance_idr = ? WHERE account_id = ?")
            .bind(funding_idr)
            .bind(account.hyphenated())
            .execute(pool)
            .await
            .expect("fund the wallet");

        // The hold, through the real guarded debit under a real `reserve_%` ref.
        let ref_ = format!("reserve_{}", Uuid::new_v4().simple());
        db::reserve_balance_transaction(pool, account, held_idr, Some(&ref_))
            .await
            .expect("reserve against the wallet");
        assert_eq!(
            wallet_balance(pool, account).await,
            funding_idr - held_idr,
            "the reservation must actually have left the wallet"
        );

        let hold = StrandedHold {
            account_id: account,
            email: None,
            reservation_ref: ref_,
            amount_idr: -held_idr,
            held_at: now,
            age_seconds: 900,
            row_count: 1,
        };
        (account, hold)
    }

    async fn wallet_balance(pool: &SqlitePool, account: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read the balance")
    }

    /// Releasing a stranded hold CREDITS the wallet by the held amount, and writes a ledger row
    /// that pairs under the same ref.
    ///
    /// The `.abs()` is the load-bearing part and it is why this test exists: the hold is stored
    /// NEGATIVE (`SUM(delta_idr)` over the reservation), so `balance_idr + amount` DEBITS the
    /// customer a second time instead of refunding them. MEASURED before this test: dropping
    /// `.abs()` kept all 15 tests in this binary passing, so a run of `--release` would have taken
    /// money from every account it was meant to compensate.
    #[tokio::test]
    async fn releasing_a_hold_credits_the_held_amount_back() {
        let (url, path) = migrated_temp_db().await;
        let pool = db::init_pool(&url).await.expect("open the migrated db");

        // The wallet was funded with 50_000 and a 10_000 reservation is outstanding, so it sits at
        // 40_000: the customer is 10_000 short of what they paid for.
        let (account, hold) = account_holding(&pool, 50_000, 10_000).await;

        let reported = release_hold(&pool, &hold).await.expect("release the hold");
        assert_eq!(
            reported, 50_000,
            "the return value is the balance_after written to the ledger, and it must be the \
             post-credit balance"
        );
        assert_eq!(
            wallet_balance(&pool, account).await,
            50_000,
            "releasing a 10_000 hold must CREDIT 10_000, restoring the funded 50_000. Seeing \
             30_000 here means the negative stored amount was added without .abs(), which DEBITS \
             the customer a second time instead of refunding them"
        );

        // The trace: an append-only POSITIVE adjustment under the SAME ref, which is what pairs the
        // row and stops it being stranded. The ledger is never mutated, so this is an addition.
        let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
            "SELECT delta_idr, reason, ref, balance_after FROM ledger \
             WHERE account_id = ? AND reason = 'adjustment'",
        )
        .bind(account.hyphenated())
        .fetch_all(&pool)
        .await
        .expect("read the adjustment");
        assert_eq!(
            rows.len(),
            1,
            "exactly one adjustment row, or the pairing is ambiguous"
        );
        let (delta, reason, ref_, balance_after) = &rows[0];
        assert_eq!(
            *delta, 10_000,
            "the adjustment must be POSITIVE: it credits back"
        );
        assert_eq!(reason, "adjustment");
        assert_eq!(
            ref_, &hold.reservation_ref,
            "the ref is passed through verbatim: rewriting it would hide the trace an operator \
             needs, and the pairing is what stops the row being stranded"
        );
        assert_eq!(
            *balance_after, 50_000,
            "balance_after must be the post-credit balance read INSIDE the transaction, not a \
             snapshot taken outside the lock"
        );

        // And the reservation itself is untouched: the ledger is append-only.
        //
        // Identified by ref AND SIGN. Both rows now share the ref by design - that is what pairs
        // them - so a query on the ref alone returns whichever row SQLite reaches first, which is
        // how the first version of this assertion read the +10_000 adjustment instead of the
        // -10_000 reservation.
        let reserved: i64 = sqlx::query_scalar(
            "SELECT delta_idr FROM ledger WHERE account_id = ? AND ref = ? AND delta_idr < 0",
        )
        .bind(account.hyphenated())
        .bind(&hold.reservation_ref)
        .fetch_one(&pool)
        .await
        .expect("read the reservation");
        assert_eq!(
            reserved, -10_000,
            "the negative reservation row stays exactly as written; the fix is a second row, never \
             an edit to the first"
        );

        // The pairing, stated as `stranded_holds` states it: the hold is no longer stranded
        // because a POSITIVE row now exists under the same ref. This is the invariant the
        // operator's `--release` run is supposed to leave behind.
        let unpaired: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ( \
               SELECT l.ref FROM ledger l \
               WHERE l.account_id = ? AND l.ref LIKE 'reserve_%' AND l.delta_idr < 0 \
               GROUP BY l.ref \
               HAVING NOT EXISTS (SELECT 1 FROM ledger m \
                                  WHERE m.account_id = l.account_id AND m.ref = l.ref \
                                    AND m.delta_idr > 0))",
        )
        .bind(account.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("count unpaired holds");
        assert_eq!(
            unpaired, 0,
            "after the release the hold must no longer be STRANDED: a positive row under the same \
             ref is what nets it to zero and takes it off the operator's report"
        );

        pool.close().await;
        let _ = std::fs::remove_file(path);
    }

    /// Releasing the SAME hold twice credits twice, which is why `release_hold` is opt-in.
    ///
    /// This is not a bug being pinned - it is the reason the binary refuses to move money without
    /// `--release`, stated as a test so the exposure is visible rather than inferred. An operator
    /// re-running the command after an incident would double-credit.
    #[tokio::test]
    async fn releasing_twice_credits_twice_which_is_why_release_is_opt_in() {
        let (url, path) = migrated_temp_db().await;
        let pool = db::init_pool(&url).await.expect("open the migrated db");
        let (account, hold) = account_holding(&pool, 50_000, 10_000).await;

        release_hold(&pool, &hold).await.expect("first release");
        release_hold(&pool, &hold).await.expect("second release");

        assert_eq!(
            wallet_balance(&pool, account).await,
            60_000,
            "a second release credits again: the guard against that is the operator's flag, not \
             this function, and a reader should be able to see that here"
        );

        pool.close().await;
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn bound_exceeds_the_worst_case_request() {
        // 120s request timeout x 1 settlement cycle = 120s worst case; the bound
        // must sit comfortably above it or every slow release would alert.
        // Constant assertion: checked at compile time (clippy::assertions_on_constants).
        const { assert!(DEFAULT_MAX_HOLD_AGE_SECONDS > 120) };
    }

    #[test]
    fn over_bound_is_strictly_greater() {
        let hold = StrandedHold {
            account_id: Uuid::nil(),
            email: Some("test@example.com".into()),
            reservation_ref: "reserve_test".into(),
            amount_idr: -1000,
            held_at: Utc::now(),
            age_seconds: 900,
            row_count: 1,
        };
        assert!(!hold.over_bound(900), "exactly at the bound is not over it");
        assert!(hold.over_bound(899));
        assert!(!hold.over_bound(901));
    }

    #[test]
    fn ages_render_for_an_incident() {
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(90), "1m30s");
        assert_eq!(format_age(3_600), "1h0m");
        assert_eq!(format_age(90_000), "1d1h");
    }

    /// Builds a stranded hold with the given age and amount, for the summary
    /// rendering tests.
    fn sample_hold(age_seconds: i64, amount_idr: i64, over: bool) -> StrandedHold {
        StrandedHold {
            account_id: Uuid::nil(),
            email: Some("test@example.com".into()),
            reservation_ref: "reserve_test".into(),
            amount_idr,
            held_at: Utc::now() - chrono::Duration::seconds(age_seconds),
            age_seconds,
            row_count: 1,
        }
        .apply_over(over)
    }

    /// Helper: marks whether the hold is reported as over the bound by setting an
    /// age above/below a fixed 900s bound used only for rendering fixtures.
    trait OverMark {
        fn apply_over(self, over: bool) -> Self;
    }
    impl OverMark for StrandedHold {
        fn apply_over(mut self, over: bool) -> Self {
            if over {
                self.age_seconds = 900 + 1;
            }
            self
        }
    }

    #[test]
    fn parse_args_defaults_to_the_standard_bound_and_report_only() {
        let options = parse_args_from(std::iter::empty()).expect("no args is valid");
        assert_eq!(options.max_hold_age_seconds, DEFAULT_MAX_HOLD_AGE_SECONDS);
        assert!(!options.release);
    }

    #[test]
    fn parse_args_accepts_a_positive_custom_bound() {
        let options = parse_args_from(
            ["--max-hold-age-seconds", "1200"]
                .into_iter()
                .map(String::from),
        )
        .expect("valid bound");
        assert_eq!(options.max_hold_age_seconds, 1200);
        assert!(!options.release);
    }

    #[test]
    fn parse_args_sets_release_flag() {
        let options =
            parse_args_from(["--release"].into_iter().map(String::from)).expect("release flag");
        assert!(options.release);
        assert_eq!(options.max_hold_age_seconds, DEFAULT_MAX_HOLD_AGE_SECONDS);
    }

    #[test]
    fn parse_args_combines_release_and_bound() {
        let options = parse_args_from(
            ["--release", "--max-hold-age-seconds", "60"]
                .into_iter()
                .map(String::from),
        )
        .expect("combined flags");
        assert!(options.release);
        assert_eq!(options.max_hold_age_seconds, 60);
    }

    #[test]
    fn parse_args_rejects_a_missing_bound_value() {
        let err = parse_args_from(["--max-hold-age-seconds"].into_iter().map(String::from))
            .expect_err("missing value");
        assert!(err.contains("needs a value"), "got {err:?}");
    }

    #[test]
    fn parse_args_rejects_a_non_integer_bound() {
        let err = parse_args_from(
            ["--max-hold-age-seconds", "ten"]
                .into_iter()
                .map(String::from),
        )
        .expect_err("non-integer value");
        assert!(err.contains("expects an integer"), "got {err:?}");
    }

    #[test]
    fn parse_args_rejects_a_non_positive_bound() {
        for bad in ["0", "-5"] {
            let err = parse_args_from(
                ["--max-hold-age-seconds", bad]
                    .into_iter()
                    .map(String::from),
            )
            .expect_err("non-positive value");
            assert!(err.contains("must be positive"), "got {err:?} for {bad}");
        }
    }

    #[test]
    fn parse_args_rejects_an_unknown_argument() {
        let err = parse_args_from(["--bogus"].into_iter().map(String::from))
            .expect_err("unknown argument");
        assert!(err.contains("unknown argument"), "got {err:?}");
    }

    #[test]
    fn summary_reports_ok_when_no_holds_exist() {
        let mut buf = Vec::new();
        print_summary(&mut buf, &[], DEFAULT_MAX_HOLD_AGE_SECONDS, false, &[]);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("mode:  report-only"));
        assert!(out.contains("holds: 0"));
        assert!(out.contains("result: OK - no reservation hold is stranded"));
        assert!(!out.contains("ALERT"));
    }

    #[test]
    fn summary_reports_holds_within_the_bound_as_non_alert() {
        let holds = vec![sample_hold(100, -500, false)];
        let mut buf = Vec::new();
        print_summary(&mut buf, &holds, DEFAULT_MAX_HOLD_AGE_SECONDS, false, &[]);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("total stranded: 500 IDR"));
        assert!(out.contains("over bound:     0 of 1"));
        assert!(!out.contains("ALERT"));
    }

    #[test]
    fn summary_reports_an_alert_when_a_hold_exceeds_the_bound() {
        let holds = vec![
            sample_hold(100, -500, false),
            sample_hold(900, -2_000, true),
        ];
        let mut buf = Vec::new();
        print_summary(&mut buf, &holds, DEFAULT_MAX_HOLD_AGE_SECONDS, false, &[]);
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("over bound:     1 of 2"));
        assert!(out.contains("result: ALERT - 1 hold(s) exceeded"));
    }

    #[test]
    fn summary_reports_released_count_in_release_mode() {
        let holds = vec![sample_hold(900, -2_000, true)];
        let released = vec![sample_hold(900, -2_000, true)];
        let mut buf = Vec::new();
        print_summary(
            &mut buf,
            &holds,
            DEFAULT_MAX_HOLD_AGE_SECONDS,
            true,
            &released,
        );
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("mode:  release (opt-in)"));
        assert!(out.contains("released:       1"));
        assert!(out.contains("result: ALERT - 1 hold(s) exceeded"));
    }
}
