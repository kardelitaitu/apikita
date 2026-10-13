//! The money path under ADVERSARIAL concurrency, driven against the real billing functions.
//!
//! WHY THIS EXISTS. Every prior claim about billing came from tests written by the same author as the
//! code, and `tools/reconcile/reconcile.sh` reporting "0 stranded holds" is reassuring AND is exactly
//! what a blind spot would report. So this drives the path the way a hostile client would - concurrent
//! holds against one wallet, settlement for less than was held, replayed settlement - and then asserts
//! the ledger reconciles exactly.
//!
//! THE MODEL, stated because getting it wrong would make the test vacuous. A hold is a NEGATIVE LEDGER
//! ROW under a `ref`; settlement (`debit_usage_transaction`) writes the matching positive row under the
//! SAME ref. There is no `reservations` table. A stranded hold is therefore an unpaired negative row,
//! which is what `db::unpaired_hold_rows` exists to find.
//!
//! THE CONTRACT THIS TEST MUST RESPECT, because violating it produces false findings rather than
//! defects. `release_reservation_transaction` credits UNCONDITIONALLY - it never checks whether the ref
//! was ever held, and `try_credit` has no idempotency guard. The repository knows this and says so at
//! `ReservationGuard::defuse` in `routes/proxy.rs`, described there as "the entire barrier": a
//! guard that releases after a committed settlement would credit the hold a SECOND time.
//!
//! So the safety property is NOT "release is idempotent" - it is **"a caller releases only refs it
//! reserved, and only once"**. A test that releases a ref which was never held will invent money and
//! then report it as a defect. The first version of this file made exactly that mistake: it released
//! all 40 burst refs when only the successful ones had been reserved, and the wallet grew by 21,000.
//!
//! VERIFIED, not assumed: the only PRODUCTION callers are `settle_after_stream` (settle) and
//! `release_quietly` (release), both in `routes/proxy.rs` and both reached only with the reservation
//! this request took. Every other call site is inside
//! a `mod tests` block. So the contract is enforced by construction on the production path.
//!
//! EVERY TEST ASSERTS ITS OWN PRECONDITION FIRST. An earlier attempt measured nothing at all because
//! its requests were 401ing before reaching the money path, and "all invariants hold" on an empty
//! ledger is true and meaningless.

use crate::db::{
    debit_usage_transaction, release_reservation_transaction, reserve_balance_transaction,
    ReservationResult, UsageSettlement,
};
use crate::test_support as support;
use sqlx::SqlitePool;
use uuid::Uuid;

/// Every negative ledger row must be paired by a later positive row under the same ref.
///
/// This is the stranded-hold invariant. `db::unpaired_hold_rows` is the production query; this is the
/// same shape restricted to one account so a failure names the fixture rather than the whole database.
async fn unpaired_holds(pool: &SqlitePool, account_id: Uuid) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger h
          WHERE h.account_id = ?
            AND h.delta_idr < 0
            AND NOT EXISTS (
                  SELECT 1 FROM ledger p
                   WHERE p.ref = h.ref AND p.delta_idr > 0 AND p.id > h.id)",
    )
    .bind(account_id.hyphenated())
    .fetch_one(pool)
    .await
    .expect("count unpaired holds")
}

/// The ledger invariant, in both directions.
async fn assert_ledger_consistent(pool: &SqlitePool, account_id: Uuid, why: &str) {
    let stored = support::balance(pool, account_id).await;
    let summed = support::ledger_sum(pool, account_id).await;
    assert_eq!(
        stored, summed,
        "{why}: wallets.balance_idr ({stored}) must equal the sum of ledger deltas ({summed}). A \
         mismatch means a balance moved without its ledger row, or the reverse"
    );
    assert!(
        stored >= 0,
        "{why}: the balance went negative ({stored}), which the CHECK constraint should make impossible"
    );
}

/// Settle a request: cost against the hold that was taken for it.
#[allow(clippy::too_many_arguments)]
async fn settle(
    pool: &SqlitePool,
    account: Uuid,
    key: Uuid,
    refb: &str,
    cost: i64,
    reserved: i64,
) -> UsageSettlement {
    debit_usage_transaction(
        pool,
        account,
        Some(key),
        "deepseek-chat",
        10,
        0,
        20,
        cost,
        Some(refb),
        reserved,
    )
    .await
    .expect("settle usage")
}

