mod common;

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use common::{
    api, credentials, error_code, mcp_request, member_session, new_state, owner_session, request,
    send, serve, Reply, PUBLIC_URL,
};
use reqwest::Url;
use serde_json::{json, Value};
use sourcelens::crypto::pkce_challenge;
use sourcelens::http;
use sourcelens::http::session::OAUTH_COOKIE;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;

const REDIRECT_URI: &str = "http://127.0.0.1:33418/callback";
const VERIFIER: &str = "verifier-0123456789-0123456789-0123456789-0123456789";
const CLIENT_STATE: &str = "client-state-1";

struct Env {
    _tmp: TempDir,
    state: AppState,
    app: Router,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());
    Env {
        _tmp: tmp,
        state,
        app,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn query_param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn location(reply: &Reply) -> &str {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text);
    reply.headers[header::LOCATION].to_str().unwrap()
}

/// `application/x-www-form-urlencoded` encoding of `pairs`.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://form.invalid/").unwrap();
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_string()
}

async fn register(app: &Router, metadata: Value) -> Reply {
    let req = Request::post("/oauth/register")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(metadata.to_string()))
        .unwrap();
    send(app, req).await
}

/// Registers a client with [`REDIRECT_URI`] and returns its `client_id`.
async fn register_client(app: &Router) -> String {
    let reply = register(
        app,
        json!({ "redirect_uris": [REDIRECT_URI], "client_name": "Claude Code" }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    reply.body["client_id"].as_str().unwrap().to_string()
}

async fn token(app: &Router, pairs: &[(&str, &str)]) -> Reply {
    let req = Request::post("/oauth/token")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(form(pairs)))
        .unwrap();
    send(app, req).await
}

/// Authorization request parameters of a valid request by `client_id`.
fn authorize_params(client_id: &str) -> Value {
    json!({
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": REDIRECT_URI,
        "state": CLIENT_STATE,
        "code_challenge": pkce_challenge(VERIFIER),
        "code_challenge_method": "S256",
        "resource": format!("{PUBLIC_URL}/mcp"),
    })
}

/// `GET /api/oauth/authorize` with `params` as query.
async fn authorize_info(app: &Router, cookie: &str, params: &Value) -> (StatusCode, Value) {
    let pairs: Vec<(&str, &str)> = params
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str().unwrap()))
        .collect();
    let uri = format!("/api/oauth/authorize?{}", form(&pairs));
    let (status, body, _) = api(app, "GET", &uri, cookie, None).await;
    (status, body)
}

/// `POST /api/oauth/authorize`: the redirect URL the browser is sent to.
async fn decide(app: &Router, cookie: &str, mut params: Value, approve: bool) -> Url {
    params["approve"] = json!(approve);
    let (status, body, text) = api(app, "POST", "/api/oauth/authorize", cookie, Some(params)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    Url::parse(body["redirectTo"].as_str().unwrap()).unwrap()
}

/// Allows `client_id` as the session's user; returns the authorization code.
async fn allow(app: &Router, cookie: &str, client_id: &str) -> String {
    let url = decide(app, cookie, authorize_params(client_id), true).await;
    assert!(url.as_str().starts_with(REDIRECT_URI), "{url}");
    assert_eq!(query_param(&url, "state").as_deref(), Some(CLIENT_STATE));
    query_param(&url, "code").expect("code")
}

async fn redeem(app: &Router, client_id: &str, code: &str, verifier: &str) -> Reply {
    let resource = format!("{PUBLIC_URL}/mcp");
    token(
        app,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", verifier),
            ("resource", resource.as_str()),
        ],
    )
    .await
}

/// Registers a client, allows it as the session's user and redeems the code: the client id and
/// the token response.
async fn connect(app: &Router, cookie: &str) -> (String, Value) {
    let client_id = register_client(app).await;
    let code = allow(app, cookie, &client_id).await;
    let reply = redeem(app, &client_id, &code, VERIFIER).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    (client_id, reply.body)
}

