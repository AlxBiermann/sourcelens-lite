use axum::Router;

use crate::state::AppState;

/// `/api/settings/github`, `/api/settings/embeddings[/test]`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
