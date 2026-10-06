//! Background task runner: claims a pending task, executes it in the sandbox,
//! and persists the terminal outcome atomically.

use std::sync::atomic::Ordering;

use serde_json::Value;
use uuid::Uuid;

use crate::db;
use crate::models::{PluginRow, TaskOutcome};
use crate::AppState;

pub async fn run_task(state: AppState, task_id: Uuid, plugin: PluginRow, input: Value) {
    match db::claim_task(&state.db, task_id).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!("task was already claimed; skipping execution");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to claim task");
            return;
        }
    }

    let result = state
        .executor
        .execute(plugin.id, &plugin.tenant_id, task_id, &plugin.wasm, &input)
        .await;

    state
        .metrics
        .fuel_consumed_total
        .fetch_add(result.fuel_consumed, Ordering::Relaxed);

    let outcome = match result.outcome {
        Ok(output) => {
            state
                .metrics
                .tasks_succeeded
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                fuel_consumed = result.fuel_consumed,
                "task executed successfully"
            );
            TaskOutcome::Success {
                output,
                fuel: result.fuel_consumed as i64,
            }
        }
        Err(failure) => {
            state.metrics.tasks_failed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                kind = failure.kind(),
                error = %failure.message(),
                fuel_consumed = result.fuel_consumed,
                "task failed"
            );
            TaskOutcome::Failure {
                kind: failure.kind(),
                message: failure.message(),
                fuel: result.fuel_consumed as i64,
            }
        }
    };

    // Single atomic terminal write. If this fails, the task stays `running`
    // and the startup reconciler closes it as `interrupted` — it can never be
    // observed as half-succeeded.
    if let Err(e) = db::finish_task(&state.db, task_id, &outcome).await {
        tracing::error!(error = %e, "failed to persist task outcome");
    }
}
