use axum::Router;

use crate::state::AppState;

/// `POST /webhooks/github`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
