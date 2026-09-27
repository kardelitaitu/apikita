use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512};
use subtle::ConstantTimeEq;

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
    Credit {
        amount_idr: i64,
    },
    /// Midtrans can still SEND a refund - a merchant-panel refund, a
    /// chargeback - and this server does NOT apply it. The platform does not do
    /// refunds: no wallet debit, no ledger row, no `topups.status` change.
    ///
    /// Deliberately its own variant rather than `Unrecognised`: a refusal is a
    /// POLICY, and routing it through the unknown-status arm would mislabel a
    /// deliberate decision as a classification gap. It is also not `Pending` -
    /// the refusal is final, not in progress.
    RefundRefused,
    /// The payment will never settle. Carries the `topups.status` value to
    /// persist, so the mapping lives here with the vocabulary it maps and the
    /// handler has no unreachable branch to forget about.
    TerminalNoAction {
        status: &'static str,
    },
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
pub fn verify_midtrans_signature(notification: &MidtransNotification, server_key: &str) -> bool {
    let expected = compute_midtrans_signature(
        &notification.order_id,
        &notification.status_code,
        &notification.gross_amount,
        server_key,
    );
    expected
        .as_bytes()
        .ct_eq(notification.signature_key.as_bytes())
        .into()
}

/// Midtrans' documented `transaction_status` values, enumerated so the set is
/// auditable rather than implied by a catch-all arm.
///
/// Adding a status is a deliberate edit here. Anything not in one of these four
/// sets is `Unrecognised` and must be surfaced, never assumed to be in progress.
/// Money arrives.
const CREDIT_STATUSES: [&str; 2] = ["capture", "settlement"];
/// Refunds. Enumerated for the same reason as the credit set: this platform does
/// NOT do refunds, so this must be an auditable list of the statuses refused BY
/// POLICY - never implied by a catch-all arm, which would make a deliberate
/// refusal indistinguishable from a status the server has never seen.
/// `partial_refund` included: it is the same refusal.
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
        return PaymentAction::RefundRefused;
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
        // The platform does NOT do refunds: a refund status is refused, never
        // debited (see `a_refund_notification_is_never_classified_as_a_debit`).
        assert_eq!(
            evaluate_payment_status("refund", 50000),
            PaymentAction::RefundRefused
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
    fn a_refund_notification_is_never_classified_as_a_debit() {
        for status in ["refund", "partial_refund"] {
            assert_eq!(
                evaluate_payment_status(status, 50_000),
                PaymentAction::RefundRefused,
                "{status} must be refused, never debited"
            );
        }
    }

    #[test]
    fn test_token_cost_calculation() {
        // Flash peak rates: in: 2676.78, out: 10707.12, cache: 53.54
        // M = 2.0
        // 1000 input tokens, 500 output tokens, 2000 cache read tokens
        let cost = calculate_token_cost_idr(2.0, 1_000, 2676.78, 2_000, 53.54, 500, 10707.12);
        // in wholesale = 1000/1e6 * 2676.78 = 2.67678
        // cache wholesale = 2000/1e6 * 53.54 = 0.10708
        // out wholesale = 500/1e6 * 10707.12 = 5.35356
        // sum = 8.13742
        // customer @ 2.0 = 16.27484 -> ceil is 17 IDR
        assert_eq!(cost, 17);
    }

    // =====================================================================
    // Billing properties (docs/local-development.md:95-117)
    //
    // Rates below are transcribed from config/apikita.toml:300-305 (the `flash`
    // model, IDR per 1M tokens at billing_basis="peak") and price = 1.5
    // (config/apikita.toml:288). They are literals on purpose: a config edit
    // that silently changes what a customer is billed should break a test, not
    // quietly rewrite the expected value.
    // =====================================================================

    const PRICE: f64 = 1.5;
    const R_IN: f64 = 2676.78;
    const R_CACHE: f64 = 53.54;
    const R_OUT: f64 = 10707.12;
    const R_IN_OFFPEAK: f64 = 1338.39;
    const R_CACHE_OFFPEAK: f64 = 26.77;
    const R_OUT_OFFPEAK: f64 = 5353.56;

    fn bill(input: u64, cache_read: u64, output: u64) -> i64 {
        calculate_token_cost_idr(PRICE, input, R_IN, cache_read, R_CACHE, output, R_OUT)
    }

    fn reserve(estimated_input: u64, max_output: u64) -> i64 {
        calculate_preflight_reservation_idr(PRICE, estimated_input, R_IN, max_output, R_OUT)
    }

    /// The property docs/local-development.md:113-117 calls "the cheapest guard
    /// against the most expensive accounting error".
    #[test]
    fn cache_read_tokens_are_never_priced_as_input_tokens() {
        let cache_tokens = 1_000_000;

        // 1M cache reads at the cache rate: 1e6/1e6 * 53.54 * 1.5 = 80.31 -> 81
        let billed_as_cache = bill(0, cache_tokens, 0);
        // The same 1M tokens misclassified as plain input: 2676.78 * 1.5 = 4015.17 -> 4016
        let billed_as_input = bill(cache_tokens, 0, 0);

        assert_eq!(billed_as_cache, 81);
        assert_eq!(billed_as_input, 4016);
        assert_ne!(
            billed_as_cache, billed_as_input,
            "cache-read and input tokens must not be priced alike: they differ ~50x"
        );
        assert!(
            billed_as_input > billed_as_cache,
            "pricing cache hits as input overcharges the customer by {billed_as_input} vs {billed_as_cache}"
        );

        // The headline ratio the doc cites (~50x at these rates).
        let ratio = R_IN / R_CACHE;
        assert!(
            ratio > 40.0 && ratio < 60.0,
            "docs/local-development.md:115 - the classes differ ~50x, measured {ratio}"
        );
    }

    /// The same total prompt, split between fresh and cached tokens, must bill
    /// strictly less than the identical prompt with no cache hits.
    #[test]
    fn a_cache_hit_is_cheaper_than_the_same_tokens_sent_as_fresh_input() {
        let with_cache = bill(100_000, 900_000, 0);
        let without_cache = bill(1_000_000, 0, 0);

        assert_eq!(with_cache, 474);
        assert_eq!(without_cache, 4016);
        assert!(
            with_cache < without_cache,
            "900k cached tokens must cost less than 900k fresh ones ({with_cache} vs {without_cache})"
        );
    }

    /// `input_tokens` and `cache_read_tokens` are disjoint COUNTERS but they are
    /// a partition of ONE prompt: upstream/client.rs:117-123 sets
    /// `input_tokens = prompt_tokens - cached_tokens`, so the cached count is a
    /// subset of the prompt. Folding it back into input double-counts it.
    #[test]
    fn a_cached_token_is_never_also_billed_as_an_input_token() {
        let prompt_tokens = 1_000_000;
        let cached = 1_000_000;

        // The correct split: nothing fresh, the whole prompt served from cache.
        let correctly_split = bill(prompt_tokens - cached, cached, 0);
        assert_eq!(correctly_split, 81);

        // The bug: adding the cached count on top of the prompt instead of
        // splitting it out, so the same tokens are billed twice at the dearest rate.
        // 2M input tokens: 2e6/1e6 * 2676.78 * 1.5 = 8030.34 -> 8031, i.e. ~99x
        // the correctly-split charge for the very same prompt.
        let double_counted = bill(prompt_tokens + cached, 0, 0);
        assert_eq!(double_counted, 8031);
        assert!(
            correctly_split < double_counted,
            "double-counting cache hits overcharges {correctly_split} -> {double_counted}"
        );
    }

    #[test]
    fn the_three_token_classes_are_priced_separately() {
        assert_eq!(bill(1_000_000, 0, 0), 4016, "input only");
        assert_eq!(bill(0, 1_000_000, 0), 81, "cache-read only");
        assert_eq!(bill(0, 0, 1_000_000), 16061, "output only");

        // The combined charge is the sum of the parts, per class.
        assert_eq!(bill(1_000_000, 1_000_000, 1_000_000), 20157);
        assert_eq!(4016 + 81 + 16061, 20158);
        assert_eq!(
            bill(1_000_000, 1_000_000, 1_000_000),
            20158 - 1,
            "the combined charge is the per-class total, off by at most one IDR of ceiling"
        );
    }

    /// The function documents `ceil` (money.rs:127). Pin the direction with a
    /// case whose exact value is an integer, so only the direction can differ.
    #[test]
    fn the_idr_cost_rounds_up_never_down_and_never_truncates_to_zero() {
        // 1M tokens * 2.0 IDR/1M * 1.0 = exactly 2.0
        assert_eq!(
            calculate_token_cost_idr(1.0, 1_000_000, 2.0, 0, 0.0, 0, 0.0),
            2
        );
        // just above an integer -> rounds UP, not to nearest, not down
        assert_eq!(
            calculate_token_cost_idr(1.0000001, 1_000_000, 2.0, 0, 0.0, 0, 0.0),
            3
        );
        // just below an integer -> stays at the integer (rounding up is exact)
        assert_eq!(
            calculate_token_cost_idr(0.9999999, 1_000_000, 2.0, 0, 0.0, 0, 0.0),
            2
        );
        // A sub-IDR charge rounds up to 1: free requests are the one thing a
        // ceiling must never produce, because a free request is unbounded usage.
        assert_eq!(calculate_token_cost_idr(1.0, 1, 1.0, 0, 0.0, 0, 0.0), 1);
    }

    #[test]
    fn zero_tokens_or_zero_rates_produce_zero_not_a_negative_charge() {
        assert_eq!(bill(0, 0, 0), 0);
        assert_eq!(
            calculate_token_cost_idr(PRICE, 1_000_000, 0.0, 1_000_000, 0.0, 1_000_000, 0.0),
            0,
            "a zero rate must charge zero, never a negative amount"
        );
        assert_eq!(
            calculate_token_cost_idr(0.0, 1_000_000, R_IN, 1_000_000, R_CACHE, 1_000_000, R_OUT),
            0,
            "a zero multiplier must charge zero"
        );
        assert!(bill(0, 0, 0) >= 0);
    }

    #[test]
    fn a_full_length_response_never_costs_more_than_its_reservation() {
        let estimated_input = 1_000;
        let max_output = 4096;
        let reserved = reserve(estimated_input, max_output);
        assert_eq!(reserved, 70);

        // The hold is a ceiling over EVERY reachable settlement of that request:
        // every cache split of the SAME prompt, and output anywhere up to the cap.
        //
        // `cache_read <= estimated_input` is the reachable domain, not a
        // convenience: upstream/client.rs:117-123 defines
        // `input_tokens = prompt_tokens - cached_tokens`, so the cached count is
        // a subset of the prompt the estimate was taken from. A settlement with
        // more cached tokens than the whole prompt is not a state the upstream
        // can report.
        for cache_read in [0u64, 1, 500, 1_000] {
            let actual = bill(estimated_input - cache_read, cache_read, max_output);
            assert!(
                actual <= reserved,
                "cache_read={cache_read}: charged {actual} but only {reserved} was held"
            );
        }
        for output in [0u64, 1, 1_024, max_output] {
            let actual = bill(estimated_input, 0, output);
            assert!(
                actual <= reserved,
                "output={output}: charged {actual} but only {reserved} was held"
            );
        }

        // The extreme: every prompt token arrives cached, and the response runs
        // to the cap. Still inside the hold, because the hold prices input at
        // the PEAK rate while a cache hit is the cheapest class there is.
        let all_cached_actual = bill(0, estimated_input, max_output);
        assert_eq!(all_cached_actual, 66);
        assert!(all_cached_actual <= reserved);
    }

    #[test]
    fn the_reservation_is_monotonic_in_output_tokens() {
        let previous = [
            (0u64, 5i64),
            (1, 5),
            (1_024, 21),
            (65_536, 1057),
            (384_000, 6172),
        ];
        for (tokens, expected) in previous {
            assert_eq!(
                reserve(1_000, tokens),
                expected,
                "reservation for {tokens} output tokens"
            );
        }

        let mut last = -1;
        for tokens in (0u64..=384_000).step_by(997) {
            let r = reserve(1_000, tokens);
            assert!(
                r >= last,
                "reservation fell from {last} to {r} at {tokens} output tokens"
            );
            last = r;
        }
    }

    #[test]
    fn the_reservation_is_monotonic_in_input_tokens() {
        let mut last = -1;
        for tokens in (0u64..=1_000_000).step_by(9_973) {
            let r = reserve(tokens, 4096);
            assert!(r >= last, "reservation fell at {tokens} input tokens");
            last = r;
        }
        assert!(reserve(2_000, 4096) >= reserve(1_000, 4096));
    }

    /// config/apikita.toml:267-270: off-peak is exactly half of peak, and
    /// billing_basis="peak" reserves at the PEAK rate so a request can never
    /// lose money.
    #[test]
    fn the_off_peak_rate_path_is_exercised_and_is_exactly_half_of_peak() {
        let peak =
            calculate_token_cost_idr(PRICE, 1_000_000, R_IN, 1_000_000, R_CACHE, 1_000_000, R_OUT);
        let off_peak = calculate_token_cost_idr(
            PRICE,
            1_000_000,
            R_IN_OFFPEAK,
            1_000_000,
            R_CACHE_OFFPEAK,
            1_000_000,
            R_OUT_OFFPEAK,
        );

        assert_eq!(peak, 20157);
        assert_eq!(off_peak, 10079);
        assert!(off_peak < peak, "off-peak must bill less than peak");

        for (peak_rate, off_peak_rate) in [
            (R_IN, R_IN_OFFPEAK),
            (R_CACHE, R_CACHE_OFFPEAK),
            (R_OUT, R_OUT_OFFPEAK),
        ] {
            assert_eq!(
                peak_rate / 2.0,
                off_peak_rate,
                "config/apikita.toml:267 - off-peak is exactly HALF of peak"
            );
        }
    }

    /// config/apikita.toml:269 - the reservation uses the PEAK rate even when
    /// the request will settle off-peak, so the hold can never under-reserve.
    #[test]
    fn the_reservation_uses_the_peak_rate_even_off_peak() {
        let peak_reservation = reserve(1_000, 4096);
        let off_peak_reservation =
            calculate_preflight_reservation_idr(PRICE, 1_000, R_IN_OFFPEAK, 4096, R_OUT_OFFPEAK);

        assert_eq!(peak_reservation, 70);
        assert_eq!(off_peak_reservation, 35);
        assert!(
            peak_reservation > off_peak_reservation,
            "reserving at off-peak would under-hold a peak-time request"
        );
    }

    /// The documented property that ties the two functions together.
    #[test]
    fn the_reservation_covers_a_settlement_that_has_cache_hits() {
        let reserved = reserve(1_000, 4096);
        let actual = bill(1_000, 2_000, 4096);
        assert_eq!(reserved, 70);
        assert_eq!(actual, 70);
        assert!(
            actual <= reserved,
            "the hold must cover a settlement with cache hits"
        );
    }

    // =====================================================================
    // Midtrans signature verification (docs/website/04-payments.md:36-48)
    //
    // This is the only control between a forged webhook and a credited wallet
    // (server/src/routes/webhooks.rs:82 - a false return is the 401). The
    // digests below are pinned as literals derived OUTSIDE Rust:
    // `printf '%s' 'order1' '200' '50000.00' 'SB-Mid-server-TEST' | sha512sum`,
    // cross-checked against Python's hashlib.sha512. Calling the function under
    // test to produce its own expected value would prove nothing.
    // =====================================================================

    /// A notification whose signature is whatever the caller passes.
    fn notif(
        order_id: &str,
        status_code: &str,
        gross_amount: &str,
        signature: &str,
    ) -> MidtransNotification {
        MidtransNotification {
            order_id: order_id.to_string(),
            status_code: status_code.to_string(),
            gross_amount: gross_amount.to_string(),
            transaction_status: "settlement".to_string(),
            signature_key: signature.to_string(),
            fraud_status: None,
        }
    }

    /// Known vector: SHA512("order1" + "200" + "50000.00" + "SB-Mid-server-TEST"),
    /// lowercase hex, computed by sha512sum and hashlib outside this crate.
    const VECTOR_SIG: &str = "9157089d17e0d30f6c7b09a99d071a612e8a7a0d393e18bbd9d71c1d449eedd0e175ccf363204d9def23fa4c1e7eada72ac26b2fa43a236da0b683564918aefa";
    const VECTOR_KEY: &str = "SB-Mid-server-TEST";

    #[test]
    fn the_signature_matches_an_independently_computed_sha512_digest() {
        let sig = compute_midtrans_signature("order1", "200", "50000.00", VECTOR_KEY);
        assert_eq!(sig, VECTOR_SIG);
        assert_eq!(sig, sig.to_lowercase(), "hex::encode emits lowercase hex");
    }

    /// The digest is only a signature if the field order is load-bearing:
    /// reordering any two fields must change it, or the concatenation is
    /// ambiguous and two different payloads could share a signature.
    #[test]
    fn reordering_the_signed_fields_changes_the_digest() {
        let correct = compute_midtrans_signature("order1", "200", "50000.00", VECTOR_KEY);
        let reordered = [
            (
                "order_id/status_code",
                compute_midtrans_signature("200", "order1", "50000.00", VECTOR_KEY),
            ),
            (
                "order_id/gross_amount",
                compute_midtrans_signature("50000.00", "200", "order1", VECTOR_KEY),
            ),
            (
                "order_id/server_key",
                compute_midtrans_signature(VECTOR_KEY, "200", "50000.00", "order1"),
            ),
            (
                "gross_amount/server_key",
                compute_midtrans_signature("order1", "200", VECTOR_KEY, "50000.00"),
            ),
            (
                "status_code/gross_amount",
                compute_midtrans_signature("order1", "50000.00", "200", VECTOR_KEY),
            ),
        ];
        for (what, digest) in reordered {
            assert_ne!(
                digest, correct,
                "reordering {what} produced the same digest - the concatenation is ambiguous"
            );
        }
    }

    /// THE REAL ATTACK: replay a genuine signature against a different amount,
    /// order, or status. Every one of the three signed fields must break it.
    #[test]
    fn tampering_with_any_signed_field_is_rejected() {
        let cases: [(&str, &str, &str, &str); 3] = [
            ("gross_amount", "order1", "200", "50000.01"),
            ("order_id", "order2", "200", "50000.00"),
            ("status_code", "order1", "201", "50000.00"),
        ];
        for (field, order_id, status_code, gross_amount) in cases {
            let tampered = notif(order_id, status_code, gross_amount, VECTOR_SIG);
            assert!(
                !verify_midtrans_signature(&tampered, VECTOR_KEY),
                "tampering with {field} was ACCEPTED - a forged webhook would credit a wallet"
            );
        }
        // The untampered payload still verifies, so the cases above are not
        // passing merely because the verifier rejects everything.
        assert!(verify_midtrans_signature(
            &notif("order1", "200", "50000.00", VECTOR_SIG),
            VECTOR_KEY
        ));
    }

    /// Positive control: only the key that produced the signature verifies it.
    #[test]
    fn the_signature_verifies_only_with_the_key_that_produced_it() {
        let good = notif("order1", "200", "50000.00", VECTOR_SIG);
        assert!(verify_midtrans_signature(&good, VECTOR_KEY));
        assert!(!verify_midtrans_signature(&good, "SB-Mid-server-OTHER"));
        assert!(!verify_midtrans_signature(&good, ""));
    }

    /// Malformed signatures. Uppercase hex is REJECTED: the comparison is
    /// byte-wise against the lowercase hex hex::encode emits.
    #[test]
    fn malformed_signatures_are_rejected() {
        let long = format!("{VECTOR_SIG}00");
        let non_hex = "z".repeat(128);
        let cases: [(&str, &str); 4] = [
            ("empty signature", ""),
            ("wrong length (short)", "9157089d17e0d30f"),
            ("wrong length (long)", &long),
            ("non-hex, right length", &non_hex),
        ];
        for (what, signature) in cases {
            assert!(
                !verify_midtrans_signature(
                    &notif("order1", "200", "50000.00", signature),
                    VECTOR_KEY
                ),
                "{what} was accepted"
            );
        }

        // Uppercase hex is the same digest in a different alphabet. Midtrans
        // documents lowercase, so rejecting it is strict and correct.
        let uppercase = VECTOR_SIG.to_uppercase();
        assert_ne!(uppercase, VECTOR_SIG);
        assert!(
            !verify_midtrans_signature(&notif("order1", "200", "50000.00", &uppercase), VECTOR_KEY),
            "uppercase hex must be rejected: the comparison is byte-wise, not case-insensitive"
        );
    }

    /// A malformed notification must not silently bypass the check. Empty and
    /// non-ASCII fields still hash deterministically and round-trip.
    #[test]
    fn empty_and_unicode_fields_still_round_trip() {
        let vectors: [(&str, &str, &str); 3] = [
            ("", "200", "50000.00"),
            ("topup_中文–💸", "200", "50000.00"),
            ("order1", "", ""),
        ];
        for (order_id, status_code, gross_amount) in vectors {
            let sig = compute_midtrans_signature(order_id, status_code, gross_amount, VECTOR_KEY);
            assert_eq!(sig.len(), 128, "SHA-512 hex is always 128 chars");
            assert_eq!(
                sig,
                compute_midtrans_signature(order_id, status_code, gross_amount, VECTOR_KEY),
                "hashing must be deterministic"
            );
            assert!(
                verify_midtrans_signature(
                    &notif(order_id, status_code, gross_amount, &sig),
                    VECTOR_KEY
                ),
                "a valid signature over {order_id:?} must round-trip"
            );
            assert!(
                !verify_midtrans_signature(
                    &notif(order_id, status_code, gross_amount, ""),
                    VECTOR_KEY
                ),
                "an empty signature must never verify, even for empty fields"
            );
        }
    }
}
