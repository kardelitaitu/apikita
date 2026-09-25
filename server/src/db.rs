use crate::error::AppError;
use chrono::Utc;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use std::str::FromStr;
use std::time::Duration;
use tracing::error;
use uuid::fmt::Hyphenated;
use uuid::Uuid;

/// Opens the application pool.
///
/// The options are not decoration. Each is a measured trap from the plan's
/// section 4.3, and each was re-measured against this exact option set:
///
/// - `foreign_keys(true)` — `PRAGMA foreign_keys` is **per connection**, not per
///   database. sqlx already defaults it ON (unlike raw SQLite, where it is OFF),
///   but it is set explicitly because with it off every `ON DELETE CASCADE` in
///   the schema is silently inert and the `ON DELETE RESTRICT` that is supposed
///   to make hard-deleting a funded account impossible stops working. Measured
///   with this option set: a dangling reference is refused, code 787.
/// - `journal_mode(Wal)` — a persistent property of the file, and `bin/migrate.rs`
///   sets it too. Setting it here as well means a database that somehow lost WAL
///   is put back into it rather than quietly running in rollback-journal mode.
///   Measured: `PRAGMA journal_mode` reads back `wal`.
/// - `synchronous(Normal)` — safe under WAL (a power loss can lose the last few
///   transactions, it cannot corrupt the database) and avoids an fsync per
///   commit, which is most of what WAL buys on this write-heavy path. Measured:
///   `PRAGMA synchronous` reads back 1, i.e. NORMAL.
/// - `busy_timeout(5s)` — this is what replaces the `FOR UPDATE` waiting the
///   Postgres code relied on. sqlx's default is already 5s; it is set here so the
///   number is visible and does not silently depend on a dependency default.
///   Measured: `PRAGMA busy_timeout` reads back 5000.
///
/// `create_if_missing` is deliberately NOT set. Creation belongs to
/// `bin/migrate.rs`, which the deploy order runs before the server. Measured: with
/// it unset, connecting to an absent file fails with `unable to open database
/// file` — which is the failure this wants, because a server that silently
/// creates an empty, schema-less database fails later and far less legibly.
pub async fn init_pool(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(database_url)?
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .foreign_keys(true);

    SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
}

/// Begins a transaction that takes SQLite's write lock up front.
///
/// `pool.begin()` issues a DEFERRED `BEGIN`. A deferred transaction that reads
/// and then writes can be refused at the lock upgrade with
/// `SQLITE_BUSY_SNAPSHOT`, and that error **cannot be resolved by retrying** —
/// the transaction has to be rolled back and restarted (plan section 4.3, trap 2).
/// `BEGIN IMMEDIATE` takes the write lock at the start, so there is no upgrade to
/// lose.
///
/// Every transaction in this module writes, and every one of them now begins with
/// its write rather than a preceding read, so the specific hazard is already
/// absent. This helper is used anyway: it makes the lock acquisition explicit
/// rather than a consequence of statement ordering, so a later edit that adds a
/// read to the top of one of these functions cannot reintroduce the trap.
///
/// sqlx executes the statement and then verifies the connection really is in a
/// transaction, failing with `BeginFailed` otherwise (measured: a statement that
/// does not open one is refused), so a typo here is loud rather than silent.
pub(crate) async fn begin_immediate(
    pool: &SqlitePool,
) -> Result<Transaction<'static, Sqlite>, AppError> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
}

#[derive(Debug, PartialEq, Eq)]
pub enum TopupCreditResult {
    /// The topup was settled and the wallet credited.
    Settled { new_balance: i64 },
    /// This order was already settled - a replayed webhook. Nothing written.
    AlreadySettled,
    /// No such order.
    NotFound,
    /// The order exists but the stored amount disagrees with the webhook's.
    /// Nothing written.
    AmountMismatch,
    /// The order exists and the amount agrees, but its status is not `pending`,
    /// so there is nothing to settle. `denied`, `expired` and `refunded` all
    /// land here. Nothing was written.
    ///
    /// This variant exists because the settlement guard is now
    /// `status = 'pending'` in SQL, which is STRICTER than the Rust check it
    /// replaced. That check short-circuited only on `'settled'`, so a replayed
    /// settlement webhook arriving after a refund fell through to settlement and
    /// RE-CREDITED the wallet, flipping the row back to `settled` and duplicating
    /// money. The stricter predicate closes that. Reporting the closed case as
    /// `AlreadySettled` would be a lie about a refunded order, so it gets its own
    /// variant, mirroring `RefundResult::NotSettled`.
    NotSettleable { status: String },
}

