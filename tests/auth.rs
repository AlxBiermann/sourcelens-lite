mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use common::{
    call, credentials, error_code, insert_user, mcp_request, new_state, request, scalar, serve,
    session_for, PUBLIC_URL,
};
use serde_json::{json, Value};
use sourcelens::config::Config;
use sourcelens::crypto::{random_hex, sha256_hex};
use sourcelens::error::{ApiError, GitHubFailure};
use sourcelens::github::app::GitHub;
use sourcelens::github::oauth::{resolve_user, GhToken, GhUser, Resolution, TokenError};
use sourcelens::http::session::{create_session, user_token_with, AuthUser};
use sourcelens::model::Role;
use sourcelens::state::{now, AppState};

const HTTPS_PUBLIC_URL: &str = "https://sourcelens.example.com";
/// Account id the fake GitHub reports for the login `hubot`.
const HUBOT_GITHUB_ID: i64 = 5001;
/// App id in the fake manifest conversion.
const CONVERTED_APP_ID: u64 = 777;

/// An invited member who has not signed in yet.
async fn insert_invitation(state: &AppState, login: &str, github_id: i64) -> i64 {
    let login = login.to_string();
    state
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO users (github_id, login, role, created_at) VALUES (?1, ?2, 'member', ?3)",
                rusqlite::params![github_id, login, now()],
            )?;
            Ok::<_, rusqlite::Error>(c.last_insert_rowid())
        })
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
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
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
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
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

// ── Sign-in ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn login_redirects_to_setup_or_github() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let (status, headers, _) = call(&state, request("GET", "/auth/github/login", None, None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::LOCATION], "/setup");

    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
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
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
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

    // An invitation binds to the GitHub account id: another account holding the invited login is
    // not the member.
    let invited = insert_invitation(&state, "invitee", 77).await;
    assert_eq!(
        resolve_user(&state.db, &gh_user(78, "invitee"), false)
            .await
            .unwrap(),
        Resolution::NotInvited
    );
    assert_eq!(
        resolve_user(&state.db, &gh_user(77, "Invitee"), false)
            .await
            .unwrap(),
        Resolution::User(invited)
    );
    let last_login_at: Option<i64> = state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT last_login_at FROM users WHERE id = ?1",
                [invited],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert!(last_login_at.is_some());

    // Unknown user with an owner present → not invited, even with a claim.
    assert_eq!(
        resolve_user(&state.db, &gh_user(99, "stranger"), true)
            .await
            .unwrap(),
        Resolution::NotInvited
    );
    assert_eq!(scalar(&state, "SELECT count(*) FROM users").await, 2);
}

#[tokio::test]
async fn renamed_account_takes_over_a_login_held_by_an_older_row() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    insert_user(&state, "octocat", "owner", Some(1)).await;
    let old_holder = insert_invitation(&state, "bob", 2).await;
    let member = insert_user(&state, "robert", "member", Some(3)).await;

    // Account 3 renamed itself to "bob" on GitHub; account 2 no longer holds that login.
    assert_eq!(
        resolve_user(&state.db, &gh_user(3, "bob"), false)
            .await
            .unwrap(),
        Resolution::User(member)
    );
    let logins: Vec<(i64, String)> = state
        .db
        .read(|c| {
            let mut stmt = c.prepare("SELECT id, login FROM users ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    assert!(logins.contains(&(member, "bob".to_string())));
    assert!(logins
        .iter()
        .any(|(id, login)| *id == old_holder && login != "bob"));

    // The older row is still bound to account 2 and gets its current login back on sign-in.
    assert_eq!(
        resolve_user(&state.db, &gh_user(2, "bobby"), false)
            .await
            .unwrap(),
        Resolution::User(old_holder)
    );
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
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 0);
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
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 1);

    let (status, _, _) = call(&state, request("POST", "/api/logout", Some(&cookie), None)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ── Members ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn owner_manages_members() {
    let (base, _) = spawn_fake_github().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github_api_url = base.clone());
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/members",
            Some(&cookie),
            Some(json!({ "login": "Hubot" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["login"], "hubot");
    assert_eq!(body["role"], "member");
    assert_eq!(body["pending"], true);
    let member = body["id"].as_i64().unwrap();
    let github_id: Option<i64> = state
        .db
        .read(move |c| {
            c.query_row("SELECT github_id FROM users WHERE id = ?1", [member], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(github_id, Some(HUBOT_GITHUB_ID));

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/members",
            Some(&cookie),
            Some(json!({ "login": "ghost-user" })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "invalid_login");

    // Someone who later registers the invited login is a different GitHub account.
    assert_eq!(
        resolve_user(&state.db, &gh_user(9999, "hubot"), false)
            .await
            .unwrap(),
        Resolution::NotInvited
    );
    assert_eq!(
        resolve_user(&state.db, &gh_user(HUBOT_GITHUB_ID, "hubot"), false)
            .await
            .unwrap(),
        Resolution::User(member)
    );

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
async fn members_cannot_manage_members() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let member = insert_user(&state, "hubot", "member", Some(2)).await;
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
}

#[tokio::test]
async fn removing_a_member_ends_their_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let member = insert_user(&state, "hubot", "member", Some(2)).await;
    let owner_cookie = session_for(&state, owner).await;
    let member_cookie = session_for(&state, member).await;

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

// ── Error format for bad input ────────────────────────────────────────────────

#[tokio::test]
async fn malformed_bodies_and_path_ids_answer_in_the_api_error_format() {
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
            Some(json!({ "login": 42 })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_code(&body), "invalid_body");
    assert!(!body["error"]["message"].as_str().unwrap_or("").is_empty());

    let (status, _, body) = call(
        &state,
        request("DELETE", "/api/members/abc", Some(&cookie), None),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "not_found");
    assert!(!body["error"]["message"].as_str().unwrap_or("").is_empty());
}

// ── App JWT ───────────────────────────────────────────────────────────────────

#[test]
fn app_jwt_is_rs256_with_string_issuer() {
    let jwt = GitHub::app_jwt(&credentials()).unwrap();
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
        .route(
            "/users/{login}",
            get(|Path(login): Path<String>, headers: HeaderMap| async move {
                let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
                if auth != Some("Bearer gho_test") {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Requires authentication" })));
                }
                if !login.eq_ignore_ascii_case("hubot") {
                    return (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" })));
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "id": HUBOT_GITHUB_ID,
                        "login": "hubot",
                        "name": "Hubot",
                        "avatar_url": "https://avatars.githubusercontent.com/u/5001",
                        "type": "User",
                    })),
                )
            }),
        )
        .route(
            "/app-manifests/{code}/conversions",
            post(|Path(code): Path<String>| async move {
                if code != "good-code" {
                    return (StatusCode::NOT_FOUND, Json(json!({ "message": "Not Found" })));
                }
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "id": CONVERTED_APP_ID,
                        "slug": "sourcelens-abcd",
                        "name": "SourceLens abcd",
                        "client_id": "Iv1.converted",
                        "client_secret": "converted-secret",
                        "webhook_secret": "converted-hook",
                        "pem": credentials().private_key_pem,
                        "html_url": "https://github.com/apps/sourcelens-abcd",
                        "owner": { "login": "octocat" },
                    })),
                )
            }),
        )
        .with_state(counters.clone());
    (serve(app).await, counters)
}