async fn refresh(app: &Router, client_id: &str, refresh_token: &str) -> Reply {
    token(
        app,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
    .await
}

/// Status of `tools/list` over `/mcp` with `access_token`.
async fn tools_list(app: &Router, access_token: &str) -> StatusCode {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string();
    let req = mcp_request(Some(access_token))
        .body(Body::from(body))
        .unwrap();
    send(app, req).await.status
}

fn oauth_error(reply: &Reply) -> &str {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    reply.body["error"].as_str().unwrap()
}

async fn grants(app: &Router, cookie: &str) -> Vec<Value> {
    let (status, body, text) = api(app, "GET", "/api/oauth/grants", cookie, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    body.as_array().unwrap().clone()
}

// ── Discovery ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn mcp_without_credentials_points_to_the_resource_metadata() {
    let env = env();
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let reply = send(&env.app, mcp_request(None).body(Body::from(body)).unwrap()).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    let challenge = reply.headers[header::WWW_AUTHENTICATE].to_str().unwrap();
    assert!(challenge.starts_with("Bearer "), "{challenge}");
    assert!(
        challenge.contains(
            "resource_metadata=\"http://localhost:3000/.well-known/oauth-protected-resource/mcp\""
        ),
        "{challenge}"
    );
}

#[tokio::test]
async fn metadata_documents_describe_the_server() {
    let env = env();
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let reply = send(&env.app, request("GET", path, None, None)).await;
        assert_eq!(reply.status, StatusCode::OK, "{path}");
        assert_eq!(reply.body["resource"], format!("{PUBLIC_URL}/mcp"));
        assert_eq!(reply.body["authorization_servers"], json!([PUBLIC_URL]));
        assert_eq!(reply.body["scopes_supported"], json!(["mcp"]));
        assert_eq!(reply.body["bearer_methods_supported"], json!(["header"]));
        assert_eq!(reply.body["resource_name"], "SourceLens");
    }

    let reply = send(
        &env.app,
        request("GET", "/.well-known/oauth-authorization-server", None, None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    let meta = &reply.body;
    assert_eq!(meta["issuer"], PUBLIC_URL);
    assert_eq!(
        meta["authorization_endpoint"],
        format!("{PUBLIC_URL}/oauth/authorize")
    );
    assert_eq!(meta["token_endpoint"], format!("{PUBLIC_URL}/oauth/token"));
    assert_eq!(
        meta["registration_endpoint"],
        format!("{PUBLIC_URL}/oauth/register")
    );
    assert_eq!(meta["response_types_supported"], json!(["code"]));
    assert_eq!(
        meta["grant_types_supported"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(
        meta["token_endpoint_auth_methods_supported"],
        json!(["none"])
    );
    assert_eq!(meta["code_challenge_methods_supported"], json!(["S256"]));

    let reply = send(
        &env.app,
        request("GET", "/.well-known/openid-configuration", None, None),
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&reply.body), "not_found");
}

// ── Registration ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn registration_checks_redirect_uris_and_stores_nothing() {
    let env = env();
    for uris in [
        json!(["javascript:alert(1)"]),
        json!(["ms-msdt:/id x"]),
        json!(["search-ms:query=x"]),
        json!(["ms-officecmd:x"]),
        json!(["MS-SETTINGS:privacy"]),
        json!(["view-source:https://example.com/"]),
        json!(["data:text/html,hi"]),
        json!(["microsoft-edge:https://example.com/"]),
        json!(["itms-services://?action=download-manifest&url=https://example.com/a.plist"]),
        json!(["http://example.com/callback"]),
        json!(["https://example.com/callback#fragment"]),
        json!([format!("https://example.com/{}", "a".repeat(520))]),
        json!([]),
        json!([
            "https://a.example/1",
            "https://a.example/2",
            "https://a.example/3",
            "https://a.example/4",
            "https://a.example/5",
            "https://a.example/6"
        ]),
    ] {
        let reply = register(&env.app, json!({ "redirect_uris": uris })).await;
        assert_eq!(oauth_error(&reply), "invalid_redirect_uri", "{uris}");
    }
    let reply = register(&env.app, json!({ "client_name": "No URIs" })).await;
    assert_eq!(oauth_error(&reply), "invalid_redirect_uri");

    let reply = register(
        &env.app,
        json!({
            "redirect_uris": [REDIRECT_URI, "http://localhost:8080/cb", "cursor://anysphere.cursor-mcp/oauth/callback"],
            "client_name": format!("  {}  ", "x".repeat(100)),
        }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let client = &reply.body;
    assert!(client["client_id"].as_str().unwrap().starts_with("slc_"));
    assert_eq!(client["client_name"], "x".repeat(80));
    assert_eq!(client["redirect_uris"][0], REDIRECT_URI);
    assert_eq!(client["token_endpoint_auth_method"], "none");
    assert_eq!(
        client["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(client["response_types"], json!(["code"]));
    assert!(client["client_id_issued_at"].as_i64().unwrap() <= now());

    let reply = register(&env.app, json!({ "redirect_uris": [REDIRECT_URI] })).await;
    assert_eq!(reply.body["client_name"], "MCP client");

    let grants = common::scalar(&env.state, "SELECT count(*) FROM oauth_grants").await;
    assert_eq!(grants, 0);
}

// ── Authorization ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn consent_page_separates_page_errors_from_redirect_errors() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;

    let (status, info) = authorize_info(&env.app, &owner, &authorize_params(&client_id)).await;
    assert_eq!(status, StatusCode::OK, "{info}");
    assert_eq!(info["clientName"], "Claude Code");
    assert_eq!(info["redirectTarget"], "127.0.0.1:33418");
    assert_eq!(info["redirectLocal"], true);
    assert_eq!(info["error"], Value::Null);
    assert_eq!(info["redirectTo"], Value::Null);

    let (status, _, _) = api(
        &env.app,
        "GET",
        "/api/oauth/authorize?response_type=code",
        "not-a-session",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let mut forged = authorize_params(&client_id);
    forged["client_id"] = json!("slc_Zm9yZ2Vk");
    let (status, body) = authorize_info(&env.app, &owner, &forged).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "invalid_client");

    let mut elsewhere = authorize_params(&client_id);
    elsewhere["redirect_uri"] = json!("https://attacker.example/callback");
    let (status, body) = authorize_info(&env.app, &owner, &elsewhere).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "invalid_redirect_uri");
    let (status, body, _) = api(
        &env.app,
        "POST",
        "/api/oauth/authorize",
        &owner,
        Some({
            let mut params = elsewhere.clone();
            params["approve"] = json!(true);
            params
        }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "invalid_redirect_uri");

    for (field, value, error) in [
        ("response_type", json!("token"), "unsupported_response_type"),
        ("code_challenge_method", json!("plain"), "invalid_request"),
        ("code_challenge", json!("too-short"), "invalid_request"),
        (
            "resource",
            json!("https://other.example/mcp"),
            "invalid_target",
        ),
    ] {
        let mut params = authorize_params(&client_id);
        params[field] = value;
        let (status, info) = authorize_info(&env.app, &owner, &params).await;
        assert_eq!(status, StatusCode::OK, "{field}: {info}");
        assert_eq!(info["clientName"], "Claude Code");
        assert_eq!(info["redirectTarget"], "127.0.0.1:33418");
        assert!(info["error"].is_string(), "{field}: {info}");
        let to = Url::parse(info["redirectTo"].as_str().unwrap()).unwrap();
        assert!(to.as_str().starts_with(REDIRECT_URI), "{to}");
        assert_eq!(query_param(&to, "error").as_deref(), Some(error), "{field}");
        assert!(query_param(&to, "error_description").is_some());
        assert_eq!(query_param(&to, "state").as_deref(), Some(CLIENT_STATE));
    }

    // The resource may carry a trailing slash.
    let mut params = authorize_params(&client_id);
    params["resource"] = json!(format!("{PUBLIC_URL}/mcp/"));
    let (_, info) = authorize_info(&env.app, &owner, &params).await;
    assert_eq!(info["clientName"], "Claude Code");

    let denied = decide(&env.app, &owner, authorize_params(&client_id), false).await;
    assert!(denied.as_str().starts_with(REDIRECT_URI), "{denied}");
    assert_eq!(
        query_param(&denied, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(query_param(&denied, "state").as_deref(), Some(CLIENT_STATE));
    assert_eq!(query_param(&denied, "code"), None);
}

#[tokio::test]
async fn request_errors_of_an_attacker_client_wait_for_the_user() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let evil = "https://evil.example/cb";
    let reply = register(
        &env.app,
        json!({ "redirect_uris": [evil], "client_name": "Evil" }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
    let mut params = authorize_params(reply.body["client_id"].as_str().unwrap());
    params["redirect_uri"] = json!(evil);
    params["response_type"] = json!("x");

    // The server answers JSON, not a redirect; the page shows the error and a button.
    let (status, info) = authorize_info(&env.app, &owner, &params).await;
    assert_eq!(status, StatusCode::OK, "{info}");
    assert_eq!(info["clientName"], "Evil");
    assert_eq!(info["redirectTarget"], "evil.example");
    assert_eq!(info["redirectLocal"], false);
    assert_eq!(info["error"], "Only response_type=code is supported.");
    let to = Url::parse(info["redirectTo"].as_str().unwrap()).unwrap();
    assert!(to.as_str().starts_with(evil), "{to}");
    assert_eq!(
        query_param(&to, "error").as_deref(),
        Some("unsupported_response_type")
    );
    assert_eq!(query_param(&to, "state").as_deref(), Some(CLIENT_STATE));
}

#[tokio::test]
async fn consent_page_names_where_the_browser_returns() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    for (uri, target) in [
        (
            "cursor://anysphere.cursor-retrieval/oauth/callback",
            "cursor://anysphere.cursor-retrieval",
        ),
        ("https://app.example:8443/cb", "app.example:8443"),
        ("https://app.example/cb", "app.example"),
    ] {
        let reply = register(&env.app, json!({ "redirect_uris": [uri] })).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{uri}: {}", reply.text);
        let mut params = authorize_params(reply.body["client_id"].as_str().unwrap());
        params["redirect_uri"] = json!(uri);
        let (status, info) = authorize_info(&env.app, &owner, &params).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {info}");
        assert_eq!(info["redirectTarget"], target, "{uri}");
        assert_eq!(info["redirectLocal"], false, "{uri}");
        assert_eq!(info["redirectTo"], Value::Null, "{uri}");
    }
}

#[tokio::test]
async fn deciding_requires_the_web_app_origin() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;
    let mut body = authorize_params(&client_id);
    body["approve"] = json!(true);
    let req = Request::post("/api/oauth/authorize")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, format!("sl_session={owner}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let reply = send(&env.app, req).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&reply.body), "origin_not_allowed");
}

// ── Token endpoint ────────────────────────────────────────────────────────────

#[tokio::test]
async fn full_flow_gives_access_to_mcp() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;
    let code = allow(&env.app, &owner, &client_id).await;

    let reply = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let tokens = &reply.body;
    assert_eq!(tokens["token_type"], "Bearer");
    assert_eq!(tokens["expires_in"], 3600);
    assert_eq!(tokens["scope"], "mcp");
    let access = tokens["access_token"].as_str().unwrap();
    let refresh_token = tokens["refresh_token"].as_str().unwrap();
    assert!(access.starts_with("slo_"), "{access}");
    assert!(refresh_token.starts_with("slr_"), "{refresh_token}");

    assert_eq!(tools_list(&env.app, access).await, StatusCode::OK);
    assert_eq!(
        tools_list(&env.app, "slo_0000").await,
        StatusCode::UNAUTHORIZED
    );

    // Only hashes are stored.
    let access_hash = sourcelens::crypto::sha256_hex(access.as_bytes());
    let stored: String = env
        .state
        .db
        .read(|conn| conn.query_row("SELECT access_hash FROM oauth_grants", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(stored, access_hash);

    // A code is spent by its first use.
    let reused = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(oauth_error(&reused), "invalid_grant");
    assert_eq!(reused.headers[header::CACHE_CONTROL], "no-store");

    let listed = grants(&env.app, &owner).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["clientName"], "Claude Code");
    assert_eq!(listed[0]["user"], "octocat");
}

#[tokio::test]
async fn wrong_verifier_or_client_spends_the_code() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;

    let code = allow(&env.app, &owner, &client_id).await;
    let reply = redeem(
        &env.app,
        &client_id,
        &code,
        "another-verifier-0123456789-0123456789-01",
    )
    .await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
    let reply = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");

    let other_client = register_client(&env.app).await;
    let code = allow(&env.app, &owner, &client_id).await;
    let reply = redeem(&env.app, &other_client, &code, VERIFIER).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");

    let code = allow(&env.app, &owner, &client_id).await;
    let reply = token(
        &env.app,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id.as_str()),
            ("redirect_uri", REDIRECT_URI),
            ("code_verifier", VERIFIER),
            ("resource", "https://other.example/mcp"),
        ],
    )
    .await;
    assert_eq!(oauth_error(&reply), "invalid_target");

    assert_eq!(
        common::scalar(&env.state, "SELECT count(*) FROM oauth_grants").await,
        0
    );
}

#[tokio::test]
async fn token_endpoint_rejects_unknown_grants_and_bodies() {
    let env = env();
    let reply = token(&env.app, &[("grant_type", "password")]).await;
    assert_eq!(oauth_error(&reply), "unsupported_grant_type");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");

    let req = Request::post("/oauth/token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({ "grant_type": "refresh_token" }).to_string(),
        ))
        .unwrap();
    let reply = send(&env.app, req).await;
    assert_eq!(oauth_error(&reply), "invalid_request");
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");

    let reply = refresh(&env.app, "slc_unknown", "slr_unknown").await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
}

#[tokio::test]
async fn refresh_rotates_both_tokens() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (client_id, first) = connect(&env.app, &owner).await;
    let old_access = first["access_token"].as_str().unwrap();
    let old_refresh = first["refresh_token"].as_str().unwrap();

    let reply = refresh(&env.app, "slc_another-client", old_refresh).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");

    let reply = refresh(&env.app, &client_id, old_refresh).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    let new_access = reply.body["access_token"].as_str().unwrap();
    let new_refresh = reply.body["refresh_token"].as_str().unwrap();
    assert!(new_access.starts_with("slo_") && new_access != old_access);
    assert!(new_refresh.starts_with("slr_") && new_refresh != old_refresh);
    assert_eq!(reply.body["expires_in"], 3600);

    assert_eq!(
        tools_list(&env.app, old_access).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(tools_list(&env.app, new_access).await, StatusCode::OK);

    // An expired access token is refused; the refresh token still works.
    env.state
        .db
        .write(|conn| conn.execute("UPDATE oauth_grants SET access_expires_at = 0", []))
        .await
        .unwrap();
    assert_eq!(
        tools_list(&env.app, new_access).await,
        StatusCode::UNAUTHORIZED
    );
    let reply = refresh(&env.app, &client_id, new_refresh).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let newest = reply.body["access_token"].as_str().unwrap();
    assert_eq!(tools_list(&env.app, newest).await, StatusCode::OK);
}

#[tokio::test]
async fn expired_authorization_code_is_refused() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;
    let code = allow(&env.app, &owner, &client_id).await;
    for pending in env.state.oauth_codes.lock().unwrap().values_mut() {
        pending.expires_at = now() - 1;
    }
    let reply = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
    assert_eq!(
        common::scalar(&env.state, "SELECT count(*) FROM oauth_grants").await,
        0
    );
}

#[tokio::test]
async fn authorization_code_is_bound_to_the_account() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let client_id = register_client(&env.app).await;
    let code = allow(&env.app, &owner, &client_id).await;
    // Another account under the same id, as after removing a user and adding the next one.
    env.state
        .db
        .write(|conn| {
            conn.execute(
                "UPDATE users SET created_at = created_at + 1 WHERE login = 'octocat'",
                [],
            )
        })
        .await
        .unwrap();
    let reply = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
}

#[tokio::test]
async fn reusing_a_replaced_refresh_token_revokes_the_grant() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (client_id, first) = connect(&env.app, &owner).await;
    let old_refresh = first["refresh_token"].as_str().unwrap();
    let reply = refresh(&env.app, &client_id, old_refresh).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let new_access = reply.body["access_token"].as_str().unwrap();
    let new_refresh = reply.body["refresh_token"].as_str().unwrap();
    assert_eq!(tools_list(&env.app, new_access).await, StatusCode::OK);

    // Presented by another client, the old token matches nothing and changes nothing.
    let reply = refresh(&env.app, "slc_another-client", old_refresh).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
    assert_eq!(tools_list(&env.app, new_access).await, StatusCode::OK);

    let reply = refresh(&env.app, &client_id, old_refresh).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
    assert_eq!(
        tools_list(&env.app, new_access).await,
        StatusCode::UNAUTHORIZED
    );
    let reply = refresh(&env.app, &client_id, new_refresh).await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
    assert!(grants(&env.app, &owner).await.is_empty());
}

