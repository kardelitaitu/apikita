#![cfg_attr(
    not(test),
    // THIS MODULE ARITHMETIC IS DENIED, not merely permitted.
    //
    // server/src/lib.rs denies unwrap_used and indexing_slicing crate-wide and
    // says so, and it used to also claim that this catches "arithmetic that can
    // overflow in release". It does not. Measured by planting i64 addition,
    // multiplication and subtraction in a production item and asking the gate:
    // all three were ACCEPTED. The lint that does say it is
    // clippy::arithmetic_side_effects, and it was not enabled because it reports
    // sites across the whole crate.
    //
    // This module is where that trade-off stops being acceptable. It is the
    // PRICING code: every IDR figure a customer is charged is produced here, and
    // [profile.release] leaves overflow-checks at its default of OFF, so an
    // overflow in the build that ships wraps silently rather than panicking. The
    // cost is not a wrong number in a log - it is a wrong number in a ledger.
    //
    // It costs nothing here. Measured before adding it: this module has ZERO
    // arithmetic_side_effects sites outside its tests. It reaches the ceiling not
    // by fixing numbers but by never multiplying a money quantity in the first
    // place - the rates are f64 per million tokens and the arithmetic is on the
    // SCALED quantity, then a single saturating cast to i64. So the deny is free
    // today and is a fence for whatever is written next.
    //
    // Scoped to non-test builds, as lib.rs is: a property test that adds 1 to a
    // counter has failed loudly and cost nothing. Seven of this module's own
    // tests would trip it.
    deny(clippy::arithmetic_side_effects)
)]

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
    /// Midtrans' own fraud verdict (`accept` / `deny` / `challenge`), which this
    /// crate PARSES AND NEVER READS.
    ///
    /// Recorded as a decision rather than left implicit, because it is the only field
    /// in the payload with no reader, and it sits on the path that moves money IN.
    /// `evaluate_payment_status` treats `transaction_status` as the authority: a
    /// challenged payment arrives as `pending` (so `Pending`, no credit) and a
    /// fraud-rejected one as `deny` (so `TerminalNoAction`), which is why consulting
    /// `fraud_status` as well would be redundant today.
    ///
    /// WHAT IS NOT CLAIMED HERE: that Midtrans can never send `settlement` together
    /// with a non-`accept` `fraud_status`. That is a claim about a third party's
    /// API, and this repository is not the place to assert it. If Midtrans ever sends
    /// that pair, the customer is credited for a payment that is not final, because
    /// only `transaction_status` is consulted. The test
    /// `a_validly_signed_settlement_credits_even_when_fraud_status_says_deny` pins
    /// exactly that, so the behaviour is deliberate and greppable rather than an
    /// oversight, and changing it is a one-line decision with a test already in
    /// place.
    ///
    /// Note the signature does NOT cover this field: `compute_midtrans_signature`
    /// hashes order_id, status_code, gross_amount and the server key. So a forged
    /// notification could set it freely - which is another reason not to treat it as
    /// an authority without changing what is signed.
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

