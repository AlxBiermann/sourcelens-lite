use axum::body::Body;
use axum::http::{header, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::error::ApiError;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui/dist"]
struct Assets;

const NOT_BUILT: &str = "UI not built. Run: npm --prefix ui ci && npm --prefix ui run build";
const BACKEND_PREFIXES: [&str; 4] = ["/api/", "/auth/", "/webhooks/", "/mcp"];

/// Router fallback: embedded UI assets for GET/HEAD, `index.html` for client-side routes.
pub async fn fallback(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if BACKEND_PREFIXES
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return ApiError::not_found("not_found", "No such endpoint.").into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let asset_path = path.trim_start_matches('/');
    if !asset_path.is_empty() {
        if let Some(file) = Assets::get(asset_path) {
            let cache = if asset_path.starts_with("assets/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            let mime = mime_guess::from_path(asset_path).first_or_octet_stream();
            return asset_response(file.data.into_owned(), mime.as_ref(), cache);
        }
    }

    match Assets::get("index.html") {
        Some(index) => asset_response(
            index.data.into_owned(),
            "text/html; charset=utf-8",
            "no-cache",
        ),
        None => (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response(),
    }
}

fn asset_response(body: Vec<u8>, content_type: &str, cache_control: &'static str) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}