/// Atomically settles a topup and credits the wallet, recording an append-only ledger row.
///
/// The whole decision is one conditional `UPDATE`. The Postgres original took a
/// row lock (`SELECT ... FOR UPDATE`) and then decided in Rust; SQLite has no
/// row locks and rejects `FOR UPDATE` as a syntax error (measured), so the guard
/// moves into the `WHERE` clause and `RETURNING` supplies the row identity the
/// credit needs. This needs no lock at all and is correct under any isolation
/// level: the write lock SQLite takes for the `UPDATE` is what serializes the
/// check against the write.
pub async fn credit_topup_transaction(
    pool: &SqlitePool,
    order_id: &str,
    webhook_amount_idr: i64,
) -> Result<TopupCreditResult, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 1. Settle the row in one statement. `status = 'pending'` and the amount
    //    check are the guard; `rows_affected()` decides whether it fired.
    let now = Utc::now();
    let settled = sqlx::query(
        "UPDATE topups SET status = 'settled', settled_at = ? \
         WHERE order_id = ? AND status = 'pending' AND amount_idr = ? \
         RETURNING id, account_id",
    )
    .bind(now)
    .bind(order_id)
    .bind(webhook_amount_idr)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(settled) = settled else {
        // 2. Nothing was written. One SELECT decides which refusal this is.
        //    Status is tested before the amount, matching the precedence the
        //    original Rust checks had: a `settled` row with a mismatched amount
        //    is a replay, not a mismatch.
        let existing = sqlx::query("SELECT status, amount_idr FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;

        tx.rollback().await?;

        return Ok(match existing {
            None => TopupCreditResult::NotFound,
            Some(row) => {
                let status: String = row.get("status");
                let stored_amount_idr: i64 = row.get("amount_idr");
                if status == "settled" {
                    TopupCreditResult::AlreadySettled
                } else if stored_amount_idr != webhook_amount_idr {
                    TopupCreditResult::AmountMismatch
                } else {
                    TopupCreditResult::NotSettleable { status }
                }
            }
        });
    };

    let topup_id: Uuid = settled.get::<Hyphenated, _>("id").into_uuid();
    let account_id: Uuid = settled.get::<Hyphenated, _>("account_id").into_uuid();

    // 3. Credit the wallet. `webhook_amount_idr` is the same value the guard
    //    proved equal to the stored amount, so it is what the ledger must carry.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(webhook_amount_idr)
    .bind(now)
    .bind(account_id.hyphenated())
    .fetch_one(&mut *tx)
    .await?;

    let new_balance: i64 = wallet.get("balance_idr");

    // 4. Append to ledger. `created_at` shares the settle instant, so the row,
    //    the wallet and the ledger all carry one timestamp.
    let ref_str = topup_id.to_string();
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'topup', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(webhook_amount_idr)
    .bind(ref_str)
    .bind(new_balance)
    .bind(now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(TopupCreditResult::Settled { new_balance })
}

/// What a refund attempt did.
#[derive(Debug, PartialEq, Eq)]
pub enum RefundResult {
    /// The wallet was debited and a `refund` ledger row appended.
    Refunded { new_balance: i64 },
    /// This order was already refunded - a replayed webhook. Nothing written.
    AlreadyRefunded,
    /// No such order.
    NotFound,
    /// The topup was never settled, so there is nothing to give back.
    NotSettled { status: String },
    /// The wallet cannot cover the refund: the money has already been spent.
    ///
    /// NOTHING was written - not the ledger, not the topup status - so the
    /// operator can see the topup still sitting in `settled` and resolve it by
    /// hand. Deliberately distinct from `Refunded`: a refund that cannot be
    /// applied is a real-world event, not a success.
    InsufficientBalance { balance_idr: i64, required_idr: i64 },
}

/// Whether a topup in `status` may be refunded, and if not, why.
#[derive(Debug, PartialEq, Eq)]
pub enum RefundDecision {
    Refund,
    AlreadyRefunded,
    NotSettled,
}

/// The pure decision behind the refund, split out so it is testable without a
/// database.
///
/// `refunded` is checked before `settled`: a second refund of the same order is
/// a replay, not a refund, and must not debit twice.
pub fn refund_decision(status: &str) -> RefundDecision {
    match status {
        "refunded" => RefundDecision::AlreadyRefunded,
        "settled" => RefundDecision::Refund,
        // `pending`, `denied`, `expired`: money never arrived, so there is
        // nothing to give back. Refunding these would create money.
        _ => RefundDecision::NotSettled,
    }
}

/// Atomically refunds a settled topup: debits the wallet and appends a `refund`
/// ledger row, in one transaction, so `balance_idr = SUM(delta_idr)` still holds.
///
/// The debit carries `balance_idr >= ?1` as a predicate on the UPDATE itself,
/// the same guard `debit_usage_transaction` uses: a concurrent request cannot race
/// the check, and `CHECK (balance_idr >= 0)` is the backstop rather than the thing
/// that refuses the debit (docs/decisions.md D3 - wallets are non-negative).
///
/// Idempotent under replay: the row is moved out of `settled` by a conditional
/// UPDATE, and only the statement that performs that move proceeds, so a second
/// refund of the same order is a no-op.
pub async fn refund_topup_transaction(
    pool: &SqlitePool,
    order_id: &str,
    amount_idr: i64,
) -> Result<RefundResult, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 1. Claim the refund by moving the row out of `settled`, in one conditional
    //    statement. The Postgres original locked the row with `FOR UPDATE` and
    //    then decided in Rust; SQLite has no row locks and rejects `FOR UPDATE`
    //    (measured), so the status transition IS the guard and the write lock it
    //    takes is what serializes two concurrent refunds.
    //
    //    Claiming before debiting is safe because both happen in this one
    //    transaction: the insufficient-balance path below rolls the claim back,
    //    leaving the topup `settled` exactly as the original did.
    let claimed = sqlx::query(
        "UPDATE topups SET status = 'refunded' WHERE order_id = ? AND status = 'settled' \
         RETURNING account_id",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(claimed) = claimed else {
        // 2. Nothing was written. One SELECT decides which refusal this is.
        let existing = sqlx::query("SELECT status FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_optional(&mut *tx)
            .await?;

        tx.rollback().await?;

        return Ok(match existing {
            None => RefundResult::NotFound,
            Some(row) => {
                let status: String = row.get("status");
                match refund_decision(&status) {
                    RefundDecision::AlreadyRefunded => RefundResult::AlreadyRefunded,
                    // `Refund` is unreachable: the claim above would have matched
                    // a `settled` row. Every other status had no money arrive, so
                    // there is nothing to give back.
                    RefundDecision::Refund | RefundDecision::NotSettled => {
                        RefundResult::NotSettled { status }
                    }
                }
            }
        });
    };

    let account_id: Uuid = claimed.get::<Hyphenated, _>("account_id").into_uuid();

    // 3. Debit the wallet. The guard is inside the statement: when it matches no
    //    row the account cannot cover the refund, and nothing may be written.
    //    `?1` is referenced twice, as the debit and as the floor (measured
    //    working, with three binds for `?1 ?2`).
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
    )
    .bind(amount_idr)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut *tx)
    .await?;

    let new_balance: i64 = match wallet {
        Some(w) => w.get("balance_idr"),
        None => {
            // The decision was already made by the predicate; this read only fills
            // in the error detail, and failing it must not turn a visible refusal
            // into an opaque 500.
            let balance_idr: i64 =
                sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                    .bind(account_id.hyphenated())
                    .fetch_optional(&mut *tx)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0);

            // Roll back explicitly: nothing is written - including the status
            // claim above - so the topup stays `settled` and the ledger gains no
            // row it cannot back.
            tx.rollback().await?;

            return Ok(RefundResult::InsufficientBalance {
                balance_idr,
                required_idr: amount_idr,
            });
        }
    };

    // 4. Append the refund row. `delta_idr` is negative: the ledger sums to the
    //    balance, and a refund takes money out.
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'refund', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(-amount_idr)
    .bind(order_id)
    .bind(new_balance)
    .bind(Utc::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(RefundResult::Refunded { new_balance })
}

/// What a settlement attempt actually did.
///
/// The signature used to be `i64` (the new balance). It had to change: the caller
/// must be able to tell a settlement that collected the full cost from one that
/// collected only what the wallet held, because only the second is a loss. A bare
/// balance made those two identical, which is how the shortfall went unnoticed.
#[derive(Debug, PartialEq, Eq)]
pub enum UsageSettlement {
    /// The wallet covered `cost_idr` in full.
    Settled { new_balance: i64 },
    /// The wallet could not cover `cost_idr` at the moment of settlement. The real
    /// token counters WERE recorded and the ledger gained a row for exactly
    /// `debited_idr`, so `balance_idr = SUM(ledger.delta_idr)` still holds;
    /// `shortfall_idr` is money that was consumed and not collected, and is always
    /// positive here. The balance is never negative (docs/decisions.md D3).
    Partial {
        new_balance: i64,
        debited_idr: i64,
        shortfall_idr: i64,
    },
}

/// The pure shortfall decision: how much of `cost_idr` the wallet can actually pay.
///
/// Split out from the SQL so the clamp is testable without a database, exactly as
/// `refund_decision` is. The rule is a clamp of the DEBIT, never of the balance:
/// the debit can be at most what the wallet holds, so the balance lands on 0 and
/// `CHECK (balance_idr >= 0)` is the backstop rather than the thing that refuses
/// the debit. Forcing the full debit would drive the balance negative, which
/// docs/decisions.md ratified as impossible.
///
/// A zero or negative balance debits nothing and the whole cost is shortfall: the
/// usage row is still written, because the tokens were genuinely consumed.
pub fn clamp_debit(cost_idr: i64, available_idr: i64) -> (i64, i64) {
    // A negative cost is not a charge; refusing to "collect" it must not turn into
    // a credit. Floored at zero, and a negative available balance debits nothing.
    let debited_idr = cost_idr.max(0).min(available_idr.max(0));
    (debited_idr, cost_idr - debited_idr)
}

/// The two ledger deltas a settlement writes, as a pure function of what was held
/// and what the request truly cost.
///
/// `reserve_balance_transaction` already wrote `-reserved_idr` when the request
/// started. Adding the two deltas here gives the whole request's net ledger move:
///
///   -reserved_idr + release_delta + charge_delta = -cost_idr
///
/// which is what `balance_idr = SUM(ledger.delta_idr)` requires at the commit
/// point. The release is written as its OWN row rather than folded into the charge
/// so the hold's reversal stays visible in an append-only log: `-reserved`,
/// `+reserved`, `-cost` is three auditable facts; `-cost` alone is one.
///
/// A negative argument is floored: a charge is never a credit and a release is
/// never a second hold.
pub fn settlement_ledger_deltas(released_idr: i64, cost_idr: i64) -> (i64, i64) {
    (released_idr.max(0), -cost_idr.max(0))
}

