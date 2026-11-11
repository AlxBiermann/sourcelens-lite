use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde_json::{json, Value};
use sourcelens::config::Config;
use sourcelens::github::app::{AppCredentials, GitHub};
use sourcelens::github::oauth::{resolve_user, GhToken, GhUser, Resolution};
use sourcelens::http;
use sourcelens::http::session::create_session;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

const PUBLIC_URL: &str = "http://localhost:3000";

fn test_credentials() -> AppCredentials {
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_only_app_key.pem"
    ))
    .unwrap();
    AppCredentials {
        app_id: 4242,
        slug: "sourcelens-test".into(),
        client_id: "Iv1.test".into(),
        client_secret: "client-secret".into(),
        webhook_secret: Some("hook-secret".into()),
        private_key_pem: pem,
        html_url: "https://github.com/apps/sourcelens-test".into(),
        source: "env",
    }
}

fn new_state(tmp: &TempDir, configure: impl FnOnce(&mut Config)) -> AppState {
    let mut cfg = Config::new(PUBLIC_URL, tmp.path());
    configure(&mut cfg);
    AppState::new(cfg).unwrap()
}

fn request(method: &str, uri: &str, cookie: Option<&str>, body: Option<Value>) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if method != "GET" {
        builder = builder.header(header::ORIGIN, PUBLIC_URL);
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("sl_session={cookie}"));
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = http::router(state.clone()).oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, headers, body)
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

async fn insert_user(state: &AppState, login: &str, role: &str, github_id: Option<i64>) -> i64 {
    let login = login.to_string();
    let role = role.to_string();
    state
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO users (github_id, login, role, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![github_id, login, role, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
        .unwrap()
}

async fn session_for(state: &AppState, user_id: i64) -> String {
    let token = GhToken {
        access_token: "gho_test".into(),
        refresh_token: None,
        expires_at: i64::MAX,
    };
    create_session(state, user_id, &token).await.unwrap()
}

async fn count(state: &AppState, sql: &'static str) -> i64 {
    state
        .db
        .read(move |c| c.query_row(sql, [], |r| r.get(0)))
        .await
        .unwrap()
}

// ── /api/state ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn state_of_fresh_instance() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let (status, _, body) = call(&state, request("GET", "/api/state", None, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["publicUrl"], PUBLIC_URL);
    assert_eq!(body["setup"]["appConfigured"], false);
    assert_eq!(body["setup"]["ownerExists"], false);
    assert_eq!(body["setup"]["appSlug"], Value::Null);
    assert_eq!(body["setup"]["installUrl"], Value::Null);
    assert_eq!(body["user"], Value::Null);
}

#[tokio::test]
async fn state_with_env_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(test_credentials()));
    let (_, _, body) = call(&state, request("GET", "/api/state", None, None)).await;
    assert_eq!(body["setup"]["appConfigured"], true);
    assert_eq!(body["setup"]["appSlug"], "sourcelens-test");
    assert_eq!(
        body["setup"]["installUrl"],
        "https://github.com/apps/sourcelens-test/installations/new"
    );
}

#[tokio::test]
async fn state_with_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;
    let (_, _, body) = call(&state, request("GET", "/api/state", Some(&cookie), None)).await;
    assert_eq!(body["setup"]["ownerExists"], true);
    assert_eq!(body["user"]["id"], owner);
    assert_eq!(body["user"]["login"], "octocat");
    assert_eq!(body["user"]["role"], "owner");
}

