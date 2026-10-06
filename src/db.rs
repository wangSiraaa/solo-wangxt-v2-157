//! PostgreSQL persistence. All writes for a task's terminal state happen in a
//! single `UPDATE ... SET status, output/error, ...`, so a failed task can
//! never be observed with a partial success output.

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool, Row};
use std::str::FromStr;

use crate::error::ApiError;
use crate::model::{Contract, ErrorStage, PluginSummary, TaskRecord, TaskStatus};

#[derive(Clone)]
pub struct Db {
    pub pool: PgPool,
}

/// Row used inside the service layer (includes the wasm blob).
pub struct StoredPlugin {
    pub plugin_id: String,
    pub tenant: String,
    pub wasm: Vec<u8>,
    pub contract: Contract,
}

impl Db {
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, ApiError> {
        // Never log statement payloads (they contain tenant data / outputs).
        let opts = PgConnectOptions::from_str(database_url)
            .map_err(|e| ApiError::unavailable(format!("invalid DATABASE_URL: {e}")))?
            .log_statements(tracing::log::LevelFilter::Off)
            .log_slow_statements(tracing::log::LevelFilter::Warn, std::time::Duration::from_secs(2));
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect_with(opts)
            .await
            .map_err(|e| ApiError::unavailable(format!("cannot reach postgres: {e}")))?;
        let db = Self { pool };
        db.migrate().await?;
        Ok(db)
    }

    async fn migrate(&self) -> Result<(), ApiError> {
        // raw_sql allows a multi-statement migration script; the regular query
        // API prepares one statement at a time and rejects multiple commands.
        sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
            .execute(&self.pool)
            .await
            .map_err(|e| ApiError::unavailable(format!("migration failed: {e}")))?;
        Ok(())
    }

