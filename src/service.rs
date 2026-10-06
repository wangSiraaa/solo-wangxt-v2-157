//! Application orchestration: validation pipeline, task lifecycle, bounded
//! blocking execution. Tenant *values* are never logged — only task ids,
//! plugin ids, tenant ids, sizes and outcome codes.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tracing::{debug, instrument, warn};

use crate::db::Db;
use crate::error::ApiError;
use crate::limits::GlobalLimits;
use crate::metrics::Metrics;
use crate::model::{
    validate_contract, validate_and_encode, Contract, ErrorStage, TaskRecord,
};
use crate::runtime::SandboxEngine;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub engine: Arc<SandboxEngine>,
    pub limits: GlobalLimits,
    pub metrics: Metrics,
    /// Bounds how many guest instances may execute at once, so a flood of
    /// infinite-loop modules cannot exhaust host threads.
    pub exec_slots: Arc<Semaphore>,
}

impl AppState {
    pub fn new(db: Db, engine: SandboxEngine, limits: GlobalLimits, max_concurrent: usize, metrics: Metrics) -> Self {
        Self {
            db,
            engine: Arc::new(engine),
            limits,
            metrics,
            exec_slots: Arc::new(Semaphore::new(max_concurrent)),
        }
    }
}

/// Upload-time pipeline result.
pub struct UploadedPlugin {
    pub plugin_id: String,
    pub sha256: String,
    pub interface: serde_json::Value,
}

/// Validate contract and module *before* anything is persisted, then store.
/// Returns distinct error codes for contract vs module failures.
#[instrument(skip(state, wasm_bytes, contract), fields(tenant, wasm_size = wasm_bytes.len()))]
pub async fn upload_plugin(
    state: &AppState,
    tenant: &str,
    name: &str,
    wasm_bytes: &[u8],
    contract: Contract,
) -> Result<UploadedPlugin, ApiError> {
    tracing::Span::current().record("tenant", tenant);

    if wasm_bytes.is_empty() || wasm_bytes.len() > state.limits.max_wasm_bytes {
        return Err(ApiError::bad_request(format!(
            "wasm module must be 1..{} bytes",
            state.limits.max_wasm_bytes
        )));
    }
    if name.is_empty() || name.len() > 128 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(ApiError::bad_request("name must be 1..128 chars of [a-zA-Z0-9_-]"));
    }

    // 1) contract-by-value validation
    let contract_json = serde_json::to_vec(&contract).expect("contract serializes");
    validate_contract(&contract, contract_json.len(), &state.limits)?;

    // 2) static module validation (decode + type check + required ABI exports).
    //    Run on a blocking thread: compilation is CPU work.
    let engine = state.engine.clone();
    let wasm = wasm_bytes.to_vec();
    let module = tokio::task::spawn_blocking(move || {
        let m = engine.validate(&wasm)?;
        let desc = engine.describe(&m);
        Ok::<_, ApiError>((m, desc))
    })
    .await
    .map_err(|e| ApiError::internal(format!("validation task panicked: {e}")))??;
    let interface = module.1;

    // 3) only now persist
    let plugin_id = new_id("plg");
    let hash = hex(wasm_bytes);
    state.db.ensure_tenant(tenant).await?;
    state
        .db
        .insert_plugin(&plugin_id, tenant, name, wasm_bytes, &hash, &contract, &interface)
        .await?;

    debug!(%plugin_id, tenant, sha256 = %hash, "plugin accepted");
    Ok(UploadedPlugin { plugin_id, sha256: hash, interface })
}

/// Result handed back to the HTTP layer for an invocation.
pub struct InvocationResult {
    pub task: TaskRecord,
    pub guest_logs: Vec<String>,
}

