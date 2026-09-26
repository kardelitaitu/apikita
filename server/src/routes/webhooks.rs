use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use std::env;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::db::{
    credit_topup_transaction, refund_topup_transaction, RefundResult, TopupCreditResult,
};
use crate::error::AppError;
use crate::money::{
    evaluate_payment_status, verify_midtrans_signature, MidtransNotification, PaymentAction,
};
use crate::routes::events::publish_balance;
use crate::routes::proxy::AppState;

/// The JSON body for a refusal that leaves no money movement ambiguous.
fn error_body(code: &str, message: &str) -> Json<serde_json::Value> {
    Json(json!({ "error": code, "message": message }))
}
/// Whether a configured secret is usable for signature verification.
///
/// `env::var` yields `Ok("")` for a variable that is SET BUT EMPTY, and an
/// empty key is catastrophic here: the published Midtrans formula
/// (docs/website/04-payments.md:40) is SHA512(order_id + status_code +
/// gross_amount + server_key), so with an empty key anyone can compute a
/// matching signature and forge a `settlement` notification that credits a
/// wallet. Absent and empty must therefore fail identically.
///
/// A key is taken VERBATIM - never trimmed into use. Whitespace is legitimate
/// key material, so trimming could silently change a valid secret; a
/// whitespace-only value carries no secret and is refused.
fn is_usable_server_key(key: &str) -> bool {
    !key.trim().is_empty()
}

