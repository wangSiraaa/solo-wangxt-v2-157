//! HTTP layer: tenant extraction, routes and request/response DTOs.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::error::{body_decode_error, ApiError};
use crate::model::{Contract, PluginSummary, TaskRecord};
use crate::service::{self, AppState};

const TENANT_HEADER: &str = "x-tenant-id";

/// Every request (except health/metrics) carries a tenant. Values are only
/// used as opaque identifiers; they are not written into log messages.
#[derive(Debug, Clone)]
pub struct Tenant(pub String);

impl Tenant {
    fn from_headers(h: &HeaderMap) -> Result<Self, ApiError> {
        let v = h
            .get(TENANT_HEADER)
            .ok_or_else(|| ApiError::bad_request(format!("missing {TENANT_HEADER} header")))?
            .to_str()
            .map_err(|_| ApiError::bad_request(format!("{TENANT_HEADER} must be ASCII")))?;
        if v.is_empty() || v.len() > 64 || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err(ApiError::bad_request(format!(
                "{TENANT_HEADER} must be 1..64 chars of [a-zA-Z0-9_-]"
            )));
        }
        Ok(Tenant(v.to_string()))
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/metrics", get(metrics))
        .route("/v1/plugins", get(list_plugins).post(upload_plugin))
        .route("/v1/plugins/{id}", get(get_plugin))
        .route("/v1/plugins/{id}/invoke", post(invoke_plugin))
        .route("/v1/tasks", get(list_tasks))
        .route("/v1/tasks/{id}", get(get_task))
        .with_state(state)
}

async fn healthz(State(s): State<AppState>) -> axum::response::Response {
    match sqlx::query("SELECT 1").execute(&s.db.pool).await {
        Ok(_) => Json(json!({ "status": "ok" })).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "status": "degraded", "error": "database unavailable" })),
        )
            .into_response(),
    }
}

async fn metrics(State(s): State<AppState>) -> impl IntoResponse {
    ([("content-type", "text/plain; version=0.0.4")], s.metrics.render())
}

#[derive(Debug, Deserialize)]
struct UploadRequest {
    name: String,
    contract: serde_json::Value,
    /// Raw wasm module, standard base64 encoded.
    wasm_base64: String,
}

async fn upload_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<UploadRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    if req.wasm_base64.len() > state.limits.max_wasm_bytes * 2 {
        return Err(ApiError::body_too_large("wasm_base64 too large"));
    }

    use base64::Engine;
    let wasm = base64::engine::general_purpose::STANDARD
        .decode(req.wasm_base64.trim())
        .map_err(|e| ApiError::bad_request(format!("wasm_base64 is not valid base64: {e}")))?;
    let contract: Contract =
        serde_json::from_value(req.contract).map_err(|e| ApiError::contract(format!("malformed contract: {e}")))?;

    let up = service::upload_plugin(&state, &tenant.0, &req.name, &wasm, contract).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "plugin_id": up.plugin_id,
            "sha256": up.sha256,
            "interface": up.interface,
        })),
    ))
}

async fn list_plugins(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    let plugins: Vec<PluginSummary> = state.db.list_plugins(&tenant.0, 100).await?;
    Ok(Json(json!({ "plugins": plugins })))
}

async fn get_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    let p = state.db.get_plugin(&tenant.0, &id).await?;
    // Note: wasm bytes are intentionally not returned.
    Ok(Json(json!({
        "plugin_id": p.plugin_id,
        "tenant": p.tenant,
        "contract": p.contract,
    })))
}

async fn invoke_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    input: Result<Json<serde_json::Value>, axum::extract::rejection::JsonRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    let Json(input) = input.map_err(body_decode_error)?;
    let res = service::invoke(&state, &tenant.0, &id, input).await?;
    // 201 Created: the task resource was created and reached a terminal state
    // synchronously. Success vs failure is carried by `status`/`error_stage`.
    Ok((StatusCode::CREATED, Json(task_envelope(res.task, res.guest_logs))))
}

async fn get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    let task = state.db.get_task(&tenant.0, &id).await?;
    Ok(Json(task_envelope(task, Vec::new())))
}

async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let tenant = Tenant::from_headers(&headers)?;
    let tasks = state.db.list_tasks(&tenant.0, 100).await?;
    let body: Vec<serde_json::Value> = tasks
        .into_iter()
        .map(|t| serde_json::to_value(&t).expect("task serializes"))
        .collect();
    Ok(Json(json!({ "tasks": body })))
}

fn task_envelope(task: TaskRecord, guest_logs: Vec<String>) -> serde_json::Value {
    let mut v = serde_json::to_value(&task).expect("task serializes");
    v["guest_logs"] = json!(guest_logs);
    v
}
