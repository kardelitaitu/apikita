use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use sqlx::PgPool;
use std::env;
use tracing::{error, info, warn};

use crate::db::{
    credit_topup_transaction, refund_topup_transaction, RefundResult, TopupCreditResult,
};
use crate::money::{
    evaluate_payment_status, verify_midtrans_signature, MidtransNotification, PaymentAction,
};

/// The `topups.status` value for a Midtrans terminal status.
///
/// Midtrans says `deny` and `expire`; the schema's CHECK constraint allows
/// `denied` and `expired` (migration 20260925000000_initial_schema.sql:78).
/// Passing Midtrans' vocabulary straight through violated the constraint, and
/// the error was swallowed - a real `deny` webhook returned 200 while the row
/// stayed `pending` (Bug B).
///
/// `cancel` maps to `denied`: there is no `cancelled` in the schema, and the
/// distinction between a payment the customer abandoned and one the issuer
/// refused is not one the money model needs - both mean the topup will never
/// settle. `denied` is the closer of the two available values, and it keeps the
/// status set closed rather than inventing a migration for a synonym.
fn terminal_status(midtrans_status: &str) -> Option<&'static str> {
    match midtrans_status {
        "deny" | "cancel" => Some("denied"),
        "expire" => Some("expired"),
        // Not a terminal status this handler knows. `None` is a refusal to
        // guess: the caller reports it instead of writing a value the CHECK
        // constraint would reject.
        _ => None,
    }
}

/// The JSON body for a refusal that leaves no money movement ambiguous.
fn error_body(code: &str, message: &str) -> Json<serde_json::Value> {
    Json(json!({ "error": code, "message": message }))
}
pub async fn handle_midtrans_webhook(
    State(pool): State<PgPool>,
    Json(payload): Json<MidtransNotification>,
) -> impl IntoResponse {
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
    let gross_idr: i64 = match payload.gross_amount.split('.').next().unwrap_or("0").parse() {
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
            match credit_topup_transaction(&pool, &payload.order_id, amount_idr).await {
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
                    (StatusCode::NOT_FOUND, Json(json!({"error": "order not found"})))
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

            match refund_topup_transaction(&pool, &payload.order_id, amount_idr).await {
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
        PaymentAction::TerminalNoAction => {
            // Translate Midtrans' vocabulary into the schema's before binding it:
            // writing `deny` straight through violated topups_status_check, and
            // the swallowed error returned 200 while the row stayed `pending`.
            let Some(status) = terminal_status(&payload.transaction_status) else {
                error!(
                    order_id = %payload.order_id,
                    status = %payload.transaction_status,
                    "Unmappable terminal status: refusing to write a value the schema rejects"
                );
                return (
                    StatusCode::BAD_REQUEST,
                    error_body("unknown_terminal_status", "unrecognised terminal status"),
                );
            };

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
            .execute(&pool)
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
        PaymentAction::Pending => (StatusCode::OK, Json(json!({"status": "pending"}))),
    }
}
#[cfg(test)]
mod tests {
    use super::*;

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

    /// `deny` and `cancel` collapse to one value on purpose: the schema has no
    /// `cancelled`, and neither status will ever settle.
    #[test]
    fn cancel_and_deny_collapse_to_denied() {
        assert_eq!(terminal_status("cancel"), terminal_status("deny"));
    }
}
