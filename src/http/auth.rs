use axum::Router;

use crate::state::AppState;

/// `GET /auth/github/login`, `GET /auth/github/callback`, `POST /api/logout`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
