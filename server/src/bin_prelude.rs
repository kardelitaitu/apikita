//! One initialisation path for the scheduled binaries.

//! WHY THIS EXISTS. `bin/ip-purge.rs`, `bin/migrate.rs`, `bin/usage-purge.rs` and
//! `bin/hold-sweep.rs` each carried the SAME fifteen-line prologue: `dotenvy::dotenv()`, the
//! `tracing_subscriber` registry with an env filter, and the `DATABASE_URL` lookup that fails with
//! one message. Four copies of a startup contract, in shipped binaries rather than tests, so a drift
//! between them is a runtime difference nobody would think to look for.
//!
//! THE DRIFT HAD ALREADY STARTED, WHICH IS THE ARGUMENT. `main.rs` - the server itself - defaults
//! `apikita_server` to `debug`, and the four tools default it to `info`. That difference is
//! deliberate (the server is what a person watches; the tools run unattended, where a debug flood is
//! a cost with no reader), but it was written nowhere and reads as a typo in five files. `serve_log`
//! and `tool_log` name the two intents so the next reader sees a decision rather than a misspelling.
//!
//! WHAT IS DELIBERATELY NOT HERE. `main.rs` does NOT call these. It has its own prologue because it
//! also loads the config, binds a listener and installs a shutdown path; folding four lines of that
//! into a shared helper would move a startup step away from the file that owns startup. The tools are
//! the ones that were identical, so the tools are what this serves.

use std::env;

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// The log filter the SERVER falls back to when `RUST_LOG` is unset: `apikita_server` at DEBUG.
///
/// Nearly unreachable in a deployment - `server/Dockerfile` sets `RUST_LOG` in the image and
/// `.env.example` carries it for local work - so this arm fires for a person running the binary by
/// hand, which is exactly who wants DEBUG.
pub const SERVE_LOG: &str = "info,apikita_server=debug";

/// The log filter the SCHEDULED TOOLS fall back to: `apikita_server` at INFO.
///
/// Deliberately one level below the server's. These run from a scheduler with nobody reading the
/// output, so DEBUG would be a flood into a log nobody opens - and the shipped image pins `RUST_LOG`
/// anyway, which makes this a local-run default rather than a production setting.
pub const TOOL_LOG: &str = "info,apikita_server=info";

/// Load `.env` if present, then install the tracing subscriber.
///
/// `dotenv` is `.ok()` rather than `?` on purpose: the container has no `.env` - every variable
/// arrives from the environment - so a missing file is the normal case there and must not be an error.
pub fn init_tracing(default_filter: &str) {
    dotenvy::dotenv().ok();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// The `DATABASE_URL` every scheduled tool needs, with the one message they all used.
///
/// RETURNED AS A `Result` rather than exiting here: `main` returning `Err` is what makes the process
/// exit non-zero, and a scheduler noticing a sweep that did not run is the whole point of that. A
/// helper that called `process::exit` itself would take that decision away from the caller.
pub fn database_url() -> Result<String, Box<dyn std::error::Error>> {
    env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set".into())
}

/// The two lines that open EVERY scheduled binary: initialise logging, then resolve the database.
///
/// ONE COPY, and this is the duplication that round 57's consolidation CREATED rather than fixed - it
/// absorbed the prologue `bin/ip-purge.rs`, `bin/migrate.rs` and `bin/usage-purge.rs` each carried
/// into this module, and their three `main` functions became identical five-line wrappers calling
/// `init_tracing` and `database_url`. The tools kept two copies; now they keep one.
///
/// IT IS A PRINTABLE STEP RATHER THAN A WRAPPER ON PURPOSE. Round 57's other finding was that a
/// helper calling `database_url` and exiting takes the decision away from `main`, and `main` returning
/// `Err` is what makes the process exit non-zero - a scheduler noticing a sweep that did not run is
/// the whole point. So this returns the URL and the caller still owns what a failure means.
///
/// `hold-sweep.rs` does NOT use it: that binary parses `--` options and its own usage text, so its
/// main is a different shape. Consolidating it would be a false one.
pub fn start() -> Result<String, Box<dyn std::error::Error>> {
    init_tracing(TOOL_LOG);
    let database_url = database_url()?;
    Ok(database_url)
}

/// A migrated temp database for a sweep binary's own test, as (url, path).
///
/// ONE COPY, from `bin/ip-purge.rs` and `bin/usage-purge.rs`, which each carried these 19 lines -
/// the same connect options, the same `max_connections(1)`, the same `sqlx::migrate!` call, the same
/// `pool.close()`. The near-duplicate cost real lines rather than a byte copy because the ONE
/// difference is the temp file's prefix (`apikita-ip-purge-test-` versus `apikita-usage-purge-test-`),
/// which is why a body-equality sweep did not flag it.
///
/// `max_connections(1)` is not decoration: the sweeps are single-writer, and a pool wider than one
/// would let a test's own connections queue where the real deployment's do not - which is the
/// difference between testing the sweep and testing the pool.
///
/// The caller owns the returned PATH and should delete it. This deliberately does not, because a
/// test that wants to inspect the database after a failed run needs the file to still be there.
pub async fn migrated_temp_db(prefix: &str) -> (String, std::path::PathBuf) {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let path =
        std::env::temp_dir().join(format!("apikita-{prefix}-test-{}.db", uuid::Uuid::new_v4()));
    let url = format!(
        "sqlite://{}",
        path.to_str()
            .expect("the temp dir is UTF-8")
            .replace('\\', "/")
    );
    let options = SqliteConnectOptions::from_str(&url)
        .expect("a sqlite:// URL this function built")
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("open temp db");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("apply migrations");
    pool.close().await;
    (url, path)
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The two filters differ, and the difference is the point of the module.
    ///
    /// A future edit that "tidied" these into one constant would silently change the server's local
    /// default or the tools' - so the property is asserted rather than left to the doc comment above.
    #[test]
    fn the_server_and_the_tools_default_to_different_levels() {
        assert!(
            SERVE_LOG.ends_with("apikita_server=debug"),
            "the server's fallback must stay at debug; it is what a person running the binary by hand reads"
        );
        assert!(
            TOOL_LOG.ends_with("apikita_server=info"),
            "the tools' fallback must stay at info; they run unattended and nobody reads a debug flood"
        );
        assert_ne!(
            SERVE_LOG, TOOL_LOG,
            "these are two intents, not one value - collapsing them removes the distinction the module exists to state"
        );
        // Both must keep the crate's own module readable, which is what the prefix is for.
        for filter in [SERVE_LOG, TOOL_LOG] {
            assert!(
                filter.contains("apikita_server="),
                "{filter} must name the crate's own target, or the binary's own logs are filtered by the global level alone"
            );
        }
    }
}
