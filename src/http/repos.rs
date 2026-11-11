use axum::Router;

use crate::state::AppState;

/// `/api/github/repositories`, `/api/repositories[/{id}[/branches|/sync]]`.
pub fn routes() -> Router<AppState> {
    Router::new()
}
