use apikita_server::{config, db, ip_tracking, routes};

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::AppConfig;
use crate::routes::events::RealtimeHub;
use crate::routes::proxy::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,apikita_server=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting ApiKita Gateway Server");

    // Load configuration
    let config_path =
        env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
    let config = match AppConfig::load_from_file(&config_path) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            // Check fallback path if running from server directory
            match AppConfig::load_from_file("../config/apikita.toml") {
                Ok(c) => Arc::new(c),
                Err(_) => {
                    error!("Failed to load configuration from {}: {}", config_path, e);
                    return Err(e);
                }
            }
        }
    };
    info!(
        currency = %config.pricing.currency,
        min_topup = config.wallet.min_topup,
        models_count = config.models.len(),
        "Configuration validated successfully"
    );

    // Database connection
    let database_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/apikita".into());

    let pool = match db::init_pool(&database_url).await {
        Ok(p) => {
            info!("Database pool established");
            p
        }
        Err(e) => {
            error!("Failed to connect to database at {}: {}", database_url, e);
            return Err(Box::new(e));
        }
    };

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(
            config.circuit_breaker.request_timeout_seconds,
        ))
        .build()?;

    // The realtime fan-out is process-wide: every open /events stream and every
    // publisher shares this one hub, so the replay buffer and the per-account
    // connection count are global rather than per-request.
    let events = Arc::new(RealtimeHub::new(&config.realtime));

    // Parse the trust rules once, here, so a malformed CIDR stops startup
    // instead of being discovered mid-incident. `AppConfig::validate` has
    // already checked them, so this cannot fail in practice.
    let trusted_proxies: Arc<[ip_tracking::IpCidr]> = Arc::from(
        ip_tracking::parse_cidrs(&config.network.trusted_proxy_cidrs)
            .expect("network.trusted_proxy_cidrs validated at load")
            .into_boxed_slice(),
    );
    if trusted_proxies.is_empty() {
        // Not fatal — the relay simply will not be trusted, and every request
        // will be recorded as coming from the relay. That silently disables the
        // sharing signal, so it is worth saying out loud at boot.
        warn_no_trusted_proxies();
    }

    let state = AppState {
        pool,
        config,
        http_client,
        events,
        // One salt per process per day. A restart mints a new one, which can
        // over-count a key's distinct IPs for that day — see the module note
        // for why over-counting is the direction we accept.
        ip_salt: Arc::new(ip_tracking::DailySalt::new()),
        trusted_proxies,
    };

    let app = routes::create_router(state);

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    info!("Listening on http://{}", addr);
    let listener = TcpListener::bind(addr).await?;

    // `with_connect_info` is what makes `ConnectInfo<SocketAddr>` available to
    // the proxy handler. Without it every request would panic at runtime rather
    // than fail to compile, so it is load-bearing for IP tracking — and it is
    // the only reason `axum::serve` is not called with `app` directly.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

fn warn_no_trusted_proxies() {
    warn!(
        "network.trusted_proxy_cidrs is empty: X-Forwarded-For will be ignored and \
         every request will be recorded as coming from the relay, so the \
         distinct-IP sharing signal in docs/ip-tracking.md will not fire"
    );
}
