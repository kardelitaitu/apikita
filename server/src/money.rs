use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MidtransNotification {
    pub order_id: String,
    pub status_code: String,
    pub gross_amount: String,
    pub transaction_status: String,
    pub signature_key: String,
    pub fraud_status: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PaymentAction {
    Credit { amount_idr: i64 },
    DebitRefund { amount_idr: i64 },
    TerminalNoAction,
    Pending,
}

/// Computes the official Midtrans SHA-512 signature key:
/// SHA512(order_id + status_code + gross_amount + server_key)
pub fn compute_midtrans_signature(
    order_id: &str,
    status_code: &str,
    gross_amount: &str,
    server_key: &str,
) -> String {
    let mut hasher = Sha512::new();
    hasher.update(order_id.as_bytes());
    hasher.update(status_code.as_bytes());
    hasher.update(gross_amount.as_bytes());
    hasher.update(server_key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Verifies signature in constant time
pub fn verify_midtrans_signature(
    notification: &MidtransNotification,
    server_key: &str,
) -> bool {
    let expected = compute_midtrans_signature(
        &notification.order_id,
        &notification.status_code,
        &notification.gross_amount,
        server_key,
    );
    expected.as_bytes().ct_eq(notification.signature_key.as_bytes()).into()
}

/// Evaluates payment status transition based on Midtrans transaction_status
pub fn evaluate_payment_status(status: &str, stored_amount_idr: i64) -> PaymentAction {
    match status {
        "capture" | "settlement" => PaymentAction::Credit {
            amount_idr: stored_amount_idr,
        },
        "refund" | "partial_refund" => PaymentAction::DebitRefund {
            amount_idr: stored_amount_idr,
        },
        "deny" | "cancel" | "expire" => PaymentAction::TerminalNoAction,
        _ => PaymentAction::Pending,
    }
}

/// Calculates token cost in integer IDR using uniform markup multiplier M
/// Cost = ceil( M * [ (input_tokens / 1e6 * R_in) + (cache_tokens / 1e6 * R_cache) + (output_tokens / 1e6 * R_out) ] )
pub fn calculate_token_cost_idr(
    multiplier: f64,
    input_tokens: u64,
    r_in: f64,
    cache_tokens: u64,
    r_cache: f64,
    output_tokens: u64,
    r_out: f64,
) -> i64 {
    let in_cost = (input_tokens as f64 / 1_000_000.0) * r_in;
    let cache_cost = (cache_tokens as f64 / 1_000_000.0) * r_cache;
    let out_cost = (output_tokens as f64 / 1_000_000.0) * r_out;
    let total_wholesale = in_cost + cache_cost + out_cost;
    let total_customer = total_wholesale * multiplier;
    total_customer.ceil() as i64
}

/// Computes worst-case pre-flight reservation in IDR
pub fn calculate_preflight_reservation_idr(
    multiplier: f64,
    estimated_input_tokens: u64,
    r_in: f64,
    max_output_tokens: u64,
    r_out: f64,
) -> i64 {
    calculate_token_cost_idr(
        multiplier,
        estimated_input_tokens,
        r_in,
        0,
        0.0,
        max_output_tokens,
        r_out,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_midtrans_signature_computation() {
        let order_id = "topup_123";
        let status_code = "200";
        let gross_amount = "50000.00";
        let server_key = "SB-Mid-server-TEST12345";

        let sig = compute_midtrans_signature(order_id, status_code, gross_amount, server_key);
        assert_eq!(sig.len(), 128); // SHA-512 in hex is 128 chars

        let notif = MidtransNotification {
            order_id: order_id.to_string(),
            status_code: status_code.to_string(),
            gross_amount: gross_amount.to_string(),
            transaction_status: "settlement".to_string(),
            signature_key: sig.clone(),
            fraud_status: None,
        };

        assert!(verify_midtrans_signature(&notif, server_key));

        let bad_notif = MidtransNotification {
            signature_key: "bad_signature".to_string(),
            ..notif
        };
        assert!(!verify_midtrans_signature(&bad_notif, server_key));
    }

    #[test]
    fn test_payment_status_action() {
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
            evaluate_payment_status("cancel", 50000),
            PaymentAction::TerminalNoAction
        );
        assert_eq!(
            evaluate_payment_status("pending", 50000),
            PaymentAction::Pending
        );
    }

    #[test]
    fn test_token_cost_calculation() {
        // Flash peak rates: in: 2676.78, out: 10707.12, cache: 53.54
        // M = 2.0
        // 1000 input tokens, 500 output tokens, 2000 cache read tokens
        let cost = calculate_token_cost_idr(
            2.0,
            1_000,
            2676.78,
            2_000,
            53.54,
            500,
            10707.12,
        );
        // in wholesale = 1000/1e6 * 2676.78 = 2.67678
        // cache wholesale = 2000/1e6 * 53.54 = 0.10708
        // out wholesale = 500/1e6 * 10707.12 = 5.35356
        // sum = 8.13742
        // customer @ 2.0 = 16.27484 -> ceil is 17 IDR
        assert_eq!(cost, 17);
    }
}
