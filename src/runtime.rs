//! The Wasmtime sandbox.
//!
//! Security model:
//! * No WASI, no filesystem, no sockets: the linker is built from scratch per
//!   task and only ever contains the functions the plugin's contract allows.
//!   An import that was not provided makes instantiation fail (the
//!   "unauthorized access" module is stopped before it can run).
//! * Fuel bounds executed work, so a tight infinite loop traps with
//!   `Trap::OutOfFuel`; epoch interruption is the wall-clock backstop.
//! * [`wasmtime::ResourceLimiter`] bounds linear-memory growth and the number
//!   of instances/memories/tables.
//! * Output size is checked after the call, before it is trusted or stored.
//!
//! The three execution phases are reported separately:
//! [`ErrorStage::Validation`] / [`ErrorStage::Instantiation`] /
//! [`ErrorStage::Invocation`].
//!
//! Resource reclamation: `execute()` owns the [`Store`]; it is dropped on
//! every return path (success, trap, error), which deallocates the instance
//! and its linear memory. The task's `Drop` hook decrements the live gauges
//! so `/metrics` proves return-to-zero after every task.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use wasmtime::{
    Engine, Instance, InstanceAllocationStrategy, Linker, Memory, Module, ResourceLimiter,
    Store, TypedFunc,
};

use crate::error::ApiError;
use crate::limits::ResolvedLimits;
use crate::metrics::Metrics;
use crate::model::{ErrorStage, REQUIRED_EXPORTS};

/// Outcome of a finished guest call.
#[derive(Debug)]
pub struct RunOutcome {
    pub output: Vec<u8>,
    pub fuel_consumed: u64,
    /// Log lines captured via the whitelisted `host_log` import.
    pub guest_logs: Vec<String>,
    pub log_truncated: bool,
}

/// Process-wide engine + compiled-module cache.
#[derive(Clone)]
pub struct SandboxEngine {
    engine: Engine,
    metrics: Metrics,
    cache: Arc<Mutex<LruCache<String, Module>>>,
}

/// Bounded compilation cache (LRU by insertion/use order).
struct LruCache<K, V> {
    cap: usize,
    inner: BTreeMap<K, V>,
    order: VecDeque<K>,
}

impl<K: Ord + Clone, V> LruCache<K, V> {
    fn new(cap: NonZeroUsize) -> Self {
        Self { cap: cap.get(), inner: BTreeMap::new(), order: VecDeque::new() }
    }
    fn get(&mut self, k: &K) -> Option<&V> {
        if self.inner.contains_key(k) {
            self.order.retain(|x| x != k);
            self.order.push_back(k.clone());
        }
        self.inner.get(k)
    }
    fn put(&mut self, k: K, v: V) {
        if self.inner.contains_key(&k) {
            self.order.retain(|x| x != &k);
        }
        self.order.push_back(k.clone());
        self.inner.insert(k, v);
        while self.inner.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.inner.remove(&old);
            }
        }
    }
}

/// Epoch granularity; the deadline is expressed in ticks of this length.
pub const EPOCH_TICK_MS: u64 = 20;

impl SandboxEngine {
    pub fn new(metrics: Metrics) -> wasmtime::Result<Self> {
        let mut config = wasmtime::Config::new();
        // Fuel = deterministic work budget; epoch = wall-clock backstop.
        config.consume_fuel(true);
        config.epoch_interruption(true);
        // OnDemand: instance resources are allocated at instantiation and
        // freed immediately when the Store drops (wasmtime docs, verbatim).
        config.allocation_strategy(InstanceAllocationStrategy::OnDemand);

        let engine = Engine::new(&config)?;

        // Epoch ticker every 20ms. A store registers a deadline in ticks;
        // once missed, execution traps wherever the guest currently is.
        let ticker = engine.clone();
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || loop {
                std::thread::sleep(Duration::from_millis(EPOCH_TICK_MS));
                ticker.increment_epoch();
            })?;