/// Atomically settles usage: releases the reservation, debits wallet, appends the
/// ledger rows, and upserts daily usage.
///
/// The balance check is a predicate on the UPDATE itself, so it cannot race a
/// concurrent request, and `CHECK (balance_idr >= 0)` is never the thing that
/// refuses the debit (which would surface as an opaque 500).
///
/// An unaffordable debit is NOT dropped: the reported usage is still recorded and
/// the debit is clamped to the balance (`UsageSettlement::Partial`). See
/// `clamp_debit` for why the debit is clamped rather than the balance forced.
///
/// `reserved_idr` is the hold `reserve_balance_transaction` took before the
/// request went upstream. Releasing it and charging the real cost happen in THIS
/// transaction, in that order, so no commit point ever shows a balance that the
/// ledger cannot explain: the hold is out of the wallet for the whole upstream
/// call, and the release row is written before the charge row.
#[allow(clippy::too_many_arguments)]
pub async fn debit_usage_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
    reserved_idr: i64,
) -> Result<UsageSettlement, AppError> {
    let mut tx = begin_immediate(pool).await?;

    // 0. Release the hold first. The guard is the same one the take used: the
    //    wallet can never have spent more than its own balance, so the release
    //    always matches when a hold was actually taken, and a zero reservation
    //    (nothing held) skips the statement entirely.
    let released_idr = if reserved_idr > 0 {
        match try_credit(&mut tx, account_id, reserved_idr).await? {
            Some(_) => reserved_idr,
            None => {
                // No wallet row: nothing was ever held, so there is nothing to
                // give back. Recording a release anyway would credit money the
                // ledger never took.
                error!(
                    account_id = %account_id,
                    reserved_idr,
                    "Reservation release matched no wallet; nothing released"
                );
                0
            }
        }
    } else {
        0
    };

    // 1. Debit wallet.
    //
    // `balance_idr >= ?1` is the guard and it lives inside the statement, not in a
    // preceding read: the statement is its own check, and because SQLite admits one
    // writer at a time, two racing debits cannot both pass against one stale
    // balance.
    let new_balance: i64 = match try_debit(&mut tx, account_id, cost_idr).await? {
        Some(new_balance) => new_balance,
        None => {
            // No row: the account cannot cover `cost_idr`, or it has no wallet. This
            // read only fills in the decision - the predicate already made it.
            let current_balance = read_balance(&mut tx, account_id).await?;

            // The answer has already been streamed to the client by the time this
            // runs (proxy.rs: settlement is detached, deliberately). Discarding the
            // usage the upstream reported here is a money defect: the request is
            // delivered for free AND leaves no ledger row and no usage_daily row, so
            // the reconciliation check in docs/observability.md sees nothing. That
            // is NOT the documented `washed` case (docs/failover.md:140-144), which
            // is an upstream that reported no usage at all.
            //
            // So record what actually happened, in this same transaction: the real
            // token counters and a ledger row for what was ACTUALLY debited. The
            // shortfall is logged at error level - a silent undercharge is the same
            // class of defect as a silent refund.
            return settle_partial_usage(
                tx,
                account_id,
                api_key_id,
                input_tokens,
                cache_read_tokens,
                output_tokens,
                cost_idr,
                ref_batch,
                released_idr,
                current_balance,
            )
            .await;
        }
    };

    // 2. Release the hold, append the ledger debit and the usage row, then commit.
    //    `released_idr` is what step 0 credited back; passing it here is what
    //    writes the matching `+hold` ledger row. Omitting it credits the wallet
    //    without a ledger row, which is exactly the drift this invariant catches.
    record_usage(
        tx,
        account_id,
        api_key_id,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cost_idr,
        ref_batch,
        released_idr,
        cost_idr,
        new_balance,
    )
    .await?;

    Ok(UsageSettlement::Settled { new_balance })
}

/// Writes the two rows a settlement owns, then commits: the append-only ledger
/// debit and the `usage_daily` upsert.
///
/// Shared by the settled and the partial path so the token counters (input,
/// cache_read and output kept separate - never summed), the daily cost and the
/// `balance_after` invariant cannot drift apart between them.
///
/// `usage_cost_idr` is what the request cost, and is what the dashboard and the
/// 30-day spend reporting read; `charged_idr` is what the ledger records. The two
/// differ only when the wallet could not cover the cost in full.
///
/// `released_idr` is a reservation being handed back, and it is written as its
/// own row BEFORE the charge. The hold was appended when the reservation was
/// taken, so reversing it here is what keeps the ledger invariant
/// (`balance_idr = SUM(ledger.delta_idr)`) true at the commit point: across the
/// whole request the ledger moves `-reserved + released - charged`, which is
/// exactly `-charged` because the whole hold comes back.
#[allow(clippy::too_many_arguments)]
async fn record_usage(
    mut tx: Transaction<'_, Sqlite>,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    usage_cost_idr: i64,
    ref_batch: Option<&str>,
    released_idr: i64,
    charged_idr: i64,
    new_balance: i64,
) -> Result<(), AppError> {
    // The release and the charge, from one pure rule so the ledger cannot drift:
    // `-reserved + release_delta + charge_delta` is exactly `-cost`.
    let (release_delta, charge_delta) = settlement_ledger_deltas(released_idr, charged_idr);

    if release_delta != 0 {
        // The balance the release left: the charge below has not been taken yet.
        insert_ledger_row(
            &mut tx,
            account_id,
            release_delta,
            ref_batch,
            new_balance - charge_delta,
        )
        .await?;
    }

    insert_ledger_row(&mut tx, account_id, charge_delta, ref_batch, new_balance).await?;

    // Upsert usage_daily.
    //
    // The conflict target is the expression index, not a column list. Measured:
    // the Postgres-shaped `ON CONFLICT (account_id, api_key_id, day)` is refused
    // outright with "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE
    // constraint", because the real key is
    // `usage_daily_scope_uniq (account_id, day, COALESCE(api_key_id, ''))`.
    // SQLite has no `ON CONFLICT ON CONSTRAINT <name>` form, so the expression
    // has to be spelled out. Measured: the expression form accumulates a NULL
    // key and a real key into two separate rows (15 and 10 from 10+5 and 7+3),
    // which is the behaviour the COALESCE index exists to provide.
    let today = Utc::now().date_naive();
    sqlx::query(
        r#"
        INSERT INTO usage_daily (
            account_id, api_key_id, day,
            input_tokens, cache_read_tokens, output_tokens, cost_idr
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT (account_id, day, COALESCE(api_key_id, '')) DO UPDATE
        SET input_tokens = usage_daily.input_tokens + EXCLUDED.input_tokens,
            cache_read_tokens = usage_daily.cache_read_tokens + EXCLUDED.cache_read_tokens,
            output_tokens = usage_daily.output_tokens + EXCLUDED.output_tokens,
            cost_idr = usage_daily.cost_idr + EXCLUDED.cost_idr
        "#,
    )
    .bind(account_id.hyphenated())
    .bind(api_key_id.map(|k| k.hyphenated()))
    .bind(today)
    .bind(input_tokens)
    .bind(cache_read_tokens)
    .bind(output_tokens)
    .bind(usage_cost_idr)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(())
}

/// The wallet balance, or 0 when the account has no wallet row.
async fn read_balance(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
) -> Result<i64, AppError> {
    // Annotated, not inferred: `unwrap_or(0)` alone would leave the scalar type to
    // default to i32, which is wrong for money. `balance_idr` is a 64-bit INTEGER,
    // and a balance above `i32::MAX` would fail to decode rather than read back.
    let balance: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
        .bind(account_id.hyphenated())
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(0);

    Ok(balance)
}

/// Debits `amount` with the balance guard inside the statement, returning the
/// balance after it, or `None` when the wallet cannot cover it.
///
/// `amount` of 0 updates nothing and returns the current balance: a zero debit is
/// a real outcome here (an empty wallet), not a reason to skip the statement.
async fn try_debit(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    // `?1` is referenced twice — once as the debit, once as the balance floor.
    // Measured: sqlx accepts the numbered form, takes three binds for `?1 ?2 ?3`,
    // and the reuse really does compare against the one value (a 150 wallet
    // debited by 100 leaves 50, and the same statement against 50 matches no
    // row). Plain `?` with a duplicated bind also works; the numbered form is
    // used because it cannot drift out of sync with the predicate.
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr - ?1, updated_at = ?2 WHERE account_id = ?3 AND balance_idr >= ?1 RETURNING balance_idr",
    )
    .bind(amount)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut **tx)
    .await?;

    Ok(wallet.map(|w| w.get("balance_idr")))
}

