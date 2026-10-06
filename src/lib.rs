//! plugin-host: a sandboxed execution boundary for third-party compute plugins.
//!
//! Guests are WebAssembly modules executed with Wasmtime under a strict
//! sandbox: explicit host-function whitelist (no network, no filesystem),
//! fuel + wall-clock deadlines, memory/output caps, and per-tenant isolation
//! of logs and data. See README.md for the full contract.

pub mod config;
pub mod contract;
pub mod db;
pub mod error;
pub mod executor;
pub mod metrics;
pub mod models;
pub mod routes;
pub mod tasks;
pub mod telemetry;
pub mod tenancy;

use std::sync::Arc;

use crate::config::AppConfig;
use crate::executor::SandboxEngine;
use crate::metrics::Metrics;

#[derive(Clone)]
pub struct AppState {
    pub db: sqlx::PgPool,
    pub executor: Arc<SandboxEngine>,
    pub metrics: Arc<Metrics>,
    pub config: AppConfig,
}

/// Build the application state: connect to PostgreSQL, run migrations, close
/// tasks left in-flight by a previous process, and start the sandbox engine
/// (including its epoch ticker).
pub async fn build_state(config: AppConfig) -> anyhow::Result<AppState> {
    let pool = db::connect(&config.database_url).await?;
    db::migrate(&pool).await?;

    // Crash recovery: tasks that were pending/running when we last stopped can
    // never report a result, so close them as `interrupted` instead of leaving
    // them half-done forever.
    let interrupted = db::reconcile_interrupted(&pool).await?;
    if interrupted > 0 {
        tracing::warn!(interrupted, "closed in-flight tasks left by a previous run");
    }

    let metrics = Metrics::new();
    let executor = SandboxEngine::new(config.limits.clone(), metrics.clone())?;
    executor.run_epoch_ticker();

    Ok(AppState {
        db: pool,
        executor,
        metrics,
        config,
    })
}