    /// Idempotently ensure a tenant row exists (tenants are provisioned
    /// lazily; isolation is enforced by tenant scoping on every query).
    pub async fn ensure_tenant(&self, tenant: &str) -> Result<(), ApiError> {
        sqlx::query("INSERT INTO tenants (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(tenant)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert plugin only after full validation has passed.
    #[allow(clippy::too_many_arguments)] // flat persistence API; one row, one call
    pub async fn insert_plugin(
        &self,
        id: &str,
        tenant: &str,
        name: &str,
        wasm: &[u8],
        sha256: &str,
        contract: &Contract,
        interface: &serde_json::Value,
    ) -> Result<(), ApiError> {
        let res = sqlx::query(
            "INSERT INTO plugins (id, tenant_id, name, wasm_bytes, sha256, contract, interface)
             VALUES ($1,$2,$3,$4,$5,$6,$7)",
        )
        .bind(id)
        .bind(tenant)
        .bind(name)
        .bind(wasm)
        .bind(sha256)
        .bind(serde_json::to_value(contract).expect("contract serializes"))
        .bind(interface)
        .execute(&self.pool)
        .await
        .map_err(|e| {
            if let sqlx::Error::Database(db) = &e {
                if db.is_unique_violation() {
                    return ApiError::conflict("plugin name already exists for this tenant");
                }
            }
            ApiError::from(e)
        })?;
        debug_assert_eq!(res.rows_affected(), 1);
        Ok(())
    }

    pub async fn get_plugin(&self, tenant: &str, id: &str) -> Result<StoredPlugin, ApiError> {
        let row = sqlx::query(
            "SELECT id, tenant_id, wasm_bytes, contract
             FROM plugins WHERE id = $1 AND tenant_id = $2",
        )
        .bind(id)
        .bind(tenant)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("plugin not found for this tenant"))?;
        Ok(StoredPlugin {
            plugin_id: row.get("id"),
            tenant: row.get("tenant_id"),
            wasm: row.get("wasm_bytes"),
            contract: serde_json::from_value(row.get("contract")).map_err(internal)?,
        })
    }

    pub async fn list_plugins(&self, tenant: &str, limit: i64) -> Result<Vec<PluginSummary>, ApiError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, name, sha256, octet_length(wasm_bytes) AS wasm_size,
                    contract, created_at
             FROM plugins WHERE tenant_id = $1
             ORDER BY created_at DESC LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                Ok(PluginSummary {
                    plugin_id: r.get("id"),
                    tenant: r.get("tenant_id"),
                    name: r.get("name"),
                    sha256: r.get("sha256"),
                    wasm_size: r.get("wasm_size"),
                    contract: serde_json::from_value(r.get("contract")).map_err(internal)?,
                    created_at: r.get::<DateTime<Utc>, _>("created_at"),
                })
            })
            .collect()
    }

    /// Create the task row already in `running`. It is only ever created
    /// after input validation has passed; finalize_task is the sole writer of
    /// success/failure outcome.
    pub async fn create_running_task(&self, id: &str, tenant: &str, plugin_id: &str) -> Result<(), ApiError> {
        sqlx::query(
            "INSERT INTO tasks (id, tenant_id, plugin_id, status, started_at)
             VALUES ($1,$2,$3,'running', now())",
        )
        .bind(id)
        .bind(tenant)
        .bind(plugin_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomically record terminal success. Output is stored in the same
    /// statement as the status flip.
    pub async fn finalize_success(
        &self,
        id: &str,
        output: &serde_json::Value,
        fuel_consumed: i64,
        duration_ms: i64,
    ) -> Result<(), ApiError> {
        let res = sqlx::query(
            "UPDATE tasks SET status='succeeded', output=$2, fuel_consumed=$3,
                    duration_ms=$4, finished_at=now()
              WHERE id=$1 AND status='running'",
        )
        .bind(id)
        .bind(output)
        .bind(fuel_consumed)
        .bind(duration_ms)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() != 1 {
            return Err(ApiError::internal("task finalize race: task not running"));
        }
        Ok(())
    }

    /// Atomically record terminal failure. Output column stays NULL
    /// (constrained by `tasks_no_output_unless_done`).
    pub async fn finalize_failure(
        &self,
        id: &str,
        stage: ErrorStage,
        code: &str,
        message: &str,
        fuel_consumed: i64,
        duration_ms: i64,
    ) -> Result<(), ApiError> {
        let res = sqlx::query(
            "UPDATE tasks SET status='failed', error_stage=$2, error_code=$3, error_message=$4,
                    fuel_consumed=$5, duration_ms=$6, finished_at=now()
              WHERE id=$1 AND status='running'",
        )
        .bind(id)
        .bind(stage.as_str())
        .bind(code)
        .bind(message)
        .bind(fuel_consumed)
        .bind(duration_ms)
        .execute(&self.pool)
        .await?;
        if res.rows_affected() != 1 {
            return Err(ApiError::internal("task finalize race: task not running"));
        }
        Ok(())
    }

    pub async fn get_task(&self, tenant: &str, id: &str) -> Result<TaskRecord, ApiError> {
        map_task(
            sqlx::query(
                "SELECT id, tenant_id, plugin_id, status, error_stage, error_code, error_message,
                        output, fuel_consumed, duration_ms, created_at, finished_at
                 FROM tasks WHERE id = $1 AND tenant_id = $2",
            )
            .bind(id)
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("task not found for this tenant"))?,
        )
    }

    pub async fn list_tasks(&self, tenant: &str, limit: i64) -> Result<Vec<TaskRecord>, ApiError> {
        let rows = sqlx::query(
            "SELECT id, tenant_id, plugin_id, status, error_stage, error_code, error_message,
                    output, fuel_consumed, duration_ms, created_at, finished_at
             FROM tasks WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT $2",
        )
        .bind(tenant)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(map_task).collect()
    }

    /// At boot any task left `running` by a crash becomes `failed`
    /// (instantiation stage) — it can never masquerade as success.
    pub async fn recover_stale(&self) -> Result<u64, ApiError> {
        let res = sqlx::query(
            "UPDATE tasks SET status='failed', error_stage='invocation',
                    error_code='interrupted', error_message='server restarted before task finished',
                    finished_at=now()
              WHERE status IN ('pending','running')",
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}

fn map_task(r: sqlx::postgres::PgRow) -> Result<TaskRecord, ApiError> {
    let status = TaskStatus::parse(r.get::<&str, _>("status"))
        .ok_or_else(|| ApiError::internal("unknown task status in db"))?;
    let error_stage = r
        .get::<Option<&str>, _>("error_stage")
        .map(|s| ErrorStage::parse(s).ok_or_else(|| ApiError::internal("unknown error stage")))
        .transpose()?;
    Ok(TaskRecord {
        task_id: r.get("id"),
        tenant: r.get("tenant_id"),
        plugin_id: r.get("plugin_id"),
        status,
        error_stage,
        error_code: r.get("error_code"),
        error_message: r.get("error_message"),
        output: r.get::<Option<serde_json::Value>, _>("output"),
        fuel_consumed: r.get("fuel_consumed"),
        duration_ms: r.get("duration_ms"),
        created_at: r.get::<DateTime<Utc>, _>("created_at"),
        finished_at: r.get("finished_at"),
    })
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(format!("persisted contract could not be decoded: {e}"))
}