/// Appends one append-only ledger row. `balance_after` is the wallet balance the
/// row leaves behind, which is what makes the ledger self-explaining after a
/// crash: the running balance can be replayed without the wallet row.
async fn insert_ledger_row(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    delta_idr: i64,
    ref_batch: Option<&str>,
    balance_after: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO ledger (account_id, delta_idr, reason, ref, balance_after, created_at) VALUES (?, ?, 'usage', ?, ?, ?)",
    )
    .bind(account_id.hyphenated())
    .bind(delta_idr)
    .bind(ref_batch)
    .bind(balance_after)
    .bind(Utc::now())
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Credits `amount` to the wallet, returning the balance after it.
///
/// Unconditional on purpose: this is only ever a RESERVATION being handed back,
/// never money arriving from outside. A hold the wallet took itself can always be
/// returned, so there is nothing to guard against — unlike `try_debit`, which
/// must never let the balance go negative.
async fn try_credit(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: Uuid,
    amount: i64,
) -> Result<Option<i64>, AppError> {
    let wallet = sqlx::query(
        "UPDATE wallets SET balance_idr = balance_idr + ?, updated_at = ? WHERE account_id = ? RETURNING balance_idr",
    )
    .bind(amount)
    .bind(Utc::now())
    .bind(account_id.hyphenated())
    .fetch_optional(&mut **tx)
    .await?;

    Ok(wallet.map(|w| w.get("balance_idr")))
}

/// What taking a reservation did.
#[derive(Debug, PartialEq, Eq)]
pub enum ReservationResult {
    /// The wallet is debited by `reserved_idr` and the ledger holds the matching
    /// negative row. The money is out of the balance for the whole request, which
    /// is the point: a concurrent request sees it gone.
    Held { reserved_idr: i64, new_balance: i64 },
    /// Nothing was held: the wallet cannot cover `reserved_idr` right now, or the
    /// account has no wallet at all.
    Insufficient { balance_idr: i64 },
    /// Nothing to hold. A zero-amount reservation must not write a ledger row — a
    /// zero delta is noise in an append-only money log.
    Zero,
}

/// Takes the worst-case reservation BEFORE the request goes upstream, and returns
/// whether it was held.
///
/// This is the fix for the overdraw defect. The old check read `balance_idr` in
/// one statement and debited nothing, so N concurrent requests from one account
/// all passed the same point-in-time value and an account holding 1 IDR could run
/// unbounded expensive requests. Here the check IS the debit:
/// `balance_idr >= ?1` is a predicate on the UPDATE, and the UPDATE is its own
/// check: SQLite admits one writer at a time, so exactly as many concurrent
/// requests as the balance can pay for are admitted and the rest match no row.
/// Concurrency is serialized by the database's write lock, not by a read.
///
/// The hold is a real, guarded debit with its own ledger row, taken in one
/// transaction and committed before the upstream is called. `balance_idr` and
/// `SUM(ledger.delta_idr)` therefore move together, and a crash between the hold
/// and the settlement leaves the money debited and the row written — a visible
/// held reservation, never a balance the ledger cannot explain.
///
/// This is NOT `allow_negative_balance_overdraft`: the CHECK constraint is never
/// bypassed and the balance never goes negative (docs/decisions.md D3).
pub async fn reserve_balance_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<ReservationResult, AppError> {
    if reserved_idr <= 0 {
        return Ok(ReservationResult::Zero);
    }

    let mut tx = begin_immediate(pool).await?;

    // The guard lives inside the statement, never in a preceding read.
    match try_debit(&mut tx, account_id, reserved_idr).await? {
        Some(new_balance) => {
            insert_ledger_row(&mut tx, account_id, -reserved_idr, ref_batch, new_balance).await?;
            tx.commit().await?;

            Ok(ReservationResult::Held {
                reserved_idr,
                new_balance,
            })
        }
        None => {
            // No row matched: the decision is already made. This read only fills in
            // the detail the caller shows the customer, and failing it must not turn
            // a visible refusal into an opaque 500. Nothing was written, so there is
            // nothing to roll back beyond the empty transaction.
            let balance_idr = read_balance(&mut tx, account_id).await.unwrap_or(0);
            tx.rollback().await?;

            Ok(ReservationResult::Insufficient { balance_idr })
        }
    }
}

/// Gives a held reservation back IN FULL, in its own transaction.
///
/// Used on the paths where no billable usage exists: the upstream was never
/// reached, the stream ended without a usage report (the documented washed case,
/// docs/failover.md:138-144), or the settlement channel closed with no outcome.
/// The credit is the exact inverse of the guarded debit that took the hold, so the
/// ledger nets to zero and no money is created.
///
/// `Ok(None)` means nothing was released — a zero reservation, or no wallet row.
pub async fn release_reservation_transaction(
    pool: &SqlitePool,
    account_id: Uuid,
    reserved_idr: i64,
    ref_batch: Option<&str>,
) -> Result<Option<i64>, AppError> {
    if reserved_idr <= 0 {
        return Ok(None);
    }

    let mut tx = begin_immediate(pool).await?;

    let Some(new_balance) = try_credit(&mut tx, account_id, reserved_idr).await? else {
        // No wallet row: nothing was ever held, so nothing is released and no
        // ledger row is written. A credit the ledger cannot back is the one thing
        // this function must never do.
        tx.rollback().await?;
        return Ok(None);
    };

    insert_ledger_row(&mut tx, account_id, reserved_idr, ref_batch, new_balance).await?;
    tx.commit().await?;

    Ok(Some(new_balance))
}