#[tokio::test]
async fn expired_refresh_token_is_refused() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (client_id, tokens) = connect(&env.app, &owner).await;
    env.state
        .db
        .write(|conn| conn.execute("UPDATE oauth_grants SET refresh_expires_at = 0", []))
        .await
        .unwrap();
    let reply = refresh(
        &env.app,
        &client_id,
        tokens["refresh_token"].as_str().unwrap(),
    )
    .await;
    assert_eq!(oauth_error(&reply), "invalid_grant");
}

#[tokio::test]
async fn connecting_the_same_client_again_replaces_its_grant() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (client_id, first) = connect(&env.app, &owner).await;
    let code = allow(&env.app, &owner, &client_id).await;
    let reply = redeem(&env.app, &client_id, &code, VERIFIER).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);

    assert_eq!(grants(&env.app, &owner).await.len(), 1);
    let old_access = first["access_token"].as_str().unwrap();
    assert_eq!(
        tools_list(&env.app, old_access).await,
        StatusCode::UNAUTHORIZED
    );
}

// ── Grants ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn revoking_a_grant_locks_the_client_out() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (_, tokens) = connect(&env.app, &owner).await;
    let access = tokens["access_token"].as_str().unwrap();
    assert_eq!(tools_list(&env.app, access).await, StatusCode::OK);

    let id = grants(&env.app, &owner).await[0]["id"].as_i64().unwrap();
    let uri = format!("/api/oauth/grants/{id}");
    let (status, _, text) = api(&env.app, "DELETE", &uri, &owner, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{text}");
    assert_eq!(tools_list(&env.app, access).await, StatusCode::UNAUTHORIZED);
    assert!(grants(&env.app, &owner).await.is_empty());

    let (status, body, _) = api(&env.app, "DELETE", &uri, &owner, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "grant_not_found");
}

