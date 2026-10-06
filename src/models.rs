//! Database rows and API DTOs.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PluginRow {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub wasm: Vec<u8>,
    pub sha256: String,
    pub size_bytes: i64,
    pub contract: Value,
    pub created_at: DateTime<Utc>,
}

/// Plugin metadata without the (potentially large) wasm bytes.
#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PluginMetaRow {
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub contract: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TaskRow {
    pub id: Uuid,
    pub plugin_id: Uuid,
    pub tenant_id: String,
    pub status: String,
    pub input: Value,
    pub output: Option<Value>,
    pub error_kind: Option<String>,
    pub error_message: Option<String>,
    pub fuel_consumed: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

/// Terminal outcome of a task, written in a single atomic UPDATE.
pub enum TaskOutcome {
    Success {
        output: Value,
        fuel: i64,
    },
    Failure {
        kind: &'static str,
        message: String,
        fuel: i64,
    },
}

// ---------- API DTOs ----------

#[derive(Debug, Deserialize)]
pub struct UploadPluginRequest {
    pub name: String,
    pub contract: Value,
    pub wasm_base64: String,
}

#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub id: Uuid,
    pub name: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub contract: Value,
    pub imports: Vec<String>,
    pub exports: Vec<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub input: Value,
}

#[derive(Debug, Serialize)]
pub struct CreateTaskResponse {
    pub id: Uuid,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct TaskResponse {
    pub id: Uuid,
    pub plugin_id: Uuid,
    pub status: String,
    pub input: Value,
    pub output: Option<Value>,
    pub error: Option<Value>,
    pub fuel_consumed: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl From<TaskRow> for TaskResponse {
    fn from(row: TaskRow) -> Self {
        let error = match (row.error_kind, row.error_message) {
            (Some(kind), message) => Some(json!({
                "kind": kind,
                "message": message.unwrap_or_default(),
            })),
            _ => None,
        };
        Self {
            id: row.id,
            plugin_id: row.plugin_id,
            status: row.status,
            input: row.input,
            output: row.output,
            error,
            fuel_consumed: row.fuel_consumed,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        }
    }
}
