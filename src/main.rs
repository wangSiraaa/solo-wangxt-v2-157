use plugin_host::config::AppConfig;
use plugin_host::{build_state, routes, telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    telemetry::init();

    let config = AppConfig::from_env();
    let state = build_state(config.clone()).await?;
    let app = routes::router(state);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    tracing::info!(bind = %config.bind, "plugin-host listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
