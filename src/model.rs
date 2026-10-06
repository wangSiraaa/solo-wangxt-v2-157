//! The plugin interface contract: typed inputs, output expectation and the
//! host-function allowlist. Also contains task/plugin state types shared with
//! the database layer.

use std::collections::BTreeSet;

use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::limits::{GlobalLimits, ResolvedLimits};

/// ABI version understood by this host. Guests must be compiled against
/// `abi-v1` (see `fixtures/`): exports `alloc`, `run`; memory exported as
/// `memory`; reads the packed argument block the host writes at offset 0;
/// returns an (offset, length) packed as `u64` (offset high, length low).
pub const ABI_VERSION: &str = "abi-v1";

/// Maximum abi-v1 argument block. The host hands the block to the guest in a
/// reserved scratch area at the top of the module's initial 64 KiB page, so
/// encoded inputs must fit there (see `runtime::execute`).
pub const ABI_MAX_ARG_BYTES: usize = 64 * 1024;

/// Names of exports every compliant module must provide.
pub const REQUIRED_EXPORTS: [&str; 3] = ["memory", "alloc", "run"];

/// The full whitelist of host functions the sandbox is *capable* of
/// providing. A plugin only gets a function when its contract lists it.
///
/// Note there is intentionally no network, filesystem or environment
/// capability anywhere in this list.
pub const HOST_FN_CATALOG: [&str; 1] = ["host_log"];

/// Contract document uploaded with each plugin (`contract_json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Contract {
    pub abi_version: String,
    pub inputs: Vec<ParamSpec>,
    /// Optional output schema hint ("json" only for now; the host validates
    /// that the returned bytes parse as JSON when set).
    #[serde(default)]
    pub output: OutputSpec,
    /// Subset of [`HOST_FN_CATALOG`] this plugin is allowed to link.
    #[serde(default)]
    pub host_allowlist: BTreeSet<String>,
    #[serde(default)]
    pub limits: LimitPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: String,
    /// One of: i64, u64, f64, bool, string, bytes.
    pub ty: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputSpec {
    /// "json" means the returned bytes must be valid JSON. Empty/other means
    /// the host only enforces the size limit.
    #[serde(default)]
    pub format: String,
}

/// Optional per-plugin limit *requests*, always clamped downwards against the
/// server ceilings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LimitPolicy {
    pub memory_mib: Option<u64>,
    pub fuel: Option<u64>,
    pub output_kib: Option<u64>,
    pub timeout_ms: Option<u64>,
}

impl LimitPolicy {
    pub fn resolve(&self, g: &GlobalLimits) -> ResolvedLimits {
        ResolvedLimits {
            memory_bytes: self
                .memory_mib
                .map(|m| (m as usize) * 1024 * 1024)
                .filter(|m| *m <= g.max_memory_bytes)
                .unwrap_or(g.max_memory_bytes),
            fuel: self
                .fuel
                .filter(|f| *f <= g.max_fuel)
                .unwrap_or(g.max_fuel),
            output_bytes: self
                .output_kib
                .map(|k| (k as usize) * 1024)
                .filter(|k| *k <= g.max_output_bytes)
                .unwrap_or(g.max_output_bytes),
            log_bytes: g.max_log_bytes,
            timeout_ms: self
                .timeout_ms
                .filter(|t| *t <= g.call_timeout_ms)
                .unwrap_or(g.call_timeout_ms),
        }
    }
}

