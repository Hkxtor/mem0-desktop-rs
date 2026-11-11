//! Unified API error matching the Python server's `{"detail": ...}` shape
//! (see `server/errors.py` and the golden files).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub detail: String,
}

impl ApiError {
    pub fn new(status: StatusCode, detail: impl Into<String>) -> Self {
        Self {
            status,
            detail: detail.into(),
        }
    }
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg.into())
    }
    pub fn too_many(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, msg.into())
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // A 5xx is a bug or an unreachable dependency; the client gets the message but
        // nobody is watching a packaged desktop app's stderr, so the log file keeps it.
        if self.status.is_server_error() {
            crate::logging::error(
                "http",
                "request failed",
                serde_json::json!({"status": self.status.as_u16(), "detail": self.detail}),
            );
        }
        // Python FastAPI emits {"detail": "<message>"} for HTTPException(detail=...).
        (self.status, Json(json!({ "detail": self.detail }))).into_response()
    }
}

impl From<mem0::Mem0Error> for ApiError {
    fn from(e: mem0::Mem0Error) -> Self {
        // Configuration/validation failures from the runtime surface as 400/503
        // depending on phase; map conservatively and let handlers refine.
        match e {
            mem0::Mem0Error::Configuration { .. } => {
                ApiError::unavailable(format!("runtime_not_configured: {e}"))
            }
            mem0::Mem0Error::Validation { .. } => ApiError::bad_request(e.to_string()),
            other => ApiError::internal(other.to_string()),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<crate::store::StoreError> for ApiError {
    fn from(e: crate::store::StoreError) -> Self {
        ApiError::internal(e.to_string())
    }
}
