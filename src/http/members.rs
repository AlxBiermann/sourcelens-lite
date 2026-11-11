use axum::Router;

use crate::state::AppState;

/// `/api/members[/{id}]`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
