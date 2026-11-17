mod common;

use axum::http::{header, StatusCode};
use common::{api, error_code, insert_user, new_state, request, scalar, send, Reply};
use serde_json::json;
use sourcelens::dev;
use sourcelens::http;
use sourcelens::http::session::SESSION_COOKIE;
use sourcelens::state::AppState;
use tempfile::TempDir;

fn dev_state(tmp: &TempDir) -> AppState {
    new_state(tmp, |cfg| cfg.dev_mode = true)
}

/// `Location` of a redirect and the session cookie it sets, if any.
fn redirect(reply: &Reply) -> (String, Option<String>) {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text);
    let location = reply.headers[header::LOCATION]
        .to_str()
        .unwrap()
        .to_string();
    let session = reply
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()?
                .strip_prefix(&format!("{SESSION_COOKIE}="))
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty());
    (location, session)
}

async fn dev_login(state: &AppState, query: &str) -> (String, Option<String>) {
    let reply = send(
        &http::router(state.clone()),
        request("GET", &format!("/auth/dev/login{query}"), None, None),
    )
    .await;
    redirect(&reply)
}

async fn row_counts(state: &AppState) -> [i64; 4] {
    [
        scalar(state, "SELECT count(*) FROM users").await,
        scalar(state, "SELECT count(*) FROM repositories").await,
        scalar(state, "SELECT count(*) FROM jobs").await,
        scalar(state, "SELECT count(*) FROM oauth_grants").await,
    ]
}

#[tokio::test]
async fn dev_login_is_unknown_without_dev_mode() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());

    let dev = send(&app, request("GET", "/auth/dev/login?as=admin", None, None)).await;
    let unknown = send(&app, request("GET", "/auth/x", None, None)).await;
    assert_eq!(dev.status, unknown.status);
    assert_eq!(dev.text, unknown.text);
    assert!(dev.headers.get(header::SET_COOKIE).is_none());
    assert_eq!(scalar(&state, "SELECT count(*) FROM sessions").await, 0);

    let state_reply = send(&app, request("GET", "/api/state", None, None)).await;
    assert_eq!(state_reply.body["devMode"], false);
}

#[tokio::test]
async fn dev_login_signs_in_as_admin_or_sso_member() {
    let tmp = TempDir::new().unwrap();
    let state = dev_state(&tmp);
    dev::prepare(&state).await.unwrap();
    let app = http::router(state.clone());

    let (location, admin) = dev_login(&state, "?as=admin").await;
    assert_eq!(location, "/repositories");
    let admin = admin.expect("session cookie");
    let (status, body, text) = api(&app, "GET", "/api/state", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(body["devMode"], true);
    assert_eq!(body["user"]["role"], "owner");
    assert_eq!(body["user"]["provider"], "sso");
    assert_eq!(body["user"]["login"], "dev-admin@example.com");
    let (status, _, text) = api(&app, "GET", "/api/keys", &admin, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    let (_, default) = dev_login(&state, "").await;
    let (_, body, _) = api(&app, "GET", "/api/state", &default.unwrap(), None).await;
    assert_eq!(body["user"]["role"], "owner");

    let (_, member) = dev_login(&state, "?as=member").await;
    let member = member.expect("session cookie");
    let (_, body, _) = api(&app, "GET", "/api/state", &member, None).await;
    assert_eq!(body["user"]["role"], "member");
    assert_eq!(body["user"]["provider"], "sso");
    let (status, body, _) = api(&app, "GET", "/api/keys", &member, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "api_keys_unavailable");

    let (location, _) = dev_login(&state, "?as=admin&next=%2Foauth%2Fauthorize%3Fx%3D1").await;
    assert_eq!(location, "/oauth/authorize?x=1");
    let (location, _) = dev_login(&state, "?next=https%3A%2F%2Fevil.example").await;
    assert_eq!(location, "/repositories");
}

#[tokio::test]
async fn prepare_seeds_an_empty_database_once() {
    let tmp = TempDir::new().unwrap();
    let state = dev_state(&tmp);
    dev::prepare(&state).await.unwrap();
    let first = row_counts(&state).await;
    assert_eq!(first[1], 3, "three demo repositories");
    assert!(first[2] > 0 && first[3] == 2, "{first:?}");
    assert_eq!(
        scalar(
            &state,
            "SELECT count(*) FROM users WHERE status = 'pending'"
        )
        .await,
        2
    );
    assert_eq!(scalar(&state, "SELECT count(*) FROM api_keys").await, 1);

    dev::prepare(&state).await.unwrap();
    assert_eq!(row_counts(&state).await, first);
}

#[tokio::test]
async fn prepare_adds_only_the_dev_accounts_to_a_used_database() {
    let tmp = TempDir::new().unwrap();
    let state = dev_state(&tmp);
    insert_user(&state, "octocat", "owner", Some(1)).await;

    dev::prepare(&state).await.unwrap();
    assert_eq!(row_counts(&state).await, [3, 0, 0, 0]);
    assert_eq!(
        scalar(
            &state,
            "SELECT count(*) FROM users WHERE sso_issuer = 'dev' AND status = 'active'"
        )
        .await,
        2
    );
}

#[tokio::test]
async fn demo_repositories_are_searchable() {
    let tmp = TempDir::new().unwrap();
    let state = dev_state(&tmp);
    dev::prepare(&state).await.unwrap();
    let app = http::router(state.clone());
    let (_, member) = dev_login(&state, "?as=member").await;

    let (status, body, text) = api(
        &app,
        "POST",
        "/api/search",
        &member.unwrap(),
        Some(json!({ "query": "calculate_invoice_total" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let results = body["results"].as_array().unwrap();
    assert!(!results.is_empty(), "{text}");
    assert_eq!(results[0]["repository"], "acme/api");
}
