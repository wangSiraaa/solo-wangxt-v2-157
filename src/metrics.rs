//! Process-local operational metrics, rendered in Prometheus text format.
//! Used to *prove* resource reclamation: `sandbox_instances_live` and
//! `sandbox_memory_bytes_live` return to zero after tasks finish.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
struct Counters {
    tasks_total: AtomicU64,
    succeeded_total: AtomicU64,
    failed_validation: AtomicU64,
    failed_instantiation: AtomicU64,
    failed_invocation: AtomicU64,
    fuel_exhausted: AtomicU64,
    timed_out: AtomicU64,
    memory_denied: AtomicU64,
    output_oversized: AtomicU64,
    /// Gauge: stores currently instantiated (must return to 0).
    instances_live: AtomicI64,
    /// Gauge: bytes of guest linear memory currently granted (best-effort
    /// accounting based on growth requests; returns to 0 on store drop).
    memory_bytes_live: AtomicI64,
    /// Gauge: tasks currently executing on blocking threads.
    calls_in_flight: AtomicI64,
}

#[derive(Clone, Default)]
pub struct Metrics(Arc<Counters>);

impl Metrics {
    pub fn new() -> Self {
        Self(Arc::new(Counters::default()))
    }

    pub fn task_created(&self) {
        self.0.tasks_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn succeeded(&self) {
        self.0.succeeded_total.fetch_add(1, Ordering::Relaxed);
    }
    pub fn failed(&self, stage: crate::model::ErrorStage) {
        let c = match stage {
            crate::model::ErrorStage::Validation => &self.0.failed_validation,
            crate::model::ErrorStage::Instantiation => &self.0.failed_instantiation,
            crate::model::ErrorStage::Invocation => &self.0.failed_invocation,
        };
        c.fetch_add(1, Ordering::Relaxed);
    }
    pub fn fuel_exhausted(&self) {
        self.0.fuel_exhausted.fetch_add(1, Ordering::Relaxed);
    }
    pub fn timed_out(&self) {
        self.0.timed_out.fetch_add(1, Ordering::Relaxed);
    }
    pub fn memory_denied(&self) {
        self.0.memory_denied.fetch_add(1, Ordering::Relaxed);
    }
    pub fn output_oversized(&self) {
        self.0.output_oversized.fetch_add(1, Ordering::Relaxed);
    }

    pub fn instance_started(&self, initial_bytes: i64) {
        self.0.instances_live.fetch_add(1, Ordering::Relaxed);
        self.0.memory_bytes_live.fetch_add(initial_bytes, Ordering::Relaxed);
        self.0.calls_in_flight.fetch_add(1, Ordering::Relaxed);
    }
    pub fn memory_grew(&self, delta_bytes: i64) {
        self.0.memory_bytes_live.fetch_add(delta_bytes, Ordering::Relaxed);
    }
    pub fn instance_finished(&self, reserved_bytes: i64) {
        self.0.instances_live.fetch_sub(1, Ordering::Relaxed);
        self.0.memory_bytes_live.fetch_sub(reserved_bytes, Ordering::Relaxed);
        self.0.calls_in_flight.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn render(&self) -> String {
        let c = &self.0;
        let f = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let g = |a: &AtomicI64| a.load(Ordering::Relaxed);
        format!(
            "# HELP sandbox_tasks_total Total tasks created.\n\
             # TYPE sandbox_tasks_total counter\n\
             sandbox_tasks_total {tasks}\n\
             sandbox_tasks_succeeded_total {ok}\n\
             sandbox_tasks_failed_total{{stage=\"validation\"}} {fv}\n\
             sandbox_tasks_failed_total{{stage=\"instantiation\"}} {fi}\n\
             sandbox_tasks_failed_total{{stage=\"invocation\"}} {fc}\n\
             sandbox_tasks_fuel_exhausted_total {fuel}\n\
             sandbox_tasks_timed_out_total {timeout}\n\
             sandbox_memory_growth_denied_total {memdenied}\n\
             sandbox_output_oversized_total {outbig}\n\
             # HELP sandbox_instances_live Guest instances currently allocated.\n\
             # TYPE sandbox_instances_live gauge\n\
             sandbox_instances_live {live}\n\
             # HELP sandbox_memory_bytes_live Guest linear-memory bytes currently granted.\n\
             # TYPE sandbox_memory_bytes_live gauge\n\
             sandbox_memory_bytes_live {memlive}\n\
             # HELP sandbox_calls_in_flight Calls currently executing.\n\
             # TYPE sandbox_calls_in_flight gauge\n\
             sandbox_calls_in_flight {inflight}\n",
            tasks = f(&c.tasks_total),
            ok = f(&c.succeeded_total),
            fv = f(&c.failed_validation),
            fi = f(&c.failed_instantiation),
            fc = f(&c.failed_invocation),
            fuel = f(&c.fuel_exhausted),
            timeout = f(&c.timed_out),
            memdenied = f(&c.memory_denied),
            outbig = f(&c.output_oversized),
            live = g(&c.instances_live),
            memlive = g(&c.memory_bytes_live),
            inflight = g(&c.calls_in_flight),
        )
    }
}
