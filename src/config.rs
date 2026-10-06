//! Runtime configuration. Everything is overridable via environment
//! variables; the defaults are conservative sandbox limits.

use std::env;
use std::time::Duration;

/// Hard limits applied to every guest execution.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Maximum accepted module size (upload).
    pub max_module_bytes: usize,
    /// Maximum serialized task input size.
    pub max_input_bytes: usize,
    /// Maximum guest output size; larger outputs are rejected unread.
    pub max_output_bytes: usize,
    /// Maximum linear memory a guest may reach (initial + grown).
    pub max_memory_bytes: usize,
    /// Maximum number of table elements across all guest tables.
    pub max_table_elements: u32,
    /// Maximum wasm operand stack.
    pub max_wasm_stack_bytes: usize,
    /// Fuel budget per task. An infinite loop burns through this and traps.
    pub fuel_limit: u64,
    /// Wall-clock deadline per task, enforced via engine epochs.
    pub wall_clock: Duration,
    /// How often the engine epoch advances.
    pub epoch_tick: Duration,
    /// Maximum number of concurrent guest executions.
    pub max_concurrent_executions: usize,
    /// HTTP body cap (module uploads are the large ones).
    pub max_http_body_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_module_bytes: 8 * 1024 * 1024,
            max_input_bytes: 64 * 1024,
            max_output_bytes: 256 * 1024,
            max_memory_bytes: 64 * 1024 * 1024,
            max_table_elements: 100_000,
            max_wasm_stack_bytes: 1024 * 1024,
            fuel_limit: 50_000_000,
            wall_clock: Duration::from_secs(2),
            epoch_tick: Duration::from_millis(25),
            max_concurrent_executions: 4,
            max_http_body_bytes: 12 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub bind: String,
    pub database_url: String,
    pub limits: Limits,
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

impl AppConfig {
    pub fn from_env() -> Self {
        let d = Limits::default();
        let limits = Limits {
            max_module_bytes: env_parse("LIMIT_MAX_MODULE_BYTES", d.max_module_bytes),
            max_input_bytes: env_parse("LIMIT_MAX_INPUT_BYTES", d.max_input_bytes),
            max_output_bytes: env_parse("LIMIT_MAX_OUTPUT_BYTES", d.max_output_bytes),
            max_memory_bytes: env_parse("LIMIT_MAX_MEMORY_BYTES", d.max_memory_bytes),
            max_table_elements: env_parse("LIMIT_MAX_TABLE_ELEMENTS", d.max_table_elements),
            max_wasm_stack_bytes: env_parse("LIMIT_MAX_WASM_STACK_BYTES", d.max_wasm_stack_bytes),
            fuel_limit: env_parse("LIMIT_FUEL", d.fuel_limit),
            wall_clock: Duration::from_millis(env_parse(
                "LIMIT_WALL_CLOCK_MS",
                d.wall_clock.as_millis() as u64,
            )),
            epoch_tick: Duration::from_millis(env_parse(
                "LIMIT_EPOCH_TICK_MS",
                d.epoch_tick.as_millis() as u64,
            )),
            max_concurrent_executions: env_parse(
                "LIMIT_MAX_CONCURRENT_EXECUTIONS",
                d.max_concurrent_executions,
            ),
            max_http_body_bytes: env_parse("LIMIT_MAX_HTTP_BODY_BYTES", d.max_http_body_bytes),
        };
        Self {
            bind: env_or("BIND_ADDR", "127.0.0.1:8080"),
            database_url: env_or(
                "DATABASE_URL",
                "postgres://postgres@127.0.0.1:54329/pluginhost",
            ),
            limits,
        }
    }
}