/// Records usage the wallet could not cover in full, and returns what was lost.
///
/// Runs on the transaction `debit_usage_transaction` already opened: the guarded
/// UPDATE matching no row is the decision that the balance is short. The clamp is
/// computed from the balance read inside that same transaction.
#[allow(clippy::too_many_arguments)]
async fn settle_partial_usage(
    mut tx: Transaction<'_, Sqlite>,
    account_id: Uuid,
    api_key_id: Option<Uuid>,
    input_tokens: i64,
    cache_read_tokens: i64,
    output_tokens: i64,
    cost_idr: i64,
    ref_batch: Option<&str>,
    released_idr: i64,
    available_idr: i64,
) -> Result<UsageSettlement, AppError> {
    let (debited_idr, _) = clamp_debit(cost_idr, available_idr);

    // The clamped debit fits by construction, but it can still miss: a concurrent
    // settlement can take the balance between the caller's guarded UPDATE and the
    // read that sized this clamp, and a row failing the guard is not locked. Re-clamp
    // once against the balance as it is now. A second miss leaves the debit at zero,
    // which is the safe floor: a zero ledger delta cannot break
    // balance_idr = SUM(ledger.delta_idr) whatever the other transaction did, and the
    // usage is recorded either way - discarding it is the defect being fixed.
    let (debited_idr, new_balance) = match try_debit(&mut tx, account_id, debited_idr).await? {
        Some(new_balance) => (debited_idr, new_balance),
        None => {
            let available_now = read_balance(&mut tx, account_id).await?;
            let (retry_idr, _) = clamp_debit(cost_idr, available_now);
            match try_debit(&mut tx, account_id, retry_idr).await? {
                Some(new_balance) => (retry_idr, new_balance),
                None => (0, read_balance(&mut tx, account_id).await?),
            }
        }
    };

    // Recomputed rather than carried: the retry above can change what was debited.
    let shortfall_idr = cost_idr - debited_idr;

    // usage_daily carries the FULL cost: the tokens were consumed and the counters
    // drive the dashboard and the 30-day spend window (routes/keys.rs). The ledger
    // carries only what was actually taken, which is what keeps
    // balance_idr = SUM(ledger.delta_idr) true.
    record_usage(
        tx,
        account_id,
        api_key_id,
        input_tokens,
        cache_read_tokens,
        output_tokens,
        cost_idr,
        ref_batch,
        released_idr,
        debited_idr,
        new_balance,
    )
    .await?;

    // A concurrent topup can land between the failed attempt and the retry, in
    // which case the cost WAS collected in full and this is an ordinary
    // settlement. Reporting it as a shortfall would be a false alarm.
    if shortfall_idr <= 0 {
        return Ok(UsageSettlement::Settled { new_balance });
    }

    // Loud, and only after the transaction committed: this is now on the books.
    error!(
        account_id = %account_id,
        key_id = ?api_key_id,
        cost_idr,
        debited_idr,
        shortfall_idr,
        new_balance,
        "Usage settled PARTIALLY: balance could not cover the reported cost"
    );

    Ok(UsageSettlement::Partial {
        new_balance,
        debited_idr,
        shortfall_idr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, TestDb};

    /// Rows returned by the reconciliation check in docs/observability.md:
    /// wallets.balance_idr must equal SUM(ledger.delta_idr).
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

    /// Everything an operator needs to see when reconciliation fails: the wallet
    /// balance, the ledger sum, and every ledger row that produced it. A bare
    /// "drift" count says money is wrong but not which row is missing.
    async fn drift_report(pool: &SqlitePool, account_id: Uuid) -> String {
        let balance: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(pool)
                .await
                .expect("read balance");

        let ledger_sum: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("sum ledger");

        let rows: Vec<(i64, String, i64, Option<String>)> = sqlx::query_as(
            "SELECT delta_idr, reason, balance_after, ref FROM ledger WHERE account_id = ? ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .fetch_all(pool)
        .await
        .expect("read ledger rows");

        format!("balance_idr={balance} ledger_sum={ledger_sum} rows={rows:?}")
    }

    /// A debit the wallet cannot cover must NOT be discarded: the reported usage
    /// is recorded and the debit is clamped to the balance, so reconciliation
    /// still holds and the shortfall is visible. A debit it CAN cover still
    /// settles in full.
    ///
    /// Every reconciliation assertion is scoped to THIS fixture's `account_id`, not
    /// to the whole database: a global drift check would fail for a concurrent
    /// writer rather than for the code under test.
    #[tokio::test]
    async fn overdraft_debit_is_clamped_to_the_balance_and_records_the_usage() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        overdraft_settlement_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the test, minus the database its caller owns.
    async fn overdraft_settlement_assertions(pool: SqlitePool, account_id: Uuid) {
        let opening_balance: i64 = 1_000;

        // The fixture opens the wallet exactly the way production does, in two steps:
        // the zero-balance row the login path creates (routes/auth.rs), then a real
        // top-up. Money only ever enters a wallet through `credit_topup_transaction`,
        // which writes the matching `+` ledger row in the same transaction. Seeding
        // `wallets.balance_idr` directly manufactures the very drift this test then
        // asserts against - a fixture that cannot pass while the code under test is
        // correct. A zero-balance wallet with no ledger rows is consistent on its own
        // (0 = SUM of nothing), so this starting point reconciles.
        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, opening_balance).await;

        let credited = credit_topup_transaction(&pool, &order_id, opening_balance)
            .await
            .expect("credit the opening balance");

        assert_eq!(
            credited,
            TopupCreditResult::Settled {
                new_balance: opening_balance
            },
            "the fixture must open the wallet through the real top-up path"
        );

        // usage_daily.api_key_id is part of the primary key, so a real key row is
        // needed before any usage can be recorded.
        let key_id = test_support::api_key(&pool, account_id).await;

        // 1. A cost one rupiah above the wallet. The answer was already streamed
        //    to the client by now, so the usage must still be recorded.
        let cost_idr = opening_balance + 1;
        let outcome = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            cost_idr,
            Some("test_overdraft"),
            0,
        )
        .await
        .expect("a partial settlement is a recorded outcome, not an error");

        assert_eq!(
            outcome,
            UsageSettlement::Partial {
                new_balance: 0,
                debited_idr: opening_balance,
                shortfall_idr: 1,
            },
            "the debit must be clamped to the balance, with the rest a visible shortfall"
        );

        // 2. The balance is spent down to exactly zero, never negative.
        let balance_after: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read balance");
        assert_eq!(balance_after, 0);

        // 3. The ledger records only what was taken, and the usage row records the
        //    FULL cost and the real counters.
        //
        //    Scoped to the 'usage' row: the opening top-up also wrote a ledger row, so
        //    an unscoped read would find two and `fetch_one` would refuse it.
        let ledger_delta: i64 = sqlx::query_scalar(
            "SELECT delta_idr FROM ledger WHERE account_id = ? AND reason = 'usage'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("the clamped debit must still append a ledger row");
        assert_eq!(
            ledger_delta, -opening_balance,
            "the ledger must record exactly what was debited, not the full cost"
        );

        let (input, cache_read, output, usage_cost): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("the reported usage must still be recorded");
        assert_eq!(
            (input, cache_read, output, usage_cost),
            (200, 0, 150, cost_idr),
            "counters stay separate and the full cost is on the row"
        );

        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must still equal SUM(ledger.delta_idr)"
        );

        // 4. A debit the wallet CAN cover still settles in full. The clamp above left
        //    the wallet at zero, so refill it through the same real path the opening
        //    balance used - a second top-up, which writes its own `+` ledger row.
        //    Never write `balance_idr` alone: that is the drift the fixture must not
        //    create.
        let refill_order_id = test_support::pending_topup(&pool, account_id, opening_balance).await;

        credit_topup_transaction(&pool, &refill_order_id, opening_balance)
            .await
            .expect("refill the wallet");

        let settled_cost: i64 = 250;
        let outcome = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            settled_cost,
            Some("test_covered"),
            0,
        )
        .await
        .expect("an affordable debit must succeed");

        assert_eq!(
            outcome,
            UsageSettlement::Settled {
                new_balance: opening_balance - settled_cost
            }
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a settled debit"
        );

        // Teardown belongs to the caller, which runs it whether these assertions
        // pass or panic.
    }

    /// REAL CONCURRENCY PROOF for the overdraw defect, plus the per-settlement
    /// reconciliation invariant.
    ///
    /// The account is funded for EXACTLY ONE request, through the real top-up path
    /// — never by writing `balance_idr`, which would manufacture the very drift this
    /// test then asserts against. Five reservations are taken concurrently. The
    /// guarded UPDATE serializes them on the wallet row, so exactly one matches and
    /// the rest are refused. Before the fix all five passed a point-in-time read.
    ///
    /// Then the winner settles: the hold is released and the true cost charged in
    /// one transaction. `ledger_drift_rows` is asserted ZERO after every single
    /// settlement, sequential and concurrent, because a missing release row is
    /// exactly the money leak this invariant exists to catch.
    ///
    /// This is the test that was `#[ignore = "requires live Postgres"]` and so was
    /// never executed automatically. SQLite turned it into a temp file, so it now
    /// runs on every CI pass — which is the whole benefit plan section 5.5 claims.
    #[tokio::test]
    async fn concurrent_requests_cannot_overdraw_a_one_request_balance() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        overdraw_concurrency_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the concurrency test, minus the database its caller owns.
    async fn overdraw_concurrency_assertions(pool: SqlitePool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const CONCURRENCY: usize = 5;

        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, RESERVATION).await;

        assert_eq!(
            credit_topup_transaction(&pool, &order_id, RESERVATION)
                .await
                .expect("fund the wallet"),
            TopupCreditResult::Settled {
                new_balance: RESERVATION
            },
            "the fixture must fund the wallet through the real top-up path"
        );

        // Five at once, each asking for the whole balance: at most one can be held.
        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for i in 0..CONCURRENCY {
            let pool = pool.clone();
            let reference = format!("test_reserve_{i}");
            tasks.push(tokio::spawn(async move {
                reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&reference)).await
            }));
        }

        let mut held = 0;
        let mut refused = 0;
        for task in tasks {
            match task
                .await
                .expect("a reservation task panicked")
                .expect("reserve")
            {
                ReservationResult::Held { reserved_idr, .. } => {
                    assert_eq!(reserved_idr, RESERVATION);
                    held += 1;
                }
                ReservationResult::Insufficient { .. } => refused += 1,
                ReservationResult::Zero => panic!("a non-zero reservation was reported as zero"),
            }
        }

        assert_eq!(held, 1, "exactly one request may be funded by a one-request balance");
        assert_eq!(refused, CONCURRENCY - 1, "the rest must be refused");

        let balance: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read balance");
        assert_eq!(balance, 0, "the single hold consumed the whole balance");

        let held_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = ? AND delta_idr < 0")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("count holds");
        assert_eq!(held_rows, 1, "a refused reservation must write no ledger row");
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after concurrent holds"
        );

        // usage_daily.api_key_id is part of the primary key, so a real key row is
        // needed before any usage can be recorded.
        let key_id = test_support::api_key(&pool, account_id).await;

        // The winner settles: the hold comes back and the true cost is charged, in
        // ONE transaction. Drift must be zero immediately afterwards — a release
        // that credits the wallet without a ledger row is the leak being guarded.
        let cost_idr = 250;
        let settled = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            cost_idr,
            Some("test_reserve"),
            RESERVATION,
        )
        .await
        .expect("settle the winner");
        assert_eq!(
            settled,
            UsageSettlement::Settled {
                new_balance: RESERVATION - cost_idr
            }
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "the release and the charge must net to the true cost: {}",
            drift_report(&pool, account_id).await
        );

        // The release row must be ON THE BOOKS, not merely reflected in the balance:
        // the whole hold back out, and exactly the cost in.
        let release_row: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(delta_idr), 0) FROM ledger WHERE account_id = ? AND delta_idr > 0 AND reason = 'usage'",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&pool)
        .await
        .expect("sum release rows");
        assert_eq!(release_row, RESERVATION, "the release must write a +hold ledger row");

        // Now the same path repeatedly, asserting the invariant after EVERY
        // settlement rather than only at the end. A drift that appears mid-run and
        // is later masked is the failure mode this catches.
        for round in 0..5 {
            let refill = test_support::pending_topup(&pool, account_id, RESERVATION).await;
            credit_topup_transaction(&pool, &refill, RESERVATION)
                .await
                .expect("refill through the real top-up path");

            let reference = format!("test_round_reserve_{round}");
            let reservation =
                reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&reference))
                    .await
                    .expect("reserve");
            assert!(matches!(reservation, ReservationResult::Held { .. }));
            assert_eq!(
                ledger_drift_rows(&pool, account_id).await,
                0,
                "drift after the hold of round {round}"
            );

            debit_usage_transaction(
                &pool,
                account_id,
                Some(key_id),
                10,
                0,
                5,
                66,
                Some(&reference),
                RESERVATION,
            )
            .await
            .expect("settle");
            assert_eq!(
                ledger_drift_rows(&pool, account_id).await,
                0,
                "drift after settlement {round}"
            );
        }
    }

    /// A refund is only ever applied to a topup that actually settled, and only
    /// once. `refunded` is checked first, so a replayed webhook cannot debit
    /// twice; `pending`/`denied`/`expired` never had money, so refunding them
    /// would create money out of nothing.
    #[test]
    fn refund_decision_only_refunds_a_settled_topup_once() {
        assert_eq!(refund_decision("settled"), RefundDecision::Refund);

        // Replay: the second refund of the same order must not debit again.
        assert_eq!(refund_decision("refunded"), RefundDecision::AlreadyRefunded);

        // No money ever arrived for these.
        assert_eq!(refund_decision("pending"), RefundDecision::NotSettled);
        assert_eq!(refund_decision("denied"), RefundDecision::NotSettled);
        assert_eq!(refund_decision("expired"), RefundDecision::NotSettled);

        // An unknown status is refused rather than assumed refundable.
        assert_eq!(refund_decision("something-new"), RefundDecision::NotSettled);
    }

    /// The refund outcome must stay distinguishable: an operator has to be able
    /// to tell a completed refund from one the wallet could not cover, because
    /// the second leaves the topup `settled` and needs a human.
    #[test]
    fn refund_outcomes_are_distinct_and_carry_the_amounts() {
        assert_ne!(
            RefundResult::Refunded { new_balance: 0 },
            RefundResult::InsufficientBalance {
                balance_idr: 0,
                required_idr: 0
            }
        );
        assert_ne!(
            RefundResult::Refunded { new_balance: 1 },
            RefundResult::AlreadyRefunded
        );
        assert_ne!(
            RefundResult::NotFound,
            RefundResult::NotSettled {
                status: "pending".to_string()
            }
        );

        // The refusal carries both figures, so the log and the operator can see
        // the shortfall without another query.
        let refusal = RefundResult::InsufficientBalance {
            balance_idr: 1000,
            required_idr: 50000,
        };
        match refusal {
            RefundResult::InsufficientBalance {
                balance_idr,
                required_idr,
            } => {
                assert_eq!(balance_idr, 1000);
                assert_eq!(required_idr, 50000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// The clamp rule behind a settlement the balance cannot cover in full.
    ///
    /// This is the whole money decision and it is pure, so it is tested here
    /// without a database. The SQL path itself (the guarded UPDATE, the ledger
    /// insert, the usage_daily upsert) is covered by the database tests above,
    /// which since phase 5 run by default rather than behind `#[ignore]`.
    #[test]
    fn clamp_debit_collects_at_most_the_balance() {
        // Covered in full: nothing clamped, nothing lost.
        assert_eq!(clamp_debit(250, 1_000), (250, 0));

        // Exactly covered: the boundary must settle in full, not as a shortfall.
        assert_eq!(clamp_debit(1_000, 1_000), (1_000, 0));

        // One rupiah short: collect the balance, and the rest is the shortfall.
        assert_eq!(clamp_debit(1_001, 1_000), (1_000, 1));

        // Wildly unaffordable: still collect every rupiah available.
        assert_eq!(clamp_debit(50_000, 1_000), (1_000, 49_000));

        // Zero balance: debit nothing, the whole cost is shortfall. The usage row
        // is still written - the tokens were really consumed.
        assert_eq!(clamp_debit(50_000, 0), (0, 50_000));
        assert_eq!(clamp_debit(0, 0), (0, 0));

        // A negative balance cannot happen (CHECK balance_idr >= 0), but if it
        // ever did, the clamp must not turn the deficit into a credit.
        assert_eq!(clamp_debit(500, -10), (0, 500));

        // A negative cost is not a charge and must not become a credit.
        assert_eq!(clamp_debit(-500, 1_000), (0, -500));
    }

    /// The clamp never moves the balance below zero: what it collects plus what it
    /// leaves behind is always the full cost, and what it collects never exceeds
    /// what the wallet holds. This is the invariant the CHECK constraint backs.
    #[test]
    fn clamp_debit_never_overdraws_and_never_invents_money() {
        for cost in [0_i64, 1, 999, 1_000, 1_001, 250_000] {
            for available in [0_i64, 1, 999, 1_000, 1_001, 250_000] {
                let (debited, shortfall) = clamp_debit(cost, available);
                assert_eq!(
                    debited + shortfall,
                    cost,
                    "cost {cost} against {available} must fully account for the charge"
                );
                assert!(
                    debited <= available.max(0),
                    "cost {cost} against {available} debited more than the wallet holds"
                );
                assert!(debited >= 0, "a debit is never a credit");
            }
        }
    }

    /// The reservation arithmetic, as a pure rule: the two deltas a settlement
    /// writes must undo the hold and leave exactly the true cost behind.
    ///
    /// `reserve_balance_transaction` already wrote `-reserved` when the request
    /// started, so the whole request's net ledger move is
    /// `-reserved + release_delta + charge_delta`, and it must be `-cost` for
    /// `balance_idr = SUM(ledger.delta_idr)` to hold at the commit point.
    #[test]
    fn a_reservation_and_its_release_net_to_the_true_cost() {
        for reserved in [0_i64, 1, 66, 1_000, 250_000] {
            for cost in [0_i64, 1, 66, 999, 1_000, 250_000] {
                let (release_delta, charge_delta) = settlement_ledger_deltas(reserved, cost);

                assert_eq!(
                    -reserved + release_delta + charge_delta,
                    -cost,
                    "reserved {reserved}, cost {cost}: the ledger must net to the true cost"
                );
                assert!(release_delta >= 0, "a release is never a second hold");
                assert!(charge_delta <= 0, "a charge is never a credit");
            }
        }
    }

    /// Nothing is held when there is nothing to hold, so nothing is released: a
    /// zero-delta ledger row is noise in an append-only money log.
    #[test]
    fn a_zero_reservation_writes_no_release_row() {
        assert_eq!(settlement_ledger_deltas(0, 0), (0, 0));
        assert_eq!(settlement_ledger_deltas(0, 500), (0, -500));
    }

    /// A negative argument must never become money: a negative cost is not a
    /// charge and a negative release is not a hold being returned.
    #[test]
    fn a_negative_delta_is_floored_not_inverted() {
        assert_eq!(settlement_ledger_deltas(-100, -100), (0, 0));
        assert_eq!(settlement_ledger_deltas(-1, 0), (0, 0));
    }

    /// A reservation is either held or refused, and the refusal carries the
    /// balance the customer actually has, so the 402 detail is not a guess.
    #[test]
    fn a_refused_reservation_is_not_mistakable_for_a_held_one() {
        assert_ne!(
            ReservationResult::Held { reserved_idr: 0, new_balance: 0 },
            ReservationResult::Insufficient { balance_idr: 0 }
        );
        assert_ne!(ReservationResult::Zero, ReservationResult::Insufficient { balance_idr: 0 });

        match (ReservationResult::Insufficient { balance_idr: 1 }) {
            ReservationResult::Insufficient { balance_idr } => assert_eq!(balance_idr, 1),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// A partial settlement must stay distinguishable from a full one, and must
    /// carry the three figures an operator needs: what was taken, what was lost,
    /// and where the balance landed.
    #[test]
    fn a_partial_settlement_is_not_mistakable_for_a_full_one() {
        assert_ne!(
            UsageSettlement::Settled { new_balance: 0 },
            UsageSettlement::Partial {
                new_balance: 0,
                debited_idr: 0,
                shortfall_idr: 0
            }
        );

        match (UsageSettlement::Partial {
            new_balance: 0,
            debited_idr: 1_000,
            shortfall_idr: 49_000,
        }) {
            UsageSettlement::Partial {
                new_balance,
                debited_idr,
                shortfall_idr,
            } => {
                assert_eq!(new_balance, 0);
                assert_eq!(debited_idr, 1_000);
                assert_eq!(shortfall_idr, 49_000);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// REGRESSION for the two money-loss defects this fix closes:
    ///
    /// 1. FINDING 1/2 - a settlement that FAILS (or is cancelled) must release the
    ///    hold. The fix is the `ReservationGuard` in proxy.rs, whose Drop calls
    /// `release_quietly` -> `release_reservation_transaction`. This test drives
    /// that exact release path directly and proves the money comes back.
    /// 2. FINDING 3 - a settlement that PAIRS its release with the same `reserve_*`
    ///    ref leaves the detection query (`unpaired_hold_rows`) at ZERO, so a hold
    /// is never mistaken for lost money.
    ///
    /// The test would FAIL before the fix on both counts: `release_quietly` was
    /// never called on the failure arm (the hold stayed debited forever), and the
    /// settlement passed `ref_batch = None` so the hold row had no matching
    /// positive row and the detection query flagged it as stranded.
    #[tokio::test]
    async fn a_failed_or_paired_settlement_never_strands_the_hold() {
        let db = TestDb::new().await;
        let account_id = test_support::account(&db.pool).await;

        hold_never_strands_assertions(db.pool.clone(), account_id).await;

        db.close().await;
    }

    /// The body of the regression test, minus the database its caller owns.
    async fn hold_never_strands_assertions(pool: SqlitePool, account_id: Uuid) {
        const RESERVATION: i64 = 10_000;
        const COST: i64 = 250;

        test_support::wallet(&pool, account_id).await;

        let order_id = test_support::pending_topup(&pool, account_id, RESERVATION).await;
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, RESERVATION)
                .await
                .expect("fund the wallet"),
            TopupCreditResult::Settled {
                new_balance: RESERVATION
            },
            "fund the wallet through the real top-up path"
        );

        let key_id = test_support::api_key(&pool, account_id).await;

        // --- Scenario A: a settlement FAILS, the hold must come back. ---
        let failed_ref = format!("reserve_{}", Uuid::new_v4().simple());
        let held = reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&failed_ref))
            .await
            .expect("reserve");
        assert!(
            matches!(held, ReservationResult::Held { .. }),
            "the wallet must be able to cover the worst case"
        );

        // A taken hold with no release yet MUST be flagged as unpaired - that is
        // how an operator tells a live hold from lost money.
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect before release"),
            1,
            "a held reservation with no release must be reported as unpaired"
        );

        // Simulate the failed-settlement arm: proxy.rs calls release_quietly, which
        // calls exactly this. The balance must return to its pre-hold value.
        let pre_hold: i64 = sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&pool)
            .await
            .expect("read pre-release balance");
        release_reservation_transaction(&pool, account_id, RESERVATION, Some(&failed_ref))
            .await
            .expect("the failed settlement releases the hold");
        let after_release: i64 =
            sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
                .bind(account_id.hyphenated())
                .fetch_one(&pool)
                .await
                .expect("read post-release balance");
        assert_eq!(
            after_release, pre_hold + RESERVATION,
            "FINDING 1/2: a failed settlement must return the whole hold to the wallet"
        );
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect after release"),
            0,
            "after the release the detection query must find no stranded hold"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after the release"
        );

        // --- Scenario B: a settlement PAIRS its release with the reservation ref.
        let paired_ref = format!("reserve_{}", Uuid::new_v4().simple());
        let held = reserve_balance_transaction(&pool, account_id, RESERVATION, Some(&paired_ref))
            .await
            .expect("reserve");
        assert!(matches!(held, ReservationResult::Held { .. }));
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect before settle"),
            1,
            "the paired hold is unpaired until it settles"
        );

        let settled = debit_usage_transaction(
            &pool,
            account_id,
            Some(key_id),
            200,
            0,
            150,
            COST,
            // FINDING 3: the settlement passes the SAME ref the hold used, so the
            // release row carries it and the detection query stays at zero.
            Some(&paired_ref),
            RESERVATION,
        )
        .await
        .expect("settle the paired request");
        assert_eq!(settled, UsageSettlement::Settled { new_balance: RESERVATION - COST });
        assert_eq!(
            unpaired_hold_rows(&pool, account_id).await.expect("detect after settle"),
            0,
            "FINDING 3: a paired settlement must not leave a stranded hold"
        );
        assert_eq!(
            ledger_drift_rows(&pool, account_id).await,
            0,
            "balance_idr must equal SUM(ledger.delta_idr) after a paired settlement"
        );
    }

    /// REGRESSION for plan section 4.3, trap 3: a `NULL` `api_key_id` must not
    /// duplicate a usage row.
    ///
    /// SQLite does not enforce `NOT NULL` on a composite key, and it treats NULLs as
    /// distinct in unique indexes. So an upsert targeted at a plain
    /// `(account_id, api_key_id, day)` never fires its conflict clause for a
    /// NULL-keyed row and degrades into a plain insert — one row per call, forever,
    /// with a per-key aggregate that silently under-reports. Measured in the port:
    /// three identical upserts produced three rows.
    ///
    /// Latent rather than live today, because the proxy always passes a key. It
    /// becomes live the moment a caller passes `None`, which the signature allows.
    /// The `COALESCE` unique index plus a matching conflict target is the fix, and
    /// this is the test the naive port would have failed.
    #[tokio::test]
    async fn two_null_key_settlements_accumulate_into_one_usage_row() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        test_support::fund(&db.pool, account_id, 100_000).await;

        // Account-level usage, twice on the same day: no key.
        for _ in 0..2 {
            debit_usage_transaction(&db.pool, account_id, None, 100, 10, 200, 5_000, None, 0)
                .await
                .expect("a settlement against a funded wallet");
        }

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count usage rows");
        assert_eq!(
            rows, 1,
            "two NULL-keyed settlements must land on ONE row, not one each"
        );

        let (input, cache_read, output, cost): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT input_tokens, cache_read_tokens, output_tokens, cost_idr
             FROM usage_daily WHERE account_id = ?",
        )
        .bind(account_id.hyphenated())
        .fetch_one(&db.pool)
        .await
        .expect("read the accumulated row");
        assert_eq!(
            (input, cache_read, output, cost),
            (200, 20, 400, 10_000),
            "the second call must accumulate onto the first, not start a new row"
        );

        // A keyed settlement on the same day is a DIFFERENT scope and must get its
        // own row, so the per-key aggregate stays separate from the account-level
        // one. The fix must not collapse the two scopes into one.
        let key_id = test_support::api_key(&db.pool, account_id).await;
        debit_usage_transaction(
            &db.pool,
            account_id,
            Some(key_id),
            100,
            10,
            200,
            5_000,
            None,
            0,
        )
        .await
        .expect("a keyed settlement");

        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage_daily WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(&db.pool)
            .await
            .expect("count usage rows");
        assert_eq!(
            rows, 2,
            "a keyed row is a separate scope from the NULL-keyed one"
        );

        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);
        db.close().await;
    }

    /// REGRESSION for the money-duplication defect plan section 4.5 found while
    /// executing: settle → refund → the original SETTLEMENT webhook replays.
    ///
    /// The guard this replaced short-circuited only on `status == 'settled'`, so a
    /// row that had already been refunded fell through and was settled a second
    /// time: the wallet gained the top-up back and the row returned to `settled`,
    /// silently undoing the refund. The money then existed twice.
    ///
    /// The fix is the `status = 'pending'` predicate — any row that is not pending
    /// is refused — plus `TopupCreditResult::NotSettleable`, a fourth outcome,
    /// because reporting a refunded order as "already settled" is a lie an operator
    /// would act on.
    #[tokio::test]
    async fn a_settlement_replayed_after_a_refund_is_refused_and_credits_nothing() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        const AMOUNT: i64 = 10_000;

        let order_id = test_support::pending_topup(&db.pool, account_id, AMOUNT).await;

        assert_eq!(
            credit_topup_transaction(&db.pool, &order_id, AMOUNT)
                .await
                .expect("settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            }
        );
        assert_eq!(test_support::balance(&db.pool, account_id).await, AMOUNT);

        assert!(
            matches!(
                refund_topup_transaction(&db.pool, &order_id, AMOUNT)
                    .await
                    .expect("refund"),
                RefundResult::Refunded { new_balance: 0 }
            ),
            "a settled topup must refund"
        );
        assert_eq!(test_support::balance(&db.pool, account_id).await, 0);
        assert_eq!(
            test_support::ledger_sum(&db.pool, account_id).await,
            0,
            "a settle followed by a refund must net to zero, not to a credit"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);

        // The replayed SETTLEMENT. This is the defect: before the fix this
        // credited AMOUNT again and flipped the row back to `settled`.
        let replay = credit_topup_transaction(&db.pool, &order_id, AMOUNT)
            .await
            .expect("a replay is a recorded outcome, not an error");
        assert!(
            matches!(&replay, TopupCreditResult::NotSettleable { status } if status == "refunded"),
            "a settlement replayed after a refund must be refused: {replay:?}"
        );
        assert_eq!(
            test_support::balance(&db.pool, account_id).await,
            0,
            "the refund must stand: no money may be re-credited"
        );
        assert_eq!(ledger_drift_rows(&db.pool, account_id).await, 0);

        // The row itself must still say `refunded`, so an operator reading it does
        // not see a settled order that has already been paid back.
        let status: String = sqlx::query_scalar("SELECT status FROM topups WHERE order_id = ?")
            .bind(&order_id)
            .fetch_one(&db.pool)
            .await
            .expect("read the topup status");
        assert_eq!(
            status, "refunded",
            "the replay must not flip the row back to settled"
        );

        db.close().await;
    }

    /// REGRESSION for plan section 4.3, trap 2, and the real concurrency test
    /// section 9 check 4 still owed.
    ///
    /// A deferred `BEGIN` that reads and then writes can be refused at the lock
    /// upgrade with `SQLITE_BUSY_SNAPSHOT` — an error that **cannot be resolved by
    /// retrying**, because the transaction has to be rolled back and restarted. It
    /// appears only under contention, so no test that opens one transaction at a
    /// time can see it.
    ///
    /// Every transaction here takes the write lock up front through
    /// `begin_immediate`, so five concurrent refunds serialize instead of
    /// deadlocking on an upgrade: exactly one refunds, the rest observe the row
    /// already refunded, and **none returns an error**. That last clause is the
    /// assertion — an `Err` here would mean either the unrecoverable upgrade or a
    /// `database is locked` once the bounded `busy_timeout` wait ran out.
    #[tokio::test]
    async fn concurrent_refunds_serialize_without_losing_the_write_lock() {
        let db = TestDb::new().await;
        let account_id = test_support::account_with_wallet(&db.pool).await;
        const AMOUNT: i64 = 10_000;
        const CONCURRENCY: usize = 5;

        let order_id = test_support::pending_topup(&db.pool, account_id, AMOUNT).await;
        assert_eq!(
            credit_topup_transaction(&db.pool, &order_id, AMOUNT)
                .await
                .expect("settle"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            }
        );

        let mut tasks = Vec::with_capacity(CONCURRENCY);
        for _ in 0..CONCURRENCY {
            let pool = db.pool.clone();
            let order_id = order_id.clone();
            tasks.push(tokio::spawn(async move {
                refund_topup_transaction(&pool, &order_id, AMOUNT).await
            }));
        }

        let mut refunded = 0;
        let mut already = 0;
        for task in tasks {
            // This `expect` IS the check: a refund must either win the lock or see
            // the row already refunded. An error means the port lost the write lock.
            match task
                .await
                .expect("a refund task panicked")
                .expect("a refund must never error under contention")
            {
                RefundResult::Refunded { .. } => refunded += 1,
                RefundResult::AlreadyRefunded => already += 1,
                other => panic!("unexpected refund outcome: {other:?}"),
            }
        }

        assert_eq!(
            refunded, 1,
            "exactly one of five concurrent refunds may debit the wallet"
        );
        assert_eq!(
            already,
            CONCURRENCY - 1,
            "the rest must see the row already refunded"
        );

        assert_eq!(
            test_support::balance(&db.pool, account_id).await,
            0,
            "the refund must happen exactly once"
        );
        assert_eq!(
            ledger_drift_rows(&db.pool, account_id).await,
            0,
            "{}",
            drift_report(&db.pool, account_id).await
        );

        db.close().await;
    }
}