/// A hold that is never settled or released is a stranded hold - the failure the money path is shaped
/// to avoid.
#[tokio::test]
async fn a_concurrent_burst_never_strands_a_hold_and_never_overdraws() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;
    let account = support::account_with_wallet(pool).await;
    support::fund(pool, account, 100_000).await;
    assert_ledger_consistent(pool, account, "after funding").await;

    // 40 concurrent holds of 3_000 against a 100_000 wallet. Concurrency is the point: a read-then-write
    // guard would let two of these through on the same balance.
    const HOLD: i64 = 3_000;
    let mut tasks = Vec::new();
    for i in 0..40 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            reserve_balance_transaction(&pool, account, HOLD, Some(&format!("burst-{i}"))).await
        }));
    }

    let mut held = 0;
    // Track WHICH refs actually hold. Releasing a ref that was never reserved would CREDIT the wallet -
    // `release_reservation_transaction` does not check the ref exists - so the release loop below must
    // name the successful ones, exactly as the production caller does. The first version of this test
    // released all 40 and the wallet grew by 21,000, which is a property of the harness rather than a
    // defect in the code.
    let mut held_refs = Vec::new();
    for (i, t) in tasks.into_iter().enumerate() {
        match t.await.expect("the reserve task must not panic") {
            Ok(ReservationResult::Held { .. }) => {
                held += 1;
                held_refs.push(format!("burst-{i}"));
            }
            Ok(ReservationResult::Insufficient { .. }) | Ok(ReservationResult::Zero) => {}
            Err(e) => panic!("reserve failed unexpectedly: {e:?}"),
        }
    }
    assert!(
        held > 0,
        "PRECONDITION FAILED: not one of 40 concurrent holds succeeded, so this test measured nothing"
    );
    assert!(
        held * HOLD <= 100_000,
        "OVERDRAW: {held} holds of {HOLD} = {} against a 100_000 wallet",
        held * HOLD
    );
    assert_eq!(
        held_refs.len() as i64,
        held,
        "internal: every Held must contribute exactly one ref"
    );

    // Release exactly the holds that were taken - the clean way out, and the production contract.
    for r in &held_refs {
        release_reservation_transaction(pool, account, HOLD, Some(r))
            .await
            .expect("release");
    }

    assert_eq!(
        unpaired_holds(pool, account).await,
        0,
        "every hold was released, so no negative row may be left without its pair"
    );
    assert_ledger_consistent(pool, account, "after the burst and release").await;
    assert_eq!(
        support::balance(pool, account).await,
        100_000,
        "releasing every hold that was TAKEN must restore the starting balance exactly; a surplus here \
         means a release credited a ref that was never reserved"
    );

    db.close().await;
}