        Ok(Self {
            engine,
            metrics,
            cache: Arc::new(Mutex::new(LruCache::new(NonZeroUsize::new(16).unwrap()))),
        })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Phase 1 — static validation: bytes must decode and type-check; every
    /// required ABI export must exist. Import satisfaction is NOT checked
    /// here: it depends on the per-plugin allowlist and is reported as an
    /// *instantiation* failure instead.
    pub fn validate(&self, wasm: &[u8]) -> Result<Module, ApiError> {
        let module = Module::new(&self.engine, wasm)
            .map_err(|e| ApiError::module(format!("wasm module validation failed: {e}")))?;
        let exports: BTreeSet<&str> = module.exports().map(|e| e.name()).collect();
        for need in REQUIRED_EXPORTS {
            if !exports.contains(need) {
                return Err(ApiError::module(format!(
                    "module fails abi-v1 validation: missing required export {need:?}"
                )));
            }
        }
        Ok(module)
    }

    /// Interface summary stored at upload time (imports/exports/types).
    pub fn describe(&self, module: &Module) -> serde_json::Value {
        serde_json::json!({
            "imports": module.imports().map(|i| serde_json::json!({
                "module": i.module(), "name": i.name(),
                "kind": format!("{:?}", i.ty()),
            })).collect::<Vec<_>>(),
            "exports": module.exports().map(|e| serde_json::json!({
                "name": e.name(), "kind": format!("{:?}", e.ty()),
            })).collect::<Vec<_>>(),
        })
    }

    fn compile_cached(&self, hash: &str, wasm: &[u8]) -> Result<Module, ApiError> {
        if let Some(m) = self.cache.lock().get(&hash.to_string()) {
            return Ok(m.clone());
        }
        let module = self.validate(wasm)?;
        self.cache.lock().put(hash.to_string(), module.clone());
        Ok(module)
    }

