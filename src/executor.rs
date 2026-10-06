//! Sandboxed WebAssembly execution engine.
//!
//! Termination and isolation are enforced with several independent layers:
//!
//! 1. **Fuel** — every guest instruction burns fuel (`Config::consume_fuel`).
//!    An infinite loop traps once the budget is spent; the host is never
//!    blocked for longer than the budget allows.
//! 2. **Epoch deadline** — a background task advances the engine epoch; a
//!    store that outlives its wall-clock deadline traps. This backstops fuel
//!    and also bounds time spent inside host functions.
//! 3. **Outer timeout** — a tokio timeout wraps the whole blocking execution
//!    as a last-resort guard.
//! 4. **StoreLimits** — caps linear memory, tables and instance counts.
//! 5. **Explicit host-function whitelist** — the linker only ever defines
//!    `env.host_log`. No WASI, no sockets, no filesystem. Modules importing
//!    anything else are rejected at upload time and cannot instantiate.
//!
//! Each execution creates a fresh `Store`; when it finishes (or traps) the
//! store is dropped and all linear memory is returned to the OS. The
//! `active_executions` gauge in [`Metrics`] proves reclamation.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;
use wasmtime::{
    Caller, Config, Engine, ExternType, FuncType, Linker, Module, Store, StoreLimits,
    StoreLimitsBuilder, ValType,
};

use crate::config::Limits;
use crate::error::{ApiError, ErrorKind};
use crate::metrics::Metrics;

/// The only host function a guest may import: `env.host_log(level, ptr, len)`.
pub const HOST_LOG_MODULE: &str = "env";
pub const HOST_LOG_NAME: &str = "host_log";

const MAX_LOG_LINE_BYTES: i32 = 4 * 1024;
const MAX_LOG_LINES_PER_TASK: u32 = 128;

pub struct SandboxEngine {
    engine: Engine,
    limits: Limits,
    modules: Mutex<HashMap<Uuid, Arc<Module>>>,
    semaphore: Arc<tokio::sync::Semaphore>,
    metrics: Arc<Metrics>,
}

struct ExecState {
    limits: StoreLimits,
    tenant_id: String,
    task_id: Uuid,
    deadline_hit: bool,
    log_lines: u32,
}

/// Failure of a single execution, mapped 1:1 onto the task `error.kind`
/// stored in PostgreSQL and returned by the API.
#[derive(Debug)]
pub enum ExecutionFailure {
    InputTooLarge { size: usize },
    Instantiation(String),
    Invocation(String),
    FuelExhausted,
    WallClockExceeded,
    OutputLimit { size: usize },
    Internal(String),
}

impl ExecutionFailure {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InputTooLarge { .. } => "input_limit_exceeded",
            Self::Instantiation(_) => "instantiation_failed",
            Self::Invocation(_) => "invocation_failed",
            Self::FuelExhausted | Self::WallClockExceeded => "resource_exhausted",
            Self::OutputLimit { .. } => "output_limit_exceeded",
            Self::Internal(_) => "internal",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::InputTooLarge { size } => {
                format!("input of {size} bytes exceeds the input size limit")
            }
            Self::Instantiation(m) => format!("instance initialization failed: {m}"),
            Self::Invocation(m) => format!("plugin invocation failed: {m}"),
            Self::FuelExhausted => "fuel limit exhausted; execution terminated".to_string(),
            Self::WallClockExceeded => {
                "wall-clock deadline exceeded; execution terminated".to_string()
            }
            Self::OutputLimit { size } => {
                format!("plugin output of {size} bytes exceeds the output size limit")
            }
            Self::Internal(m) => format!("internal executor error: {m}"),
        }
    }
}

/// Result of one execution attempt. `fuel_consumed` is reported on both
/// success and failure so resource abuse is visible in the task record.
pub struct ExecResult {
    pub outcome: Result<Value, ExecutionFailure>,
    pub fuel_consumed: u64,
}

/// Summary of a validated module, returned in the upload response.
#[derive(Debug, serde::Serialize)]
pub struct ModuleInspection {
    pub imports: Vec<String>,
    pub exports: Vec<String>,
}

