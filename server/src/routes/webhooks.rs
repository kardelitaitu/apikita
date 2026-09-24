use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use sqlx::PgPool;
use std::env;
use tracing::{error, info, warn};

use crate::money::{evaluate_payment_status, verify_midtrans_signature, MidtransNotification, PaymentAction};
use crate::db::{credit_topup_transaction, TopupCreditResult};

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
        PaymentAction::DebitRefund { amount_idr: _ } => {
            warn!(
                order_id = %payload.order_id,
                "Received refund status from Midtrans webhook"
            );
            let _ = sqlx::query("UPDATE topups SET status = 'refunded' WHERE order_id = $1")
                .bind(&payload.order_id)
                .execute(&pool)
                .await;
            (StatusCode::OK, Json(json!({"status": "refund_recorded"})))
        }
        PaymentAction::TerminalNoAction => {
            info!(
                order_id = %payload.order_id,
                status = %payload.transaction_status,
                "Terminal non-credit status recorded"
            );
            let _ = sqlx::query("UPDATE topups SET status = $1 WHERE order_id = $2 AND status = 'pending'")
                .bind(&payload.transaction_status)
                .bind(&payload.order_id)
                .execute(&pool)
                .await;
            (StatusCode::OK, Json(json!({"status": "terminal_recorded"})))
        }
        PaymentAction::Pending => (StatusCode::OK, Json(json!({"status": "pending"}))),
    }
}
