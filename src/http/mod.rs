pub mod auth;
pub mod keys;
pub mod members;
pub mod oauth_server;
pub mod repos;
pub mod search;
pub mod session;
pub mod settings;
pub mod setup;
pub mod ui;

use std::time::Duration;

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
use tower_http::trace::{MakeSpan, OnResponse, TraceLayer};
use tracing::Span;

use crate::crypto;
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

/// Sent on every response when the public URL is https.
const STRICT_TRANSPORT_SECURITY: &str = "max-age=31536000";

/// Layers, outermost first: request id → access log (trace) → compression → security headers →
/// same-origin check → routes. `/healthz` sits outside the request id and the access log, so
/// health probes do not fill the log; it still gets the security headers.
pub fn router(state: AppState) -> Router {
    let mut app_routes = Router::new()
        .merge(setup::routes())
        .merge(auth::routes())
        .merge(members::routes())
        .merge(repos::routes())
        .merge(keys::routes())
        .merge(settings::routes())
        .merge(search::routes())
        .merge(oauth_server::routes());
    if state.cfg.dev_mode {
        app_routes = app_routes.merge(crate::dev::routes());
    }
    let app_routes = app_routes.layer(DefaultBodyLimit::max(API_BODY_LIMIT));
    let webhook_routes =
        crate::github::webhook::routes().layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT));
    let mcp_routes = crate::mcp::routes().layer(DefaultBodyLimit::max(MCP_BODY_LIMIT));
    let health_routes =
        Router::new()
            .route("/healthz", get(healthz))
            .layer(middleware::map_response_with_state(
                state.clone(),
                add_security_headers,
            ));
    let access_log = TraceLayer::new_for_http()
        .make_span_with(RequestSpan)
        .on_request(())
        .on_response(AccessLog);

    Router::new()
        .merge(app_routes)
        .merge(webhook_routes)
        .merge(mcp_routes)
        .fallback(ui::fallback)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_same_origin,
        ))
        .layer(middleware::map_response_with_state(
            state.clone(),
            add_security_headers,
        ))
        .layer(CompressionLayer::new())
        .layer(access_log)
        .layer(middleware::from_fn(assign_request_id))
        .merge(health_routes)
        .with_state(state)
}

async fn healthz(State(state): State<AppState>) -> Response {
    let checked = state
        .db
        .read(|conn| conn.query_row("SELECT 1", [], |_| Ok(())))
        .await;
    match checked {
        Ok(()) => "ok".into_response(),
        Err(err) => {
            tracing::warn!("health check failed: {err}");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

/// Id of one request, shown in its access log line and returned as `x-request-id`.
#[derive(Clone)]
struct RequestId(String);

async fn assign_request_id(mut req: Request, next: Next) -> Response {
    let id = crypto::random_hex(8);
    let value = HeaderValue::from_str(&id);
    req.extensions_mut().insert(RequestId(id));
    let mut response = next.run(req).await;
    if let Ok(value) = value {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    response
}

/// Span of a request: method, path and request id. Never the query string or the full URI, at
/// any log level, because one-time secrets (`setup_token`, OAuth `code` and `state`) travel there;
/// every trace event of the request (response, failure, end of stream) is recorded in this span.
#[derive(Clone, Copy)]
struct RequestSpan;

impl<B> MakeSpan<B> for RequestSpan {
    fn make_span(&mut self, request: &axum::http::Request<B>) -> Span {
        let id = request
            .extensions()
            .get::<RequestId>()
            .map_or("", |id| id.0.as_str());
        tracing::info_span!(
            "request",
            method = %request.method(),
            path = %request.uri().path(),
            request_id = %id
        )
    }
}

/// The access log: one `INFO` line per request once its response is ready.
#[derive(Clone, Copy)]
struct AccessLog;

impl<B> OnResponse<B> for AccessLog {
    fn on_response(self, response: &axum::http::Response<B>, latency: Duration, span: &Span) {
        tracing::info!(
            parent: span,
            status = response.status().as_u16(),
            latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
            "request finished"
        );
    }
}

async fn add_security_headers(State(state): State<AppState>, mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(name, HeaderValue::from_static(value));
    }
    if state.cfg.public_is_https() {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static(STRICT_TRANSPORT_SECURITY),
        );
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