/// Validate a contract *by value* (independent of wasm bytes): ABI version,
/// type names, allowlist names, size.
pub fn validate_contract(c: &Contract, raw_len: usize, g: &GlobalLimits) -> Result<(), ApiError> {
    if raw_len > g.max_contract_bytes {
        return Err(ApiError::contract(format!(
            "contract document exceeds {} bytes",
            g.max_contract_bytes
        )));
    }
    if c.abi_version != ABI_VERSION {
        return Err(ApiError::contract(format!(
            "unsupported abi_version {:?}; host speaks {ABI_VERSION}",
            c.abi_version
        )));
    }
    // An empty input list is valid: a plugin may take no parameters.
    let mut seen = BTreeSet::new();
    for p in &c.inputs {
        if p.name.is_empty() {
            return Err(ApiError::contract("input parameter has empty name"));
        }
        if !seen.insert(&p.name) {
            return Err(ApiError::contract(format!("duplicate input name {:?}", p.name)));
        }
        if !matches!(p.ty.as_str(), "i64" | "u64" | "f64" | "bool" | "string" | "bytes") {
            return Err(ApiError::contract(format!(
                "input {:?} has unsupported type {:?}",
                p.name, p.ty
            )));
        }
    }
    for f in &c.host_allowlist {
        if !HOST_FN_CATALOG.contains(&f.as_str()) {
            return Err(ApiError::contract(format!(
                "host function {:?} is not in the host whitelist {:?}; \
                 network/filesystem/env capabilities do not exist",
                f, HOST_FN_CATALOG
            )));
        }
    }
    if c.output.format.as_str() != "" && c.output.format != "json" {
        return Err(ApiError::contract(format!(
            "unsupported output format {:?} (want \"json\" or \"\")",
            c.output.format
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Input validation & ABI-v1 argument-block encoding
// ---------------------------------------------------------------------------

/// Tag bytes written before each parameter value in the argument block.
/// Little-endian values; strings/bytes: 4-byte LE length then UTF-8 bytes.
const TAG_I64: u8 = 1;
const TAG_U64: u8 = 2;
const TAG_F64: u8 = 3;
const TAG_BOOL: u8 = 4;
const TAG_STRING: u8 = 5;
const TAG_BYTES: u8 = 6;

/// Validate the JSON input object against the contract and pack it into the
/// abi-v1 binary argument block:
///
/// ```text
/// [param_count: u32 LE]
/// repeated, in declared order:
///   [tag: u8][value: 8 bytes LE]                      (scalars)
///   [tag: u8][len: u32 LE][bytes: len]                (string/bytes)
/// ```
pub fn validate_and_encode(
    contract: &Contract,
    input: &serde_json::Value,
    g: &GlobalLimits,
) -> Result<Vec<u8>, ApiError> {
    let obj = input
        .as_object()
        .ok_or_else(|| ApiError::input("input body must be a JSON object"))?;

    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&(contract.inputs.len() as u32).to_le_bytes());

    for p in &contract.inputs {
        let Some(v) = obj.get(&p.name) else {
            if p.required {
                return Err(ApiError::input(format!("missing required parameter {:?}", p.name)));
            }
            // Missing optional -> encode the zero value for its tag. Guests
            // read the tag and can treat zero-length/0 as absent.
            encode_value(&mut buf, &p.ty, &serde_json::Value::Null)
                .map_err(|e| ApiError::input(format!("parameter {:?}: {e}", p.name)))?;
            continue;
        };
        if v.is_null() {
            return Err(ApiError::input(format!(
                "parameter {:?} must not be null",
                p.name
            )));
        }
        encode_value(&mut buf, &p.ty, v).map_err(|e| ApiError::input(format!("parameter {:?}: {e}", p.name)))?;
        if buf.len() > g.max_input_bytes.min(ABI_MAX_ARG_BYTES) {
            return Err(ApiError::input(format!(
                "encoded argument block exceeds {} bytes",
                g.max_input_bytes.min(ABI_MAX_ARG_BYTES)
            )));
        }
    }
    Ok(buf)
}

fn encode_value(buf: &mut Vec<u8>, ty: &str, v: &serde_json::Value) -> Result<(), String> {
    // Null (only produced for a missing optional parameter) -> typed zero.
    if v.is_null() {
        match ty {
            "i64" | "u64" | "f64" => {
                buf.push(if ty == "f64" { TAG_F64 } else if ty == "i64" { TAG_I64 } else { TAG_U64 });
                buf.extend_from_slice(&[0u8; 8]);
                return Ok(());
            }
            "bool" => {
                buf.push(TAG_BOOL);
                buf.extend_from_slice(&[0u8; 8]);
                return Ok(());
            }
            "string" | "bytes" => {
                buf.push(if ty == "string" { TAG_STRING } else { TAG_BYTES });
                buf.extend_from_slice(&0u32.to_le_bytes());
                return Ok(());
            }
            other => return Err(format!("unknown type {other:?}")),
        }
    }
    match ty {
        "i64" => {
            let n = json_i64(v)?;
            buf.push(TAG_I64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        "u64" => {
            let n = json_u64(v)?;
            buf.push(TAG_U64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        "f64" => {
            let n = v
                .as_f64()
                .ok_or_else(|| "expected a number".to_string())?;
            buf.push(TAG_F64);
            buf.extend_from_slice(&n.to_le_bytes());
        }
        "bool" => {
            let b = v.as_bool().ok_or_else(|| "expected true/false".to_string())?;
            buf.push(TAG_BOOL);
            buf.extend_from_slice(&[0u8; 7]);
            buf.push(b as u8);
        }
        "string" => {
            let s = v.as_str().ok_or_else(|| "expected a string".to_string())?;
            buf.push(TAG_STRING);
            buf.extend_from_slice(
                &u32::try_from(s.len())
                    .map_err(|_| "string too long".to_string())?
                    .to_le_bytes(),
            );
            buf.extend_from_slice(s.as_bytes());
        }
        "bytes" => {
            let s = v.as_str().ok_or_else(|| "expected a base64 string".to_string())?;
            let raw = base64::engine::general_purpose::STANDARD
                .decode(s)
                .map_err(|e| format!("invalid base64: {e}"))?;            buf.push(TAG_BYTES);
            buf.extend_from_slice(
                &u32::try_from(raw.len())
                    .map_err(|_| "bytes value too long".to_string())?
                    .to_le_bytes(),
            );
            buf.extend_from_slice(&raw);
        }
        other => return Err(format!("unknown type {other:?}")),
    }
    Ok(())
}

/// Accept JSON integers and integer-valued floats within i64 range.
fn json_i64(v: &serde_json::Value) -> Result<i64, String> {
    if let Some(n) = v.as_i64() {
        return Ok(n);
    }
    if let Some(n) = v.as_u64() {
        return i64::try_from(n).map_err(|_| "value out of i64 range".to_string());
    }
    if let Some(n) = v.as_f64() {
        if n.fract() == 0.0 && n.is_finite() && (-9.223372036854776e18..9.223372036854776e18).contains(&n) {
            return Ok(n as i64);
        }
    }
    Err("expected an integer".to_string())
}

fn json_u64(v: &serde_json::Value) -> Result<u64, String> {
    if let Some(n) = v.as_u64() {
        return Ok(n);
    }
    if let Some(n) = v.as_i64() {
        return u64::try_from(n).map_err(|_| "value out of u64 range".to_string());
    }
    if let Some(n) = v.as_f64() {
        if n.fract() == 0.0 && n.is_finite() && (0.0..1.8446744073709552e19).contains(&n) {
            return Ok(n as u64);
        }
    }
    Err("expected a non-negative integer".to_string())
}

// ---------------------------------------------------------------------------
// Persisted domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Validated and persisted, execution not started.
    Pending,
    /// Wasm instance is live.
    Running,
    /// Guest returned and output was validated.
    Succeeded,
    /// Failed before/during/after the call; `error_stage` says where.
    Failed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Succeeded => "succeeded",
            TaskStatus::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => TaskStatus::Pending,
            "running" => TaskStatus::Running,
            "succeeded" => TaskStatus::Succeeded,
            "failed" => TaskStatus::Failed,
            _ => return None,
        })
    }
}

/// Which phase failed. Kept distinct so callers can tell contract/validation
/// problems (fixable by the caller) apart from sandbox enforcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorStage {
    /// Static module validation (decode, type-check, required exports).
    Validation,
    /// Imports resolution / instance instantiation / memory write-in.
    Instantiation,
    /// Guest `run` trapped, exhausted fuel/memory, timed out, or produced a
    /// bad output (oversized / not JSON when promised).
    Invocation,
}

impl ErrorStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorStage::Validation => "validation",
            ErrorStage::Instantiation => "instantiation",
            ErrorStage::Invocation => "invocation",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "validation" => ErrorStage::Validation,
            "instantiation" => ErrorStage::Instantiation,
            "invocation" => ErrorStage::Invocation,
            _ => return None,
        })
    }
}

/// Row of `plugins` (without the wasm blob) returned on list/get endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct PluginSummary {
    pub plugin_id: String,
    pub tenant: String,
    pub name: String,
    pub sha256: String,
    pub wasm_size: i64,
    pub contract: Contract,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Row of `tasks`.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub tenant: String,
    pub plugin_id: String,
    pub status: TaskStatus,
    pub error_stage: Option<ErrorStage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Present iff status == succeeded. Always NULL on failure: there is no
    /// such thing as a failed task with half an output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<serde_json::Value>,
    pub fuel_consumed: i64,
    pub duration_ms: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
}
