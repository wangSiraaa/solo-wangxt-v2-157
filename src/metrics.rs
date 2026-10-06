//! Metrics used both for observability and for proving that sandbox resources
//! are reclaimed after an execution ends (`active_executions` returns to 0).

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug)]
pub struct Metrics {
    /// Currently executing guests. Returns to 0 when all stores are dropped.
    pub active_executions: AtomicI64,
    pub executions_started: AtomicU64,
    pub tasks_succeeded: AtomicU64,
    pub tasks_failed: AtomicU64,
    pub fuel_consumed_total: AtomicU64,
    started_at: Instant,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active_executions: AtomicI64::new(0),
            executions_started: AtomicU64::new(0),
            tasks_succeeded: AtomicU64::new(0),
            tasks_failed: AtomicU64::new(0),
            fuel_consumed_total: AtomicU64::new(0),
            started_at: Instant::now(),
        })
    }

    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "active_executions": self.active_executions.load(Ordering::Relaxed),
            "executions_started": self.executions_started.load(Ordering::Relaxed),
            "tasks_succeeded": self.tasks_succeeded.load(Ordering::Relaxed),
            "tasks_failed": self.tasks_failed.load(Ordering::Relaxed),
            "fuel_consumed_total": self.fuel_consumed_total.load(Ordering::Relaxed),
            "uptime_seconds": self.started_at.elapsed().as_secs(),
        })
    }
}
