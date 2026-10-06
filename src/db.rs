//! PostgreSQL access. All queries are scoped by `tenant_id` so one tenant can
//! never read or address another tenant's plugins or tasks.

use std::str::FromStr;

use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{PluginMetaRow, PluginRow, TaskOutcome, TaskRow};

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    ensure_database_exists(database_url).await?;
    PgPoolOptions::new()
        .max_connections(8)
        .connect(database_url)
        .await
}

/// Create the target database if it does not exist yet, by connecting to the
/// `postgres` maintenance database. Keeps local/acceptance setup trivial.
async fn ensure_database_exists(database_url: &str) -> Result<(), sqlx::Error> {
    let options = PgConnectOptions::from_str(database_url)?;
    let Some(dbname) = options.get_database().map(str::to_string) else {
        return Ok(()); // server default database
    };
    if dbname == "postgres" {
        return Ok(());
    }
    let maintenance = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options.database("postgres"))
        .await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&dbname)
            .fetch_one(&maintenance)
            .await?;
    if !exists {
        // CREATE DATABASE cannot be parameterized; validate the identifier.
        if dbname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            sqlx::query(&format!("CREATE DATABASE \"{dbname}\""))
                .execute(&maintenance)
                .await?;
            tracing::info!(database = %dbname, "created database");
        } else {
            return Err(sqlx::Error::Configuration(
                format!("unsafe database name: {dbname:?}").into(),
            ));
        }
    }
    maintenance.close().await;
    Ok(())
}

pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
        .execute(pool)
        .await?;
    Ok(())
}

/// Close tasks that were in flight when a previous process died. They can
/// never produce a result, so they are failed atomically as `interrupted`
/// rather than left half-written.
pub async fn reconcile_interrupted(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE tasks
            SET status = 'failed',
                error_kind = 'interrupted',
                error_message = 'server restarted while the task was in flight',
                finished_at = now()
          WHERE status IN ('pending', 'running')",
    )
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

// ---------- plugins ----------

#[allow(clippy::too_many_arguments)]
pub async fn insert_plugin(pool: &PgPool, plugin: &PluginRow) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO plugins (id, tenant_id, name, wasm, sha256, size_bytes, contract)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(plugin.id)
    .bind(&plugin.tenant_id)
    .bind(&plugin.name)
    .bind(&plugin.wasm)
    .bind(&plugin.sha256)
    .bind(plugin.size_bytes)
    .bind(&plugin.contract)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_plugin(
    pool: &PgPool,
    id: Uuid,
    tenant: &str,
) -> Result<Option<PluginRow>, sqlx::Error> {
    sqlx::query_as::<_, PluginRow>(
        "SELECT id, tenant_id, name, wasm, sha256, size_bytes, contract, created_at
           FROM plugins WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(tenant)
    .fetch_optional(pool)
    .await
}

pub async fn list_plugins(pool: &PgPool, tenant: &str) -> Result<Vec<PluginMetaRow>, sqlx::Error> {
    sqlx::query_as::<_, PluginMetaRow>(
        "SELECT id, tenant_id, name, sha256, size_bytes, contract, created_at
           FROM plugins WHERE tenant_id = $1 ORDER BY created_at DESC",
    )
    .bind(tenant)
    .fetch_all(pool)
    .await
}

// ---------- tasks ----------

pub async fn insert_task(
    pool: &PgPool,
    id: Uuid,
    plugin_id: Uuid,
    tenant: &str,
    input: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tasks (id, plugin_id, tenant_id, status, input)
         VALUES ($1, $2, $3, 'pending', $4)",
    )
    .bind(id)
    .bind(plugin_id)
    .bind(tenant)
    .bind(input)
    .execute(pool)
    .await?;
    Ok(())
}

/// Atomically move a task pending -> running. Returns false if someone else
/// already claimed it.
pub async fn claim_task(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE tasks SET status = 'running', started_at = now()
          WHERE id = $1 AND status = 'pending'",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Write the terminal state of a task in a single UPDATE. Combined with the
/// `tasks_outcome_integrity` CHECK constraint this guarantees that a failed
/// task never leaves behind a partial success (no output, error set) and a
/// successful task never carries an error.
pub async fn finish_task(
    pool: &PgPool,
    id: Uuid,
    outcome: &TaskOutcome,
) -> Result<(), sqlx::Error> {
    let (status, output, kind, message, fuel) = match outcome {
        TaskOutcome::Success { output, fuel } => {
            ("succeeded", Some(output.clone()), None, None, *fuel)
        }
        TaskOutcome::Failure {
            kind,
            message,
            fuel,
        } => (
            "failed",
            None,
            Some(kind.to_string()),
            Some(message.clone()),
            *fuel,
        ),
    };
    sqlx::query(
        "UPDATE tasks
            SET status = $2, output = $3, error_kind = $4, error_message = $5,
                fuel_consumed = $6, finished_at = now()
          WHERE id = $1 AND status = 'running'",
    )
    .bind(id)
    .bind(status)
    .bind(output)
    .bind(kind)
    .bind(message)
    .bind(fuel)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn get_task(
    pool: &PgPool,
    id: Uuid,
    tenant: &str,
) -> Result<Option<TaskRow>, sqlx::Error> {
    sqlx::query_as::<_, TaskRow>(
        "SELECT id, plugin_id, tenant_id, status, input, output, error_kind,
                error_message, fuel_consumed, created_at, started_at, finished_at
           FROM tasks WHERE id = $1 AND tenant_id = $2",
    )
    .bind(id)
    .bind(tenant)
    .fetch_optional(pool)
    .await
}
