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
/// A replayed refund (`AlreadyRefunded`) and a refund the wallet cannot cover
/// (`InsufficientBalance` - the 409 path) wrote nothing, so neither may
/// announce a balance that did not move.
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
        Ok(k) => k,
        Err(_) => {
            error!("MIDTRANS_SERVER_KEY environment variable is not configured");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server misconfigured"})),
            );
        }
    };

    // 1. Signature verification
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

    /// The refund mirror. The 409 insufficient-balance path is the one that
    /// matters: it is a CONFLICT, not a success, and NOTHING was written - so
    /// announcing a balance there would invent money movement.
    #[test]
    fn a_refund_announces_a_balance_only_when_it_debited() {
        assert_eq!(
            refund_balance_to_publish(&Ok(RefundResult::Refunded {
                new_balance: 12_000
            })),
            Some(12_000)
        );

        for unchanged in [
            Ok(RefundResult::AlreadyRefunded),
            Ok(RefundResult::NotFound),
            Ok(RefundResult::NotSettled {
                status: "pending".into(),
            }),
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
}
