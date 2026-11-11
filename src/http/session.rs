use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::error::{ApiError, ApiResult};
use crate::github::oauth::GhToken;
use crate::state::AppState;

pub const SESSION_COOKIE: &str = "sl_session";

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub login: String,
    pub role: String,
    pub session_hash: String,
}

#[derive(Debug, Clone)]
pub struct Owner(pub AuthUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        _parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        todo!("github-auth")
    }
}

impl FromRequestParts<AppState> for Owner {
    type Rejection = ApiError;

    async fn from_request_parts(
        _parts: &mut Parts,
        _state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        todo!("github-auth")
    }
}

/// Inserts the session row and returns the cookie value.
pub async fn create_session(
    _state: &AppState,
    _user_id: i64,
    _token: &GhToken,
) -> ApiResult<String> {
    todo!("github-auth")
}
