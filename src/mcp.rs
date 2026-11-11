use axum::Router;

use crate::state::AppState;

/// `/mcp` (POST; GET/DELETE → 405).
pub fn routes() -> Router<AppState> {
    Router::new()
}
