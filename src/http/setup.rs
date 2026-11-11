use axum::Router;

use crate::state::AppState;

/// `GET /api/state`, `POST /api/setup/manifest`, `GET /auth/github/app-created`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