    /// Compile (or fetch cached), instantiate with the allowlisted linker and
    /// execute `run`. Synchronous by design: the service schedules it on a
    /// Tokio blocking thread so async workers never stall.
    #[allow(clippy::too_many_lines)]
    pub fn execute(
        &self,
        hash: &str,
        wasm: &[u8],
        args: &[u8],
        allowlist: &BTreeSet<String>,
        limits: ResolvedLimits,
    ) -> Result<RunOutcome, ApiError> {
        let module = self.compile_cached(hash, wasm)?;

        // Per-task state. Dropping the Store (on ANY return path below) drops
        // the instance and its linear memory; the Drop impl here pairs the
        // "live" metrics for reclamation accounting.
        struct TaskState {
            mem_limit: usize,
            mem_granted: usize,
            /// Count the denial once per task: a guest may invoke
            /// memory.grow millions of times, and metrics should describe
            /// tasks, not individual instructions.
            mem_denied_reported: bool,
            logs: Vec<String>,
            log_bytes: usize,
            log_cap: usize,
            log_truncated: bool,
            metrics: Metrics,
        }

        impl ResourceLimiter for TaskState {
            fn memory_growing(&mut self, current: usize, desired: usize, _max: Option<usize>) -> anyhow::Result<bool> {
                if desired > self.mem_limit {
                    if !self.mem_denied_reported {
                        self.mem_denied_reported = true;
                        self.metrics.memory_denied();
                    }
                    // Refuse the growth: memory.grow returns -1 to the guest.
                    return Ok(false);
                }
                self.mem_granted = desired;
                self.metrics.memory_grew((desired - current) as i64);
                Ok(true)
            }
            fn table_growing(&mut self, _current: u32, desired: u32, _max: Option<u32>) -> anyhow::Result<bool> {
                // Tables are not part of the ABI and contribute nothing but
                // attack surface: deny any table.
                Ok(desired == 0)
            }
            fn instances(&self) -> usize {
                1
            }
            fn tables(&self) -> usize {
                0
            }
            fn memories(&self) -> usize {
                1
            }
        }

        impl Drop for TaskState {
            fn drop(&mut self) {
                // The Store is going away; all guest memory it granted is
                // reclaimed by Wasmtime. Account for it unconditionally.
                self.metrics.instance_finished(self.mem_granted as i64);
            }
        }

        let mut store = Store::new(
            &self.engine,
            TaskState {
                mem_limit: limits.memory_bytes,
                mem_granted: 0,
                mem_denied_reported: false,
                logs: Vec::new(),
                log_bytes: 0,
                log_cap: limits.log_bytes,
                log_truncated: false,
                metrics: self.metrics.clone(),
            },
        );
        store.limiter(|s| s as &mut dyn ResourceLimiter);
        store.set_fuel(limits.fuel).map_err(|e| ApiError::internal(format!("fuel setup failed: {e}")))?;
        // Wall-clock backstop: traps at the first tick after the deadline.
        let ticks = (limits.timeout_ms / EPOCH_TICK_MS).max(1);
        store.set_epoch_deadline(ticks);
        self.metrics.instance_started(0);

        // ---- Phase 2: linker + instantiation (explicit whitelist) --------
        let mut linker: Linker<TaskState> = Linker::new(&self.engine);
        if allowlist.contains("host_log") {
            linker
                .func_wrap(
                    "env",
                    "host_log",
                    |mut caller: wasmtime::Caller<'_, TaskState>, ptr: i32, len: i32| {
                        let mem = caller
                            .get_export("memory")
                            .and_then(|e| e.into_memory())
                            .ok_or_else(|| anyhow::anyhow!("memory export unavailable"))?;
                        let (ptr, len) = match (usize::try_from(ptr), usize::try_from(len)) {
                            (Ok(p), Ok(l)) => (p, l),
                            _ => return Err(anyhow::anyhow!("host_log: invalid pointer/length")),
                        };
                        if len > caller.data().mem_limit.min(caller.data().log_cap) {
                            return Err(anyhow::anyhow!("host_log: length implausibly large"));
                        }
                        let text = {
                            let data = mem.data(&caller);
                            let Some(slice) = data.get(ptr..ptr.saturating_add(len)) else {
                                return Err(anyhow::anyhow!("host_log: pointer range outside linear memory"));
                            };
                            let st = caller.data();
                            if st.log_bytes.saturating_add(len) > st.log_cap {
                                return Err(anyhow::anyhow!("host_log: tenant log budget exhausted"));
                            }
                            String::from_utf8_lossy(slice).into_owned()
                        };
                        let st = caller.data_mut();
                        if st.log_bytes + text.len() > st.log_cap {
                            st.log_truncated = true;
                            return Err(anyhow::anyhow!("host_log: tenant log budget exhausted"));
                        }
                        st.log_bytes += text.len();
                        st.logs.push(text);
                        Ok(())
                    },
                )
                .map_err(|e| ApiError::internal(format!("linker setup failed: {e}")))?;
        }

        let pre = linker.instantiate_pre(&module).map_err(|e| {
            ApiError::execution(
                ErrorStage::Instantiation,
                format!("module imports cannot be satisfied by the allowlisted host: {e}"),
            )
        })?;
        let instance: Instance = pre.instantiate(&mut store).map_err(|e| {
            ApiError::execution(ErrorStage::Instantiation, format!("instance instantiation failed: {e}"))
        })?;

