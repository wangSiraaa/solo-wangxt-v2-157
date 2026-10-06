//! HTTP API. All plugin/task routes are tenant-scoped via `x-tenant-id`.

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use base64::Engine;
use chrono::Utc;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::Instrument;
use uuid::Uuid;

use crate::contract;
use crate::db;
use crate::error::{ApiError, ErrorKind};
use crate::models::{
    CreateTaskRequest, CreateTaskResponse, PluginResponse, TaskResponse, UploadPluginRequest,
};
use crate::tenancy::{tenant_span_middleware, Tenant};
use crate::AppState;

pub fn router(state: AppState) -> Router {
    let body_limit = state.config.limits.max_http_body_bytes;
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/metrics", get(metrics_snapshot))
        .route("/v1/plugins", post(upload_plugin).get(list_plugins))
        .route("/v1/plugins/{plugin_id}", get(get_plugin))
        .route("/v1/plugins/{plugin_id}/tasks", post(create_task))
        .route("/v1/tasks/{task_id}", get(get_task))
        .layer(DefaultBodyLimit::max(body_limit))
        .layer(middleware::from_fn(tenant_span_middleware))
        .with_state(state)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn metrics_snapshot(State(state): State<AppState>) -> Json<Value> {
    Json(state.metrics.snapshot())
}

// ---------- plugins ----------

async fn upload_plugin(
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
    Json(req): Json<UploadPluginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if req.name.is_empty() || req.name.len() > 128 {
        return Err(ApiError::new(ErrorKind::BadRequest, "invalid plugin name"));
    }
    let wasm = base64::engine::general_purpose::STANDARD
        .decode(req.wasm_base64.as_bytes())
        .map_err(|e| ApiError::new(ErrorKind::BadRequest, format!("invalid base64: {e}")))?;

    // Stage 1: contract must be a valid JSON Schema.
    contract::compile_contract(&req.contract)?;

    // Stage 2: module validation (wasm correctness, import whitelist, ABI).
    let inspection = state.executor.validate_module(&wasm)?;

    let sha256 = hex_sha256(&wasm);
    let plugin = crate::models::PluginRow {
        id: Uuid::new_v4(),
        tenant_id: tenant.clone(),
        name: req.name,
        size_bytes: wasm.len() as i64,
        wasm,
        sha256,
        contract: req.contract,
        created_at: Utc::now(),
    };
    db::insert_plugin(&state.db, &plugin).await?;

    tracing::info!(
        plugin_id = %plugin.id,
        name = %plugin.name,
        sha256 = %plugin.sha256,
        size_bytes = plugin.size_bytes,
        "plugin uploaded"
    );

    let response = PluginResponse {
        id: plugin.id,
        name: plugin.name,
        sha256: plugin.sha256,
        size_bytes: plugin.size_bytes,
        contract: plugin.contract,
        imports: inspection.imports,
        exports: inspection.exports,
        created_at: plugin.created_at,
    };
    Ok((StatusCode::CREATED, Json(response)))
}

async fn list_plugins(
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::models::PluginMetaRow>>, ApiError> {
    Ok(Json(db::list_plugins(&state.db, &tenant).await?))
}

async fn get_plugin(
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
    Path(plugin_id): Path<Uuid>,
) -> Result<Json<crate::models::PluginMetaRow>, ApiError> {
    let plugin = db::get_plugin(&state.db, plugin_id, &tenant)
        .await?
        .ok_or_else(|| ApiError::not_found("plugin"))?;
    Ok(Json(crate::models::PluginMetaRow {
        id: plugin.id,
        tenant_id: plugin.tenant_id,
        name: plugin.name,
        sha256: plugin.sha256,
        size_bytes: plugin.size_bytes,
        contract: plugin.contract,
        created_at: plugin.created_at,
    }))
}

// ---------- tasks ----------

async fn create_task(
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
    Path(plugin_id): Path<Uuid>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let plugin = db::get_plugin(&state.db, plugin_id, &tenant)
        .await?
        .ok_or_else(|| ApiError::not_found("plugin"))?;

    let input_bytes = serde_json::to_vec(&req.input)
        .map_err(|e| ApiError::new(ErrorKind::BadRequest, format!("invalid input: {e}")))?;
    if input_bytes.len() > state.config.limits.max_input_bytes {
        return Err(ApiError::new(
            ErrorKind::BadRequest,
            format!(
                "input of {} bytes exceeds the {} byte limit",
                input_bytes.len(),
                state.config.limits.max_input_bytes
            ),
        ));
    }

    // Contract validation happens before the task is even recorded: invalid
    // input never creates a task row.
    let validator = contract::compile_contract(&plugin.contract)?;
    contract::validate_input(&validator, &req.input)?;

    let task_id = Uuid::new_v4();
    db::insert_task(&state.db, task_id, plugin.id, &tenant, &req.input).await?;

    // Parameters are logged only inside this tenant's context, and the raw
    // input only at DEBUG; other tenants' logs never see them.
    tracing::info!(
        %task_id,
        plugin_id = %plugin.id,
        input_sha256 = %hex_sha256(&input_bytes),
        "task accepted"
    );
    tracing::debug!(%task_id, input = %req.input, "task input");

    let span = tracing::info_span!(
        "task_execution",
        tenant_id = %tenant,
        %task_id,
        plugin_id = %plugin.id,
    );
    tokio::spawn(
        crate::tasks::run_task(state.clone(), task_id, plugin, req.input).instrument(span),
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(CreateTaskResponse {
            id: task_id,
            status: "pending",
        }),
    ))
}

async fn get_task(
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
    Path(task_id): Path<Uuid>,
) -> Result<Json<TaskResponse>, ApiError> {
    let task = db::get_task(&state.db, task_id, &tenant)
        .await?
        .ok_or_else(|| ApiError::not_found("task"))?;
    Ok(Json(task.into()))
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}
