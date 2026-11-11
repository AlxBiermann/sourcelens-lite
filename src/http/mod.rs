pub mod auth;
pub mod keys;
pub mod members;
pub mod repos;
pub mod search;
pub mod session;
pub mod settings;
pub mod setup;
pub mod ui;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{header, Method};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::error::ApiError;
use crate::state::AppState;

const API_BODY_LIMIT: usize = 1024 * 1024;
const WEBHOOK_BODY_LIMIT: usize = 8 * 1024 * 1024;
const MCP_BODY_LIMIT: usize = 1024 * 1024;

pub fn router(state: AppState) -> Router {
    let app_routes = Router::new()
        .merge(setup::routes())
        .merge(auth::routes())
        .merge(members::routes())
        .merge(repos::routes())
        .merge(keys::routes())
        .merge(settings::routes())
        .merge(search::routes())
        .layer(DefaultBodyLimit::max(API_BODY_LIMIT));
    let webhook_routes =
        crate::github::webhook::routes().layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT));
    let mcp_routes = crate::mcp::routes().layer(DefaultBodyLimit::max(MCP_BODY_LIMIT));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(app_routes)
        .merge(webhook_routes)
        .merge(mcp_routes)
        .fallback(ui::fallback)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_same_origin,
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// CSRF protection: state-changing `/api/` requests must come from the UI's origin.
async fn require_same_origin(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let checked = req.uri().path().starts_with("/api/")
        && !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if checked {
        let origin = req
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_end_matches('/'));
        let allowed = origin.is_some_and(|origin| {
            origin == state.cfg.public_origin() || state.cfg.dev_origin.as_deref() == Some(origin)
        });
        if !allowed {
            return ApiError::forbidden(
                "origin_not_allowed",
                "This request did not come from the SourceLens web app.",
            )
            .into_response();
        }
    }
    next.run(req).await
}
