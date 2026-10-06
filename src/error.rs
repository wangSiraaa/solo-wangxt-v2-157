//! API error type. Each failure stage of the plugin lifecycle maps to a
//! distinct, machine-readable `kind` so callers can tell module validation,
//! contract validation, instantiation and invocation failures apart.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Wasm module failed validation (bad binary, disallowed imports,
    /// missing ABI exports, too large).
    ModuleValidationFailed,
    /// The supplied contract is not a valid JSON Schema.
    InvalidContract,
    /// Task input does not satisfy the plugin's contract.
    ContractViolation,
    NotFound,
    BadRequest,
    Internal,
}

#[derive(Debug)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub message: String,
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(ErrorKind::NotFound, format!("{what} not found"))
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        // Never leak SQL or connection details to the caller.
        tracing::error!(error = %err, "database error");
        Self::new(ErrorKind::Internal, "database error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.kind {
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::BadRequest | ErrorKind::InvalidContract => StatusCode::BAD_REQUEST,
            ErrorKind::ModuleValidationFailed | ErrorKind::ContractViolation => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        let mut error = json!({
            "kind": self.kind,
            "message": self.message,
        });
        if let Some(details) = self.details {
            error["details"] = details;
        }
        (status, Json(json!({ "error": error }))).into_response()
    }
}