#[tokio::test]
async fn installation_token_is_cached() {
    let (base, counters) = spawn_fake_github().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| {
        cfg.github = Some(credentials());
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
    assert_eq!(err.github, Some(GitHubFailure::Status(404)));

    let err = gh
        .send(|| gh.http().get(format!("{base}/limited")))
        .await
        .unwrap_err();
    assert_eq!(err.code, "github_rate_limited");
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.github, Some(GitHubFailure::RateLimited));

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

// ── Member removal revokes keys ───────────────────────────────────────────────

async fn insert_key(state: &AppState, created_by: i64) -> String {
    let secret = format!("sl_{}", random_hex(32));
    let hash = sha256_hex(secret.as_bytes());
    let last4 = secret[secret.len() - 4..].to_string();
    state
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO api_keys (name, key_hash, last4, all_repos, created_by, created_at)
                 VALUES ('agent', ?1, ?2, 1, ?3, ?4)",
                rusqlite::params![hash, last4, created_by, now()],
            )
        })
        .await
        .unwrap();
    secret
}

async fn mcp_ping(state: &AppState, secret: &str) -> StatusCode {
    let req = mcp_request(Some(secret))
        .body(Body::from(
            json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string(),
        ))
        .unwrap();
    call(state, req).await.0
}

#[tokio::test]
async fn removing_a_member_revokes_their_api_keys() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let member = insert_user(&state, "hubot", "member", Some(2)).await;
    let owner_cookie = session_for(&state, owner).await;
    let member_key = insert_key(&state, member).await;
    let owner_key = insert_key(&state, owner).await;
    assert_eq!(mcp_ping(&state, &member_key).await, StatusCode::OK);

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
    assert_eq!(
        mcp_ping(&state, &member_key).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(mcp_ping(&state, &owner_key).await, StatusCode::OK);
}

// ── Manifest flow state ───────────────────────────────────────────────────────

async fn get_location(state: &AppState, uri: &str) -> String {
    let (status, headers, _) = call(state, request("GET", uri, None, None)).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "{uri}");
    headers[header::LOCATION].to_str().unwrap().to_string()
}

