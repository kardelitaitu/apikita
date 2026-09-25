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
    /// The payment will never settle. Carries the `topups.status` value to
    /// persist, so the mapping lives here with the vocabulary it maps and the
    /// handler has no unreachable branch to forget about.
    TerminalNoAction { status: &'static str },
    /// Midtrans sent a `transaction_status` this server does not recognise.
    ///
    /// This variant exists so an unknown value can never be silently absorbed.
    /// Before it, anything outside the lists below fell through to `Pending`:
    /// a new, misspelled, or differently-cased status left the topup `pending`
    /// forever while the handler answered 200 and logged nothing.
    ///
    /// An unrecognised status is NOT evidence that a payment is in progress.
    Unrecognised,
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

/// Midtrans' documented `transaction_status` values, enumerated so the set is
/// auditable rather than implied by a catch-all arm.
///
/// Adding a status is a deliberate edit here. Anything not in one of these four
/// sets is `Unrecognised` and must be surfaced, never assumed to be in progress.
/// Money arrives.
const CREDIT_STATUSES: [&str; 2] = ["capture", "settlement"];
/// Money goes back. `partial_refund` included: an unhandled status corrupts the
/// ledger (docs/server/api-spec.md:285-288).
const REFUND_STATUSES: [&str; 2] = ["refund", "partial_refund"];
/// Legitimate in-progress states: nothing to do yet, and 200 is the right answer.
/// `pending` is what Midtrans sends while the customer has not paid; `authorize`
/// precedes a card capture. These must keep behaving as before.
const IN_PROGRESS_STATUSES: [&str; 2] = ["pending", "authorize"];

/// The `topups.status` value for a Midtrans terminal status, or None.
///
/// Midtrans says `deny` and `expire`; the schema's CHECK constraint allows
/// `denied` and `expired` (migration 20260925000000_initial_schema.sql:78).
/// Passing Midtrans' vocabulary straight through violated the constraint, and
/// the error was swallowed - a real `deny` webhook returned 200 while the row
/// stayed `pending`.
///
/// `cancel` maps to `denied`: there is no `cancelled` in the schema, and the
/// distinction between a payment the customer abandoned and one the issuer
/// refused is not one the money model acts on - neither will ever settle.
pub fn terminal_status(midtrans_status: &str) -> Option<&'static str> {
    match midtrans_status {
        "deny" | "cancel" => Some("denied"),
        "expire" => Some("expired"),
        _ => None,
    }
}

/// Evaluates payment status transition based on Midtrans transaction_status
///
/// The four sets above are checked explicitly and everything else is
/// `Unrecognised`. There is deliberately NO catch-all that maps an unknown
/// status onto `Pending`.
pub fn evaluate_payment_status(status: &str, stored_amount_idr: i64) -> PaymentAction {
    if CREDIT_STATUSES.contains(&status) {
        return PaymentAction::Credit {
            amount_idr: stored_amount_idr,
        };
    }
    if REFUND_STATUSES.contains(&status) {
        return PaymentAction::DebitRefund {
            amount_idr: stored_amount_idr,
        };
    }
    if let Some(schema_status) = terminal_status(status) {
        return PaymentAction::TerminalNoAction {
            status: schema_status,
        };
    }
    if IN_PROGRESS_STATUSES.contains(&status) {
        return PaymentAction::Pending;
    }
    PaymentAction::Unrecognised
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
            PaymentAction::TerminalNoAction { status: "denied" }
        );
        assert_eq!(
            evaluate_payment_status("deny", 50000),
            PaymentAction::TerminalNoAction { status: "denied" }
        );
        assert_eq!(
            evaluate_payment_status("expire", 50000),
            PaymentAction::TerminalNoAction { status: "expired" }
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