/// Verifies the Midtrans webhook signature.
///
/// # The comparison, and what "constant time" actually buys here
///
/// Compared with `subtle`, the same primitive `require_bot_token` and `verify_password` use: a
/// byte-by-byte early-exit comparison leaks the secret's PREFIX through timing, and this signature
/// is what authorises a wallet credit.
///
/// WHAT THE GUARANTEE ACTUALLY IS, because the primitive is weaker than "constant time" and - unlike
/// the two sites named above - the weakness is REACHABLE here. `subtle`'s `ConstantTimeEq for [T]`
/// short-circuits on the LENGTHS of its arguments before comparing any byte; its own source
/// (subtle 2.6.1, `lib.rs`) says *"This function short-circuits if the lengths of the input slices
/// are different."* The two operands here are NOT the same width:
///
///   `expected`            = `hex::encode(sha512(...))`, so always 128 lowercase hex chars
///   `signature_key`       = whatever the caller put in the notification body, unvalidated
///
/// So the slices are equal in length only for a 128-byte submission, and every other length returns
/// `false` after the length check alone. MEASURED against a reimplementation of subtle's shape: a
/// wrong-length call runs in ~73 ns where an equal-length one runs in ~3100 ns - the short-circuit
/// is real and observable.
///
/// WHY THAT IS ACCEPTABLE HERE, stated rather than assumed. The length of the EXPECTED value is not
/// secret: Midtrans documents the signature as `sha512(order_id + status_code + gross_amount +
/// server_key)` in hex, so 128 is public knowledge and an attacker who sends 128 bytes already knows
/// they sent the right length. What must not leak is the PREFIX, and the same measurement shows no
/// positional gradient - differing at byte 0 (~3073 ns), byte 127 (~3096 ns) and matching (~3197 ns)
/// are indistinguishable - where a plain `==` separates them (55 ns against the first byte, 59 ns
/// against the last). That gradient is the leak `subtle` exists to remove, and it is gone.
///
/// The length leak is NOT tested, and no test here attempts to: timing is not a property a unit test
/// can assert at these margins, and a test that slept-and-compared would be flaky on CI and would
/// still prove nothing about the real code. What IS tested is the behaviour the check exists for -
/// `money.rs`'s signature tests cover a match, a mismatch and a wrong-length input all returning the
/// right answer - and the reasoning above is recorded where a reader of the comparison will meet it.
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

    /// The pricing function swept rather than illustrated, and the property that
    /// matters most is MONOTONICITY.
    ///
    /// Every other test in this file pins a figure. A figure is one point; the
    /// economic claim the whole billing model rests on is a shape: MORE TOKENS NEVER
    /// COST LESS. A non-monotonic price is not a wrong number, it is a discount a
    /// customer can farm - send a larger prompt, pay less - and no example catches
    /// it, because an example only ever lands on one side of the claim.
    ///
    /// The ladder is ascending and ends at u64::MAX, which is not a hypothetical
    /// input: the token counts arrive from an upstream provider's report over a JSON
    /// body, and a hostile or broken one can carry any u64. That is where the
    /// arithmetic stops being f64 and becomes an i64, and the cast SATURATES rather
    /// than wrapping - so the last rung is the one that proves the ceiling holds at
    /// the top of the range instead of falling off it.
    ///
    /// Each rung is checked against the one below it, in EVERY class independently
    /// and with all three moving together, because a defect that makes one class
    /// non-monotonic need not move the others.
    ///
    /// WHAT IT DOES NOT CATCH, measured rather than assumed. The ladder spans seven
    /// orders of magnitude, which is what makes it a real sweep - and it also means a
    /// change to the RATE is invisible here. A first mutation divided the multiplier
    /// for large prompts, halving the price at every rung above a million tokens, and
    /// the sweep passed: at a billion tokens the halved price is still two million
    /// against four thousand at a million. Monotonicity in the TOKEN COUNT is what is
    /// pinned; a bulk discount - wrong economically, but not non-monotonic - is not
    /// what this test is for, and catching it needs closely-spaced rungs either side of
    /// a threshold, which is a different test for a different purpose.
    #[test]
    fn the_cost_never_decreases_as_any_token_class_grows() {
        const R_IN: f64 = 2676.78;
        const R_CACHE: f64 = 53.54;
        const R_OUT: f64 = 10707.12;

        // Ascending, spanning every order of magnitude a token count can plausibly
        // take, plus the saturating cast at the top.
        let ladder: [u64; 9] = [
            0,
            1,
            2,
            100,
            1_000,
            1_000_000,
            1_000_000_000,
            u64::MAX / 2,
            u64::MAX,
        ];

        let at = |input, cache, output| {
            calculate_token_cost_idr(PRICE, input, R_IN, cache, R_CACHE, output, R_OUT)
        };

        for window in ladder.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            for (label, low, high) in [
                ("input", at(lo, 0, 0), at(hi, 0, 0)),
                ("cache-read", at(0, lo, 0), at(0, hi, 0)),
                ("output", at(0, 0, lo), at(0, 0, hi)),
                ("all three", at(lo, lo, lo), at(hi, hi, hi)),
            ] {
                assert!(
                    high >= low,
                    "{label} tokens {lo} -> {hi} moved the price {low} -> {high}",
                );
            }
        }
    }
    /// The charge is EXACTLY k times the charge for one unit, to within the
    /// accumulated ceiling - and that bound is derived, not guessed.
    ///
    /// With W the wholesale cost of one unit and M the multiplier, the function
    /// returns ceil(n * W * M). Writing c = ceil(n * W * M), and using that ceil(x)
    /// lies in (x-1, x], the true product k*n*W*M is in (k*c - k, k*c], so exactly:
    ///
    ///       k*c - (k-1)  <=  cost(k*n)  <=  k*c
    ///
    /// That is a statement about COLLECTION, not just shape. The rounding error the
    /// platform keeps for itself never exceeds ONE RUPIE PER UNIT BILLED, whatever
    /// the size, the rate or the multiplier - so an operator can say what the worst
    /// case costs, and a customer can bound what rounding can take.
    ///
    /// It is also the test a monotonicity sweep cannot be. Monotonicity asks only
    /// that bigger is not smaller, and a BULK DISCOUNT satisfies it: halve the price
    /// above a million tokens and every rung still rises. This bound does not, because
    /// the discount shows up as the charge falling short of k times by far more than
    /// the (k-1) allowance. The monotonicity test records that it could not catch
    /// that; this one can, and is mutation-checked doing so.
    ///
    /// THE UPPER EDGE CARRIES ONE EXTRA RUPIE, and that is measured rather than
    /// padding. The bound above is exact only in real arithmetic, and this test
    /// FAILED against it: at 3 x 1e9 input tokens the charge came to k*c + 1. The
    /// cause is the f64 product, because 2676.78 is not representable, so
    /// 3000.0 * 2676.78 * 1.5 lands a few billionths ABOVE the exact 12_045_510.0
    /// and the ceiling then bills a whole extra rupiah.
    ///
    /// So the honest answer to "can this ever overcharge?" is: BY ONE RUPIE PER
    /// REQUEST relative to exact proportionality, whenever the true total lands on a
    /// whole number. It does NOT scale with the amount - the f64 error is relative,
    /// around 2e-16, so at a billion rupiah the absolute error is still far under a
    /// rupiah and only the ceiling decides. (Where the i64 cast SATURATES, at the far
    /// end of any ladder, proportionality is not a meaningful claim at all: the charge
    /// is pinned at i64::MAX whatever the units.)
    #[test]
    fn the_charge_scales_exactly_with_the_units_billed() {
        const R_IN: f64 = 2676.78;
        const R_CACHE: f64 = 53.54;
        const R_OUT: f64 = 10707.12;

        // Unit sizes chosen to straddle the interesting magnitudes: below a million
        // (where per-million scaling is well under one rupiah), around it, and far
        // above it, where a f64-per-million product is large enough for its own
        // rounding to matter.
        for n in [1u64, 999, 1_000, 1_000_000, 7_777_777, 1_000_000_000] {
            for k in [2u64, 3, 10, 1000] {
                // One class at a time, so a failure names which one.
                for label in ["input", "cache-read", "output"] {
                    let at = |units| match label {
                        "input" => calculate_token_cost_idr(PRICE, units, R_IN, 0, 0.0, 0, 0.0),
                        "cache-read" => {
                            calculate_token_cost_idr(PRICE, 0, 0.0, units, R_CACHE, 0, 0.0)
                        }
                        _ => calculate_token_cost_idr(PRICE, 0, 0.0, 0, 0.0, units, R_OUT),
                    };

                    let one = at(n);
                    let many = at(n.saturating_mul(k));
                    let scaled = one.saturating_mul(k as i64);
                    let allowance = k as i64 - 1;

                    assert!(
                        many <= scaled + 1,
                        "{label}: {k} x {n} billed {} but one unit alone bills {one}, so k \\
                         units should bill at most {} - more than proportionally, and more \\
                         than the one rupiah of f64 rounding",
                        many,
                        scaled + 1
                    );
                    assert!(
                        many >= scaled - allowance,
                        "{label}: {k} x {n} billed {} but one unit alone bills {one}, so k \\
                         units should bill at least {} - the allowance is one rupiah per \\
                         unit, and a discount hides in the gap",
                        many,
                        scaled - allowance
                    );
                }
            }
        }
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

    /// The hold covers a CACHE-READ settlement, and the reason it does is a CONFIG
    /// RULE stated somewhere else entirely.
    ///
    /// The pre-flight reservation prices the whole prompt at the INPUT rate, because
    /// at reservation time nobody knows which tokens the upstream will report as
    /// cache hits. Settlement then splits those same tokens and prices the cache
    /// subset at its own, cheaper, rate. So the hold covers the settlement exactly
    /// because cache_read_peak <= input_peak, which config.rs now REFUSES to
    /// violate.
    ///
    /// Two files, one invariant, and until now nothing connected them. The test above
    /// proves the property with its OWN hardcoded rates, so it would keep passing if
    /// the shipped config ever put the cache rate above the input rate - which is
    /// precisely the configuration that would strand a hold on every cache-heavy
    /// request, silently, at settlement time.
    ///
    /// So this reads the SHIPPED rates and re-derives the sweep from them. If the two
    /// are ever transposed, this fails, and it names which rule broke.
    #[test]
    fn the_hold_covers_a_cache_settlement_at_the_shipped_rates() {
        use crate::config::AppConfig;

        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        let model = config
            .models
            .iter()
            .find(|m| m.name == "flash")
            .expect("the shipped config must carry the flash model");
        let r_in = model.rates.input_peak;
        let r_cache = model.rates.cache_read_peak;
        let r_out = model.rates.output_peak;

        // The rule itself, asserted FIRST so the failure names the cause rather than
        // a downstream symptom. It is the same inequality config.rs enforces in
        // validate(); asserting it here as well is what makes the connection visible
        // from the money side.
        assert!(
            r_cache <= r_in,
            "cache_read_peak ({r_cache}) is above input_peak ({r_in}). The hold prices \
             a cache hit at the INPUT rate, so a higher cache rate would make every \
             cache-heavy request settle ABOVE its own reservation"
        );

        for estimated_input in [1u64, 1_000, 1_000_000, 7_777_777] {
            for max_output in [0u64, 1_024, 384_000] {
                let reserved = calculate_preflight_reservation_idr(
                    model.price,
                    estimated_input,
                    r_in,
                    max_output,
                    r_out,
                );
                // Every split of the prompt the upstream could report, from none
                // cached to all of it.
                for cache_read in [0u64, 1, estimated_input / 2, estimated_input] {
                    let actual = calculate_token_cost_idr(
                        model.price,
                        estimated_input - cache_read,
                        r_in,
                        cache_read,
                        r_cache,
                        max_output,
                        r_out,
                    );
                    assert!(
                        actual <= reserved,
                        "prompt {estimated_input} (of which {cache_read} cached) and \
                         {max_output} output: charged {actual} against a hold of \
                         {reserved} - a shortfall is written off silently, because the \
                         debit is clamped to what was reserved"
                    );
                }
            }
        }
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

    /// The off-peak rates, asserted against the rates the config SHIPS and against
    /// the property the money model actually needs.
    ///
    /// The two tests below pin peak and off-peak to LITERALS copied out of
    /// config/apikita.toml. That is the weakness fixed for the cache rates two
    /// rounds ago, in this same file: if the config is edited, they keep passing
    /// while asserting a fiction.
    ///
    /// WHAT THIS ASSERTS, and why it is not the claim the neighbouring test's NAME
    /// makes. That test says off-peak is "exactly half of peak", and for the model it
    /// was copied from (flash) that is true. It is NOT true of the whole list: the
    /// off-peak rates are half the provider's YUAN figure, and because the CNY to
    /// IDR conversion is not exact, halving before converting and halving after
    /// differ by a fraction of a rupiah per million tokens. Two of the six shipped
    /// models show it - 6022.76 against a half of 12045.50, and 3212.14 against a
    /// half of 6424.27 - and the other four land exactly. The config comment said
    /// "exactly HALF" as a blanket statement; it now states the two exceptions.
    ///
    /// What the money model REQUIRES is weaker and is what is asserted here, over
    /// every model and every class:
    ///
    ///   1. off-peak is never dearer than peak, so a hold taken at the peak rate can
    ///      never be short for a request that settles off-peak;
    ///   2. a reservation priced at peak is never below the same reservation priced
    ///      off-peak, which is the same statement applied to the function the
    ///      handler actually calls.
    ///
    /// Neither is currently asserted against the shipped config, and a config edit
    /// that transposed a pair would pass every test in this file.
    ///
    /// What is NOT claimed: that any request settles off-peak. billing_basis is
    /// unwired - every settlement prices from the peak rates - so this is a
    /// guarantee that the hold is sized for the dearer case, not a description of a
    /// path that runs. config.rs's unwired list and the decision register say so; the
    /// repetition here keeps the money side honest too.
    #[test]
    fn off_peak_is_never_dearer_than_peak_in_any_shipped_model() {
        use crate::config::AppConfig;

        let config = AppConfig::load_from_file("../config/apikita.toml")
            .or_else(|_| AppConfig::load_from_file("config/apikita.toml"))
            .expect("config/apikita.toml must load");
        assert!(
            !config.models.is_empty(),
            "a config with no models would pass every assertion below over an empty set"
        );

        for model in &config.models {
            for (name, peak, off_peak) in [
                ("input", model.rates.input_peak, model.rates.input_offpeak),
                (
                    "cache-read",
                    model.rates.cache_read_peak,
                    model.rates.cache_read_offpeak,
                ),
                (
                    "output",
                    model.rates.output_peak,
                    model.rates.output_offpeak,
                ),
            ] {
                assert!(
                    off_peak <= peak,
                    "model {}: {name} off-peak {off_peak} is ABOVE peak {peak}, so a \
                     hold taken at the peak rate would be short for a request that \
                     settles off-peak",
                    model.name
                );
            }

            // The same statement applied to the function the handler calls.
            for input in [0u64, 1, 1_000, 1_000_000, 7_777_777] {
                for output in [0u64, 1_024, 384_000] {
                    let at_peak = calculate_preflight_reservation_idr(
                        model.price,
                        input,
                        model.rates.input_peak,
                        output,
                        model.rates.output_peak,
                    );
                    let at_off_peak = calculate_preflight_reservation_idr(
                        model.price,
                        input,
                        model.rates.input_offpeak,
                        output,
                        model.rates.output_offpeak,
                    );
                    assert!(
                        at_peak >= at_off_peak,
                        "model {}: a hold for {input} input and {output} output is \
                         {at_peak} at the peak rate and only {at_off_peak} off-peak, so \
                         the hold is NOT the dearer of the two",
                        model.name
                    );
                }
            }
        }
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
    /// The ledger is the AUTHORITATIVE record of every movement of customer money.
    /// tools/reconcile/reconcile.sql says so in its header: "The ledger is
    /// AUTHORITATIVE; wallets is a cache of it". An edit to it is therefore a MONEY
    /// DEFECT by construction, not a style question.
    ///
    /// docs/launch-checklist.md:87 ticks an append-only claim that was TRUE but
    /// enforced by NOTHING - it held by inspection. A later UPDATE written as a
    /// "correction" would pass every other test and leave the tick green above it.
    ///
    /// Same class as a doc overstating a route (W30) with the polarity reversed: the
    /// claim is accurate TODAY, and the risk is a FUTURE edit.
    mod ledger_is_append_only {
        use std::fs;
        use std::path::{Path, PathBuf};

        /// Every .rs under src/, resolved from the crate root so the scan does not
        /// depend on the process CWD.
        fn rust_sources() -> Vec<PathBuf> {
            fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
                let entries = fs::read_dir(dir).expect("a readable source dir");
                for entry in entries {
                    let path = entry.expect("readable dir entry").path();
                    if path.is_dir() {
                        walk(&path, out);
                    } else if path.extension().is_some_and(|e| e == "rs") {
                        out.push(path);
                    }
                }
            }
            let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
            let mut out = Vec::new();
            walk(&src, &mut out);
            out
        }

        /// The scan must be shown to have read REAL files. W38 lost cycles to an
        /// assertion that passed over a tree it never found, so an empty walk is a
        /// FAILURE here rather than a vacuous pass.
        #[test]
        fn the_scan_actually_read_the_tree() {
            let files = rust_sources();
            assert!(
                files.len() > 10,
                "the source scan found only {} .rs file(s), so it is not reading the real tree and every assertion below would pass vacuously",
                files.len()
            );
            assert!(
                files.iter().any(|p| p.ends_with("money.rs")),
                "money.rs is not among the scanned files, so the scan looks elsewhere"
            );
        }

        #[test]
        fn no_source_statement_mutates_the_ledger() {
            // THIS FILE IS SKIPPED, and that is not a loophole. The needles below appear
            // as string literals in this very function, so scanning it would report the
            // test as its own offender - which is what the first version did. The scan
            // still covers every OTHER source file, and this file is checked by the
            // rest of the suite in the ordinary way.
            //
            // Case-insensitive: SQL keywords appear in either case.
            let this_file = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("money.rs");
            let mut offenders = Vec::new();
            for path in rust_sources() {
                if path == this_file {
                    continue;
                }
                let text = fs::read_to_string(&path).expect("a source file is UTF-8");
                let upper = text.to_uppercase();
                for (needle, label) in [
                    ("UPDATE LEDGER", "UPDATE ledger"),
                    ("DELETE FROM LEDGER", "DELETE FROM ledger"),
                ] {
                    if let Some(at) = upper.find(needle) {
                        let line = upper[..at].matches('\n').count() + 1;
                        offenders.push(format!("{}:{} contains {}", path.display(), line, label));
                    }
                }
            }
            assert!(
                offenders.is_empty(),
                "the ledger is append-only (docs/launch-checklist.md ticks it), but these statements mutate it: {offenders:#?}. Every customer money movement is recorded there and wallets is only a cache of it. Correct a mistake with an OFFSETTING entry, never an edit."
            );
        }

        /// `docs/launch-checklist.md` ticks "**No client-reachable path can write
        /// `balance_idr`**", and that claim was held by NOTHING - the same state the
        /// ledger claim above was in before its test existed ("true by inspection").
        ///
        /// MEASURED: planting a direct `UPDATE wallets SET balance_idr = balance_idr + ?` in a
        /// ROUTE module left all 636 tests passing. The route modules are where
        /// client-reachability comes from, so that is the one place this has to be watched.
        ///
        /// The neighbouring ledger scan does not cover this and cannot: its needles are
        /// `UPDATE LEDGER` and `DELETE FROM LEDGER`, and its own failure message says
        /// "wallets is only a cache of it" - so it knows the two are linked while guarding
        /// exactly one. That gap is why this scan exists next to it rather than inside it.
        ///
        /// The rule being protected: money enters a wallet only through the money module's
        /// transaction, which writes the matching ledger row in the same transaction. A route
        /// that writes `balance_idr` directly manufactures the drift Gate 2 exists to catch,
        /// and does it from a path a customer can reach.
        #[test]
        fn no_route_writes_the_wallet_balance_directly() {
            // Only the route modules. A binary (`bin/hold-sweep.rs`) legitimately updates the
            // balance, in a transaction that also writes the ledger row, and it is not
            // client-reachable; `identity/accounts.rs` inserts a wallet at zero at signup.
            // Neither is what this claim is about, so scoping to `routes/` is the assertion
            // rather than an omission.
            let routes = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("routes");
            let sources: Vec<PathBuf> = rust_sources()
                .into_iter()
                .filter(|p| p.starts_with(&routes))
                .collect();

            // Non-vacuity, the same control the sibling test uses and for the same reason: a
            // scan that found no files would pass without reading anything.
            assert!(
                sources.len() > 5,
                "the route scan found only {} .rs file(s), so it is not reading the real tree and the assertion below would pass vacuously",
                sources.len()
            );

            let this_file = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("money.rs");
            let mut offenders = Vec::new();
            for path in sources {
                if path == this_file {
                    continue;
                }
                let text = fs::read_to_string(&path).expect("a source file is UTF-8");
                // A TEST in a route module may write the balance directly; several fixtures do,
                // and they are not shipped code. Cut at the test module, the same scope rule
                // `tools/backup-check` needed for its `db::` scan.
                let shipped = match text.find("#[cfg(test)]") {
                    Some(at) => &text[..at],
                    None => text.as_str(),
                };
                let upper = shipped.to_uppercase();
                for (needle, label) in [
                    ("UPDATE WALLETS", "UPDATE wallets"),
                    ("INSERT INTO WALLETS", "INSERT INTO wallets"),
                ] {
                    if let Some(at) = upper.find(needle) {
                        let line = upper[..at].matches('\n').count() + 1;
                        offenders.push(format!("{}:{} contains {}", path.display(), line, label));
                    }
                }
            }
            assert!(
                offenders.is_empty(),
                "no client-reachable path may write `balance_idr` (docs/launch-checklist.md ticks it), but a ROUTE writes the wallet table directly: {offenders:#?}. Money enters a wallet only through the money module's transaction, which writes the matching ledger row in the same one - a route writing the cache directly manufactures the drift Gate 2 catches."
            );
        }
    }
    /// Every IDR price in `config/apikita.toml` must be its CNY price x the documented
    /// factor.
    ///
    /// The config states the rule itself (`config/apikita.toml:16-26`):
    ///
    ///   "1 CNY = 2,676.78 IDR ... Every IDR figure below is that CNY price x 2,676.78.
    ///    To re-derive after an FX move: IDR_rate = CNY_rate * 2676.78 ... do not
    ///    hand-edit the IDR values without updating this line."
    ///
    /// NOTHING COMPUTED IT. A search for the factor in `src/` found only hardcoded TEST
    /// FIXTURES, so the rule the file calls out was enforced by care alone. A single
    /// typed digit, or an FX move applied to some models and not others, shifts every
    /// reservation and settlement for that model - and passes every other test, because
    /// those use hardcoded rates rather than the config. The symptom would be a slightly
    /// wrong invoice, the hardest kind of bug to notice and the one a customer finds.
    mod config_prices_are_derived_from_the_documented_fx_rate {
        use std::fs;
        use std::path::Path;

        /// The factor the config documents. A literal, so a change to the comment fails
        /// this test rather than silently re-baselining it.
        const FX_IDR_PER_CNY: f64 = 2676.78;

        /// The factor as the config ITSELF declares it, read from the header line
        /// "1 CNY = <factor> IDR".
        ///
        /// This is why the test reads the comment rather than trusting the constant
        /// above: the file's own procedure is "to re-derive after an FX move ... do not
        /// hand-edit the IDR values without updating this line". A test that only knew the
        /// constant could not catch the instruction being followed for the COMMENT and not
        /// the prices - which is exactly the half-done edit the sentence warns against.
        fn documented_factor(toml: &str) -> f64 {
            let marker = "1 CNY = ";
            let line = toml
                .lines()
                .find(|l| l.contains(marker))
                .unwrap_or_else(|| panic!("config/apikita.toml no longer states the FX factor"));
            let rest = &line[line.find(marker).expect("marker present") + marker.len()..];
            let digits: String = rest
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                .filter(|c| *c != ',')
                .collect();
            digits
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("cannot parse the FX factor from: {line}"))
        }

        /// One `<name> = <idr>  # <cny>` pair. Both comment forms appear in the file: a
        /// bare yen figure and a quoted one.
        struct Pair {
            name: String,
            idr: f64,
            cny: f64,
        }

        fn pairs(toml: &str) -> Vec<Pair> {
            let mut out = Vec::new();
            for line in toml.lines() {
                let Some((code, comment)) = line.split_once('#') else {
                    continue;
                };
                let Some((key, value)) = code.split_once('=') else {
                    continue;
                };
                let name = key.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                let Some(idr) = value.trim().parse::<f64>().ok() else {
                    continue; // a string, bool or array value
                };
                let Some(yen) = comment.find('\u{a5}') else {
                    continue;
                };
                let rest = comment[yen + '\u{a5}'.len_utf8()..].trim_start();
                let digits: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '.')
                    .collect();
                let Some(cny) = digits.parse::<f64>().ok() else {
                    continue;
                };
                out.push(Pair { name, idr, cny });
            }
            out
        }

        fn config() -> String {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../config/apikita.toml");
            fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read config: {e}"))
        }

        /// The fixture guard. W38/W41/W43 all lost cycles to an assertion that passed over
        /// a set it never populated, and a parser is exactly what stops matching after an
        /// unrelated formatting change.
        #[test]
        fn the_parser_found_the_prices() {
            let found = pairs(&config());
            assert!(
                found.len() >= 30,
                "only {} price pairs parsed from config/apikita.toml; the format changed",
                found.len()
            );
            assert!(
                found.iter().any(|p| p.name == "input_peak"),
                "input_peak was not parsed, so the parser is not reading the real prices"
            );
        }

        #[test]
        fn every_idr_price_is_its_cny_price_times_the_documented_factor() {
            let toml = config();
            // THE FACTOR THE FILE DECLARES, not the constant - see documented_factor. A test
            // that only knew the constant could not catch the instruction in the header being
            // followed for the COMMENT and not for the prices.
            let factor = documented_factor(&toml);
            assert!(
                (factor - FX_IDR_PER_CNY).abs() < 0.005,
                "config/apikita.toml declares an FX factor of {factor}, but this test was written for {FX_IDR_PER_CNY}. If the factor genuinely moved, every IDR value must move with it - re-derive them and update the constant here in the same commit."
            );
            let mut wrong = Vec::new();
            for pair in pairs(&toml) {
                let expected = pair.cny * factor;
                // One decimal place in the file, so anything within a digit of rounding is
                // exact by construction.
                if (pair.idr - expected).abs() > 0.06 {
                    wrong.push(format!(
                        "{}: file says {} IDR, but {} CNY x {factor} = {:.2}",
                        pair.name, pair.idr, pair.cny, expected
                    ));
                }
            }
            assert!(
                wrong.is_empty(),
                "these IDR prices are not their CNY price times the documented factor: {wrong:#?}"
            );
        }

        /// A derived price with NO CNY figure beside it cannot be checked, so it must not
        /// exist. This catches a value ADDED without its derivation, which the arithmetic
        /// test above cannot see - it only looks at pairs.
        #[test]
        fn no_price_is_declared_without_its_cny_source() {
            // Exempted BY NAME with a reason, rather than by a blanket suffix rule that
            // would also excuse a real price (the W44 lesson about exemption lists).
            const EXEMPT: &[&str] = &["low_balance_threshold_idr"];
            let toml = config();
            let paired: Vec<String> = pairs(&toml).into_iter().map(|p| p.name).collect();
            let mut orphans = Vec::new();
            for line in toml.lines() {
                let Some((key, _)) = line.split_once('=') else {
                    continue;
                };
                let name = key.trim();
                if !(name.ends_with("_idr")
                    || name.ends_with("_peak")
                    || name.ends_with("_offpeak"))
                {
                    continue;
                }
                if paired.iter().any(|p| p == name) || EXEMPT.contains(&name) {
                    continue;
                }
                orphans.push(name.to_string());
            }
            assert!(
                orphans.is_empty(),
                "these money-shaped keys carry no CNY figure, so nothing can check them: {orphans:#?}"
            );
        }
    }

    /// `tools/fake-midtrans` forges the signature this module verifies, and NOTHING checked that the
    /// two agree.
    ///
    /// WHY THIS IS WORTH A GUARD. `tools/fake-midtrans/send-webhook.mjs` recomputes
    /// `SHA512(order_id + status_code + gross_amount + server_key)` in JavaScript, and its README
    /// transcribes both that formula and `MidtransNotification` verbatim from `money.rs`. That is two
    /// copies of a SECURITY formula, the exact restatement shape this repository keeps finding - and
    /// unlike the price card or the retention windows, nothing read either copy. MEASURED: the tool is
    /// named by no `check.sh` under `tools/`, so it is never executed by CI. It is run by hand, against
    /// a live server, which is the moment a drift is most expensive to diagnose.
    ///
    /// WHAT A DRIFT WOULD LOOK LIKE, and why the wrong diagnosis is the likelier one. If the server's
    /// concatenation order changed, the tool would keep producing a syntactically valid signature that
    /// the server rejects. The person running it sees a webhook refused with `invalid signature`, and
    /// the natural reading is that the WEBHOOK is broken - not that the FORGERY TOOL is out of date.
    /// A guard that couples the two turns that into a failing test naming the file.
    ///
    /// WHAT THIS ASSERTS, and its honest limit. It compares the ORDER OF THE FOUR FIELD NAMES as each
    /// file writes them, plus the hash algorithm - the parts a drift would change and a reader cannot
    /// check across two languages. It does NOT execute the JavaScript, so it cannot prove the two
    /// produce the same digest; it proves the two files name the same inputs in the same sequence,
    /// which is what a change to either would have to alter. The end-to-end proof is the tool run by
    /// hand against a server, and that is stated here rather than implied.
    #[test]
    fn the_forgery_tool_concatenates_the_signature_the_way_this_module_does() {
        // Imported here rather than at the module head: the sibling test above brings them in at its
        // own scope, and a module-level `use std::fs` would be an unused import for the non-test build.
        use std::fs;
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("server/ has a parent")
            .to_path_buf();
        let tool = root
            .join("tools")
            .join("fake-midtrans")
            .join("send-webhook.mjs");
        let js = fs::read_to_string(&tool)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", tool.display()));

        // The Rust side, taken from a call rather than restated: the parameter names in order.
        let rust = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("money.rs"),
        )
        .expect("money.rs must be readable");
        let at = rust
            .find("pub fn compute_midtrans_signature(")
            .expect("the signature function must still exist");
        let sig = &rust[at..];
        let params: Vec<&str> = sig[..sig.find(") ->").expect("a return type")]
            .lines()
            .skip(1)
            .filter_map(|l| {
                let name = l.trim().split(':').next()?.trim();
                (!name.is_empty() && name != "pub fn compute_midtrans_signature(").then_some(name)
            })
            .collect();
        assert_eq!(
            params,
            vec!["order_id", "status_code", "gross_amount", "server_key"],
            "compute_midtrans_signature's parameters changed, so the tool's transcription and this \
             guard are both describing a formula that no longer exists"
        );

        // The tool's four `.update(...)` calls, in order. `camelCase` is the JS spelling of the Rust
        // snake_case above, so the comparison is on the camelCase names the tool actually uses.
        let js_order: Vec<String> = js
            .lines()
            .filter_map(|l| {
                let rest = l.trim().strip_prefix(".update(")?;
                let name = rest.split(')').next()?;
                Some(name.to_string())
            })
            .collect();
        let expected_js = vec!["orderId", "statusCode", "grossAmount", "serverKey"];
        assert_eq!(
            js_order, expected_js,
            "tools/fake-midtrans/send-webhook.mjs concatenates {js_order:?} but money.rs computes \
             SHA512(order_id + status_code + gross_amount + server_key). The tool is NOT run by any \
             check script, so this drift would surface as a webhook refused with `invalid signature` \
             and be read as a broken webhook rather than a stale forgery tool. Fix the order here, or \
             fix the tool - and the README's transcription of the formula along with it."
        );

        // The algorithm, which is the other half of the agreement.
        assert!(
            js.contains("'sha512'") || js.contains("\"sha512\""),
            "the tool no longer uses SHA-512, so it cannot forge what this module verifies"
        );
        assert!(
            sig.contains("Sha512::new()"),
            "compute_midtrans_signature no longer uses Sha512"
        );

        // Vacuity guard: an empty parse would make both comparisons above trivially true.
        assert!(
            !js_order.is_empty() && js_order.len() >= 4,
            "only {} .update(...) call(s) were parsed from the tool, so the comparison above is not \
             looking at the four-field concatenation it claims to check",
            js_order.len()
        );
    }

    /// The tool's PAYLOAD has to name the same fields this struct deserializes, and nothing checked
    /// that either.
    ///
    /// The guard above couples the signature FORMULA. This couples the JSON, which is the other half
    /// of what the tool forges, and it fails differently: `MidtransNotification` has no
    /// `deny_unknown_fields` and `fraud_status` is `Option`, so a RENAMED field on either side does
    /// not error at all. The tool sends a key the server ignores, the struct's field arrives as...
    /// nothing, and the request fails as `invalid_request` or - worse, for an `Option` - is ACCEPTED
    /// with the value silently absent. A missing `signature_key` would be refused; a missing
    /// `fraud_status` would not, and the tool would keep reporting a successful forgery.
    ///
    /// So the assertion is on the SET of field names, and it is symmetric: a field added to the struct
    /// and not to the tool fails, and the reverse fails too. `fraud_status` is included because the
    /// tool does send it.
    #[test]
    fn the_forgery_tool_sends_the_fields_this_struct_deserializes() {
        use std::fs;
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("server/ has a parent")
            .to_path_buf();
        let tool = root
            .join("tools")
            .join("fake-midtrans")
            .join("send-webhook.mjs");
        let js = fs::read_to_string(&tool)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", tool.display()));

        // The struct's fields, from the declaration rather than restated.
        let rust = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("money.rs"),
        )
        .expect("money.rs must be readable");
        let at = rust
            .find("pub struct MidtransNotification {")
            .expect("the notification struct must still exist");
        let body = &rust[at..];
        // Start AFTER the opening brace, or the declaration line is parsed as a field named
        // `struct MidtransNotification` - which is what the first version of this did, and it failed
        // on correct code with that name in the diff.
        let body = &body[body.find('{').expect("the declaration has a brace") + 1..];
        let body = &body[..body.find("\n}").expect("the struct has a closing brace")];
        let mut struct_fields: Vec<&str> = body
            .lines()
            .filter_map(|l| {
                let rest = l.trim().strip_prefix("pub ")?;
                Some(rest.split(':').next()?.trim())
            })
            .filter(|n| !n.is_empty())
            .collect();
        struct_fields.sort_unstable();

        // The tool's returned object literal: the keys before the closing brace.
        let payload_at = js
            .find("  return {")
            .expect("the tool must still return a payload object");
        let payload = &js[payload_at..];
        let payload = &payload[..payload.find('}').expect("the payload object closes")];
        let mut js_fields: Vec<&str> = payload
            .lines()
            .skip(1)
            .filter_map(|l| {
                let t = l.trim();
                let (key, _) = t.split_once(':')?;
                let key = key.trim();
                (!key.is_empty() && !key.contains(' ')).then_some(key)
            })
            .collect();
        js_fields.sort_unstable();

        // Vacuity guards first: either parse returning nothing would make the comparison trivially
        // true, and both are one refactor away from doing exactly that.
        assert!(
            struct_fields.len() >= 5,
            "only {} field(s) were parsed from MidtransNotification, so this is not looking at the \
             struct: {struct_fields:?}",
            struct_fields.len()
        );
        assert!(
            js_fields.len() >= 5,
            "only {} key(s) were parsed from the tool's payload, so this is not looking at its \
             object literal: {js_fields:?}",
            js_fields.len()
        );

        assert_eq!(
            js_fields, struct_fields,
            "tools/fake-midtrans/send-webhook.mjs sends {js_fields:?} but MidtransNotification \
             deserializes {struct_fields:?}. This struct has NO `deny_unknown_fields` and \
             `fraud_status` is an Option, so a renamed field does not error - the tool sends a key the \
             server ignores and the request either fails as `invalid_request` or is ACCEPTED with the \
             value silently absent. The tool is named by no `check.sh` under `tools/`, so this drift \
             would surface only when someone ran it by hand and read a refusal as a broken webhook."
        );
    }
}