// ── Manifest flow ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn manifest_requires_the_setup_token() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/setup/manifest",
            None,
            Some(json!({ "setupToken": "wrong" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "invalid_setup_token");
}

#[tokio::test]
async fn manifest_for_personal_account_and_organization() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let token = state.setup_token.to_string();

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/setup/manifest",
            None,
            Some(json!({ "setupToken": token })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let post_url = body["postUrl"].as_str().unwrap();
    assert!(
        post_url.starts_with("https://github.com/settings/apps/new?state="),
        "{post_url}"
    );
    let manifest = &body["manifest"];
    assert_eq!(manifest["hook_attributes"]["active"], false);
    assert_eq!(
        manifest["hook_attributes"]["url"],
        format!("{PUBLIC_URL}/webhooks/github")
    );
    assert_eq!(
        manifest["redirect_url"],
        format!("{PUBLIC_URL}/auth/github/app-created")
    );
    assert_eq!(
        manifest["callback_urls"],
        json!([format!("{PUBLIC_URL}/auth/github/callback")])
    );
    assert_eq!(manifest["default_permissions"]["contents"], "read");

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/setup/manifest",
            None,
            Some(json!({ "setupToken": token, "organization": "acme" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["postUrl"]
        .as_str()
        .unwrap()
        .starts_with("https://github.com/organizations/acme/settings/apps/new?state="));
}

#[tokio::test]
async fn manifest_webhook_is_active_for_a_public_host() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::new("https://sourcelens.example.com", tmp.path());
    let state = AppState::new(cfg).unwrap();
    let token = state.setup_token.to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/api/setup/manifest")
        .header(header::ORIGIN, "https://sourcelens.example.com")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(json!({ "setupToken": token }).to_string()))
        .unwrap();
    let (status, _, body) = call(&state, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["manifest"]["hook_attributes"]["active"], true);
}

#[tokio::test]
async fn manifest_rejected_when_app_configured_or_owner_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(test_credentials()));
    let token = state.setup_token.to_string();
    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/setup/manifest",
            None,
            Some(json!({ "setupToken": token })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "app_already_configured");

    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    insert_user(&state, "octocat", "owner", Some(1)).await;
    let token = state.setup_token.to_string();
    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/setup/manifest",
            None,
            Some(json!({ "setupToken": token })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "invalid_setup_token");
}

#[tokio::test]
async fn app_created_with_unknown_state_redirects_to_setup() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let (status, headers, _) = call(
        &state,
        request(
            "GET",
            "/auth/github/app-created?code=abc&state=nope",
            None,
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/setup?error=state");
}

#[tokio::test]
async fn manifest_credentials_round_trip_through_settings() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    assert!(state.github.credentials().await.unwrap().is_none());
    let mut creds = test_credentials();
    creds.source = "manifest";
    state
        .github
        .store_manifest_credentials(&creds)
        .await
        .unwrap();
    let loaded = state.github.credentials().await.unwrap().unwrap();
    assert_eq!(loaded.app_id, 4242);
    assert_eq!(loaded.client_secret, "client-secret");
    assert_eq!(loaded.source, "manifest");
}

// ── Sign-in ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn login_redirects_to_setup_or_github() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let (status, headers, _) = call(&state, request("GET", "/auth/github/login", None, None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/setup");

    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(test_credentials()));
    let (status, headers, _) = call(&state, request("GET", "/auth/github/login", None, None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let location = headers[header::LOCATION].to_str().unwrap();
    assert!(location.starts_with("https://github.com/login/oauth/authorize?"));
    assert!(location.contains("client_id=Iv1.test"));
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("sl_oauth="));
    assert!(cookie.contains("HttpOnly"));
}

#[tokio::test]
async fn callback_with_mismatched_state_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(test_credentials()));
    let (_, headers, _) = call(&state, request("GET", "/auth/github/login", None, None)).await;
    let oauth_cookie = headers[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let req = Request::builder()
        .uri("/auth/github/callback?code=abc&state=forged")
        .header(header::COOKIE, oauth_cookie)
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = call(&state, req).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/login?error=state");
}

fn gh_user(id: i64, login: &str) -> GhUser {
    GhUser {
        id,
        login: login.into(),
        name: Some(format!("{login} name")),
        avatar_url: Some(format!("https://avatars.example/{id}")),
    }
}

