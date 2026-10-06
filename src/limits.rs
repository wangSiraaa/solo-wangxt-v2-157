//! Process-wide hard ceilings and per-task resource limits.
//!
//! Every value that a tenant can influence (fuel, memory, output size, log
//! volume) is clamped against a server-side hard ceiling. Contracts can only
//! *reduce* limits, never raise them.

/// Default / ceiling resource policy. Clamping happens in
/// [`crate::model::LimitPolicy::resolve`].
#[derive(Debug, Clone, Copy)]
pub struct GlobalLimits {
    /// Maximum linear memory a single instance may reserve, in bytes.
    pub max_memory_bytes: usize,
    /// Maximum fuel (roughly: Wasm instructions) a single call may consume.
    pub max_fuel: u64,
    /// Maximum bytes the guest is allowed to return from `abi_output`.
    pub max_output_bytes: usize,
    /// Maximum bytes of host log the guest may emit per task.
    pub max_log_bytes: usize,
    /// Maximum total bytes of the encoded argument block passed to the guest.
    pub max_input_bytes: usize,
    /// Maximum size of an uploaded wasm module, in bytes.
    pub max_wasm_bytes: usize,
    /// Maximum size of the JSON contract document, in bytes.
    pub max_contract_bytes: usize,
    /// Wall-clock budget for a single call. Epoch interruption enforces this
    /// even when the guest never reaches a fuel check.
    pub call_timeout_ms: u64,
}

impl Default for GlobalLimits {
    fn default() -> Self {
        Self {
            max_memory_bytes: 16 * 1024 * 1024, // 16 MiB
            max_fuel: 10_000_000,
            max_output_bytes: 64 * 1024, // 64 KiB
            max_log_bytes: 16 * 1024,
            max_input_bytes: 1024 * 1024, // 1 MiB encoded args
            max_wasm_bytes: 8 * 1024 * 1024, // 8 MiB module
            max_contract_bytes: 64 * 1024,
            call_timeout_ms: 10_000,
        }
    }
}

impl GlobalLimits {
    /// Overridable from the environment for tests / smaller CI machines.
    pub fn from_env() -> Self {
        let mut l = Self::default();
        if let Some(v) = env_u64("SANDBOX_MAX_MEMORY_MIB") {
            l.max_memory_bytes = (v as usize) * 1024 * 1024;
        }
        if let Some(v) = env_u64("SANDBOX_MAX_FUEL") {
            l.max_fuel = v;
        }
        if let Some(v) = env_u64("SANDBOX_MAX_OUTPUT_KB") {
            l.max_output_bytes = (v as usize) * 1024;
        }
        if let Some(v) = env_u64("SANDBOX_TIMEOUT_MS") {
            l.call_timeout_ms = v;
        }
        l
    }
}

fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Per-task limits after clamping a contract request against [`GlobalLimits`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ResolvedLimits {
    pub memory_bytes: usize,
    pub fuel: u64,
    pub output_bytes: usize,
    pub log_bytes: usize,
    pub timeout_ms: u64,
}
