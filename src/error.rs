use std::fmt;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Error returned by every `/api` handler, rendered as `{"error":{"code":"…","message":"…"}}`.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// How GitHub answered when a GitHub call caused this error.
    pub github: Option<GitHubFailure>,
}

/// GitHub's answer behind an [`ApiError`], so callers can tell missing access from rate limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubFailure {
    /// A non-2xx status that is not a rate limit.
    Status(u16),
    RateLimited,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            github: None,
        }
    }

    pub fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }

    pub fn conflict(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code, message)
    }

    pub fn unprocessable(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    pub fn bad_gateway(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, code, message)
    }

    pub fn internal(cause: impl fmt::Display) -> Self {
        tracing::error!("internal error: {cause}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "Internal error.",
        )
    }

    /// The same error, marked with GitHub's answer.
    pub fn with_github(mut self, github: Option<GitHubFailure>) -> Self {
        self.github = github;
        self
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

impl From<rusqlite::Error> for ApiError {
    fn from(err: rusqlite::Error) -> Self {
        Self::internal(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, Json(body)).into_response()
    }
}
