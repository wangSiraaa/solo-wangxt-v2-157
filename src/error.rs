//! Typed API errors. Tenant input values never appear in log/error output;
//! messages describe *what* was wrong structurally.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::model::ErrorStage;

/// Machine-readable error codes. Values are part of the API contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// HTTP-layer / malformed request.
    BadRequest,
    BodyTooLarge,
    NotFound,
    Conflict,
    /// Input object failed the plugin contract (no task is created).
    ContractInput,
    /// Plugin contract document itself invalid (upload rejected).
    ContractInvalid,
    /// Plugin module failed static validation (upload rejected).
    ModuleInvalid,
    /// Task execution: module could not be instantiated for this invocation.
    InstantiationFailed,
    /// Task execution: the guest call failed/trapped/was killed.
    InvocationFailed,
    Unavailable,
    Internal,
}

impl ErrorCode {
    fn as_str(&self) -> &'static str {
        match self {
            ErrorCode::BadRequest => "bad_request",
            ErrorCode::BodyTooLarge => "body_too_large",
            ErrorCode::NotFound => "not_found",
            ErrorCode::Conflict => "conflict",
            ErrorCode::ContractInput => "contract_input_violation",
            ErrorCode::ContractInvalid => "contract_invalid",
            ErrorCode::ModuleInvalid => "module_invalid",
            ErrorCode::InstantiationFailed => "instantiation_failed",
            ErrorCode::InvocationFailed => "invocation_failed",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::Internal => "internal",
        }
    }
}

/// Errors carrying a fixed status, stable code and a *structural* message.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: ErrorCode,
    pub message: String,
    /// For failed-task responses this carries the execution phase.
    pub stage: Option<ErrorStage>,
}

impl ApiError {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), stage: None }
    }

    pub fn bad_request(m: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::BadRequest, m)
    }
    pub fn body_too_large(m: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, ErrorCode::BodyTooLarge, m)
    }
    pub fn not_found(m: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, ErrorCode::NotFound, m)
    }
    pub fn conflict(m: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::Conflict, m)
    }
    pub fn input(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::ContractInput, m)
    }
    pub fn contract(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::ContractInvalid, m)
    }
    pub fn module(m: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::ModuleInvalid, m)
    }
    /// Execution-phase failures are still reported with HTTP 201: the task
    /// resource *was* created, then finished as `failed`.
    pub fn execution(stage: ErrorStage, m: impl Into<String>) -> Self {
        let code = match stage {
            ErrorStage::Validation => ErrorCode::ModuleInvalid,
            ErrorStage::Instantiation => ErrorCode::InstantiationFailed,
            ErrorStage::Invocation => ErrorCode::InvocationFailed,
        };
        let status = match stage {
            // Validation of a previously accepted module means stored bytes
            // are corrupt — surface as 500-ish via the task failure body.
            ErrorStage::Validation => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorStage::Instantiation | ErrorStage::Invocation => StatusCode::CREATED,
        };
        Self { status, code, message: m.into(), stage: Some(stage) }
    }
    pub fn unavailable(m: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, m)
    }
    pub fn internal(m: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal, m)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}
impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Log the structural error, never request parameters.
        match self.status.is_server_error() {
            true => tracing::error!(error_code = self.code.as_str(), error = %self.message, "request failed"),
            false => tracing::warn!(error_code = self.code.as_str(), error = %self.message, "request rejected"),
        }
        let mut body = json!({
            "error": { "code": self.code.as_str(), "message": self.message }
        });
        if let Some(stage) = self.stage {
            body["error"]["stage"] = json!(stage);
        }
        (self.status, Json(body)).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::RowNotFound = e {
            return ApiError::not_found("resource not found");
        }
        tracing::error!(error = %e, "database error");
        ApiError::internal("database error")
    }
}

/// Map any failure to read/parse the JSON request body.
pub fn body_decode_error(e: axum::extract::rejection::JsonRejection) -> ApiError {
    use axum::extract::rejection::JsonRejection;
    match &e {
        JsonRejection::JsonDataError(_) | JsonRejection::JsonSyntaxError(_) => {
            ApiError::bad_request(format!("invalid JSON body: {e}"))
        }
        JsonRejection::MissingJsonContentType(_) => {
            ApiError::bad_request("request must use content-type: application/json")
        }
        _ => ApiError::bad_request(format!("could not read JSON body: {e}")),
    }
}