/// The balance is never overdrawn under concurrent holds.
///
/// WHAT THIS TEST ACTUALLY PINS, established by mutation rather than assumed. MEASURED: removing the
/// `AND balance_idr >= ?1` floor from `try_debit` leaves this test PASSING with an identical outcome
/// (held = 10 of 20, final balance 0, zero errors). So the in-statement guard is NOT what makes this
/// safe - `begin_immediate` is: `BEGIN IMMEDIATE` takes the write lock at the start of every reserve
/// transaction, so the twenty concurrent holds run strictly one at a time, the wallet reaches zero, and
/// the rest are refused. The guard is defence in depth.
///
/// That distinction matters because the repository calls the CHECK constraint a BACKSTOP, "the layer
/// that catches you when the layer above fails". A test cannot show a guard works by observing an
/// outcome the serialisation also produces. What this test CAN honestly pin is the OUTCOME - no
/// overdraw, no negative balance, ledger consistent across concurrently issued holds - which is the
/// property a customer depends on whichever layer delivers it.
///
/// WHAT IT WOULD CATCH: a reserve that did not run in a transaction, a `try_debit` that read the balance
/// and then wrote outside a lock, or a settlement path that credited before checking. Each changes the
/// outcome, and the outcome is what is asserted.
#[tokio::test]
async fn concurrent_holds_never_exceed_the_balance_they_share() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;
    let account = support::account_with_wallet(pool).await;
    support::fund(pool, account, 10_000).await;

    const HOLD: i64 = 1_000;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            reserve_balance_transaction(&pool, account, HOLD, Some(&format!("cap-{i}"))).await
        }));
    }

    let mut held = 0i64;
    let mut refs = Vec::new();
    for (i, t) in tasks.into_iter().enumerate() {
        match t.await.expect("no panic") {
            Ok(ReservationResult::Held { .. }) => {
                held += 1;
                refs.push(format!("cap-{i}"));
            }
            Ok(ReservationResult::Insufficient { .. }) | Ok(ReservationResult::Zero) => {}
            Err(e) => panic!(
                "a hold that cannot be covered must be REFUSED, not an error: {e:?}. An error means \
                 the overdraw was attempted and rejected lower down - the backstop working rather than \
                 the guard"
            ),
        }
    }

    assert!(held > 0, "PRECONDITION FAILED: nothing was held");
    assert!(
        held * HOLD <= 10_000,
        "OVERDRAW: {held} holds of {HOLD} = {} against a 10_000 wallet",
        held * HOLD
    );
    assert_eq!(
        support::balance(pool, account).await,
        10_000 - held * HOLD,
        "the wallet must hold exactly what the successful holds left in it"
    );
    assert_ledger_consistent(pool, account, "after the cap test").await;

    // The round trip: releasing what was taken restores the start, and nothing else moved.
    for r in &refs {
        release_reservation_transaction(pool, account, HOLD, Some(r))
            .await
            .expect("release");
    }
    assert_eq!(
        support::balance(pool, account).await,
        10_000,
        "releasing every hold that was taken must restore the starting balance exactly"
    );
    assert_ledger_consistent(pool, account, "after releasing the cap holds").await;

    db.close().await;
}

/// Settlement for LESS than was held - the path a mid-stream client failure takes. The unused remainder
/// must come back.
///
/// WHY THIS DOES NOT ASSERT "no unpaired holds", which the first version did and got wrong.
/// `debit_usage_transaction` pairs the hold in the same transaction by writing a POSITIVE row for the
/// FULL `reserved_idr` (the release), alongside the charge. The settlement row and the release row are
/// two separate positive rows under one ref, and a partial cost does not reduce the release. So what
/// must be true is the LEDGER INVARIANT - wallet equals the sum of deltas - plus the exact final balance.
/// Counting "unpaired" rows here measures the wrong thing.
#[tokio::test]
async fn settling_for_less_than_the_hold_returns_the_difference() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;
    let account = support::account_with_wallet(pool).await;
    let key = support::api_key(pool, account).await;
    support::fund(pool, account, 50_000).await;

    let refb = "partial-1";
    match reserve_balance_transaction(pool, account, 5_000, Some(refb))
        .await
        .expect("reserve")
    {
        ReservationResult::Held { .. } => {}
        other => panic!("PRECONDITION FAILED: expected a hold, got {other:?} - nothing to settle"),
    }
    let after_hold = support::balance(pool, account).await;
    assert_eq!(
        after_hold, 45_000,
        "PRECONDITION FAILED: the hold did not leave the wallet, so settlement would prove nothing"
    );

    let outcome = settle(pool, account, key, refb, 1_200, 5_000).await;
    assert!(
        matches!(outcome, UsageSettlement::Settled { .. }),
        "a settlement below the hold must settle cleanly, got {outcome:?}"
    );

    assert_ledger_consistent(pool, account, "after settling below the hold").await;
    assert_eq!(
        support::balance(pool, account).await,
        50_000 - 1_200,
        "settlement must charge exactly the true cost and return the rest of the hold"
    );

    // And the hold must be FULLY paired, so nothing is left stranded: at least one positive row under
    // the same ref, later than the negative one.
    let positives: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger WHERE account_id = ? AND ref = ? AND delta_idr > 0",
    )
    .bind(account.hyphenated())
    .bind(refb)
    .fetch_one(pool)
    .await
    .expect("count the pairing rows");
    assert!(
        positives >= 1,
        "the hold must be paired by at least one positive row under the same ref, found {positives} \
         - an unpaired hold is money that left the wallet and came back nowhere"
    );

    db.close().await;
}

