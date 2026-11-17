mod common;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use common::{error_code, new_state, request, send};
use sourcelens::http;

#[tokio::test]
async fn healthz_checks_the_database() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let reply = send(&http::router(state), request("GET", "/healthz", None, None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text, "ok");
    assert_eq!(reply.headers[header::X_FRAME_OPTIONS], "DENY");
    // Outside the access log and the request id.
    assert!(!reply.headers.contains_key("x-request-id"));
}

#[tokio::test]
async fn responses_carry_security_headers_and_a_request_id() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state);
    let reply = send(&app, request("GET", "/api/state", None, None)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(reply.headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(reply.headers[header::REFERRER_POLICY], "same-origin");
    assert!(reply.headers.contains_key(header::CONTENT_SECURITY_POLICY));
    assert!(!reply
        .headers
        .contains_key(header::STRICT_TRANSPORT_SECURITY));

    let id = reply.headers["x-request-id"].to_str().unwrap().to_string();
    assert_eq!(id.len(), 16, "{id}");
    assert!(id.bytes().all(|b| b.is_ascii_hexdigit()), "{id}");
    let again = send(&app, request("GET", "/api/state", None, None)).await;
    assert_ne!(again.headers["x-request-id"].to_str().unwrap(), id);
}

#[tokio::test]
async fn https_public_url_adds_hsts() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| {
        cfg.public_url = "https://sourcelens.example.com".into();
    });
    let app = http::router(state);
    for uri in ["/api/state", "/healthz", "/repositories"] {
        let reply = send(&app, request("GET", uri, None, None)).await;
        assert_eq!(
            reply.headers[header::STRICT_TRANSPORT_SECURITY],
            "max-age=31536000",
            "{uri}"
        );
    }
}

#[tokio::test]
async fn unknown_backend_paths_are_json_404() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state);
    for uri in ["/api/nope", "/mcp/x"] {
        let reply = send(&app, request("GET", uri, None, None)).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(error_code(&reply.body), "not_found", "{uri}");
    }

    // Only `/mcp` and paths below it belong to the MCP endpoint; this is a UI route.
    let reply = send(&app, request("GET", "/mcpanything", None, None)).await;
    assert!(
        matches!(
            reply.status,
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
        ),
        "{}",
        reply.status
    );
    assert_ne!(error_code(&reply.body), "not_found");
}

#[tokio::test]
async fn ui_answers_a_matching_etag_with_304() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state);
    let first = send(&app, request("GET", "/", None, None)).await;
    if first.status == StatusCode::SERVICE_UNAVAILABLE {
        // `ui/dist` is not built, so there is nothing to tag.
        return;
    }
    assert_eq!(first.status, StatusCode::OK);
    assert!(!first.text.is_empty());
    let etag = first.headers[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "{etag}");

    for if_none_match in [etag.clone(), format!("\"other\", {etag}"), "*".to_string()] {
        let req = Request::get("/")
            .header(header::IF_NONE_MATCH, &if_none_match)
            .body(Body::empty())
            .unwrap();
        let reply = send(&app, req).await;
        assert_eq!(reply.status, StatusCode::NOT_MODIFIED, "{if_none_match}");
        assert!(reply.text.is_empty(), "{if_none_match}");
        assert_eq!(reply.headers[header::ETAG], etag.as_str());
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-cache");
    }

    let req = Request::get("/")
        .header(header::IF_NONE_MATCH, "\"other\"")
        .body(Body::empty())
        .unwrap();
    let reply = send(&app, req).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.text, first.text);
}
