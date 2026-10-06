//! Environment-driven configuration.

use crate::limits::GlobalLimits;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub database_url: String,
    pub db_max_connections: u32,
    /// HTTP request body ceiling (bytes), covers wasm uploads.
    pub max_body_bytes: usize,
    pub limits: GlobalLimits,
}

impl Config {
    pub fn from_env() -> Self {
        let limits = GlobalLimits::from_env();
        let bind = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://sandbox:sandbox@127.0.0.1:5432/sandbox".into());
        let db_max_connections = std::env::var("DB_MAX_CONNECTIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let max_body_bytes = std::env::var("MAX_BODY_BYTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(limits.max_wasm_bytes + 1024 * 1024);
        Self { bind, database_url, db_max_connections, max_body_bytes, limits }
    }
}