impl SandboxEngine {
    pub fn new(limits: Limits, metrics: Arc<Metrics>) -> anyhow::Result<Arc<Self>> {
        let mut config = Config::new();
        config.consume_fuel(true);
        config.epoch_interruption(true);
        config.max_wasm_stack(limits.max_wasm_stack_bytes);
        let engine = Engine::new(&config)?;
        Ok(Arc::new(Self {
            engine,
            semaphore: Arc::new(tokio::sync::Semaphore::new(
                limits.max_concurrent_executions,
            )),
            modules: Mutex::new(HashMap::new()),
            limits,
            metrics,
        }))
    }

    /// Advance the engine epoch forever; stores past their deadline trap.
    pub fn run_epoch_ticker(self: &Arc<Self>) {
        let engine = self.engine.clone();
        let tick = self.limits.epoch_tick;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                engine.increment_epoch();
            }
        });
    }

    /// Validate an uploaded module: wasm correctness, import whitelist and
    /// ABI exports. This is the *module validation* failure stage.
    pub fn validate_module(&self, wasm: &[u8]) -> Result<ModuleInspection, ApiError> {
        if wasm.is_empty() {
            return Err(ApiError::new(
                ErrorKind::ModuleValidationFailed,
                "module is empty",
            ));
        }
        if wasm.len() > self.limits.max_module_bytes {
            return Err(ApiError::new(
                ErrorKind::ModuleValidationFailed,
                format!(
                    "module of {} bytes exceeds the {} byte limit",
                    wasm.len(),
                    self.limits.max_module_bytes
                ),
            ));
        }

        let module = Module::new(&self.engine, wasm).map_err(|e| {
            ApiError::new(
                ErrorKind::ModuleValidationFailed,
                "module failed WebAssembly validation",
            )
            .with_details(serde_json::json!({ "compiler_error": format!("{e:#}") }))
        })?;

        // Host-function whitelist: only `env.host_log` may be imported. No
        // WASI, no networking, no filesystem — anything else is refused here.
        let allowed = [format!("{HOST_LOG_MODULE}.{HOST_LOG_NAME}")];
        let imports: Vec<String> = module
            .imports()
            .map(|i| format!("{}.{}", i.module(), i.name()))
            .collect();
        let disallowed: Vec<&String> = imports.iter().filter(|i| !allowed.contains(i)).collect();
        if !disallowed.is_empty() {
            return Err(ApiError::new(
                ErrorKind::ModuleValidationFailed,
                "module imports host functions outside the whitelist",
            )
            .with_details(serde_json::json!({
                "disallowed_imports": disallowed,
                "allowed_imports": allowed,
            })));
        }

        // ABI: memory + alloc + run with exact signatures.
        let exports: Vec<String> = module.exports().map(|e| e.name().to_string()).collect();
        let find = |name: &str| module.exports().find(|e| e.name() == name).map(|e| e.ty());
        let mut missing = Vec::new();
        if !matches!(find("memory"), Some(ExternType::Memory(_))) {
            missing.push("memory: exported linear memory");
        }
        if !matches!(
            find("alloc"),
            Some(ExternType::Func(ft)) if func_sig_matches(&ft, &[ValType::I32], &[ValType::I32])
        ) {
            missing.push("alloc: func(len: i32) -> ptr: i32");
        }
        if !matches!(
            find("run"),
            Some(ExternType::Func(ft)) if func_sig_matches(&ft, &[ValType::I32, ValType::I32], &[ValType::I64])
        ) {
            missing.push("run: func(ptr: i32, len: i32) -> packed_out: i64");
        }
        if !missing.is_empty() {
            return Err(ApiError::new(
                ErrorKind::ModuleValidationFailed,
                "module does not implement the required ABI",
            )
            .with_details(serde_json::json!({
                "missing_exports": missing,
                "abi": "memory; alloc(len: i32) -> i32; run(ptr: i32, len: i32) -> i64 where the result packs (out_ptr << 32) | out_len; out must be UTF-8 JSON",
            })));
        }

        Ok(ModuleInspection { imports, exports })
    }

    /// Execute one task. Always terminates within the configured bounds.
    pub async fn execute(
        self: &Arc<Self>,
        plugin_id: Uuid,
        tenant_id: &str,
        task_id: Uuid,
        wasm: &[u8],
        input: &Value,
    ) -> ExecResult {
        let input_bytes = match serde_json::to_vec(input) {
            Ok(b) => b,
            Err(e) => {
                return ExecResult {
                    outcome: Err(ExecutionFailure::Internal(format!(
                        "failed to serialize input: {e}"
                    ))),
                    fuel_consumed: 0,
                }
            }
        };
        if input_bytes.len() > self.limits.max_input_bytes {
            return ExecResult {
                outcome: Err(ExecutionFailure::InputTooLarge {
                    size: input_bytes.len(),
                }),
                fuel_consumed: 0,
            };
        }
        let module = match self.cached_module(plugin_id, wasm) {
            Ok(m) => m,
            Err(f) => {
                return ExecResult {
                    outcome: Err(f),
                    fuel_consumed: 0,
                }
            }
        };

        // Bound concurrency so a burst of tasks cannot starve the host.
        let permit = match self.semaphore.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => {
                return ExecResult {
                    outcome: Err(ExecutionFailure::Internal("executor shut down".into())),
                    fuel_consumed: 0,
                }
            }
        };

        let engine = self.engine.clone();
        let limits = self.limits.clone();
        let metrics = self.metrics.clone();
        let tenant = tenant_id.to_string();
        let wall_clock = self.limits.wall_clock;

        // Guest execution is CPU-bound; run it on the blocking pool. Fuel and
        // the epoch deadline guarantee it terminates, so the outer timeout is
        // only a last-resort guard and is deliberately generous.
        let handle = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            run_sync(
                &engine,
                &limits,
                &metrics,
                module,
                tenant,
                task_id,
                input_bytes,
            )
        });
        match tokio::time::timeout(wall_clock * 4 + Duration::from_secs(1), handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(join_err)) => ExecResult {
                outcome: Err(ExecutionFailure::Internal(format!(
                    "executor task failed: {join_err}"
                ))),
                fuel_consumed: 0,
            },
            Err(_) => ExecResult {
                outcome: Err(ExecutionFailure::WallClockExceeded),
                fuel_consumed: 0,
            },
        }
    }

    fn cached_module(&self, plugin_id: Uuid, wasm: &[u8]) -> Result<Arc<Module>, ExecutionFailure> {
        {
            let guard = self.modules.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(m) = guard.get(&plugin_id) {
                return Ok(Arc::clone(m));
            }
        }
        // The module was validated at upload; recompilation here only fails if
        // the stored bytes were corrupted.
        let module = Module::new(&self.engine, wasm).map_err(|e| {
            ExecutionFailure::Instantiation(format!("module compilation failed: {e:#}"))
        })?;
        let mut guard = self.modules.lock().unwrap_or_else(|e| e.into_inner());
        Ok(Arc::clone(
            guard.entry(plugin_id).or_insert_with(|| Arc::new(module)),
        ))
    }
}