#[tokio::test]
async fn user_resolution_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});

    // No owner, no claim → setup required.
    assert_eq!(
        resolve_user(&state.db, &gh_user(1, "first"), false)
            .await
            .unwrap(),
        Resolution::SetupRequired
    );
    // No owner, claim → becomes owner.
    let Resolution::User(owner) = resolve_user(&state.db, &gh_user(1, "first"), true)
        .await
        .unwrap()
    else {
        panic!("first user with claim must become owner");
    };
    let role: String = state
        .db
        .read(move |c| {
            c.query_row("SELECT role FROM users WHERE id = ?1", [owner], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(role, "owner");

    // Known github_id → profile fields updated.
    let renamed = GhUser {
        id: 1,
        login: "renamed".into(),
        name: Some("New Name".into()),
        avatar_url: Some("https://avatars.example/new".into()),
    };
    assert_eq!(
        resolve_user(&state.db, &renamed, false).await.unwrap(),
        Resolution::User(owner)
    );
    let (login, name, avatar): (String, String, String) = state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT login, name, avatar_url FROM users WHERE id = ?1",
                [owner],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        (login.as_str(), name.as_str(), avatar.as_str()),
        ("renamed", "New Name", "https://avatars.example/new")
    );

    // Invited login (case-insensitive) → github_id bound.
    let invited = insert_user(&state, "Invitee", "member", None).await;
    assert_eq!(
        resolve_user(&state.db, &gh_user(77, "invitee"), false)
            .await
            .unwrap(),
        Resolution::User(invited)
    );
    let github_id: Option<i64> = state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT github_id FROM users WHERE id = ?1",
                [invited],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(github_id, Some(77));

    // Unknown user with an owner present → not invited, even with a claim.
    assert_eq!(
        resolve_user(&state.db, &gh_user(99, "stranger"), true)
            .await
            .unwrap(),
        Resolution::NotInvited
    );
    assert_eq!(count(&state, "SELECT count(*) FROM users").await, 2);
}

// ── Sessions and Origin ───────────────────────────────────────────────────────

#[tokio::test]
async fn session_required_and_expiry_enforced() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;

    let (status, _, body) = call(&state, request("GET", "/api/members", None, None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "unauthenticated");

    let cookie = session_for(&state, owner).await;
    let (status, _, _) = call(&state, request("GET", "/api/members", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::OK);

    state
        .db
        .write(|c| c.execute("UPDATE sessions SET expires_at = ?1", [now() - 1]))
        .await
        .unwrap();
    let (status, _, body) = call(&state, request("GET", "/api/members", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(error_code(&body), "unauthenticated");
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;

    let (status, headers, _) =
        call(&state, request("POST", "/api/logout", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let set_cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(set_cookie.starts_with("sl_session=;"));
    assert!(set_cookie.contains("Max-Age=0"));

    let (status, _, _) = call(&state, request("GET", "/api/members", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(count(&state, "SELECT count(*) FROM sessions").await, 0);
}

#[tokio::test]
async fn origin_is_checked_for_state_changing_api_requests() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;

    let without_origin = Request::builder()
        .method("POST")
        .uri("/api/logout")
        .header(header::COOKIE, format!("sl_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&state, without_origin).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "origin_not_allowed");

    let foreign = Request::builder()
        .method("POST")
        .uri("/api/logout")
        .header(header::ORIGIN, "https://evil.example")
        .header(header::COOKIE, format!("sl_session={cookie}"))
        .body(Body::empty())
        .unwrap();
    let (status, _, body) = call(&state, foreign).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "origin_not_allowed");
    assert_eq!(count(&state, "SELECT count(*) FROM sessions").await, 1);

    let (status, _, _) = call(&state, request("POST", "/api/logout", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ── Members ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn owner_manages_members() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/members",
            Some(&cookie),
            Some(json!({ "login": "hubot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["login"], "hubot");
    assert_eq!(body["role"], "member");
    assert_eq!(body["pending"], true);

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/members",
            Some(&cookie),
            Some(json!({ "login": "HUBOT" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "member_exists");

    for bad in ["not a login", "", "under_score", &"x".repeat(40)] {
        let (status, _, body) = call(
            &state,
            request(
                "POST",
                "/api/members",
                Some(&cookie),
                Some(json!({ "login": bad })),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(error_code(&body), "invalid_login");
    }

    let (status, _, body) = call(&state, request("GET", "/api/members", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::OK);
    let logins: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["login"].as_str().unwrap())
        .collect();
    assert_eq!(logins, ["octocat", "hubot"]);
    assert_eq!(body[0]["pending"], false);

    let (status, _, body) = call(
        &state,
        request(
            "DELETE",
            &format!("/api/members/{owner}"),
            Some(&cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "cannot_remove_owner");
}

#[tokio::test]
async fn members_cannot_manage_members_and_removal_ends_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let member = insert_user(&state, "hubot", "member", Some(2)).await;
    let owner_cookie = session_for(&state, owner).await;
    let member_cookie = session_for(&state, member).await;

    let (status, _, body) = call(
        &state,
        request("GET", "/api/members", Some(&member_cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "owner_required");
    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/members",
            Some(&member_cookie),
            Some(json!({ "login": "someone" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "owner_required");
    let (status, _, body) = call(
        &state,
        request(
            "DELETE",
            &format!("/api/members/{owner}"),
            Some(&member_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "owner_required");

    let (status, _, _) = call(
        &state,
        request(
            "DELETE",
            &format!("/api/members/{member}"),
            Some(&owner_cookie),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let remaining: i64 = state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM sessions WHERE user_id = ?1",
                [member],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    let (status, _, _) = call(
        &state,
        request("GET", "/api/state", Some(&member_cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = call(
        &state,
        request("GET", "/api/state", Some(&member_cookie), None),
    )
    .await;
    assert_eq!(body["user"], Value::Null);
}

// ── App JWT ───────────────────────────────────────────────────────────────────

#[test]
fn app_jwt_is_rs256_with_string_issuer() {
    let jwt = GitHub::app_jwt(&test_credentials()).unwrap();
    let segments: Vec<&str> = jwt.split('.').collect();
    assert_eq!(segments.len(), 3);
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[0]).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(segments[1]).unwrap()).unwrap();
    assert_eq!(claims["iss"], "4242");
    let iat = claims["iat"].as_i64().unwrap();
    let exp = claims["exp"].as_i64().unwrap();
    assert_eq!(exp - iat, 600);
    assert!(iat <= now() && exp > now());
    assert!(!segments[2].is_empty());
}

// ── Fake GitHub ───────────────────────────────────────────────────────────────

#[derive(Default)]
struct Counters {
    token_requests: AtomicUsize,
    flaky_requests: AtomicUsize,
}

async fn spawn_fake_github() -> (String, Arc<Counters>) {
    let counters = Arc::new(Counters::default());
    let app = Router::new()
        .route(
            "/app/installations/{id}/access_tokens",
            post(
                |State(c): State<Arc<Counters>>, Path(id): Path<u64>, headers: HeaderMap| async move {
                    c.token_requests.fetch_add(1, Ordering::SeqCst);
                    let auth = headers
                        .get(header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let jwt = auth.strip_prefix("Bearer ").unwrap_or("");
                    if jwt.split('.').count() != 3 {
                        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Bad credentials" })))
                            .into_response();
                    }
                    (
                        StatusCode::CREATED,
                        Json(json!({ "token": format!("ghs_installation_{id}"), "expires_at": "2099-01-01T00:00:00Z" })),
                    )
                        .into_response()
                },
            ),
        )
        .route(
            "/missing",
            get(|| async { (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" }))) }),
        )
        .route(
            "/limited",
            get(|| async {
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .header("x-ratelimit-remaining", "0")
                    .header("x-ratelimit-reset", (now() + 30).to_string())
                    .body(Body::from(r#"{"message":"API rate limit exceeded"}"#))
                    .unwrap()
            }),
        )
        .route(
            "/flaky",
            get(|State(c): State<Arc<Counters>>| async move {
                if c.flaky_requests.fetch_add(1, Ordering::SeqCst) == 0 {
                    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "message": "Unavailable" })))
                } else {
                    (StatusCode::OK, Json(json!({ "ok": true })))
                }
            }),
        )
        .route(
            "/headers",
            get(|headers: HeaderMap| async move {
                let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
                Json(json!({ "accept": get("accept"), "apiVersion": get("x-github-api-version") }))
            }),
        )
        .with_state(counters.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), counters)
}

#[tokio::test]
async fn installation_token_is_cached() {
    let (base, counters) = spawn_fake_github().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| {
        cfg.github = Some(test_credentials());
        cfg.github_api_url = base.clone();
    });
    let first = state.github.installation_token(7).await.unwrap();
    let second = state.github.installation_token(7).await.unwrap();
    assert_eq!(first, "ghs_installation_7");
    assert_eq!(second, first);
    assert_eq!(counters.token_requests.load(Ordering::SeqCst), 1);

    state.github.forget_installation_token(7);
    state.github.installation_token(7).await.unwrap();
    assert_eq!(counters.token_requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn send_maps_github_responses() {
    let (base, counters) = spawn_fake_github().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github_api_url = base.clone());
    let gh = &state.github;

    let err = gh
        .send(|| gh.http().get(format!("{base}/missing")))
        .await
        .unwrap_err();
    assert_eq!(err.code, "github_error");
    assert_eq!(err.upstream_status, Some(404));

    let err = gh
        .send(|| gh.http().get(format!("{base}/limited")))
        .await
        .unwrap_err();
    assert_eq!(err.code, "github_rate_limited");
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.upstream_status, Some(403));

    let response = gh
        .send(|| gh.http().get(format!("{base}/flaky")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(counters.flaky_requests.load(Ordering::SeqCst), 2);

    let defaults: Value = gh
        .send(|| gh.http().get(format!("{base}/headers")))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(defaults["accept"], "application/vnd.github+json");
    assert_eq!(defaults["apiVersion"], "2022-11-28");
    let custom: Value = gh
        .send(|| {
            gh.http()
                .get(format!("{base}/headers"))
                .header(header::ACCEPT, "application/vnd.github.raw+json")
        })
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(custom["accept"], "application/vnd.github.raw+json");
}