/// Verification query: confirms that wallet balance equals sum of ledger entries.
pub async fn verify_wallet_reconciliation(
    pool: &SqlitePool,
    account_id: Uuid,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        r#"
        SELECT
            w.balance_idr AS wallet_balance,
            COALESCE(SUM(l.delta_idr), 0) AS ledger_sum
        FROM wallets w
        LEFT JOIN ledger l ON l.account_id = w.account_id
        WHERE w.account_id = ?
        GROUP BY w.balance_idr
        "#,
    )
    .bind(account_id.hyphenated())
    .fetch_optional(pool)
    .await?;

    match row {
        Some(r) => {
            let wallet_balance: i64 = r.get("wallet_balance");
            let ledger_sum: i64 = r.get("ledger_sum");
            Ok(wallet_balance == ledger_sum)
        }
        None => Err(AppError::NotFound("Wallet not found".into())),
    }
}

/// Reconciliation sweep for STRANDED HOLDS - the money-loss defect this fix
/// closes (a reservation taken but never paired with a release or a charge, so
/// the customer's money sits debited against a request that was never billed).
///
/// The proxy reserves with ref = 'reserve_<uuid>' and writes a NEGATIVE
/// -reserved ledger row (reason = 'usage'). A healthy reservation is later
/// paired in debit_usage_transaction (releasing the hold in the SAME
/// transaction, writing a POSITIVE +reserved row under the SAME ref) or by
/// release_reservation_transaction. So a reserve ref with a negative row but NO
/// positive row under the same ref is money that left the wallet and came back
/// nowhere.
///
/// ledger.ref has no unique constraint (deliberately - many rows share one
/// reservation ref), so this correlated query is the only way to find the
/// stranded ones. Run it on a schedule; ZERO rows is the invariant. A non-zero
/// count means a release failed to land and an operator must investigate, or the
/// guard's fire-and-forget Drop (proxy.rs) did not reach the database.
pub async fn unpaired_hold_rows(pool: &SqlitePool, account_id: Uuid) -> Result<i64, AppError> {
    let count: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) FROM (
            SELECT l.account_id, l.ref AS r
            FROM ledger l
            WHERE l.account_id = ?
              AND l.ref LIKE 'reserve_%'
              AND l.delta_idr < 0
            GROUP BY l.account_id, r
            HAVING NOT EXISTS (
                SELECT 1 FROM ledger m
                WHERE m.account_id = l.account_id
                  AND m.ref = l.ref
                  AND m.delta_idr > 0
            )
        ) AS stranded
        "#,
    )
    .bind(account_id.hyphenated())
    .fetch_one(pool)
    .await?;
    Ok(count)
}