fn func_sig_matches(ft: &FuncType, params: &[ValType], results: &[ValType]) -> bool {
    // wasmtime's `ValType` has no `PartialEq`; use its explicit `eq` method.
    let ps: Vec<ValType> = ft.params().collect();
    let rs: Vec<ValType> = ft.results().collect();
    ps.len() == params.len()
        && rs.len() == results.len()
        && ps.iter().zip(params).all(|(a, b)| ValType::eq(a, b))
        && rs.iter().zip(results).all(|(a, b)| ValType::eq(a, b))
}

/// Decremented on drop: proof that every execution's resources are reclaimed.
struct ActiveExecutionGuard {
    metrics: Arc<Metrics>,
}

impl ActiveExecutionGuard {
    fn new(metrics: &Arc<Metrics>) -> Self {
        metrics.active_executions.fetch_add(1, Ordering::Relaxed);
        Self {
            metrics: metrics.clone(),
        }
    }
}

impl Drop for ActiveExecutionGuard {
    fn drop(&mut self) {
        self.metrics
            .active_executions
            .fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Instantiation,
    Invocation,
}

/// Map a trap to a failure kind. Deadline/fuel exhaustion win over the raw
/// trap text so resource termination is reported as such.
fn classify(store: &mut Store<ExecState>, stage: Stage, err: String) -> ExecutionFailure {
    if store.data().deadline_hit {
        return ExecutionFailure::WallClockExceeded;
    }
    if matches!(store.get_fuel(), Ok(0)) {
        return ExecutionFailure::FuelExhausted;
    }
    match stage {
        Stage::Instantiation => ExecutionFailure::Instantiation(err),
        Stage::Invocation => ExecutionFailure::Invocation(err),
    }
}

fn fuel_used(store: &mut Store<ExecState>, limit: u64) -> u64 {
    store
        .get_fuel()
        .map(|remaining| limit.saturating_sub(remaining))
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn run_sync(
    engine: &Engine,
    limits: &Limits,
    metrics: &Arc<Metrics>,
    module: Arc<Module>,
    tenant_id: String,
    task_id: Uuid,
    input_bytes: Vec<u8>,
) -> ExecResult {
    metrics.executions_started.fetch_add(1, Ordering::Relaxed);
    let _active = ActiveExecutionGuard::new(metrics);

    let fuel_limit = limits.fuel_limit;

    let state = ExecState {
        limits: StoreLimitsBuilder::new()
            .memory_size(limits.max_memory_bytes)
            .table_elements(limits.max_table_elements as usize)
            .instances(1)
            .memories(1)
            .tables(4)
            .build(),
        tenant_id,
        task_id,
        deadline_hit: false,
        log_lines: 0,
    };
    let mut store = Store::new(engine, state);

    // Defined after `store`/`fuel_limit` so the identifiers resolve here.
    macro_rules! bail {
        ($failure:expr) => {
            return ExecResult {
                outcome: Err($failure),
                fuel_consumed: fuel_used(&mut store, fuel_limit),
            }
        };
    }
    store.limiter(|s| &mut s.limits);
    if let Err(e) = store.set_fuel(fuel_limit) {
        bail!(ExecutionFailure::Internal(format!(
            "failed to set fuel: {e}"
        )));
    }
    let deadline_ticks =
        (limits.wall_clock.as_millis() as u64 / limits.epoch_tick.as_millis().max(1) as u64).max(1)
            + 1;
    store.set_epoch_deadline(deadline_ticks);
    store.epoch_deadline_callback(|mut ctx| {
        ctx.data_mut().deadline_hit = true;
        Err(anyhow::anyhow!("wall-clock execution deadline exceeded"))
    });

    // The linker defines exactly one host function. No WASI is ever added.
    let mut linker = Linker::new(engine);
    if let Err(e) = linker.func_wrap(HOST_LOG_MODULE, HOST_LOG_NAME, host_log) {
        bail!(ExecutionFailure::Internal(format!(
            "failed to link host functions: {e}"
        )));
    }

    // ---- instantiation stage ----
    let instance = match linker.instantiate(&mut store, &module) {
        Ok(i) => i,
        Err(e) => bail!(classify(&mut store, Stage::Instantiation, format!("{e:#}"))),
    };
    let memory = match instance.get_memory(&mut store, "memory") {
        Some(m) => m,
        None => bail!(ExecutionFailure::Instantiation(
            "module does not export `memory`".into()
        )),
    };
    let alloc = match instance.get_typed_func::<i32, i32>(&mut store, "alloc") {
        Ok(f) => f,
        Err(e) => bail!(ExecutionFailure::Instantiation(format!(
            "`alloc` export has the wrong type: {e:#}"
        ))),
    };
    let run = match instance.get_typed_func::<(i32, i32), i64>(&mut store, "run") {
        Ok(f) => f,
        Err(e) => bail!(ExecutionFailure::Instantiation(format!(
            "`run` export has the wrong type: {e:#}"
        ))),
    };

    // ---- invocation stage ----
    let in_len = match i32::try_from(input_bytes.len()) {
        Ok(n) => n,
        Err(_) => bail!(ExecutionFailure::InputTooLarge {
            size: input_bytes.len(),
        }),
    };
    let in_ptr = match alloc.call(&mut store, in_len) {
        Ok(p) => p,
        Err(e) => bail!(classify(
            &mut store,
            Stage::Invocation,
            format!("alloc: {e:#}")
        )),
    };
    if let Err(e) = memory.write(&mut store, in_ptr as usize, &input_bytes) {
        bail!(ExecutionFailure::Invocation(format!(
            "guest memory rejected the input buffer: {e}"
        )));
    }
    let packed = match run.call(&mut store, (in_ptr, in_len)) {
        Ok(p) => p,
        Err(e) => bail!(classify(&mut store, Stage::Invocation, format!("{e:#}"))),
    };

    let out_ptr = ((packed as u64) >> 32) as usize;
    let out_len = ((packed as u64) & 0xffff_ffff) as usize;
    if out_len > limits.max_output_bytes {
        bail!(ExecutionFailure::OutputLimit { size: out_len });
    }
    let output: Value = {
        let data = memory.data(&store);
        let Some(end) = out_ptr.checked_add(out_len) else {
            bail!(ExecutionFailure::Invocation(
                "output pointer arithmetic overflow".into()
            ));
        };
        if end > data.len() {
            bail!(ExecutionFailure::Invocation(format!(
                "output range {out_ptr}..{end} is outside guest memory ({} bytes)",
                data.len()
            )));
        }
        let text = match std::str::from_utf8(&data[out_ptr..end]) {
            Ok(t) => t,
            Err(_) => bail!(ExecutionFailure::Invocation(
                "output is not valid UTF-8".into()
            )),
        };
        match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => bail!(ExecutionFailure::Invocation(format!(
                "output is not valid JSON: {e}"
            ))),
        }
    };

    ExecResult {
        outcome: Ok(output),
        fuel_consumed: fuel_used(&mut store, fuel_limit),
    }
    // `store` is dropped here: instance, linear memory and tables are freed.
}

/// The single whitelisted host function: `env.host_log(level, ptr, len)`.
///
/// Every line is emitted with the owning tenant's identity, so guest
/// parameters from different tenants never mix in the logs. Logging is
/// budgeted (line count and line length) so a guest cannot flood the host.
fn host_log(
    mut caller: Caller<'_, ExecState>,
    level: i32,
    ptr: i32,
    len: i32,
) -> anyhow::Result<()> {
    let (tenant_id, task_id, line_no) = {
        let d = caller.data();
        (d.tenant_id.clone(), d.task_id, d.log_lines)
    };
    if line_no >= MAX_LOG_LINES_PER_TASK {
        return Ok(()); // budget spent: drop silently
    }
    if !(0..=MAX_LOG_LINE_BYTES).contains(&len) {
        return Ok(()); // oversized line: drop
    }
    let memory = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| anyhow::anyhow!("guest has no exported memory"))?;
    let message = {
        let data = memory.data(&caller);
        let start = ptr as usize;
        let end = start
            .checked_add(len as usize)
            .ok_or_else(|| anyhow::anyhow!("host_log range overflow"))?;
        if end > data.len() {
            anyhow::bail!("host_log range {start}..{end} is outside guest memory");
        }
        String::from_utf8_lossy(&data[start..end]).into_owned()
    };
    caller.data_mut().log_lines += 1;
    match level {
        0 => tracing::debug!(tenant_id = %tenant_id, %task_id, source = "guest", "{message}"),
        2 => tracing::warn!(tenant_id = %tenant_id, %task_id, source = "guest", "{message}"),
        3 => tracing::error!(tenant_id = %tenant_id, %task_id, source = "guest", "{message}"),
        _ => tracing::info!(tenant_id = %tenant_id, %task_id, source = "guest", "{message}"),
    }
    Ok(())
}