/// The account that owns a topup order, or None when the order does not exist.
///
/// A read-only lookup for the realtime publish, which happens AFTER the money
/// transaction committed. The event must be scoped to the owning account or the
/// per-account subscriber filter in events.rs (DEFECT 1) silently drops it.
/// This cannot fail the webhook: the money is already settled, and a missing
/// account_id only means the dashboard waits for its next snapshot.
async fn topup_account_id(pool: &sqlx::PgPool, order_id: &str) -> Option<Uuid> {
    sqlx::query_scalar::<_, Uuid>("SELECT account_id FROM topups WHERE order_id = $1")
        .bind(order_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// The balance to announce for a topup credit outcome, or None when nothing
/// changed. Only a fresh settle moves the wallet: a replayed webhook
/// (`AlreadySettled`), an amount mismatch and a missing order all wrote
/// nothing and must publish nothing (docs/realtime.md:146-157).
fn credit_balance_to_publish(result: &Result<TopupCreditResult, AppError>) -> Option<i64> {
    match result {
        Ok(TopupCreditResult::Settled { new_balance }) => Some(*new_balance),
        _ => None,
    }
}

/// The balance to announce for a refund outcome, or None when nothing changed.
/// A replayed refund (`AlreadyRefunded`), a refund whose amount disagrees with
/// the stored row (`AmountMismatch` - the 400 path) and a refund the wallet
/// cannot cover (`InsufficientBalance` - the 409 path) all wrote nothing, so
/// none of them may announce a balance that did not move.
fn refund_balance_to_publish(result: &Result<RefundResult, AppError>) -> Option<i64> {
    match result {
        Ok(RefundResult::Refunded { new_balance }) => Some(*new_balance),
        _ => None,
    }
}

pub async fn handle_midtrans_webhook(
    State(state): State<AppState>,
    Json(payload): Json<MidtransNotification>,
) -> impl IntoResponse {
    // The handler works on the pool throughout; only the realtime publish needs
    // the hub, which is why the extractor is AppState rather than PgPool.
    let pool = &state.pool;
    let server_key = match env::var("MIDTRANS_SERVER_KEY") {
        Ok(k) if is_usable_server_key(&k) => k,
        Ok(_) => {
            // Configured but empty or whitespace-only: distinct from ABSENT so
            // an operator can tell the two misconfigurations apart. Never fall
            // through to verification - an empty key makes the signature
            // worthless and a forged notification would credit a wallet.
            error!(
                "MIDTRANS_SERVER_KEY is configured but empty or whitespace-only; \
                 refusing to verify signatures against an empty secret"
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server misconfigured"})),
            );
        }
        Err(_) => {
            error!("MIDTRANS_SERVER_KEY environment variable is not configured");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server misconfigured"})),
            );
        }
    };

    // 1. Signature verification
    // The comparison is byte-wise over the lowercase hex `hex::encode` emits, so
    // an UPPERCASE-hex signature is rejected. Midtrans documents lowercase, so
    // this is strict and correct - but it is intentional, not incidental.
    if !verify_midtrans_signature(&payload, &server_key) {
        warn!(
            order_id = %payload.order_id,
            "Midtrans webhook rejected: invalid signature"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid signature"})),
        );
    }

    // 2. Evaluate status
    let gross_idr: i64 = match payload
        .gross_amount
        .split('.')
        .next()
        .unwrap_or("0")
        .parse()
    {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "invalid gross_amount format"})),
            );
        }
    };

    let action = evaluate_payment_status(&payload.transaction_status, gross_idr);
    match action {
        PaymentAction::Credit { amount_idr } => {
            let result = credit_topup_transaction(pool, &payload.order_id, amount_idr).await;

            // Only a fresh settle moves the wallet, so only a fresh settle is
            // announced. The value is the POST-credit balance the transaction
            // returned, never a stale pre-write read; it is absolute, not a
            // delta (docs/realtime.md:93,146-157); and it is scoped to the
            // owning account, without which the per-account subscriber filter
            // in events.rs (DEFECT 1) would silently drop it.
            if let Some(new_balance) = credit_balance_to_publish(&result) {
                if let Some(account_id) = topup_account_id(pool, &payload.order_id).await {
                    publish_balance(&state.events, account_id, new_balance);
                }
            }

            match result {
                Ok(TopupCreditResult::Settled { new_balance }) => {
                    info!(
                        order_id = %payload.order_id,
                        new_balance,
                        "Successfully credited topup"
                    );

                    (StatusCode::OK, Json(json!({"status": "settled"})))
                }
                Ok(TopupCreditResult::AlreadySettled) => {
                    info!(
                        order_id = %payload.order_id,
                        "Webhook idempotency: order already settled"
                    );
                    (StatusCode::OK, Json(json!({"status": "already_settled"})))
                }
                Ok(TopupCreditResult::NotFound) => {
                    warn!(
                        order_id = %payload.order_id,
                        "Webhook rejected: order not found"
                    );
                    (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "order not found"})),
                    )
                }
                Ok(TopupCreditResult::AmountMismatch) => {
                    error!(
                        order_id = %payload.order_id,
                        "Webhook rejected: amount mismatch with stored record"
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "amount mismatch"})),
                    )
                }
                Err(err) => {
                    error!(
                        order_id = %payload.order_id,
                        error = %err,
                        "Failed to process topup transaction"
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "internal processing error"})),
                    )
                }
            }
        }
        PaymentAction::DebitRefund { amount_idr } => {
            info!(
                order_id = %payload.order_id,
                amount_idr,
                "Received refund status from Midtrans webhook"
            );

            let result = refund_topup_transaction(pool, &payload.order_id, amount_idr).await;

            // Only a committed debit moves the wallet: a replayed refund
            // (AlreadyRefunded) and the 409 insufficient-balance path wrote
            // nothing, so they publish nothing. Absolute value, scoped to the
            // owning account (docs/realtime.md:93,146-157; DEFECT 1).
            if let Some(new_balance) = refund_balance_to_publish(&result) {
                if let Some(account_id) = topup_account_id(pool, &payload.order_id).await {
                    publish_balance(&state.events, account_id, new_balance);
                }
            }

            match result {
                Ok(RefundResult::Refunded { new_balance }) => {
                    info!(
                        order_id = %payload.order_id,
                        amount_idr,
                        new_balance,
                        "Refund debited the wallet and appended a ledger row"
                    );

                    (StatusCode::OK, Json(json!({ "status": "refunded" })))
                }
                Ok(RefundResult::AlreadyRefunded) => {
                    info!(
                        order_id = %payload.order_id,
                        "Refund idempotency: order already refunded"
                    );
                    (
                        StatusCode::OK,
                        Json(json!({ "status": "already_refunded" })),
                    )
                }
                Ok(RefundResult::NotFound) => {
                    warn!(
                        order_id = %payload.order_id,
                        "Refund rejected: order not found"
                    );
                    (
                        StatusCode::NOT_FOUND,
                        error_body("order_not_found", "order not found"),
                    )
                }
                Ok(RefundResult::NotSettled { status }) => {
                    error!(
                        order_id = %payload.order_id,
                        status = %status,
                        "Refund rejected: topup never settled, so there is nothing to refund"
                    );
                    (
                        StatusCode::CONFLICT,
                        error_body("topup_not_settled", "topup was never settled"),
                    )
                }
                Ok(RefundResult::AmountMismatch) => {
                    // The refund branch used to debit whatever `gross_amount` the
                    // payload carried, so a signed notification naming more than
                    // the top-up drained the wallet. Same refusal, and the same
                    // body, as the credit branch's mismatch above: the amount
                    // comes from OUR stored row, never the payload
                    // (docs/server/api-spec.md:284, :295).
                    error!(
                        order_id = %payload.order_id,
                        amount_idr,
                        "Refund rejected: amount mismatch with stored record"
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "amount mismatch"})),
                    )
                }
                Ok(RefundResult::InsufficientBalance {
                    balance_idr,
                    required_idr,
                }) => {
                    // The money is already spent. NOTHING was written: the topup
                    // stays `settled` and the ledger gained no row it cannot back,
                    // so the operator can see this and resolve it by hand. This is
                    // deliberately NOT a 200 - a refund that cannot be applied must
                    // be visible, which is the whole lesson of Bug B.
                    error!(
                        order_id = %payload.order_id,
                        balance_idr,
                        required_idr,
                        "REFUND COULD NOT BE APPLIED: wallet cannot cover it; topup left settled for manual resolution"
                    );
                    (
                        StatusCode::CONFLICT,
                        error_body(
                            "refund_insufficient_balance",
                            "wallet balance cannot cover this refund",
                        ),
                    )
                }
                Err(err) => {
                    error!(
                        order_id = %payload.order_id,
                        error = %err,
                        "Failed to process refund transaction"
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_body("internal_processing_error", "internal processing error"),
                    )
                }
            }
        }
        // The schema value travels with the action: `evaluate_payment_status`
        // only produces this variant for a status `terminal_status` mapped, so
        // there is no unmappable case left to forget.
        PaymentAction::TerminalNoAction { status } => {
            info!(
                order_id = %payload.order_id,
                midtrans_status = %payload.transaction_status,
                status = %status,
                "Terminal non-credit status recorded"
            );

            // The write is NOT swallowed: a failed persist must not answer 200.
            match sqlx::query(
                "UPDATE topups SET status = $1 WHERE order_id = $2 AND status = 'pending'",
            )
            .bind(status)
            .bind(&payload.order_id)
            .execute(pool)
            .await
            {
                Ok(result) if result.rows_affected() > 0 => (
                    StatusCode::OK,
                    Json(json!({ "status": "terminal_recorded" })),
                ),
                Ok(_) => {
                    // The row exists but was not `pending` - already settled,
                    // refunded, or already terminal. Nothing to record, and
                    // importantly nothing overwritten.
                    info!(
                        order_id = %payload.order_id,
                        status = %status,
                        "Terminal status not applied: topup is no longer pending"
                    );
                    (
                        StatusCode::OK,
                        Json(json!({ "status": "terminal_not_applicable" })),
                    )
                }
                Err(err) => {
                    error!(
                        order_id = %payload.order_id,
                        status = %status,
                        error = %err,
                        "Failed to record terminal status"
                    );
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error_body("internal_processing_error", "internal processing error"),
                    )
                }
            }
        }
        // A status Midtrans sent that this server does not know. Before this arm
        // existed, such a value fell through to `Pending`: the topup stayed
        // `pending` forever, the handler answered 200, and nothing was logged.
        PaymentAction::Unrecognised => {
            // WARN, not info: an unknown status is an operational signal, and the
            // raw value is included so the set can be extended deliberately.
            warn!(
                order_id = %payload.order_id,
                status = %payload.transaction_status,
                "Unrecognised Midtrans transaction_status: not classified as pending"
            );
            (
                StatusCode::OK,
                Json(json!({
                    "status": "unrecognised_status",
                    "detail": "transaction_status is not one this server recognises; no state changed",
                })),
            )
        }
        // A legitimate in-progress status (`pending`, `authorize`). 200 is the
        // correct answer and Midtrans should not retry.
        PaymentAction::Pending => (StatusCode::OK, Json(json!({ "status": "pending" }))),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    // Only the tests exercise the raw status mapping: production code reaches it
    // through `evaluate_payment_status`, which is what the import above names.
    use crate::money::terminal_status;

    /// Bug B: Midtrans' vocabulary is not the schema's. Every mapped value must
    /// be one the topups_status_check constraint accepts
    /// (migration 20260925000000_initial_schema.sql:78), or the UPDATE fails.
    #[test]
    fn terminal_statuses_map_onto_the_schema_vocabulary() {
        // The exact values the CHECK constraint allows.
        const ALLOWED: [&str; 5] = ["pending", "settled", "denied", "expired", "refunded"];

        for midtrans in ["deny", "cancel", "expire"] {
            let mapped = terminal_status(midtrans)
                .unwrap_or_else(|| panic!("{midtrans} must map to something"));
            assert!(
                ALLOWED.contains(&mapped),
                "{midtrans} -> {mapped} is not permitted by topups_status_check"
            );
        }

        // The precise mapping.
        assert_eq!(terminal_status("deny"), Some("denied"));
        assert_eq!(terminal_status("expire"), Some("expired"));
        assert_eq!(terminal_status("cancel"), Some("denied"));
    }

    /// An unmappable status must be refused, not guessed: binding an unknown
    /// string would reproduce Bug B with a different word.
    #[test]
    fn an_unknown_terminal_status_is_refused() {
        assert_eq!(terminal_status("capture"), None);
        assert_eq!(terminal_status("settlement"), None);
        assert_eq!(terminal_status("pending"), None);
        assert_eq!(terminal_status(""), None);
        assert_eq!(terminal_status("DENY"), None, "matching is exact");
    }

    /// The regression this change exists for. Every value the live tester swept
    /// against the endpoint must NOT be classified as Pending - before this,
    /// all nine of the unrecognised ones were, so the topup stayed pending
    /// forever while the handler answered 200 and logged nothing.
    #[test]
    fn an_unrecognised_status_is_never_classified_as_pending() {
        // The exact sweep that refuted the previous fix.
        for status in [
            "foobar",
            "unknown",
            "expired",
            "denied",
            "DENY",
            "settlement",
            "",
            "cancel_pending",
            "refund_pending",
        ] {
            // `settlement` is a credit, not unrecognised - it is in the sweep
            // because it must not be Pending either.
            let action = evaluate_payment_status(status, 50000);
            assert_ne!(
                action,
                PaymentAction::Pending,
                "{status:?} must not be silently absorbed as Pending"
            );
        }

        // The genuinely unknown ones are explicitly Unrecognised, not merely
        // 'something other than Pending'.
        for status in [
            "foobar",
            "unknown",
            "expired",
            "denied",
            "DENY",
            "",
            "cancel_pending",
            "refund_pending",
        ] {
            assert_eq!(
                evaluate_payment_status(status, 50000),
                PaymentAction::Unrecognised,
                "{status:?} should be reported as unrecognised"
            );
        }
    }

    /// Legitimate in-progress statuses must keep behaving exactly as before:
    /// `pending` is what Midtrans sends while the customer has not paid, and
    /// `authorize` precedes a card capture. Neither may become Unrecognised.
    #[test]
    fn legitimate_in_progress_statuses_still_classify_as_pending() {
        assert_eq!(
            evaluate_payment_status("pending", 50000),
            PaymentAction::Pending
        );
        assert_eq!(
            evaluate_payment_status("authorize", 50000),
            PaymentAction::Pending
        );
    }

    /// The money-moving statuses are unaffected by the new arm.
    #[test]
    fn money_statuses_still_classify_correctly() {
        assert_eq!(
            evaluate_payment_status("settlement", 50000),
            PaymentAction::Credit { amount_idr: 50000 }
        );
        assert_eq!(
            evaluate_payment_status("capture", 50000),
            PaymentAction::Credit { amount_idr: 50000 }
        );
        assert_eq!(
            evaluate_payment_status("refund", 50000),
            PaymentAction::DebitRefund { amount_idr: 50000 }
        );
        assert_eq!(
            evaluate_payment_status("partial_refund", 50000),
            PaymentAction::DebitRefund { amount_idr: 50000 }
        );
    }

    /// The terminal arm now carries its schema value, so a terminal status can
    /// never reach the handler without a value the CHECK constraint accepts.
    #[test]
    fn terminal_actions_carry_a_schema_valid_status() {
        const ALLOWED: [&str; 5] = ["pending", "settled", "denied", "expired", "refunded"];

        for status in ["deny", "cancel", "expire"] {
            match evaluate_payment_status(status, 50000) {
                PaymentAction::TerminalNoAction { status } => assert!(
                    ALLOWED.contains(&status),
                    "{status} is not permitted by topups_status_check"
                ),
                other => panic!("{status} should be terminal, got {other:?}"),
            }
        }
    }

    /// The regression this change exists for: a successful top-up credit must
    /// announce the balance the transaction returned, and nothing else may.
    /// `AlreadySettled` is a replayed webhook - no money moved - so publishing
    /// there would show a balance that did not change.
    #[test]
    fn only_a_fresh_settle_announces_a_balance() {
        assert_eq!(
            credit_balance_to_publish(&Ok(TopupCreditResult::Settled {
                new_balance: 75_000
            })),
            Some(75_000)
        );

        for unchanged in [
            Ok(TopupCreditResult::AlreadySettled),
            Ok(TopupCreditResult::NotFound),
            Ok(TopupCreditResult::AmountMismatch),
            Err(AppError::NotFound("no such order".into())),
        ] {
            assert_eq!(
                credit_balance_to_publish(&unchanged),
                None,
                "{unchanged:?} changed no balance and must publish nothing"
            );
        }
    }

    /// The regression this change exists for. `env::var` returns `Ok("")` for a
    /// variable that is SET BUT EMPTY, and the old `match` accepted that `Ok`.
    /// Verification then hashed `order_id + status_code + gross_amount` with
    /// nothing appended - a formula published in docs/website/04-payments.md -
    /// so anyone could forge a `settlement` notification and credit a wallet.
    /// The predicate is pure, so this pins the contract without touching (and
    /// racing on) the process-wide environment.
    #[test]
    fn an_empty_or_whitespace_secret_is_never_usable() {
        assert!(!is_usable_server_key(""), "an empty secret must be refused");
        assert!(
            !is_usable_server_key("   "),
            "a whitespace-only secret must be refused"
        );
        assert!(
            !is_usable_server_key("\t\n "),
            "a whitespace-only secret must be refused"
        );

        // A real key must still be accepted, so the cases above are not passing
        // merely because the predicate rejects everything.
        assert!(
            is_usable_server_key("SB-Mid-server-TEST"),
            "a real key must be usable"
        );
        // A padded key is NOT trimmed into use: it is taken verbatim, so it is
        // usable as-is (only whitespace-ONLY is rejected).
        assert!(
            is_usable_server_key("  SB-Mid-server-TEST  "),
            "a padded key is accepted verbatim, never trimmed"
        );
    }

    /// The refund mirror. The 409 insufficient-balance path is the one that
    /// matters: it is a CONFLICT, not a success, and NOTHING was written - so
    /// announcing a balance there would invent money movement. `AmountMismatch`
    /// joins it: a rejected refund debits nothing, so it publishes nothing.
    #[test]
    fn a_refund_announces_a_balance_only_when_it_debited() {
        assert_eq!(
            refund_balance_to_publish(&Ok(RefundResult::Refunded {
                new_balance: 12_000
            })),
            Some(12_000)
        );

        // The amount-mismatch refusal is the regression this change exists for:
        // the debit that used to happen here is gone, so the announcement must be
        // gone with it. Pinned on its own so a future edit cannot quietly re-add
        // it to the "publishes" set.
        assert_eq!(
            refund_balance_to_publish(&Ok(RefundResult::AmountMismatch)),
            None,
            "a rejected refund wrote nothing and must publish nothing"
        );

        for unchanged in [
            Ok(RefundResult::AlreadyRefunded),
            Ok(RefundResult::NotFound),
            Ok(RefundResult::NotSettled {
                status: "pending".into(),
            }),
            Ok(RefundResult::AmountMismatch),
            Ok(RefundResult::InsufficientBalance {
                balance_idr: 1_000,
                required_idr: 50_000,
            }),
            Err(AppError::NotFound("no such order".into())),
        ] {
            assert_eq!(
                refund_balance_to_publish(&unchanged),
                None,
                "{unchanged:?} changed no balance and must publish nothing"
            );
        }
    }

    // =====================================================================
    // LIVE: the Midtrans webhook HANDLER
    //
    // Everything above this line is pure. The handler itself - the only thing
    // in this process that turns an HTTP body into money - had never been
    // executed by the suite. These tests drive it end to end against a live
    // Postgres and assert the DOCUMENTED contract
    // (docs/server/api-spec.md:266-294, docs/website/04-payments.md:31-64),
    // then re-assert the one safety net this project has
    // (docs/observability.md:112-139) after EVERY branch:
    //     wallets.balance_idr = SUM(ledger.delta_idr)
    //
    // Run with:
    //   DATABASE_URL=postgres://postgres:dev@localhost:5432/apikita \
    //     cargo test --lib -- --ignored
    //
    // ENVIRONMENT SERIALISATION. The handler reads MIDTRANS_SERVER_KEY through
    // `env::var`, and that variable is PROCESS-WIDE: a value one test sets is
    // visible to every other test thread in this binary. BOTH halves live in
    // `crate::routes::test_env`, SHARED with routes::account so that ONE lock
    // covers every writer in the crate (two private mutexes would exclude
    // nothing):
    //
    //   (a) EnvLock - the single process-wide mutex, held for the whole body of
    //       every test that touches these variables, so no two of them can
    //       interleave their writes; and
    //   (b) EnvGuard - RAII that restores the PREVIOUS value on drop, on the
    //       success path and the panic path alike, so no test leaks a key into a
    //       later test or a later `cargo test` in the same process.
    //
    // (b) is the half that matters: a leaked key is worse than no test.
    // =====================================================================

    use crate::config::AppConfig;
    use crate::money::compute_midtrans_signature;
    use crate::routes::events::RealtimeHub;
    use crate::routes::test_env::{EnvGuard, EnvLock};
    use axum::body::to_bytes;
    use sqlx::PgPool;
    use std::sync::Arc;
    use std::time::Duration;

    /// The key every live test here installs. Deliberately NOT the
    /// fake-midtrans default, so a test that passes without installing it is
    /// impossible to mistake for one that did.
    const LIVE_TEST_SERVER_KEY: &str = "SB-Mid-server-WEBHOOK-LIVE-TEST";


    async fn live_pool() -> PgPool {
        let database_url = std::env::var("DATABASE_URL")
            .expect("set DATABASE_URL to a migrated Postgres instance");
        crate::db::init_pool(&database_url)
            .await
            .expect("connect to Postgres")
    }

    /// The AppState the router would hand the handler, built from the same
    /// config file the server loads.
    fn live_app_state(pool: PgPool) -> AppState {
        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load for the live tests");
        let trusted = crate::ip_tracking::parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("the config validates its own trusted proxy rules");

        AppState {
            pool,
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .expect("build a test HTTP client"),
            events: Arc::new(RealtimeHub::new(&config.realtime)),
            config: Arc::new(config),
            ip_salt: Arc::new(crate::ip_tracking::DailySalt::new()),
            trusted_proxies: Arc::from(trusted.into_boxed_slice()),
        }
    }

    /// Takes the process-wide env lock, installs the server key, builds the
    /// fixture, runs the assertions in their own task, then tears the fixture
    /// down in FK order whether they passed or panicked - and only then releases
    /// the lock and restores the environment.
    ///
    /// The assertions are spawned so a panicking one arrives as a JoinError
    /// instead of unwinding through the teardown. That is what makes the
    /// cleanup unconditional, and it is why the LOCK is held HERE rather than
    /// inside the task (a MutexGuard is not Send). Holding it here is also what
    /// excludes routes::account's Snap tests, which take the same lock.
    async fn run_live<F, Fut>(assertions: F)
    where
        F: FnOnce(PgPool, Uuid, AppState) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let _env = EnvLock::acquire();
        let pool = live_pool().await;
        let key_guard = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);

        let account_id = fixture_account(&pool).await;
        let state = live_app_state(pool.clone());

        let outcome = tokio::spawn(assertions(pool.clone(), account_id, state)).await;

        delete_fixture_rows(&pool, &[account_id]).await;

        outcome.expect("the live webhook assertions panicked");
        drop(key_guard);
    }

    /// An account with the zero-balance wallet the login path creates. A wallet
    /// with no ledger rows is consistent on its own (0 = SUM of nothing), so
    /// this starting point reconciles.
    async fn fixture_account(pool: &PgPool) -> Uuid {
        let pb_user_id = format!("test_{}", Uuid::new_v4().simple());
        let account_id: Uuid =
            sqlx::query_scalar("INSERT INTO accounts (pb_user_id) VALUES ($1) RETURNING id")
                .bind(&pb_user_id)
                .fetch_one(pool)
                .await
                .expect("create account");

        sqlx::query("INSERT INTO wallets (account_id, balance_idr) VALUES ($1, 0)")
            .bind(account_id)
            .execute(pool)
            .await
            .expect("create the zero-balance wallet the login path would create");

        account_id
    }

    /// A `pending` topup, written the way routes/account.rs::create_topup writes
    /// it (minus the Snap token, which needs a live Midtrans). Returns its
    /// `order_id`, the key Midtrans notifies on.
    async fn pending_topup(pool: &PgPool, account_id: Uuid, amount_idr: i64) -> String {
        let order_id = format!("test_topup_{}", Uuid::new_v4().simple());
        sqlx::query("INSERT INTO topups (account_id, amount_idr, order_id) VALUES ($1, $2, $3)")
            .bind(account_id)
            .bind(amount_idr)
            .bind(&order_id)
            .execute(pool)
            .await
            .expect("create topup");
        order_id
    }

    /// Deletes every row a fixture created, in FK order (ledger, topups and
    /// wallets are ON DELETE RESTRICT, so the order is load-bearing).
    async fn delete_fixture_rows(pool: &PgPool, account_ids: &[Uuid]) {
        for account_id in account_ids {
            for statement in [
                "DELETE FROM usage_daily WHERE account_id = $1",
                "DELETE FROM ledger WHERE account_id = $1",
                "DELETE FROM api_keys WHERE account_id = $1",
                "DELETE FROM topups WHERE account_id = $1",
                "DELETE FROM sessions WHERE account_id = $1",
                "DELETE FROM wallets WHERE account_id = $1",
                "DELETE FROM accounts WHERE id = $1",
            ] {
                sqlx::query(statement)
                    .bind(account_id)
                    .execute(pool)
                    .await
                    .unwrap_or_else(|err| panic!("cleanup failed on `{statement}`: {err}"));
            }
        }
    }

    /// A Midtrans notification whose signature is computed by the REAL
    /// `money::compute_midtrans_signature`, so this fixture cannot drift from
    /// the implementation. `gross_amount` is passed as the exact signed STRING
    /// (e.g. "50000.00"), never a number: the hash covers the string form, and
    /// "50000" hashes differently from "50000.00".
    fn notification(
        order_id: &str,
        status_code: &str,
        gross_amount: &str,
        transaction_status: &str,
        signing_key: &str,
    ) -> MidtransNotification {
        MidtransNotification {
            order_id: order_id.to_string(),
            status_code: status_code.to_string(),
            gross_amount: gross_amount.to_string(),
            transaction_status: transaction_status.to_string(),
            signature_key: compute_midtrans_signature(
                order_id,
                status_code,
                gross_amount,
                signing_key,
            ),
            fraud_status: None,
        }
    }

    /// Drives the handler exactly the way the router does and reads its body.
    ///
    /// The key this reads was installed by `run_live` under the shared env lock,
    /// and that lock is held for this whole test - so nothing can clobber
    /// `MIDTRANS_SERVER_KEY` between the install and the handler reading it, and
    /// this function needs no re-install of its own. It used to re-set the
    /// variable immediately before the call, to defend against account.rs's
    /// Snap tests writing an invalid key with no restore and no lock; those
    /// tests now take the same lock and restore through the same guard, which
    /// removes the race at its source instead of repairing its symptom here.
    async fn post(state: &AppState, payload: MidtransNotification) -> (StatusCode, serde_json::Value) {
        let res = handle_midtrans_webhook(State(state.clone()), Json(payload))
            .await
            .into_response();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body");
        let body: serde_json::Value = serde_json::from_slice(&bytes)
            .expect("docs/error-model.md:10 - every response is JSON");
        (status, body)
    }

    async fn balance_of(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(pool)
            .await
            .expect("read balance")
    }

    async fn topup_status_of(pool: &PgPool, order_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM topups WHERE order_id = $1")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup status")
    }

    async fn ledger_count(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = $1")
            .bind(account_id)
            .fetch_one(pool)
            .await
            .expect("count ledger rows")
    }

    /// Every ledger row for the account with that reason, oldest first, as
    /// (delta_idr, ref).
    async fn ledger_rows_of(
        pool: &PgPool,
        account_id: Uuid,
        reason: &str,
    ) -> Vec<(i64, Option<String>)> {
        sqlx::query_as(
            "SELECT delta_idr, ref FROM ledger WHERE account_id = $1 AND reason = $2 ORDER BY id",
        )
        .bind(account_id)
        .bind(reason)
        .fetch_all(pool)
        .await
        .expect("read ledger rows")
    }

    /// The reconciliation check from docs/observability.md: wallets.balance_idr
    /// must equal SUM(ledger.delta_idr). Scoped to THIS fixture's account, so a
    /// concurrent writer cannot fail it for a reason unrelated to the handler.
    async fn drift_rows(pool: &PgPool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT w.account_id
                FROM wallets w
                LEFT JOIN ledger l ON l.account_id = w.account_id
                WHERE w.account_id = $1
                GROUP BY w.account_id, w.balance_idr
                HAVING w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id)
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// THE INVARIANT, asserted after EVERY branch of every test below. A write
    /// that lands on one side only - a credit with no ledger row, a debit with
    /// no ledger row - cannot pass this, whatever the HTTP status said.
    async fn assert_reconciled(pool: &PgPool, account_id: Uuid, context: &str) {
        assert_eq!(
            drift_rows(pool, account_id).await,
            0,
            "{context}: wallets.balance_idr must equal SUM(ledger.delta_idr)"
        );
    }

    // -----------------------------------------------------------------------
    // 1. A BAD SIGNATURE is rejected and NOTHING changes.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_rejects_a_bad_signature_and_writes_nothing() {
        run_live(bad_signature_assertions).await;
    }

    async fn bad_signature_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;

        // Internally consistent, but signed with SOMEBODY ELSE'S key: exactly
        // the shape a forger without the secret produces.
        let forged = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            "SB-Mid-server-SOMEONE-ELSE",
        );

        let (status, body) = post(&state, forged).await;

        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "docs/server/api-spec.md:274 - a signature mismatch is 401. body: {body}"
        );
        assert_eq!(body["error"], json!("invalid signature"), "{body}");

        // The assertions that matter, against DIRECT SELECTs rather than the
        // HTTP status: a handler that answered 401 and credited anyway would
        // pass a status-only test.
        assert_eq!(
            balance_of(&pool, account_id).await,
            0,
            "a rejected webhook must not credit the wallet"
        );
        assert_eq!(
            topup_status_of(&pool, &order_id).await,
            "pending",
            "a rejected webhook must not settle the topup"
        );
        assert_eq!(
            ledger_count(&pool, account_id).await,
            0,
            "a rejected webhook must append no ledger row"
        );
        assert_reconciled(&pool, account_id, "after a bad signature").await;
    }

    // -----------------------------------------------------------------------
    // 2. A WRONG AMOUNT with a VALID signature is rejected, nothing changes.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_rejects_a_wrong_amount_with_a_valid_signature() {
        run_live(wrong_amount_assertions).await;
    }

    async fn wrong_amount_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const STORED: i64 = 50_000;
        let order_id = pending_topup(&pool, account_id, STORED).await;

        // A genuine signature over an amount that is NOT the stored one. The
        // amount must come from OUR row, never the payload
        // (docs/server/api-spec.md:287).
        let inflated = notification(
            &order_id,
            "200",
            "60000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );

        let (status, body) = post(&state, inflated).await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "docs/server/api-spec.md:276 - an amount that disagrees with the stored row is              rejected. body: {body}"
        );
        assert_eq!(body["error"], json!("amount mismatch"), "{body}");

        assert_eq!(
            balance_of(&pool, account_id).await,
            0,
            "a mismatched amount must credit NOTHING - not the payload amount, not the stored one"
        );
        assert_eq!(
            topup_status_of(&pool, &order_id).await,
            "pending",
            "a mismatched amount must leave the topup pending"
        );
        assert_eq!(ledger_count(&pool, account_id).await, 0);
        assert_reconciled(&pool, account_id, "after a mismatched amount").await;
    }

    // -----------------------------------------------------------------------
    // 3. SETTLEMENT credits EXACTLY ONCE; a REPLAY does not credit again.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_settlement_credits_exactly_once_and_a_replay_does_not() {
        run_live(settlement_then_replay_assertions).await;
    }

    async fn settlement_then_replay_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;

        let settle = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );

        let (status, body) = post(&state, settle.clone()).await;

        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["status"],
            json!("settled"),
            "docs/server/api-spec.md:278-282 - the credit settles. body: {body}"
        );

        assert_eq!(
            balance_of(&pool, account_id).await,
            AMOUNT,
            "the wallet must be up by EXACTLY the amount"
        );
        assert_eq!(topup_status_of(&pool, &order_id).await, "settled");

        let rows = ledger_rows_of(&pool, account_id, "topup").await;
        assert_eq!(
            rows.len(),
            1,
            "a settlement appends exactly ONE ledger row with reason='topup': {rows:?}"
        );
        assert_eq!(rows[0].0, AMOUNT, "and it is the full amount: {rows:?}");
        assert_reconciled(&pool, account_id, "after a settlement").await;

        // REPLAY: Midtrans retries. docs/website/04-payments.md:61 - return 200
        // and do nothing. A double credit is real money.
        let (status, body) = post(&state, settle).await;
        assert_eq!(status, StatusCode::OK, "a replay is a 200, not an error: {body}");
        assert_eq!(body["status"], json!("already_settled"), "{body}");

        assert_eq!(
            balance_of(&pool, account_id).await,
            AMOUNT,
            "a replayed webhook must NOT credit again"
        );
        assert_eq!(
            ledger_count(&pool, account_id).await,
            1,
            "the ledger row COUNT must be unchanged by the replay"
        );
        assert_eq!(ledger_rows_of(&pool, account_id, "topup").await.len(), 1);
        assert_eq!(topup_status_of(&pool, &order_id).await, "settled");
        assert_reconciled(&pool, account_id, "after a replayed settlement").await;
    }

    // -----------------------------------------------------------------------
    // 4. DENY / EXPIRE / CANCEL persist the SCHEMA vocabulary.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_terminal_statuses_persist_the_schema_vocabulary() {
        run_live(terminal_status_assertions).await;
    }

    async fn terminal_status_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;

        // Midtrans' word -> the word the topups_status_check constraint accepts
        // (migration 20260925000000_initial_schema.sql:78). The defect this
        // pins: Midtrans' vocabulary was bound straight through, the constraint
        // rejected it, and the error was swallowed - 200 while the row stayed
        // pending. So the ROW is asserted, not the response.
        for (midtrans_status, expected) in [
            ("deny", "denied"),
            ("expire", "expired"),
            ("cancel", "denied"),
        ] {
            let order_id = pending_topup(&pool, account_id, AMOUNT).await;
            let payload = notification(
                &order_id,
                "200",
                "50000.00",
                midtrans_status,
                LIVE_TEST_SERVER_KEY,
            );

            let (status, body) = post(&state, payload).await;

            assert_eq!(status, StatusCode::OK, "{midtrans_status}: body: {body}");
            assert_eq!(
                topup_status_of(&pool, &order_id).await,
                expected,
                "{midtrans_status} must persist the SCHEMA word `{expected}`, not Midtrans'                  own. A silent no-op answers 200 too, so the row is the assertion. body: {body}"
            );
            assert_eq!(
                balance_of(&pool, account_id).await,
                0,
                "{midtrans_status} must not move money"
            );
            assert_eq!(ledger_count(&pool, account_id).await, 0);
            assert_reconciled(&pool, account_id, &format!("after {midtrans_status}")).await;
        }
    }

    // -----------------------------------------------------------------------
    // 5. AN UNRECOGNISED status performs NO write and claims no credit.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_an_unrecognised_status_writes_nothing() {
        run_live(unrecognised_status_assertions).await;
    }

    async fn unrecognised_status_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;

        // "foobar" is not a Midtrans status. The defect: an unknown value used
        // to fall through to Pending, so the topup stayed pending forever while
        // the handler answered 200 and logged nothing.
        let payload = notification(
            &order_id,
            "200",
            "50000.00",
            "foobar",
            LIVE_TEST_SERVER_KEY,
        );

        let (status, body) = post(&state, payload).await;

        // The topup must be EXACTLY as it was - not merely "not settled".
        assert_eq!(
            topup_status_of(&pool, &order_id).await,
            "pending",
            "an unrecognised status performs NO write. body: {body}"
        );
        assert_eq!(balance_of(&pool, account_id).await, 0);
        assert_eq!(ledger_count(&pool, account_id).await, 0);
        assert_reconciled(&pool, account_id, "after an unrecognised status").await;

        // ...and it must not report a money-moving outcome. The status code is
        // 200 because docs/server/api-spec.md:293 requires a fast 2xx so
        // Midtrans does not retry; the BODY is therefore the only signal, and
        // it must not be any of the success words.
        assert_eq!(
            status,
            StatusCode::OK,
            "docs/server/api-spec.md:293 - respond 200 quickly so Midtrans does not retry: {body}"
        );
        for success in [
            "settled",
            "already_settled",
            "refunded",
            "already_refunded",
            "terminal_recorded",
            "pending",
        ] {
            assert_ne!(
                body["status"],
                json!(success),
                "an unrecognised status must not report `{success}`: {body}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 6. REFUND debits once; a replayed refund does not debit twice.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_refund_debits_once_and_a_replay_does_not() {
        run_live(refund_then_replay_assertions).await;
    }

    async fn refund_then_replay_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;

        // Fund the wallet through the REAL path - a topups row, then
        // credit_topup_transaction, which appends the matching +topup ledger
        // row in the same transaction. Writing wallets.balance_idr directly
        // would manufacture the very drift the reconciliation assertion below
        // then reports.
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, AMOUNT)
                .await
                .expect("settle the fixture topup"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            },
            "the fixture must fund the wallet through the real top-up path"
        );
        assert_reconciled(&pool, account_id, "after funding the fixture").await;

        let refund = notification(&order_id, "200", "50000.00", "refund", LIVE_TEST_SERVER_KEY);

        let (status, body) = post(&state, refund.clone()).await;

        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["status"],
            json!("refunded"),
            "docs/server/api-spec.md:289-292 - refund is a DEBIT, even though the policy is              non-refundable. body: {body}"
        );

        assert_eq!(
            balance_of(&pool, account_id).await,
            0,
            "the refund must debit the wallet by the amount"
        );
        assert_eq!(topup_status_of(&pool, &order_id).await, "refunded");

        let rows = ledger_rows_of(&pool, account_id, "refund").await;
        assert_eq!(
            rows.len(),
            1,
            "a refund appends exactly ONE ledger row with reason='refund': {rows:?}"
        );
        assert_eq!(
            rows[0].0, -AMOUNT,
            "and its delta is NEGATIVE, so the ledger still sums to the balance: {rows:?}"
        );
        assert_reconciled(&pool, account_id, "after a refund").await;

        // REPLAY: a second refund of the same order is a replay, not a refund.
        let (status, body) = post(&state, refund).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["status"], json!("already_refunded"), "{body}");

        assert_eq!(
            balance_of(&pool, account_id).await,
            0,
            "a replayed refund must NOT debit twice"
        );
        assert_eq!(
            ledger_rows_of(&pool, account_id, "refund").await.len(),
            1,
            "a replayed refund must NOT append a second ledger row"
        );
        assert_eq!(topup_status_of(&pool, &order_id).await, "refunded");
        assert_reconciled(&pool, account_id, "after a replayed refund").await;
    }

    // -----------------------------------------------------------------------
    // 6. A REFUND whose amount is NOT the stored one is rejected, nothing changes.
    // -----------------------------------------------------------------------

    #[ignore = "requires live Postgres: DATABASE_URL pointing at a migrated schema"]
    #[tokio::test]
    async fn live_webhook_refund_rejects_an_amount_that_is_not_the_stored_one() {
        run_live(refund_wrong_amount_assertions).await;
    }

    /// THE DEFECT, end to end through the handler. The refund branch parsed
    /// `gross_amount` out of the PAYLOAD and debited it without ever reading
    /// `topups.amount_idr`, so a signed refund notification carrying more than
    /// the top-up debited the larger figure - one call draining the wallet while
    /// the top-up still read `refunded` for an amount it never was. The ledger
    /// could not catch it either: the row was written from the SAME unchecked
    /// value, so the books stayed internally consistent while the money was gone.
    ///
    /// docs/server/api-spec.md:284 mandates comparing the amount against the
    /// STORED row; :295 says "the amount comes from our stored row, never the
    /// payload". The credit branch honours that; this one must too.
    async fn refund_wrong_amount_assertions(pool: PgPool, account_id: Uuid, state: AppState) {
        const STORED: i64 = 50_000;
        const INFLATED: i64 = 100_000;

        // TWO settled top-ups, so the wallet holds 100_000 and an inflated refund
        // of the FIRST one is AFFORDABLE. The guarded UPDATE
        // (`balance_idr >= $1`) cannot save us here, which is exactly the
        // reported consequence: a single notification drains the whole wallet.
        let first = pending_topup(&pool, account_id, STORED).await;
        assert_eq!(
            credit_topup_transaction(&pool, &first, STORED)
                .await
                .expect("settle the first fixture topup"),
            TopupCreditResult::Settled {
                new_balance: STORED
            }
        );
        let second = pending_topup(&pool, account_id, STORED).await;
        assert_eq!(
            credit_topup_transaction(&pool, &second, STORED)
                .await
                .expect("settle the second fixture topup"),
            TopupCreditResult::Settled {
                new_balance: 2 * STORED
            }
        );
        assert_reconciled(&pool, account_id, "after funding the fixture").await;

        // A genuine signature over 100000.00 for a top-up that was 50_000.
        let inflated = notification(
            &first,
            "200",
            &format!("{INFLATED}.00"),
            "refund",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post(&state, inflated).await;

        // The MONEY is asserted before the HTTP status: the reported defect is a
        // debit, and a handler that answers 400 while crediting anyway would pass
        // a status-only test.
        assert_eq!(
            balance_of(&pool, account_id).await,
            2 * STORED,
            "a mismatched refund must debit NOTHING - not the payload amount, not the stored one"
        );
        assert_eq!(
            topup_status_of(&pool, &first).await,
            "settled",
            "a refused refund must not mark the top-up refunded"
        );
        assert_eq!(
            ledger_rows_of(&pool, account_id, "refund").await.len(),
            0,
            "a refused refund must append no ledger row"
        );
        assert_reconciled(&pool, account_id, "after a mismatched refund").await;

        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an amount that disagrees with the stored top-up must be rejected,              never debited. body: {body}"
        );
        assert_eq!(body["error"], json!("amount mismatch"), "{body}");

        // The STORED amount still refunds cleanly: the refusal is about the
        // amount, not about refusing refunds.
        let honest = notification(&first, "200", "50000.00", "refund", LIVE_TEST_SERVER_KEY);
        let (status, body) = post(&state, honest).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["status"], json!("refunded"), "{body}");
        assert_eq!(balance_of(&pool, account_id).await, STORED);
        assert_eq!(topup_status_of(&pool, &first).await, "refunded");
        assert_reconciled(&pool, account_id, "after the stored-amount refund").await;
    }
}
