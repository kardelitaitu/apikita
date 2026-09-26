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
    pb_user_id: String,
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

struct Options {
    max_hold_age_seconds: i64,
    release: bool,
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options {
        max_hold_age_seconds: DEFAULT_MAX_HOLD_AGE_SECONDS,
        release: false,
    };

    let mut args = env::args().skip(1);
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
            a.pb_user_id,
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
        GROUP BY l.account_id, a.pb_user_id, l.ref
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
            pb_user_id: row.get("pb_user_id"),
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
    let mut tx = pool.begin().await?;
    let new_balance: i64 = sqlx::query_scalar(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? \
         WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(amount)
    .bind(&now)
    .bind(hold.account_id)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) \
         VALUES (?, ?, 'adjustment', ?, ?, ?)",
    )
    .bind(hold.account_id)
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
    holds: &[StrandedHold],
    bound_seconds: i64,
    release: bool,
    released: &[StrandedHold],
) {
    // Plain stdout: the output is meant to be pasted into an incident verbatim,
    // so it must not depend on a log filter being set.
    println!("stranded-hold sweep");
    println!(
        "bound: {}s ({}) - a hold older than this is an incident, not a late release",
        bound_seconds,
        format_age(bound_seconds)
    );
    println!(
        "mode:  {}",
        if release {
            "release (opt-in)"
        } else {
            "report-only"
        }
    );
    println!("holds: {}", holds.len());

    if holds.is_empty() {
        println!("result: OK - no reservation hold is stranded; zero rows is the invariant");
        return;
    }

    println!();
    println!(
        "{:<8} {:<38} {:<44} {:>14} {:>10} {:>5}",
        "AGE", "ACCOUNT", "RESERVATION REF", "AMOUNT_IDR", "HELD_AT", "ROWS"
    );
    for hold in holds {
        println!(
            "{:<8} {:<38} {:<44} {:>14} {:>10} {:>5}",
            format_age(hold.age_seconds),
            hold.account_id,
            hold.reservation_ref,
            hold.amount_idr,
            hold.held_at.format("%Y-%m-%dT%H:%M:%SZ"),
            hold.row_count
        );
        println!("         pb_user_id={}", hold.pb_user_id);
    }

    let over: Vec<&StrandedHold> = holds
        .iter()
        .filter(|h| h.over_bound(bound_seconds))
        .collect();
    let over_total: i64 = over.iter().map(|h| h.amount_idr.abs()).sum();
    let held_total: i64 = holds.iter().map(|h| h.amount_idr.abs()).sum();

    println!();
    println!("total stranded: {} IDR", held_total);
    println!(
        "over bound:     {} of {} ({} IDR)",
        over.len(),
        holds.len(),
        over_total
    );

    if release {
        println!("released:       {}", released.len());
    }
    if !over.is_empty() {
        println!(
            "result: ALERT - {} hold(s) exceeded the {} bound; money is stranded",
            over.len(),
            format_age(bound_seconds)
        );
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
            error!("Could not connect to Postgres: {err}");
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
                pb_user_id = %hold.pb_user_id,
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
                        pb_user_id: hold.pb_user_id.clone(),
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
            pb_user_id: "pb_test".into(),
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
}
