pub mod account;
pub mod auth;
pub mod events;
pub mod health;
pub mod keys;
pub mod proxy;
pub mod webhooks;

use axum::{
    routing::{get, patch, post},
    Router,
};
use proxy::AppState;

pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Ops
        .route("/health", get(health::health_check))
        // Auth
        .route("/auth/exchange", post(auth::exchange_token))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/logout-all", post(auth::logout_all))
        // Account & Wallet
        .route("/api/me", get(account::get_me))
        .route("/api/usage", get(account::get_usage))
        .route("/api/topups", get(account::get_topups).post(account::create_topup))
        // API Keys
        .route("/api/keys", get(keys::list_keys).post(keys::create_key))
        .route("/api/keys/{id}", patch(keys::update_key))
        .route("/api/keys/{id}/revoke", post(keys::revoke_key))
        // Live updates (SSE)
        .route("/events", get(events::sse_events_handler))
        // Webhooks
        .route("/webhooks/midtrans", post(webhooks::handle_midtrans_webhook))
        // Proxy
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .with_state(state)
}
