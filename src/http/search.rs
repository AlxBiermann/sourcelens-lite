use axum::Router;

use crate::state::AppState;

/// `POST /api/search`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
