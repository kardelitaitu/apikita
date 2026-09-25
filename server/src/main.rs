use apikita_server::{config, db, routes};

use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::config::AppConfig;
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
    let config_path = env::var("APIKITA_CONFIG_PATH").unwrap_or_else(|_| "config/apikita.toml".into());
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
    let database_url = env::var("DATABASE_URL").unwrap_or_else(|_| {
        "postgres://postgres:postgres@localhost:5432/apikita".into()
    });

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
        .timeout(std::time::Duration::from_secs(config.circuit_breaker.request_timeout_seconds))
        .build()?;

    let state = AppState {
        pool,
        config,
        http_client,
    };

    let app = routes::create_router(state);

    let port: u16 = env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    info!("Listening on http://{}", addr);
    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