#[tokio::test]
async fn members_see_their_own_grants_and_admins_all() {
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let member = member_session(&env.state, "mona").await;
    connect(&env.app, &owner).await;
    let (_, member_tokens) = connect(&env.app, &member).await;

    let own = grants(&env.app, &member).await;
    assert_eq!(own.len(), 1);
    assert_eq!(own[0]["user"], "mona");
    assert!(own[0]["createdAt"].as_i64().is_some());

    let all = grants(&env.app, &owner).await;
    assert_eq!(all.len(), 2);
    let owner_grant = all.iter().find(|grant| grant["user"] == "octocat").unwrap();
    let member_grant = all.iter().find(|grant| grant["user"] == "mona").unwrap();

    let uri = format!("/api/oauth/grants/{}", owner_grant["id"]);
    let (status, body, _) = api(&env.app, "DELETE", &uri, &member, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "grant_not_found");
    assert_eq!(grants(&env.app, &owner).await.len(), 2);

    let uri = format!("/api/oauth/grants/{}", member_grant["id"]);
    let (status, _, _) = api(&env.app, "DELETE", &uri, &owner, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(grants(&env.app, &member).await.is_empty());
    let access = member_tokens["access_token"].as_str().unwrap();
    assert_eq!(tools_list(&env.app, access).await, StatusCode::UNAUTHORIZED);
}

// ── Returning to the consent page after sign-in ───────────────────────────────

/// The payload of the sign-in cookie the response sets.
fn sign_in_cookie(state: &AppState, reply: &Reply) -> (String, Value) {
    let value = reply
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()?
                .strip_prefix(&format!("{OAUTH_COOKIE}="))
                .map(str::to_string)
        })
        .expect("sign-in cookie");
    let sealed = URL_SAFE_NO_PAD.decode(&value).unwrap();
    let payload = serde_json::from_slice(&state.key.open(&sealed).unwrap()).unwrap();
    (value, payload)
}

