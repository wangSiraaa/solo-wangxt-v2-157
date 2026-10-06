//! Structured (JSON) logging. Span fields — including `tenant_id` — are
//! included in every event, which is what keeps tenant parameters separated
//! in the log stream.

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,plugin_host=debug"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(true)
        .with_target(true)
        .init();
}
