//! Tenant extraction and per-request log isolation.
//!
//! Every request must carry an `x-tenant-id` header. The middleware attaches
//! it to a tracing span so every log line emitted while serving the request —
//! including guest parameters logged at DEBUG — is tagged with exactly one
//! tenant. Parameters of tenant A can never appear in tenant B's records.

use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

use crate::error::{ApiError, ErrorKind};

pub const TENANT_HEADER: &str = "x-tenant-id";

#[derive(Debug, Clone)]
pub struct Tenant(pub String);

impl Tenant {
    pub fn id(&self) -> &str {
        &self.0
    }
}

fn valid_tenant_id(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

impl<S> FromRequestParts<S> for Tenant
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        match parts
            .headers
            .get(TENANT_HEADER)
            .and_then(|v| v.to_str().ok())
        {
            Some(t) if valid_tenant_id(t) => Ok(Tenant(t.to_string())),
            _ => Err(ApiError::new(
                ErrorKind::BadRequest,
                format!("missing or invalid `{TENANT_HEADER}` header"),
            )),
        }
    }
}

/// Wrap every request in a span carrying the tenant id (or `<none>`), so all
/// downstream logs are attributable to exactly one tenant.
pub async fn tenant_span_middleware(req: Request, next: Next) -> Response {
    let tenant = req
        .headers()
        .get(TENANT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let request_id = uuid::Uuid::new_v4();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let span = tracing::info_span!(
        "http_request",
        tenant_id = tenant.as_deref().unwrap_or("<none>"),
        %request_id,
        method,
        path,
    );
    next.run(req).instrument(span).await
}
