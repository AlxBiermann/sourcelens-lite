use axum::Router;

use crate::error::ApiResult;
use crate::state::AppState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScope {
    pub key_id: i64,
    pub repo_ids: Vec<i64>,
}

/// `/api/keys[/{id}]`.
pub fn routes() -> Router<AppState> {
    Router::new()
}

pub async fn authenticate(_state: &AppState, _bearer: &str) -> ApiResult<KeyScope> {
    todo!("keys-mcp")
}
