use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::crypto;
use crate::error::ApiError;

#[derive(rust_embed::RustEmbed)]
#[folder = "ui/dist"]
struct Assets;

const NOT_BUILT: &str = "UI not built. Run: npm --prefix ui ci && npm --prefix ui run build";
const BACKEND_PREFIXES: [&str; 4] = ["/api/", "/auth/", "/webhooks/", "/.well-known/"];

/// Router fallback: embedded UI assets for GET/HEAD, `index.html` for client-side routes.
pub async fn fallback(method: Method, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path();
    if path == "/mcp"
        || path.starts_with("/mcp/")
        || BACKEND_PREFIXES
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
            return asset_response(file, &headers, mime.as_ref(), cache);
        }
    }

    match Assets::get("index.html") {
        Some(index) => asset_response(index, &headers, "text/html; charset=utf-8", "no-cache"),
        None => (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response(),
    }
}

/// The file with its `ETag`, or `304 Not Modified` when the request already holds that version.
fn asset_response(
    file: rust_embed::EmbeddedFile,
    request: &HeaderMap,
    content_type: &str,
    cache_control: &'static str,
) -> Response {
    let etag = format!("\"{}\"", crypto::hex(&file.metadata.sha256_hash()));
    let mut response = if etag_matches(request, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        // Embedded data is `Cow::Borrowed` in release builds; the body then points at it.
        let mut response = Response::new(Body::from(file.data));
        if let Ok(value) = HeaderValue::from_str(content_type) {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        response
    };
    let headers = response.headers_mut();
    if let Ok(value) = HeaderValue::from_str(&etag) {
        headers.insert(header::ETAG, value);
    }
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

/// `If-None-Match` names `etag` (strong or weak), or is `*`.
fn etag_matches(request: &HeaderMap, etag: &str) -> bool {
    request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|tag| tag == "*" || tag == etag || tag.strip_prefix("W/") == Some(etag))
}
