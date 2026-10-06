//! Server bootstrap: config, logging with tenant-aware redaction, DB,
//! Wasmtime engine, Axum router.


use std::time::Duration;

use axum::extract::DefaultBodyLimit;

use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use wasm_plugin_sandbox::config::Config;
use wasm_plugin_sandbox::service::AppState;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Structured JSON logs. Request parameter values are never passed to
    // tracing macros in this codebase; the redactor below is defense in depth
    // for accidental inclusion of known sensitive header names.
    let json_layer = fmt::layer().json().flatten_event(true);
    let filter = EnvFilter::try_from_env("RUST_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info,wasm_plugin_sandbox=debug,sqlx=warn"));
    tracing_subscriber::registry().with(filter).with(json_layer).init();

    let cfg = Config::from_env();
    tracing::info!(
        bind = %cfg.bind,
        max_memory_bytes = cfg.limits.max_memory_bytes,
        max_fuel = cfg.limits.max_fuel,
        max_output_bytes = cfg.limits.max_output_bytes,
        timeout_ms = cfg.limits.call_timeout_ms,
        "starting sandbox service"
    );

    let db = wasm_plugin_sandbox::db::Db::connect(&cfg.database_url, cfg.db_max_connections).await?;
    let recovered = db.recover_stale().await?;
    if recovered > 0 {
        tracing::warn!(count = recovered, "recovered stale tasks from previous process as failed");
    }

    let metrics = wasm_plugin_sandbox::metrics::Metrics::new();
    let engine = wasm_plugin_sandbox::runtime::SandboxEngine::new(metrics.clone())
        .map_err(|e| std::io::Error::other(format!("wasmtime init: {e}")))?;

    let max_concurrent = std::env::var("MAX_CONCURRENT_EXECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64usize);
    let state = AppState::new(db, engine, cfg.limits, max_concurrent, metrics);

    let listener = tokio::net::TcpListener::bind(&cfg.bind).await?;
    // axum 0.8 Serve has no tcp_keepalive knob; configure the socket directly.
    let std_listener = listener.into_std()?;
    #[cfg(unix)]
    {
        use socket2::{SockRef, TcpKeepalive};
        let ka = TcpKeepalive::new().with_time(Duration::from_secs(30));
        SockRef::from(&std_listener).set_tcp_keepalive(&ka)?;
    }
    std_listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(std_listener)?;
    tracing::info!(addr = %listener.local_addr()?, "listening");

    let app = wasm_plugin_sandbox::http::router(state.clone())
        .layer(DefaultBodyLimit::max(cfg.max_body_bytes));

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    tracing::info!("shutdown complete; closing database pool");
    state.db.pool.close().await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.expect("ctrl-c handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("sigterm handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