/// The REAL idempotency boundary, stated as the repository states it.
///
/// `debit_usage_transaction` is NOT idempotent per ref, and the first version of this test asserted that
/// it was - which produced a false "charged twice" finding. What it actually does: step 0 releases the
/// hold in the same transaction, so calling it again with the same ref and the same `reserved_idr`
/// releases that hold a SECOND time and credits the wallet. That is `release_reservation_transaction`
/// crediting unconditionally, reached through settlement.
///
/// What protects the money is therefore NOT the function - it is that only `settle_after_stream` in
/// `routes/proxy.rs` calls it, once per request, with the reservation that request took. This test pins
/// that boundary so the assumption
/// is recorded rather than implied: a SECOND settlement of the same ref moves the balance UPWARD, and
/// anyone who later adds a retry must handle that.
///
/// It is a characterisation test, not an approval: it fails if the behaviour changes, which is what
/// should happen if someone makes settlement genuinely idempotent.
#[tokio::test]
async fn a_second_settlement_of_the_same_ref_is_not_idempotent_and_that_is_the_documented_contract()
{
    let db = support::TestDb::new().await;
    let pool = &db.pool;
    let account = support::account_with_wallet(pool).await;
    let key = support::api_key(pool, account).await;
    support::fund(pool, account, 50_000).await;

    let refb = "replay-1";
    match reserve_balance_transaction(pool, account, 5_000, Some(refb))
        .await
        .expect("reserve")
    {
        ReservationResult::Held { .. } => {}
        other => panic!("PRECONDITION FAILED: expected a hold, got {other:?}"),
    }

    let _ = settle(pool, account, key, refb, 2_000, 5_000).await;
    let after_first = support::balance(pool, account).await;
    assert_eq!(
        after_first,
        50_000 - 2_000,
        "PRECONDITION FAILED: the first settlement did not charge what it should have, so the \
         comparison below would be meaningless"
    );

    // The second settlement re-releases the hold. Documented shape: the balance RISES by the hold.
    let second = settle(pool, account, key, refb, 2_000, 5_000).await;
    let after_second = support::balance(pool, account).await;

    assert_ne!(
        after_first, after_second,
        "SETTLEMENT IS NOW IDEMPOTENT. If that is deliberate, this characterisation test must be \
         rewritten to assert the new contract (second call returned {second:?}) - and the retry logic \
         in proxy.rs should be re-read, because it was written against the old one"
    );
    assert_eq!(
        after_second,
        after_first + 5_000 - 2_000,
        "the second settlement did BOTH things again: it re-released the 5,000 hold (+5,000) and \
         re-charged the 2,000 cost (-2,000), so the net is +3,000. This is the documented consequence \
         of release crediting unconditionally, reached through settlement - NOT a bug, because nothing \
         in production settles a ref twice"
    );

    // The invariant that DOES hold throughout: the ledger and the wallet never disagree.
    assert_ledger_consistent(pool, account, "after the second settlement").await;

    db.close().await;
}

/// A settlement that costs MORE than was held must not overdraw. `docs/decisions.md` D3 says the
/// balance never goes negative, and this is the path where the clamp does real work.
#[tokio::test]
async fn a_settlement_above_the_hold_never_overdraws() {
    let db = support::TestDb::new().await;
    let pool = &db.pool;
    let account = support::account_with_wallet(pool).await;
    let key = support::api_key(pool, account).await;
    // A small wallet so the shortfall path is reachable.
    support::fund(pool, account, 3_000).await;

    let refb = "over-1";
    match reserve_balance_transaction(pool, account, 2_000, Some(refb))
        .await
        .expect("reserve")
    {
        ReservationResult::Held { .. } => {}
        other => panic!("PRECONDITION FAILED: expected a hold, got {other:?}"),
    }

    // The request cost far more than the hold and more than the wallet.
    let outcome = settle(pool, account, key, refb, 100_000, 2_000).await;

    let balance = support::balance(pool, account).await;
    assert!(
        balance >= 0,
        "the balance went negative ({balance}) settling above the hold, which D3 forbids"
    );
    assert!(
        matches!(
            outcome,
            UsageSettlement::Partial { .. } | UsageSettlement::Settled { .. }
        ),
        "settling above the hold must produce a bounded outcome, got {outcome:?}"
    );
    assert_ledger_consistent(pool, account, "after settling above the hold").await;

    db.close().await;
}
