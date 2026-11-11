pub mod auth;
pub mod keys;
pub mod members;
pub mod repos;
pub mod search;
pub mod session;
pub mod settings;
pub mod setup;
pub mod ui;

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Request, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

use crate::error::ApiError;
use crate::state::AppState;

const API_BODY_LIMIT: usize = 1024 * 1024;
const WEBHOOK_BODY_LIMIT: usize = 8 * 1024 * 1024;
const MCP_BODY_LIMIT: usize = 1024 * 1024;

/// The UI loads scripts, styles and fonts only from this origin, avatars from GitHub, and submits
/// the manifest form to GitHub; nothing may frame it.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; \
    img-src 'self' https://avatars.githubusercontent.com data:; \
    style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'self'; \
    frame-ancestors 'none'; base-uri 'none'; form-action 'self' https://github.com";

const SECURITY_HEADERS: [(HeaderName, &str); 4] = [
    (header::X_FRAME_OPTIONS, "DENY"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::REFERRER_POLICY, "same-origin"),
    (header::CONTENT_SECURITY_POLICY, CONTENT_SECURITY_POLICY),
];

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
        .layer(middleware::map_response(add_security_headers))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
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

/// JSON request body whose rejection (bad JSON, wrong types, missing content type) is answered as
/// 400 `invalid_body` in the API error format.
pub struct Body<T>(pub T);

impl<T, S> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(invalid_body(&rejection)),
        }
    }
}

fn invalid_body(rejection: &JsonRejection) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "The request body is too large.",
        );
    }
    ApiError::bad_request(
        "invalid_body",
        format!("The request body is invalid: {}", rejection.body_text()),
    )
}

/// Numeric `{id}` path segment; anything else is 404 `not_found` in the API error format.
pub struct PathId(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for PathId {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Path::<i64>::from_request_parts(parts, state).await {
            Ok(Path(id)) => Ok(Self(id)),
            Err(_) => Err(ApiError::not_found(
                "not_found",
                "Nothing exists at this address.",
            )),
        }
    }
}