#[tokio::test]
async fn wrong_manifest_state_does_not_cancel_the_pending_flow() {
    let (base, _) = spawn_fake_github().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github_api_url = base.clone());
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
    let manifest_state = post_url.split("state=").nth(1).unwrap().to_string();

    for forged in [
        "/auth/github/app-created?code=good-code&state=forged",
        "/auth/github/app-created?code=good-code",
    ] {
        assert_eq!(get_location(&state, forged).await, "/setup?error=state");
    }
    assert!(state.github.credentials().await.unwrap().is_none());

    let location = get_location(
        &state,
        &format!("/auth/github/app-created?code=good-code&state={manifest_state}"),
    )
    .await;
    assert_eq!(location, "/setup?step=install");
    let creds = state.github.credentials().await.unwrap().unwrap();
    assert_eq!(creds.app_id, CONVERTED_APP_ID);
    assert_eq!(creds.client_id, "Iv1.converted");
    assert_eq!(creds.webhook_secret.as_deref(), Some("converted-hook"));
    assert_eq!(creds.source, "manifest");

    // The state is single-use.
    let replay = format!("/auth/github/app-created?code=good-code&state={manifest_state}");
    assert_eq!(get_location(&state, &replay).await, "/setup?error=state");
}

// ── Token refresh ─────────────────────────────────────────────────────────────

/// A session whose GitHub token expired, and the user it belongs to.
async fn expired_session(state: &AppState) -> AuthUser {
    let owner = insert_user(state, "octocat", "owner", Some(1)).await;
    let token = GhToken {
        access_token: "gho_expired".into(),
        refresh_token: Some("ghr_once".into()),
        expires_at: now() - 10,
    };
    let cookie = create_session(state, owner, &token).await.unwrap();
    AuthUser {
        id: owner,
        login: "octocat".into(),
        role: Role::Owner,
        session_hash: sha256_hex(cookie.as_bytes()),
    }
}

#[tokio::test]
async fn concurrent_token_refreshes_send_one_request() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let user = expired_session(&state).await;
    let refreshes = Arc::new(AtomicUsize::new(0));

    let refresh = |refresh_token: String| {
        let refreshes = refreshes.clone();
        async move {
            assert_eq!(refresh_token, "ghr_once");
            refreshes.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok::<_, TokenError>(GhToken {
                access_token: "gho_fresh".into(),
                refresh_token: Some("ghr_next".into()),
                expires_at: now() + 3600,
            })
        }
    };
    let (first, second) = tokio::join!(
        user_token_with(&state, &user, refresh),
        user_token_with(&state, &user, refresh),
    );
    assert_eq!(first.unwrap(), "gho_fresh");
    assert_eq!(second.unwrap(), "gho_fresh");
    assert_eq!(refreshes.load(Ordering::SeqCst), 1);
    assert!(state.refresh_locks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn transient_refresh_failure_keeps_the_session() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let user = expired_session(&state).await;

    let err = user_token_with(&state, &user, |_| async {
        Err(TokenError::Request(ApiError::bad_gateway(
            "github_unreachable",
            "GitHub could not be reached.",
        )))
    })
    .await
    .unwrap_err();
    assert_eq!(err.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(err.code, "github_unavailable");
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 1);

    let err = user_token_with(&state, &user, |_| async {
        Err(TokenError::OAuth("bad_refresh_token".into()))
    })
    .await
    .unwrap_err();
    assert_eq!(err.code, "github_reauth_required");
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 0);
}

// ── Security headers and cookies ──────────────────────────────────────────────

#[tokio::test]
async fn security_headers_on_api_and_ui_responses() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    for uri in ["/api/state", "/repositories", "/assets/missing.js"] {
        let (_, headers, _) = call(&state, request("GET", uri, None, None)).await;
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{uri}");
        assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{uri}");
        assert_eq!(headers[header::REFERRER_POLICY], "same-origin", "{uri}");
        let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{uri}: {csp}");
        assert!(csp.contains("default-src 'self'"), "{uri}: {csp}");
    }
}

#[tokio::test]
async fn api_responses_are_gzip_compressed_on_request() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let gzip = Request::builder()
        .uri("/api/state")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = call(&state, gzip).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn https_instance_uses_host_locked_cookies() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::new(HTTPS_PUBLIC_URL, tmp.path());
    cfg.github = Some(credentials());
    let state = AppState::new(cfg).unwrap();

    let (_, headers, _) = call(&state, request("GET", "/auth/github/login", None, None)).await;
    let oauth = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(oauth.starts_with("__Host-sl_oauth="), "{oauth}");
    assert!(
        oauth.contains("; Secure") && oauth.contains("Path=/"),
        "{oauth}"
    );

    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let session = session_for(&state, owner).await;
    let state_user = |cookie: String| {
        Request::builder()
            .uri("/api/state")
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    };
    let (_, _, body) = call(&state, state_user(format!("__Host-sl_session={session}"))).await;
    assert_eq!(body["user"]["id"], owner);
    let (_, _, body) = call(&state, state_user(format!("sl_session={session}"))).await;
    assert_eq!(body["user"], Value::Null);

    let logout = Request::builder()
        .method("POST")
        .uri("/api/logout")
        .header(header::ORIGIN, HTTPS_PUBLIC_URL)
        .header(header::COOKIE, format!("__Host-sl_session={session}"))
        .body(Body::empty())
        .unwrap();
    let (status, headers, _) = call(&state, logout).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let cleared = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cleared.starts_with("__Host-sl_session=;"), "{cleared}");
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 0);
}