/// Full task pipeline. Contract-input violations reject *before* a task row
/// exists; everything after that produces a task row with a single terminal
/// state.
pub async fn invoke(
    state: &AppState,
    tenant: &str,
    plugin_id: &str,
    input: serde_json::Value,
) -> Result<InvocationResult, ApiError> {
    // Load plugin (tenant scoped).
    let plugin = state.db.get_plugin(tenant, plugin_id).await?;

    // Validate input against the declared contract and encode the argument
    // block. No task exists yet, so a contract violation leaves no residue.
    let args = validate_and_encode(&plugin.contract, &input, &state.limits)?;
    let limits = plugin.contract.limits.resolve(&state.limits);
    let allowlist: BTreeSet<String> = plugin.contract.host_allowlist.clone();

    // Admission: bound concurrent instances host-wide.
    let _slot = state
        .exec_slots
        .try_acquire()
        .map_err(|_| ApiError::unavailable("execution capacity saturated; retry later"))?;

    let task_id = new_id("tsk");
    state.db.create_running_task(&task_id, tenant, plugin_id).await?;
    state.metrics.task_created();

    let engine = state.engine.clone();
    let metrics = state.metrics.clone();
    let hash = hex(&plugin.wasm);
    let wasm = plugin.wasm;
    let timeout = limits.timeout_ms;

    // Blocking CPU work; the fuel/epoch deadline inside Wasmtime guarantees
    // the tightest loop traps well within this outer Tokio timeout.
    let started = Instant::now();
    let join = tokio::task::spawn_blocking(move || {
        engine.execute(&hash, &wasm, &args, &allowlist, limits)
    });
    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(timeout.saturating_add(1000)),
        join,
    )
    .await;

    let duration_ms = started.elapsed().as_millis() as i64;
    let result = match outcome {
        Err(_elapsed) => {
            // Extremely defensive: epoch interruption should have trapped
            // already. Mark failed; the blocking thread's Store is dropped at
            // the end of that thread regardless.
            warn!(%task_id, tenant, "outer task timeout fired");
            metrics.timed_out();
            Err(ApiError::execution(
                ErrorStage::Invocation,
                "task terminated: exceeded wall-clock budget",
            ))
        }
        Ok(join_res) => match join_res {
            Err(join_err) => Err(ApiError::internal(format!("execution worker panicked: {join_err}"))),
            Ok(run_res) => run_res.inspect_err(|e| {
                if e.message.contains("fuel budget exhausted") {
                    metrics.fuel_exhausted();
                }
                if e.message.contains("wall-clock") {
                    metrics.timed_out();
                }
                if e.message.contains("output") && e.message.contains("exceeds") {
                    metrics.output_oversized();
                }
                if e.message.contains("linear memory") || e.message.contains("memory limit") {
                    metrics.memory_denied();
                }
            }),
        },
    };
    // Live-instance/memory gauges are paired by TaskState's Drop inside the
    // Wasmtime store (all return paths), which is the actual reclaim point.

    let (guest_logs, stage, code) = match result {
        Ok(out) => {
            // Output contract check (e.g. must be JSON) before it can ever be
            // called a success.
            if let Err(e) = verify_output(&plugin.contract, &out.output) {
                state
                    .db
                    .finalize_failure(
                        &task_id,
                        ErrorStage::Invocation,
                        "output_contract_violation",
                        &e.message,
                        out.fuel_consumed as i64,
                        duration_ms,
                    )
                    .await?;
                metrics.failed(ErrorStage::Invocation);
                let task = state.db.get_task(tenant, &task_id).await?;
                return Ok(InvocationResult { task, guest_logs: out.guest_logs });
            }
            let value = serde_json::from_slice::<serde_json::Value>(&out.output).unwrap_or(
                serde_json::Value::String(base64_encode(&out.output)),
            );
            state
                .db
                .finalize_success(&task_id, &value, out.fuel_consumed as i64, duration_ms)
                .await?;
            metrics.succeeded();
            (out.guest_logs, None, None)
        }
        Err(e) => {
            let stage = e.stage.unwrap_or(ErrorStage::Invocation);
            state
                .db
                .finalize_failure(
                    &task_id,
                    stage,
                    match e.status.is_server_error() {
                        false => failure_code(stage),
                        true => "internal",
                    },
                    &e.message,
                    0,
                    duration_ms,
                )
                .await?;
            metrics.failed(stage);
            (Vec::new(), Some(stage), Some(e))
        }
    };
    let _ = code; // status differentiation is carried by the task body below

    let task = state.db.get_task(tenant, &task_id).await?;
    debug_assert!(stage.is_some() == (task.status == crate::model::TaskStatus::Failed));
    Ok(InvocationResult { task, guest_logs })
}

fn failure_code(stage: ErrorStage) -> &'static str {
    match stage {
        ErrorStage::Validation => "module_invalid",
        ErrorStage::Instantiation => "instantiation_failed",
        ErrorStage::Invocation => "invocation_failed",
    }
}

fn verify_output(contract: &Contract, raw: &[u8]) -> Result<(), ApiError> {
    if contract.output.format == "json" {
        serde_json::from_slice::<serde_json::Value>(raw)
            .map_err(|e| ApiError::execution(ErrorStage::Invocation, format!("output is not valid JSON per contract: {e}")))?;
    }
    Ok(())
}

pub fn hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn base64_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn new_id(prefix: &str) -> String {
    let mut buf = [0u8; 16];
    // Host-side use of /dev/urandom is fine; guests never get this access.
    use std::io::Read;
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf).map(|_| ()))
        .expect("urandom");
    format!("{prefix}_{}", hex_from_bytes(&buf))
}

fn hex_from_bytes(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len() * 2);
    for b in data {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}