fn login_uri(path: &str, next: &str) -> String {
    format!("{path}?{}", form(&[("next", next)]))
}

const CONSENT: &str = "/oauth/authorize?client_id=slc_abc&state=xyz";

#[tokio::test]
async fn github_sign_in_keeps_only_a_consent_page_next() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
    let app = http::router(state.clone());

    let reply = send(
        &app,
        request("GET", &login_uri("/auth/github/login", CONSENT), None, None),
    )
    .await;
    assert!(location(&reply).starts_with("https://github.com/login/oauth/authorize?"));
    assert_eq!(sign_in_cookie(&state, &reply).1["next"], CONSENT);

    for next in [
        "/repositories",
        "https://evil.example/oauth/authorize?x=1",
        "//evil.example/oauth/authorize?x=1",
        "/oauth/authorize?x=1\r\nSet-Cookie: a=b",
    ] {
        let reply = send(
            &app,
            request("GET", &login_uri("/auth/github/login", next), None, None),
        )
        .await;
        assert_eq!(
            sign_in_cookie(&state, &reply).1["next"],
            Value::Null,
            "{next}"
        );
    }
}

/// A fake OpenID provider: discovery and a token endpoint answering with `id_token`.
#[derive(Default)]
struct Idp {
    base: OnceLock<String>,
    id_token: Mutex<String>,
}

