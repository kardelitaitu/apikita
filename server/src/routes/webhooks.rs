use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
};
use serde_json::json;
use std::env;
use tracing::{error, info, warn};
use uuid::fmt::Hyphenated;
use uuid::Uuid;

#[cfg(test)]
use crate::db::SHIPPED_CREDIT_EXPIRY_MONTHS;
use crate::db::{credit_topup_transaction, TopupCreditResult};
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
async fn topup_account_id(pool: &sqlx::SqlitePool, order_id: &str) -> Option<Uuid> {
    sqlx::query_scalar::<_, Hyphenated>("SELECT account_id FROM topups WHERE order_id = ?")
        .bind(order_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        // `Hyphenated` is how a TEXT uuid column comes back; the callers want a Uuid.
        .map(|h| h.into_uuid())
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

pub async fn handle_midtrans_webhook(
    State(state): State<AppState>,
    payload: Result<Json<MidtransNotification>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // A body that fails to deserialize never reaches the typed payload, and
    // axum's default rejection is a PLAIN-TEXT response - a contract violation
    // (docs/error-model.md:10: every error returns JSON). Answered here, in
    // the same flat JSON shape the rest of this endpoint uses, with the
    // extractor's own status: 400 for syntactically invalid JSON, 415 for a
    // missing content-type, 422 for JSON that does not fit the notification.
    // Midtrans retries a non-2xx, which is correct: a malformed delivery is
    // worth re-sending once the sender's format is fixed.
    let Json(payload) = match payload {
        Ok(payload) => payload,
        Err(rejection) => {
            warn!(
                status = %rejection.status(),
                body = %rejection.body_text(),
                "Midtrans webhook rejected: malformed request body"
            );
            return (
                rejection.status(),
                Json(json!({"error": "invalid webhook body"})),
            );
        }
    };

    // The handler works on the pool throughout; only the realtime publish needs
    // the hub, which is why the extractor is AppState rather than SqlitePool.
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
        // `topup.rejected` is the event name docs/observability.md:39 defines
        // and the alert registry (docs/observability.md:99, tools/alert/probe.sh)
        // fires on: "any `topup.rejected`" line in the captured stdout. A
        // rejection logged under any other name is an alert that never fires.
        warn!(
            event = "topup.rejected",
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
            let result = credit_topup_transaction(
                pool,
                &payload.order_id,
                amount_idr,
                state.config.wallet.credit_expiry_months,
            )
            .await;

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
                // NOT AN ALERT AND A 404, AND BOTH ARE WORTH A SECOND LOOK. The two
                // neighbouring rejections are more careful than this one: an amount
                // mismatch emits the documented `topup.rejected` event, which is what the
                // `webhook_rejection` alert fires on, and an unverifiable signature emits
                // its own. This arm warns and stops - so a payment for an order we do not
                // have, which means a customer paid and we cannot credit them, is visible
                // only in a log line nobody is required to read.
                //
                // The 404 is the other half. Midtrans, like most senders, treats a non-2xx
                // as a failed delivery and retries on its own schedule, so a condition
                // that can never succeed - the order does not exist and never will - is
                // answered in a way that invites the sender to keep asking. A 200 with a
                // refusal body would stop the retries; the credit behaviour is identical
                // either way, since `NotFound` means no row and no row means no credit.
                //
                // NEITHER IS DECIDED HERE, because both are judgements about the payment
                // provider's contract rather than bugs: 404 is more honest to the sender
                // about what happened, and retrying is harmless for an unknown order while
                // an alert that fires on every stray webhook trains people to ignore it.
                // What is not defensible is leaving it undocumented, which is what this
                // comment is for.
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
                    // Same event-name contract as the signature refusal above:
                    // the registry fires on "any `topup.rejected`".
                    error!(
                        event = "topup.rejected",
                        order_id = %payload.order_id,
                        "Webhook rejected: amount mismatch with stored record"
                    );
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": "amount mismatch"})),
                    )
                }
                Ok(TopupCreditResult::NotSettleable { status }) => {
                    // A settlement webhook arrived for a row that is not
                    // `pending` - the order was denied, expired, or refunded and
                    // then settled again. NO money moved: the guard in
                    // `credit_topup_transaction` refused the transition, which is
                    // what stops a replayed settlement webhook from re-crediting a
                    // refunded order.
                    //
                    // Logged at error level because the delivery is contradictory
                    // and worth seeing, but answered 200: a non-2xx would make
                    // Midtrans retry a webhook that can never succeed. The refund
                    // path's 409 is a different situation - there an operator has
                    // money to resolve by hand.
                    error!(
                        order_id = %payload.order_id,
                        status = %status,
                        "Webhook ignored: order is not settleable"
                    );
                    (
                        StatusCode::OK,
                        Json(json!({"status": "not_settleable", "order_status": status})),
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
        // The platform does NOT do refunds. Midtrans can still SEND one - a
        // merchant-panel refund, a chargeback - and this server applies nothing:
        // no wallet debit, no ledger row, no `topups.status` change.
        //
        // 200 with an explicit refusal body, NOT a 4xx. Midtrans retries a
        // non-2xx for a period, so a 4xx would re-refuse an event that can never
        // succeed - and it would make a deliberate refusal indistinguishable from
        // a transient 5xx in both Midtrans' dashboard and our alerting. The
        // distinct body is the observable signal.
        //
        // Deliberately NOT routed to the `Unrecognised` arm: that would mislabel a
        // policy decision as a classification gap, and an unrecognised status is
        // never assumed to be in progress. The refusal is enumerated explicitly in
        // `money::REFUND_STATUSES`.
        PaymentAction::RefundRefused => {
            // The event field is the machine-readable marker an operator's probe greps
            // for, exactly as the two rejections above set topup.rejected
            // (webhooks.rs:133, :208). Without it this refusal is INVISIBLE to alerting,
            // which is the gap docs/launch-checklist.md:68 names.
            //
            // A DISTINCT name, deliberately, not topup.rejected. They look similar and
            // mean opposite things: a rejection is a payment that failed to land and may
            // owe someone money, while a refusal is a refund DECLINED BY POLICY, which is
            // the system working. Sharing the name would page on routine enforcement and
            // would let a refusal spike hide inside a rejection count.
            //
            // Alerting matters even though the behaviour is correct, because a refusal is
            // the ONE webhook outcome where nothing moves - and a status-mapping
            // regression routing real events into this arm would look identical.
            error!(
                event = "refund.refused",
                order_id = %payload.order_id,
                midtrans_status = %payload.transaction_status,
                "Refund refused: this platform does not do refunds; no money moved"
            );
            (
                StatusCode::OK,
                Json(json!({ "status": "refund_not_supported" })),
            )
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
                "UPDATE topups SET status = ? WHERE order_id = ? AND status = 'pending'",
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

    /// The credit statuses still credit; the refund statuses are refused by policy.
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
        // The platform does NOT do refunds: both refund statuses are refused, not
        // debited. They are NOT `Unrecognised` either - the refusal is a policy, and
        // an unknown status is a different thing entirely.
        assert_eq!(
            evaluate_payment_status("refund", 50000),
            PaymentAction::RefundRefused
        );
        assert_eq!(
            evaluate_payment_status("partial_refund", 50000),
            PaymentAction::RefundRefused
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
            Ok(TopupCreditResult::NotSettleable {
                status: "refunded".into(),
            }),
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
    use crate::test_support::{self, TestDb};
    use axum::body::to_bytes;
    use sqlx::SqlitePool;
    use std::sync::Arc;
    use std::time::Duration;

    /// The key every live test here installs. Deliberately NOT the
    /// fake-midtrans default, so a test that passes without installing it is
    /// impossible to mistake for one that did.
    const LIVE_TEST_SERVER_KEY: &str = "SB-Mid-server-WEBHOOK-LIVE-TEST";

    /// The AppState the router would hand the handler, built from the same
    /// config file the server loads.
    fn live_app_state(pool: SqlitePool) -> AppState {
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
    /// fixture in its OWN migrated SQLite database, runs the assertions in their
    /// own task, then closes the database whether they passed or panicked - and
    /// only then releases the lock and restores the environment.
    ///
    /// The assertions are spawned so a panicking one arrives as a JoinError
    /// instead of unwinding through the teardown. That is what makes the cleanup
    /// unconditional, and it is why the LOCK is held HERE rather than inside the
    /// task (a MutexGuard is not Send). Holding it here is also what excludes
    /// routes::account's Snap tests, which take the same lock.
    ///
    /// The Postgres original deleted its rows by name in FK order here. SQLite
    /// makes that unnecessary: the database is a file, so TestDb builds one per
    /// fixture and close() removes it - there is no shared state to delete rows
    /// out of, and therefore no teardown order to get wrong.
    async fn run_live<F, Fut>(assertions: F)
    where
        F: FnOnce(SqlitePool, Uuid, AppState) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let _env = EnvLock::acquire();
        let db = TestDb::new().await;
        let key_guard = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);

        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let outcome = tokio::spawn(assertions(db.pool.clone(), account_id, state)).await;

        db.close().await;

        outcome.expect("the live webhook assertions panicked");
        drop(key_guard);
    }

    /// An account with the zero-balance wallet the login path creates. A wallet
    /// with no ledger rows is consistent on its own (0 = SUM of nothing), so
    /// this starting point reconciles.
    ///
    /// test_support::account_with_wallet is the same fixture the db.rs and
    /// abuse.rs suites use. The SQLite schema has no DEFAULT for id, created_at
    /// or updated_at, so the Postgres RETURNING id shape would fail at runtime
    /// with a NOT NULL constraint error rather than here.
    async fn fixture_account(pool: &SqlitePool) -> Uuid {
        test_support::account_with_wallet(pool).await
    }

    /// A `pending` topup, written the way routes/account.rs::create_topup writes
    /// it (minus the Snap token, which needs a live Midtrans). Returns its
    /// `order_id`, the key Midtrans notifies on.
    async fn pending_topup(pool: &SqlitePool, account_id: Uuid, amount_idr: i64) -> String {
        test_support::pending_topup(pool, account_id, amount_idr).await
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
    async fn post(
        state: &AppState,
        payload: MidtransNotification,
    ) -> (StatusCode, serde_json::Value) {
        let res = handle_midtrans_webhook(State(state.clone()), Ok(Json(payload)))
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

    async fn balance_of(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT balance_idr FROM wallets WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("read balance")
    }

    async fn topup_status_of(pool: &SqlitePool, order_id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM topups WHERE order_id = ?")
            .bind(order_id)
            .fetch_one(pool)
            .await
            .expect("read topup status")
    }

    async fn ledger_count(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ledger WHERE account_id = ?")
            .bind(account_id.hyphenated())
            .fetch_one(pool)
            .await
            .expect("count ledger rows")
    }

    /// Every ledger row for the account with that reason, oldest first, as
    /// (delta_idr, ref).
    async fn ledger_rows_of(
        pool: &SqlitePool,
        account_id: Uuid,
        reason: &str,
    ) -> Vec<(i64, Option<String>)> {
        sqlx::query_as(
            "SELECT delta_idr, ref FROM ledger WHERE account_id = ? AND reason = ? ORDER BY id",
        )
        .bind(account_id.hyphenated())
        .bind(reason)
        .fetch_all(pool)
        .await
        .expect("read ledger rows")
    }

    /// The reconciliation check from docs/observability.md: wallets.balance_idr
    /// must equal SUM(ledger.delta_idr). Scoped to THIS fixture's account, so a
    /// concurrent writer cannot fail it for a reason unrelated to the handler.
    ///
    /// FULL OUTER JOIN, matching `tools/reconcile/reconcile.sh`, the gate this
    /// transcribes. A LEFT JOIN driven from `wallets` asks a weaker question: it sees
    /// only accounts that HAVE a wallet row, while `ledger.account_id` references
    /// `accounts(id)` and not `wallets`, so a ledger row with no wallet is permitted by
    /// the schema and was invisible to the old form. Measured: 5000 IDR of ledger with
    /// no wallet row reports drift=1 here and reported drift=0 before, so an assertion
    /// built on this helper could pass on an account the shipped gate fails. It is a
    /// sibling of `ledger_drift_rows` in `db.rs`, `routes/auth.rs` and
    /// `routes/account.rs`, which carry the same note.
    ///
    /// NOT PINNED BY A TEST OF ITS OWN, and that is measured rather than assumed:
    /// reverting this SQL to the weak LEFT JOIN it used to be survives the ENTIRE suite.
    /// `routes/admin.rs` and `routes/account.rs` carry guards that do catch their own
    /// copies (`the_admin_drift_helper_sees_ledger_money_with_no_wallet_row` and its
    /// sibling); **this file, `routes/webhooks.rs`, and the copies in `routes/keys.rs`
    /// and `routes/proxy.rs`** have none, so nothing would fail if this file silently
    /// regressed to the weaker rule.
    ///
    /// (The four notes were written from one text and each listed its own file among the
    /// others, so this copy named `webhooks.rs` as though it were elsewhere. Every path
    /// here is qualified because a reader arriving from any one of the four should not
    /// have to work out which "this one" the sentence means.)
    ///
    /// Adding a fourth identical test would close the symptom and leave four copies of one
    /// rule, which is the condition that produced the defect. If this helper is touched
    /// again the real fix is to delete the copies and call one shared definition - the
    /// duplication has now cost two rounds.
    async fn drift_rows(pool: &SqlitePool, account_id: Uuid) -> i64 {
        sqlx::query_scalar(
            r#"
            SELECT COUNT(*) FROM (
                SELECT COALESCE(w.account_id, l.account_id) AS account_id
                FROM wallets w
                FULL OUTER JOIN ledger l ON l.account_id = w.account_id
                WHERE COALESCE(w.account_id, l.account_id) = ?
                GROUP BY w.account_id, l.account_id, w.balance_idr
                HAVING w.account_id IS NULL
                    OR w.balance_idr <> COALESCE(SUM(l.delta_idr), 0)
            ) AS drift
            "#,
        )
        .bind(account_id.hyphenated())
        .fetch_one(pool)
        .await
        .expect("reconciliation query")
    }

    /// THE INVARIANT, asserted after EVERY branch of every test below. A write
    /// that lands on one side only - a credit with no ledger row, a debit with
    /// no ledger row - cannot pass this, whatever the HTTP status said.
    async fn assert_reconciled(pool: &SqlitePool, account_id: Uuid, context: &str) {
        assert_eq!(
            drift_rows(pool, account_id).await,
            0,
            "{context}: wallets.balance_idr must equal SUM(ledger.delta_idr)"
        );
    }

    // -----------------------------------------------------------------------
    // 1. A BAD SIGNATURE is rejected and NOTHING changes.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_rejects_a_bad_signature_and_writes_nothing() {
        run_live(bad_signature_assertions).await;
    }

    async fn bad_signature_assertions(pool: SqlitePool, account_id: Uuid, state: AppState) {
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
            "docs/server/api-spec.md, POST /webhooks/midtrans - a signature mismatch is 401. body: {body}"
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

    #[tokio::test]
    async fn live_webhook_rejects_a_wrong_amount_with_a_valid_signature() {
        run_live(wrong_amount_assertions).await;
    }

    async fn wrong_amount_assertions(pool: SqlitePool, account_id: Uuid, state: AppState) {
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

    #[tokio::test]
    async fn live_webhook_settlement_credits_exactly_once_and_a_replay_does_not() {
        run_live(settlement_then_replay_assertions).await;
    }

    async fn settlement_then_replay_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        state: AppState,
    ) {
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
        assert_eq!(
            status,
            StatusCode::OK,
            "a replay is a 200, not an error: {body}"
        );
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

    #[tokio::test]
    async fn live_webhook_terminal_statuses_persist_the_schema_vocabulary() {
        run_live(terminal_status_assertions).await;
    }

    async fn terminal_status_assertions(pool: SqlitePool, account_id: Uuid, state: AppState) {
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

    #[tokio::test]
    async fn live_webhook_an_unrecognised_status_writes_nothing() {
        run_live(unrecognised_status_assertions).await;
    }

    async fn unrecognised_status_assertions(pool: SqlitePool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;

        // "foobar" is not a Midtrans status. The defect: an unknown value used
        // to fall through to Pending, so the topup stayed pending forever while
        // the handler answered 200 and logged nothing.
        let payload = notification(&order_id, "200", "50000.00", "foobar", LIVE_TEST_SERVER_KEY);

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
            "refund_not_supported",
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
    // 6. REFUND is REFUSED: the platform does not do refunds, so nothing moves.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_refund_is_refused_and_moves_no_money() {
        run_live(refund_refused_assertions).await;
    }

    /// The policy guard, end to end through the handler.
    ///
    /// Midtrans can still SEND a refund - a merchant-panel refund, a chargeback -
    /// and this server does not apply it. The refusal must be OBSERVABLE (a
    /// distinct 200 body, never a 4xx Midtrans would retry, never a bare success
    /// word) and it must be TOTAL: the balance does not move, the topup stays
    /// `settled`, no ledger row appears, and the books still reconcile.
    ///
    /// Asserted TWICE with the SAME payload: a replay is not merely harmless, it is
    /// IDENTICAL. There is no `already_refunded` state to reach, because no refund
    /// state is ever written.
    async fn refund_refused_assertions(pool: SqlitePool, account_id: Uuid, state: AppState) {
        const AMOUNT: i64 = 50_000;

        // Fund through the REAL path, so the wallet and the ledger agree before the
        // refund arrives. Writing wallets.balance_idr directly would manufacture the
        // very drift the reconciliation assertion below then reports.
        let order_id = pending_topup(&pool, account_id, AMOUNT).await;
        assert_eq!(
            credit_topup_transaction(&pool, &order_id, AMOUNT, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("settle the fixture topup"),
            TopupCreditResult::Settled {
                new_balance: AMOUNT
            },
            "the fixture must fund the wallet through the real top-up path"
        );
        assert_reconciled(&pool, account_id, "after funding the fixture").await;

        let refund = notification(&order_id, "200", "50000.00", "refund", LIVE_TEST_SERVER_KEY);

        for attempt in 1..=2 {
            let (status, body) = post(&state, refund.clone()).await;

            // The observable signal, first: a deliberate refusal is a 200 with a
            // body that is NOT any success word. A 4xx here would make Midtrans
            // retry an event that can never succeed, and would be indistinguishable
            // from a transient 5xx in the dashboard.
            assert_eq!(
                status,
                StatusCode::OK,
                "attempt {attempt}: a refusal is a 200 with an explicit body, not a 4xx: {body}"
            );
            assert_eq!(
                body["status"],
                json!("refund_not_supported"),
                "attempt {attempt}: the platform does not refund: {body}"
            );

            // ...and the MONEY, asserted directly rather than through the status: a
            // handler that answered the refusal body and debited anyway would pass a
            // status-only test.
            assert_eq!(
                balance_of(&pool, account_id).await,
                AMOUNT,
                "attempt {attempt}: a refused refund must not debit the wallet"
            );
            assert_eq!(
                topup_status_of(&pool, &order_id).await,
                "settled",
                "attempt {attempt}: a refused refund must leave the topup settled"
            );
            assert_eq!(
                ledger_rows_of(&pool, account_id, "refund").await.len(),
                0,
                "attempt {attempt}: a refused refund must append no ledger row"
            );
            assert_eq!(
                ledger_count(&pool, account_id).await,
                1,
                "attempt {attempt}: the credit must remain the ONLY ledger row"
            );
            assert_reconciled(&pool, account_id, "after a refused refund").await;
        }

        // A SETTLEMENT replayed after the refusal is still a no-op. The refusal
        // left the row `settled`, so the replay is a plain `AlreadySettled`: no
        // credit, no ledger row, no drift. A refusal must not open a second way to
        // re-credit a row.
        let replay = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post(&state, replay).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["status"],
            json!("already_settled"),
            "a settlement replayed after a refusal must credit nothing: {body}"
        );
        assert_eq!(balance_of(&pool, account_id).await, AMOUNT);
        assert_eq!(ledger_count(&pool, account_id).await, 1);
        assert_eq!(topup_status_of(&pool, &order_id).await, "settled");
        assert_reconciled(
            &pool,
            account_id,
            "after a settlement replayed after a refusal",
        )
        .await;
    }
    // -----------------------------------------------------------------------
    // 6b. The refusal is UNCONDITIONAL: an inflated amount is refused too.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_refund_is_refused_even_with_an_inflated_amount() {
        run_live(refund_inflated_amount_assertions).await;
    }

    /// The refusal is a POLICY, not an amount check.
    ///
    /// THE OLD DEFECT: the refund branch parsed `gross_amount` out of the PAYLOAD
    /// and debited it without ever reading `topups.amount_idr`, so a signed
    /// notification naming more than the top-up drained the wallet. The first fix
    /// compared the payload against the stored row. Now the amount is irrelevant:
    /// no refund is applied at any figure, which is what this test pins.
    ///
    /// The wallet here holds TWO top-ups, so the inflated refund is AFFORDABLE -
    /// the balance guard cannot be what saves us, which is the point.
    async fn refund_inflated_amount_assertions(
        pool: SqlitePool,
        account_id: Uuid,
        state: AppState,
    ) {
        const STORED: i64 = 50_000;
        const INFLATED: i64 = 100_000;

        let first = pending_topup(&pool, account_id, STORED).await;
        assert_eq!(
            credit_topup_transaction(&pool, &first, STORED, SHIPPED_CREDIT_EXPIRY_MONTHS)
                .await
                .expect("settle the first fixture topup"),
            TopupCreditResult::Settled {
                new_balance: STORED
            }
        );
        let second = pending_topup(&pool, account_id, STORED).await;
        assert_eq!(
            credit_topup_transaction(&pool, &second, STORED, SHIPPED_CREDIT_EXPIRY_MONTHS)
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

        assert_eq!(
            status,
            StatusCode::OK,
            "the amount does not change the refusal: {body}"
        );
        assert_eq!(
            body["status"],
            json!("refund_not_supported"),
            "an inflated refund must be refused like any other: {body}"
        );

        assert_eq!(
            balance_of(&pool, account_id).await,
            2 * STORED,
            "a refused refund must debit NOTHING - not the payload amount, not the stored one"
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
        assert_eq!(
            ledger_count(&pool, account_id).await,
            2,
            "only the two credits may exist"
        );
        assert_reconciled(&pool, account_id, "after an inflated refund").await;

        // The honest figure is refused EXACTLY the same way, so the two are
        // indistinguishable from the outside - the refusal really is about the
        // policy, not the amount.
        let honest = notification(&first, "200", "50000.00", "refund", LIVE_TEST_SERVER_KEY);
        let (status, body) = post(&state, honest).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["status"], json!("refund_not_supported"), "{body}");
        assert_eq!(balance_of(&pool, account_id).await, 2 * STORED);
        assert_eq!(topup_status_of(&pool, &first).await, "settled");
        assert_reconciled(&pool, account_id, "after the stored-amount refund").await;
    }

    // -----------------------------------------------------------------------
    // MALFORMED BODIES. The typed `Json(payload)` extractor answers BEFORE the
    // handler runs when the body is not a `MidtransNotification`, and axum's
    // default rejection is a PLAIN-TEXT response - which violates
    // docs/error-model.md:10 ("Every error returns the same JSON"). The only
    // way to reach that extractor path is a raw request through a real router,
    // which the typed `post` helper above cannot do.
    // -----------------------------------------------------------------------

    /// Posts a RAW body through a real router, the way Midtrans would.
    async fn post_raw(state: &AppState, body: &str) -> (StatusCode, String, serde_json::Value) {
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::post;
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route("/webhooks/midtrans", post(handle_midtrans_webhook))
            .with_state(state.clone());

        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/midtrans")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .expect("build the raw request"),
            )
            .await
            .expect("the router must respond");

        let status = res.status();
        let content_type = res
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let bytes = to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("every response must have a readable body");
        let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, content_type, parsed)
    }

    /// docs/error-model.md:10 - EVERY error returns the same JSON. A body that
    /// fails to deserialize (a field missing, Midtrans renaming one, a truncated
    /// POST) is answered by the EXTRACTOR, not the handler, so the signature
    /// check never runs and the JSON guarantee was never asserted - until now.
    #[tokio::test]
    async fn live_webhook_answers_a_malformed_body_with_json_not_plain_text() {
        run_live(|pool, _account_id, state| async move {
            // Install a capturing subscriber so the extractor-rejection
            // `warn!` field expressions (webhooks.rs:85-86) are evaluated and
            // counted as covered - they are skipped when no subscriber is active.
            let _capture = capture_logs();
            // (a) Valid JSON that is not a MidtransNotification: required
            //     fields are missing.
            let (status, content_type, body) = post_raw(&state, r#"{"order_id":"x"}"#).await;
            assert!(
                content_type.starts_with("application/json"),
                "a malformed body must be answered with JSON (error-model.md:10), \
                 got content-type '{content_type}' status {status}"
            );
            assert!(
                !body.is_null(),
                "the rejection body must parse as JSON, got: {body}"
            );
            assert!(
                status.is_client_error(),
                "a malformed body is a client error, got {status}"
            );

            // (b) Not JSON at all.
            let (status, content_type, body) = post_raw(&state, "not json at all").await;
            assert!(
                content_type.starts_with("application/json"),
                "a non-JSON body must be answered with JSON (error-model.md:10), \
                 got content-type '{content_type}' status {status}"
            );
            assert!(!body.is_null(), "the rejection body must parse as JSON");
            assert!(status.is_client_error(), "got {status}");

            // (c) Control: a WELL-FORMED but unsigned notification still goes
            //     through the handler and keeps its own JSON shape - the fix
            //     must not have moved the happy path.
            let unsigned = MidtransNotification {
                order_id: "order_x".to_string(),
                status_code: "200".to_string(),
                gross_amount: "50000.00".to_string(),
                transaction_status: "settlement".to_string(),
                signature_key: "not-a-real-signature".to_string(),
                fraud_status: None,
            };
            let bytes = serde_json::to_string(&unsigned).expect("serialize the control body");
            let (status, content_type, body) = post_raw(&state, &bytes).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
            assert!(
                content_type.starts_with("application/json"),
                "the signature refusal must stay JSON, got '{content_type}'"
            );

            // Nothing above may have written anything, whatever it answered.
            assert_reconciled(&pool, _account_id, "after malformed bodies").await;
        })
        .await;
    }

    // -----------------------------------------------------------------------
    // THE REJECTION EVENT LOG. docs/observability.md:39 names the event
    // `topup.rejected` (warn) — "Webhook verification failure - investigate" —
    // and the alert registry (docs/observability.md:99, tools/alert) fires on
    // "any `topup.rejected`" line in the server's captured stdout. A rejection
    // that is logged under a DIFFERENT name is an alert that can never fire.
    // -----------------------------------------------------------------------

    /// An `io::Write` sink that captures the log lines the process would emit,
    /// so the assertion reads what an operator's probe would read.
    struct LogSink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogSink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log sink lock").extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// Installs a capturing subscriber for the current thread and returns the
    /// captured buffer. The guard must be held while the code under test runs.
    fn capture_logs() -> (
        tracing::subscriber::DefaultGuard,
        std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    ) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let writer_sink = sink.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(move || LogSink(writer_sink.clone()))
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        (guard, sink)
    }

    fn captured_lines(sink: &std::sync::Mutex<Vec<u8>>) -> String {
        String::from_utf8(sink.lock().expect("log sink lock").clone()).expect("log lines are utf-8")
    }

    /// `fraud_status` is parsed and never read, and this pins what that means.
    ///
    /// Midtrans sends its own fraud verdict alongside `transaction_status`. This
    /// crate treats the latter as the authority and ignores the former, so a
    /// validly-signed notification that says `fraud_status: "deny"` alongside
    /// `transaction_status: "settlement"` is CREDITED. That is the current
    /// behaviour, asserted here so it is a decision rather than an oversight.
    ///
    /// The test is built on the reasoning that makes it safe today: a challenged
    /// payment arrives as `pending` (no credit) and a fraud-rejected one as
    /// `deny` (`TerminalNoAction`), so `fraud_status` adds nothing the primary
    /// signal does not already carry. What is NOT claimed is that Midtrans can never
    /// send `settlement` with a non-`accept` fraud verdict — that is a claim about a
    /// third party's API, and this repository is not the place to assert it.
    ///
    /// Worth pinning for a second reason: the SIGNATURE DOES NOT COVER
    /// `fraud_status`. `compute_midtrans_signature` hashes order_id, status_code,
    /// gross_amount and the server key, so this field is the one part of the payload
    /// an attacker could alter freely. That is precisely why it must not become an
    /// authority without also becoming signed — and why treating it as one now would
    /// be worse than ignoring it.
    #[tokio::test]
    async fn a_validly_signed_settlement_credits_even_when_fraud_status_says_deny() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let order_id = pending_topup(&db.pool, account_id, 50_000).await;
        let mut payload = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        // Signed correctly for every field the signature ACTUALLY covers.
        payload.fraud_status = Some("deny".into());

        let (status, body) = post(&state, payload).await;
        assert_eq!(status, StatusCode::OK, "the body is {body}");
        assert_eq!(body["status"], json!("settled"), "the body is {body}");
        assert_eq!(
            topup_status_of(&db.pool, &order_id).await,
            "settled",
            "fraud_status is deliberately not consulted: transaction_status is the \
             authority, and Midtrans reports a challenged payment as pending and a \
             fraud-rejected one as deny. If this ever needs to change, the change is \
             to consult it HERE and to add it to compute_midtrans_signature, because a \
             field the signature does not cover cannot be an authority."
        );
    }

    #[tokio::test]
    async fn a_webhook_rejection_is_logged_under_the_documented_topup_rejected_event() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        // (a) A bad signature: the registry's verification-failure case.
        let bad_signature = notification("order_x", "200", "50000.00", "settlement", "wrong-key");
        {
            let (_guard, sink) = capture_logs();
            let _ = handle_midtrans_webhook(State(state.clone()), Ok(Json(bad_signature))).await;
            let logged = captured_lines(&sink);
            assert!(
                logged.contains("topup.rejected"),
                "a rejected signature must be logged under the documented event name \
                 'topup.rejected' (docs/observability.md:39); the probe and the alert registry \
                 key on it. Logged instead:\n{logged}"
            );
        }

        // (b) An amount mismatch: a VALID signature over a wrong amount.
        let order_id = pending_topup(&db.pool, account_id, 50_000).await;
        let inflated = notification(
            &order_id,
            "200",
            "99999.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        {
            let (_guard, sink) = capture_logs();
            let _ = handle_midtrans_webhook(State(state.clone()), Ok(Json(inflated))).await;
            let logged = captured_lines(&sink);
            assert!(
                logged.contains("topup.rejected"),
                "an amount mismatch must be logged under the documented event name \
                 'topup.rejected' (docs/observability.md:39). Logged instead:\n{logged}"
            );
        }

        // (c) Control: a legitimate settlement is NOT a rejection and must not
        //     raise the event - an alert that fires on every payment is as
        //     broken as one that never fires.
        let order_id = pending_topup(&db.pool, account_id, 10_000).await;
        let settled = notification(
            &order_id,
            "200",
            "10000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        {
            let (_guard, sink) = capture_logs();
            let _ = handle_midtrans_webhook(State(state.clone()), Ok(Json(settled))).await;
            let logged = captured_lines(&sink);
            assert!(
                !logged.contains("topup.rejected"),
                "a settled payment must not be logged as a rejection. Logged:\n{logged}"
            );
        }

        assert_reconciled(&db.pool, account_id, "after the rejection logging").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // THE REFUND-REFUSAL EVENT. docs/launch-checklist.md:68 names this as the one
    // outstanding code-shaped item in Gate 2: "unalerted, a refusal is
    // indistinguishable from a bug".
    //
    // WHY IT NEEDS ITS OWN EVENT RATHER THAN REUSING `topup.rejected`: they look
    // similar and mean opposite things. A `topup.rejected` is a payment that FAILED to
    // land - someone may be owed money and the registry says "investigate
    // immediately". A refund refusal is a refund REQUESTED AND DECLINED BY POLICY, which
    // is the system working. Folding them together would page on routine policy
    // enforcement, and - worse - would let a refund-refusal spike hide inside a
    // rejection count.
    //
    // WHY IT NEEDS ALERTING AT ALL, since the behaviour is correct: a refusal is the
    // ONE webhook outcome where NOTHING MOVES - the topup stays `settled`, no ledger
    // row is appended, the balance is untouched. That is also exactly what a
    // status-mapping regression routing real events into this arm would look like. Both
    // cases are "no visible change", so without a distinct log marker they are
    // indistinguishable to an operator.
    // -----------------------------------------------------------------------

    /// The refusal carries a stable, greppable event name - the thing `probe.sh`
    /// greps for. RED FIRST: the log line has a MESSAGE and two fields but no `event`,
    /// so the one alert path that could see it cannot.
    #[tokio::test]
    async fn a_refund_refusal_is_logged_under_its_own_documented_event() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let order_id = pending_topup(&db.pool, account_id, 50_000).await;
        let refund = notification(&order_id, "200", "50000.00", "refund", LIVE_TEST_SERVER_KEY);

        let (status, body) = {
            let (_guard, sink) = capture_logs();
            let (status, body) = post(&state, refund).await;
            let logged = captured_lines(&sink);

            // The event name an operator's probe keys on. Without it the refusal is
            // invisible to alerting, which is the whole finding.
            assert!(
                logged.contains("refund.refused"),
                "a refund refusal must carry the documented event `refund.refused`, or the probe cannot see it and an unalerted refusal stays indistinguishable from a bug (docs/launch-checklist.md:68). Logged:\n{logged}"
            );
            // The order id must travel with it, or an operator who sees the page cannot
            // find which top-up to look at.
            assert!(
                logged.contains(&order_id),
                "the refusal must name the order so it can be investigated. Logged:\n{logged}"
            );
            // POSITIVE CONTROL: a refusal must NOT also raise the REJECTION event, or
            // the two would be conflated and a policy decision would page as a failed
            // payment.
            assert!(
                !logged.contains("topup.rejected"),
                "a refund refusal is a POLICY decision, not a failed payment: raising topup.rejected would page on routine enforcement. Logged:\n{logged}"
            );
            (status, body)
        };

        // The behaviour is unchanged and still correct: 200 with the explicit refusal
        // body, and NO money moved.
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["status"], json!("refund_not_supported"));
        assert_reconciled(&db.pool, account_id, "after the refusal logging").await;

        db.close().await;
    }

    // -----------------------------------------------------------------------
    // SHARED HELPER. `post` does not install a subscriber, so the handler's
    // `warn!`/`error!`/`info!` field expressions are skipped (and left
    // uncovered) when no subscriber is active. This variant installs a capturing
    // subscriber for the duration of the call so those arms are fully exercised.
    // -----------------------------------------------------------------------
    async fn post_logged(
        state: &AppState,
        payload: MidtransNotification,
    ) -> (StatusCode, serde_json::Value) {
        let (_guard, _sink) = capture_logs();
        post(state, payload).await
    }

    // -----------------------------------------------------------------------
    // 7. SERVER MISCONFIGURATION: the signing secret is absent or empty.
    // -----------------------------------------------------------------------

    /// An EMPTY/whitespace secret is refused as `server misconfigured`, NOT a
    /// forged-signature 401: an empty key makes the published formula compute a
    /// matchable signature, so it must never be used. The env is mutated here
    /// under the shared lock, with the previous value restored on drop.
    #[tokio::test]
    async fn live_webhook_refuses_an_empty_or_whitespace_server_key() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", "   ");
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        // The signature is computed with the same (refused) key, so it would be
        // valid - but the handler refuses BEFORE verifying, which is the point.
        let payload = notification("order_x", "200", "50000.00", "settlement", "   ");
        let (status, body) = post_logged(&state, payload).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "an empty/whitespace key must refuse verification, not attempt it: {body}"
        );
        assert_eq!(body["error"], json!("server misconfigured"), "{body}");
        assert_reconciled(&db.pool, account_id, "after a misconfigured empty key").await;
        db.close().await;
    }

    /// The secret is entirely ABSENT (not merely empty). Distinct from the empty
    /// case so an operator can tell the two misconfigurations apart.
    #[tokio::test]
    async fn live_webhook_refuses_a_missing_server_key() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::remove("MIDTRANS_SERVER_KEY");
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let payload = notification("order_x", "200", "50000.00", "settlement", "whatever");
        let (status, body) = post_logged(&state, payload).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "a missing key must refuse verification: {body}"
        );
        assert_eq!(body["error"], json!("server misconfigured"), "{body}");
        assert_reconciled(&db.pool, account_id, "after a missing key").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 8. A BAD gross_amount format is rejected before any money moves.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_rejects_an_unparseable_gross_amount() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        // A valid signature over a non-numeric amount: the amount parse runs
        // AFTER the signature check, so this exercises the parse failure arm
        // specifically, not the rejection arm.
        let payload = notification(
            "order_x",
            "200",
            "not-a-number",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post(&state, payload).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an unparseable gross_amount is a 400: {body}"
        );
        assert_eq!(
            body["error"],
            json!("invalid gross_amount format"),
            "{body}"
        );
        assert_reconciled(&db.pool, account_id, "after a bad gross_amount").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 9. A settlement for an UNKNOWN order is NotFound, not a credit.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_rejects_a_settlement_for_an_unknown_order() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let order_id = "no-such-order-00000000";
        let payload = notification(
            order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post_logged(&state, payload).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "a settlement for an order that does not exist is 404: {body}"
        );
        assert_eq!(body["error"], json!("order not found"), "{body}");
        assert_eq!(balance_of(&db.pool, account_id).await, 0);
        assert_eq!(ledger_count(&db.pool, account_id).await, 0);
        assert_reconciled(&db.pool, account_id, "after an unknown-order settlement").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 10. A settlement for a row that is NOT settleable (denied/expire/cancel)
    //     is refused with 200 + not_settleable; NO money moves.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_refuses_a_settlement_for_a_non_settleable_order() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&db.pool, account_id, AMOUNT).await;
        // First move the row out of `pending` via a real terminal status, so the
        // later settlement finds a non-settleable row.
        let deny = notification(&order_id, "200", "50000.00", "deny", LIVE_TEST_SERVER_KEY);
        let (status, body) = post(&state, deny).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(topup_status_of(&db.pool, &order_id).await, "denied");

        let settle = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post_logged(&state, settle).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["status"],
            json!("not_settleable"),
            "a settlement for a denied order must be refused as not_settleable: {body}"
        );
        assert_eq!(body["order_status"], json!("denied"), "{body}");
        assert_eq!(balance_of(&db.pool, account_id).await, 0, "no money moved");
        assert_eq!(ledger_count(&db.pool, account_id).await, 0);
        assert_reconciled(&db.pool, account_id, "after a not_settleable settlement").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 11. A terminal status for a row that is ALREADY past pending records
    //     nothing and answers terminal_not_applicable.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_terminal_status_for_a_non_pending_row_is_not_applicable() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        const AMOUNT: i64 = 50_000;
        let order_id = pending_topup(&db.pool, account_id, AMOUNT).await;
        // Settle it first; the row is now `settled`, not `pending`.
        let settle = notification(
            &order_id,
            "200",
            "50000.00",
            "settlement",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post(&state, settle).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(topup_status_of(&db.pool, &order_id).await, "settled");

        // Now a terminal status (expire -> expired). The UPDATE WHERE status =
        // 'pending' affects 0 rows, so it records nothing.
        let expire = notification(&order_id, "200", "50000.00", "expire", LIVE_TEST_SERVER_KEY);
        let (status, body) = post_logged(&state, expire).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(
            body["status"],
            json!("terminal_not_applicable"),
            "a terminal status for a non-pending row must be not_applicable: {body}"
        );
        assert_eq!(
            topup_status_of(&db.pool, &order_id).await,
            "settled",
            "the row must be left untouched"
        );
        assert_reconciled(&db.pool, account_id, "after a not_applicable terminal").await;
        db.close().await;
    }

    // -----------------------------------------------------------------------
    // 12. A legitimate in-progress status answers 200 pending and writes nothing.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn live_webhook_an_in_progress_status_is_pending_and_writes_nothing() {
        let _env = EnvLock::acquire();
        let _key = EnvGuard::set("MIDTRANS_SERVER_KEY", LIVE_TEST_SERVER_KEY);
        let db = TestDb::new().await;
        let account_id = fixture_account(&db.pool).await;
        let state = live_app_state(db.pool.clone());

        let order_id = pending_topup(&db.pool, account_id, 50_000).await;
        let payload = notification(
            &order_id,
            "201",
            "50000.00",
            "pending",
            LIVE_TEST_SERVER_KEY,
        );
        let (status, body) = post(&state, payload).await;
        assert_eq!(status, StatusCode::OK, "body: {body}");
        assert_eq!(body["status"], json!("pending"), "{body}");
        assert_eq!(balance_of(&db.pool, account_id).await, 0);
        assert_eq!(ledger_count(&db.pool, account_id).await, 0);
        assert_eq!(topup_status_of(&db.pool, &order_id).await, "pending");
        assert_reconciled(&db.pool, account_id, "after a pending status").await;
        db.close().await;
    }

    /// The flat refusal body shape the handler shares with the rest of the
    /// endpoint. Pure, so it is pinned directly.
    #[test]
    fn error_body_produces_the_flat_error_and_message_shape() {
        let body = error_body("internal_processing_error", "internal processing error");
        assert_eq!(body.0["error"], json!("internal_processing_error"));
        assert_eq!(body.0["message"], json!("internal processing error"));
    }
}
