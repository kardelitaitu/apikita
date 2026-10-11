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

    // Database connection.
    //
    // The default is the same local SQLite file `.env.example` documents. It must
    // not be a Postgres URL: this deployment has no Postgres, so a missing
    // DATABASE_URL would otherwise fail inside the driver with a parse error about
    // a scheme rather than an honest "the database is not there".
    let database_url =
        env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://data/server.db".into());

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

    let tasks = apikita_server::shutdown::TaskTracker::new();

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
        tasks: Arc::clone(&tasks),
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
    //
    // ---------------------------------------------------------------------
    // TWO-PHASE SHUTDOWN, and the second phase is the one that is easy to miss.
    //
    // `with_graceful_shutdown` stops accepting and drains CONNECTIONS. It does NOT await detached
    // `tokio::spawn` tasks, because those are owned by the runtime rather than by the connection that
    // started them. This server bills and releases on detached tasks, so a shutdown that stopped at
    // phase one could tell a customer their request completed while the settlement had not been
    // written - the answer delivered and the wallet never debited, which is quieter than the stranded
    // hold because nothing is left over to alert on.
    //
    // So: phase one drains connections, phase two drains the tracked detached work. The order is
    // forced - draining tasks first would abandon in-flight requests that are still producing them.
    //
    // WHAT THIS DOES NOT DO. It does not make a SIGKILL survivable, and it does not remove the
    // process-kill window that `routes/proxy.rs` documents as deliberate. `db::unpaired_hold_rows`,
    // the `stranded_hold` alert and `hold-sweep --release` remain the backstop for anything this
    // cannot finish - which is why those still have to exist.
    //
    // THE TIMEOUT IS BOUNDED ON PURPOSE. An unbounded drain turns a deploy into an outage: the
    // orchestrator's own kill deadline arrives first, and it arrives without the log line that says
    // what was abandoned. Two seconds is under the usual TERM-to-KILL grace and is long enough for a
    // settlement that was going to finish at all.
    const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let shutdown_tasks = Arc::clone(&tasks);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        info!("shutdown: no longer accepting; draining in-flight connections");
    })
    .await?;

    // Phase two.
    let abandoned = tasks.drain(DRAIN_TIMEOUT).await;
    let _ = shutdown_tasks;
    if abandoned == 0 {
        info!("shutdown: every detached task finished; exiting cleanly");
    } else {
        // WARN, not ERROR, and not silent. Money-path work is being abandoned, and the operator needs
        // the number to know whether `hold-sweep --release` has anything to find. A silent exit here
        // would make the next `stranded_hold` alert the first anybody hears of it.
        warn!(
            abandoned,
            timeout_secs = DRAIN_TIMEOUT.as_secs(),
            "shutdown: detached work was still running when the drain expired. A settlement or hold \
             release may not have been written; `hold-sweep --release` and the `stranded_hold` alert \
             are the intended recovery"
        );
    }

    Ok(())
}

/// Resolve when the process is asked to stop.
///
/// SIGTERM is what an orchestrator sends (`docker stop`, a Kubernetes pod eviction, a systemd restart),
/// and SIGINT is Ctrl-C. Both are handled because a deploy and a human both stop this process, and a
/// handler for only one of them means the other path silently loses its drain.
///
/// NO SIGNAL HANDLER AT ALL would also "work" - the default action terminates the process - which is
/// exactly what this replaced, and why the absence of one was not obvious from the outside: the
/// container still stopped.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing a Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing a SIGTERM handler")
            .recv()
            .await;
    };

    // Windows has no SIGTERM, so the `docker stop` path on a Windows host is a hard terminate and
    // this future never resolves. Declared rather than cfg'd out so the select below keeps one shape.
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => info!("received Ctrl-C"),
        _ = terminate => info!("received SIGTERM"),
    }
}

fn warn_no_trusted_proxies() {
    warn!(
        "network.trusted_proxy_cidrs is empty: X-Forwarded-For will be ignored and \
         every request will be recorded as coming from the relay, so the \
         distinct-IP sharing signal in docs/ip-tracking.md will not fire"
    );
}