async fn spawn_idp() -> Arc<Idp> {
    let idp = Arc::new(Idp::default());
    let router = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(|State(idp): State<Arc<Idp>>| async move {
                let base = idp.base.get().unwrap();
                Json(json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/authorize"),
                    "token_endpoint": format!("{base}/token"),
                }))
            }),
        )
        .route(
            "/token",
            post(|State(idp): State<Arc<Idp>>| async move {
                Json(json!({
                    "access_token": "idp-access-token",
                    "token_type": "Bearer",
                    "id_token": idp.id_token.lock().unwrap().clone(),
                }))
            }),
        )
        .with_state(idp.clone());
    idp.base.set(serve(router).await).unwrap();
    idp
}

#[tokio::test]
async fn sso_sign_in_returns_to_the_consent_page() {
    let idp = spawn_idp().await;
    let issuer = idp.base.get().unwrap().clone();
    let env = env();
    let owner = owner_session(&env.state, "octocat").await;
    let (status, body, _) = api(
        &env.app,
        "PUT",
        "/api/settings/sso",
        &owner,
        Some(json!({
            "enabled": true,
            "name": "Acme",
            "issuer": issuer,
            "clientId": "sl-client",
            "clientSecret": "s3cret",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sso_issuer = issuer.clone();
    env.state
        .db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO users (login, role, status, sso_issuer, sso_sub, created_at)
                 VALUES ('ada', 'member', 'active', ?1, 'ada', ?2)",
                rusqlite::params![sso_issuer, now()],
            )
        })
        .await
        .unwrap();

    for (next, landing) in [
        (CONSENT, CONSENT),
        ("/settings/keys", "/repositories"),
        ("https://evil.example/oauth/authorize?x=1", "/repositories"),
    ] {
        let reply = send(
            &env.app,
            request("GET", &login_uri("/auth/sso/login", next), None, None),
        )
        .await;
        let provider = Url::parse(location(&reply)).unwrap();
        let (cookie, _) = sign_in_cookie(&env.state, &reply);
        let claims = json!({
            "iss": issuer,
            "sub": "ada",
            "aud": "sl-client",
            "exp": now() + 300,
            "nonce": query_param(&provider, "nonce").unwrap(),
        });
        *idp.id_token.lock().unwrap() = format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(json!({ "alg": "RS256" }).to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let oauth_state = query_param(&provider, "state").unwrap();
        let req = Request::builder()
            .uri(format!(
                "/auth/sso/callback?code=the-code&state={oauth_state}"
            ))
            .header(header::COOKIE, format!("{OAUTH_COOKIE}={cookie}"))
            .body(Body::empty())
            .unwrap();
        let reply = send(&env.app, req).await;
        assert_eq!(location(&reply), landing, "{next}");
    }
}