        let memory: Memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| ApiError::execution(ErrorStage::Instantiation, "missing memory export"))?;
        let run: TypedFunc<(), i64> = instance.get_typed_func(&mut store, "run").map_err(|e| {
            ApiError::execution(ErrorStage::Instantiation, format!("run export unusable: {e}"))
        })?;

        // Place the argument block in the reserved ABI scratch area at the
        // top of the initial first page. The guest allocator is NOT trusted to
        // hand back a usable pointer (a buggy or malicious bump allocator can
        // point beyond memory); validation guarantees at least one page.
        let page = 64 * 1024usize;
        let initial = memory.data(&store).len();
        if initial < page || args.len() > page {
            return Err(ApiError::execution(
                ErrorStage::Instantiation,
                format!(
                    "abi-v1 requires >= 1 page (64 KiB) initial memory and <= {page} byte argument block; \
                     memory={initial}, args={}",
                    args.len()
                ),
            ));
        }
        let ptr = page - args.len();
        memory.write(&mut store, ptr, args).map_err(|e| {
            ApiError::execution(
                ErrorStage::Instantiation,
                format!("argument block write failed: {e}"),
            )
        })?;
        // abi-v1 discovery convention: two mutable globals name the block.
        let g_ptr = instance.get_global(&mut store, "abi_args_ptr");
        let g_len = instance.get_global(&mut store, "abi_args_len");
        match (g_ptr, g_len) {
            (Some(gp), Some(gl)) => {
                gp.set(&mut store, (ptr as i64).into()).map_err(|e| {
                    ApiError::execution(ErrorStage::Instantiation, format!("cannot set abi_args_ptr: {e}"))
                })?;
                gl.set(&mut store, (args.len() as i64).into()).map_err(|e| {
                    ApiError::execution(ErrorStage::Instantiation, format!("cannot set abi_args_len: {e}"))
                })?;
            }
            _ => {
                return Err(ApiError::execution(
                    ErrorStage::Instantiation,
                    "module must export mutable globals abi_args_ptr and abi_args_len",
                ));
            }
        }

        // ---- Phase 3: invocation -----------------------------------------
        let fuel_before = store.get_fuel().unwrap_or(0);
        let call = run.call(&mut store, ());
        let fuel_after = store.get_fuel().unwrap_or(fuel_before);
        let fuel_consumed = fuel_before.saturating_sub(fuel_after);

        let packed = call.map_err(|e| map_trap(e, fuel_consumed))?;

        // Output convention: (offset << 32) | length.
        let out_off = (packed >> 32) as u32 as usize;
        let out_len = (packed as u32) as usize;
        if out_len > limits.output_bytes {
            return Err(ApiError::execution(
                ErrorStage::Invocation,
                format!(
                    "guest output of {out_len} bytes exceeds per-task output limit of {} bytes",
                    limits.output_bytes
                ),
            ));
        }
        let output = {
            let data = memory.data(&store);
            data.get(out_off..out_off.saturating_add(out_len))
                .ok_or_else(|| {
                    ApiError::execution(
                        ErrorStage::Invocation,
                        "guest returned a pointer range outside linear memory",
                    )
                })?
                .to_vec()
        };

        let mut st = store.into_data();
        Ok(RunOutcome {
            output,
            fuel_consumed,
            guest_logs: std::mem::take(&mut st.logs),
            log_truncated: st.log_truncated,
        })
    }
}

/// Classify how the call ended. Fuel exhaustion and epoch deadlines are
/// *resource enforcement events* — the operationally relevant answer is
/// "task terminated", not a generic trap.
fn map_trap(e: wasmtime::Error, fuel_consumed: u64) -> ApiError {
    let trap = e
        .chain()
        .find_map(|err| err.downcast_ref::<wasmtime::Trap>().copied());
    match trap {
        Some(wasmtime::Trap::OutOfFuel) => ApiError::execution(
            ErrorStage::Invocation,
            format!(
                "task terminated: fuel budget exhausted after {fuel_consumed} fuel (possible infinite loop)"
            ),
        ),
        Some(wasmtime::Trap::Interrupt) => ApiError::execution(
            ErrorStage::Invocation,
            "task terminated: wall-clock deadline reached (possible infinite loop)",
        ),
        Some(other) => {
            ApiError::execution(ErrorStage::Invocation, format!("guest invocation trapped: {other}"))
        }
        None => ApiError::execution(ErrorStage::Invocation, format!("guest invocation failed: {e}")),
    }
}
