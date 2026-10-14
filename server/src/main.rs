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

    // THIS FALLBACK IS `debug` AND THE FOUR TOOLS' IS `info`, WHICH IS DELIBERATE AND WAS
    // UNDOCUMENTED. MEASURED: `main.rs` defaults `apikita_server` to DEBUG while `bin/ip-purge.rs`,
    // `bin/migrate.rs`, `bin/usage-purge.rs` and `bin/hold-sweep.rs` all default it to INFO - the same
    // expression with one level changed and nothing anywhere saying why.
    //
    // The reason it is safe is that the fallback is nearly unreachable in production:
    // `server/Dockerfile` sets `RUST_LOG=info,apikita_server=info` in the image, and `.env.example`
    // carries `RUST_LOG` for local work, so this arm fires only for a developer running the binary with
    // no `.env`. `debug` is the right default there - it is the server a person is watching, and the
    // tools are the ones a scheduler runs unattended, where a debug flood is a cost with no reader.
    //
    // The difference is now written down rather than inferred from an expression that differs by one
    // word in five files, which is the shape that reads as a typo to everyone who is not the author.
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
    // THE TIMEOUT BOUNDS BOTH PHASES, AND THE FIRST VERSION OF THIS DID NOT.
    //
    // MEASURED under a real SIGTERM in WSL, because Windows cannot deliver a catchable signal:
    //
    //     idle server, SIGTERM          -> shutdown in 8.5 ms, exit 0
    //     ONE half-open upload, SIGTERM -> shutdown took 2,101 SECONDS, killed by hand
    //
    // The timeout applied only to phase two. `with_graceful_shutdown` waits INDEFINITELY for in-flight
    // connections to finish, and a slowloris connection never finishes - so the "bounded" drain was
    // unbounded in the only case where it mattered. That was a REGRESSION against the bare
    // `axum::serve` it replaced: a deploy would hang until the orchestrator's own kill deadline, which
    // arrives without the log line the comment above promises.
    //
    // So phase one is raced against the same deadline. A connection that will not finish is abandoned
    // rather than waited for - the trade the comment above already argued for, just not implemented on
    // the phase that needed it.
    //
    // THE BUDGET IS PER PHASE, NOT SHARED, and that is worth stating because the total is therefore up
    // to 2 x DRAIN_TIMEOUT rather than DRAIN_TIMEOUT. MEASURED with a half-open upload: 4,153 ms total,
    // because phase one used its 5s and phase two found the tracker already empty. A shared budget
    // would need `Instant` arithmetic that this does not, and the orchestrator's TERM-to-KILL grace is
    // what actually bounds the whole thing. Do not read DRAIN_TIMEOUT as the total.
    //
    // THE CLOCK STARTS WHEN THE SIGNAL ARRIVES, NOT WHEN THE SERVER DOES, and the first version of this
    // got that wrong in the most expensive way available.
    //
    // MEASURED: `timeout(DRAIN_TIMEOUT, serving)` wraps the SERVE LOOP, so the deadline ran from
    // startup. The server listened on :18152, served /health, and then exited by itself 5.009 seconds
    // later with NO signal and NO client - a five-second lifespan. The warning it printed blamed "a
    // slow or half-open client" when the cause was that nothing had called anything.
    //
    // A WSL test had passed moments earlier ONLY because the script sent SIGTERM within ~5s of startup,
    // which is the coincidence that hid it. The lesson is not "test more" - it is that a timeout must
    // be stated over the interval it actually governs, and this one governed the process lifetime.
    //
    // So the signal is awaited FIRST and the deadline applied to the drain that follows it. The signal
    // fires exactly once, so a `Notify` is enough to share it between the graceful handler and this
    // await without either one consuming it from the other.
    const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let signalled = Arc::new(tokio::sync::Notify::new());
    let serving = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown({
        let signalled = Arc::clone(&signalled);
        async move {
            shutdown_signal().await;
            info!("shutdown: no longer accepting; draining in-flight connections");
            signalled.notify_waiters();
        }
    });

    // RUN UNTIL SIGNALLED, THEN BOUND THE DRAIN - and the two must be raced, not sequenced.
    //
    // THE BUG THIS REPLACES, because it is the same mistake twice in one function. `axum::serve(..)`
    // returns a LAZY future: it does not listen until it is polled. An earlier version awaited the
    // signal first and only then polled it, so the server never bound its listener and never served -
    // MEASURED: `/health` failed after 305 seconds with the process alive, and removing every layer did
    // not help because the layers were never the cause.
    //
    // Before that, the timeout wrapped the serve loop from STARTUP, giving the process a five-second
    // lifespan. Both failures came from applying a deadline to the wrong interval.
    //
    // The shape that is correct: `select!` the running server against the signal. `serving` is polled
    // from the first moment, so it listens; the signal branch fires when one arrives; and only THEN
    // does a clock start, over the drain.
    use std::future::IntoFuture;
    // `IntoFuture` is what makes the serve loop raceable; the trait has to be in scope for
    // `.into_future()`, and without it the call is a compile error rather than a silent no-op - the
    // good direction for this to fail in.
    let mut serving = std::pin::pin!(serving.into_future());

    tokio::select! {
        // The server ran to completion on its own, which means the listener failed rather than a signal
        // arriving. Report it and fall through to the drain.
        result = &mut serving => {
            match result {
                Ok(()) => info!("shutdown: the listener closed without a signal"),
                Err(err) => warn!(error = %err, "shutdown: the server stopped with an error"),
            }
        }
        // A signal arrived. The graceful handler has already been notified by the same event, so the
        // serve loop is winding down; the deadline below starts HERE rather than at startup.
        _ = signalled.notified() => {}
    }

    // PHASE ONE, BOUNDED - and the clock starts at the signal.
    let connections_drained = match tokio::time::timeout(DRAIN_TIMEOUT, &mut serving).await {
        Ok(Ok(())) => true,
        Ok(Err(err)) => {
            // The listener failed while shutting down. Worth saying, but it does not change what
            // happens next: the detached work still has to be drained.
            warn!(error = %err, "shutdown: the server stopped with an error");
            true
        }
        Err(_) => {
            warn!(
                timeout_secs = DRAIN_TIMEOUT.as_secs(),
                "shutdown: in-flight connections did not finish within the drain window and are being \
                 abandoned. A slow or half-open client can cause this; the process is exiting anyway"
            );
            false
        }
    };

    // PHASE TWO. Its own budget, not the remainder of phase one's - see the note on DRAIN_TIMEOUT
    // above for why the total can therefore reach twice that, and why the orchestrator's grace period
    // is what really bounds it.
    let abandoned = tasks.drain(DRAIN_TIMEOUT).await;
    if connections_drained && abandoned == 0 {
        info!("shutdown: every connection and detached task finished; exiting cleanly");
    } else {
        // WARN, not ERROR, and not silent. Money-path work may be abandoned, and the operator needs the
        // number to know whether `hold-sweep --release` has anything to find. A silent exit here would
        // make the next `stranded_hold` alert the first anybody hears of it.
        warn!(
            detached_abandoned = abandoned,
            connections_drained,
            timeout_secs = DRAIN_TIMEOUT.as_secs(),
            "shutdown: the drain window expired with work outstanding. A settlement or hold release may \
             not have been written; `hold-sweep --release` and the `stranded_hold` alert are the \
             intended recovery"
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
