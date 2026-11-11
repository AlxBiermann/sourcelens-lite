//! Setup shared by the integration test crates.
// Every test crate compiles this module on its own and uses a different subset of it.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use ring::hmac;
use rusqlite::Connection;
use serde_json::Value;
use sourcelens::config::Config;
use sourcelens::crypto;
use sourcelens::db::Db;
use sourcelens::github::app::AppCredentials;
use sourcelens::github::oauth::GhToken;
use sourcelens::http;
use sourcelens::http::session::{create_session, SESSION_COOKIE};
use sourcelens::index::store;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

pub const PUBLIC_URL: &str = "http://localhost:3000";
/// Webhook secret of [`credentials`].
pub const WEBHOOK_SECRET: &str = "hook-secret";
/// Installation id of every repository inserted with [`insert_repo`].
pub const INSTALLATION_ID: i64 = 42;

// ── State and database ───────────────────────────────────────────────────────

/// GitHub App credentials signed with the test-only private key.
pub fn credentials() -> AppCredentials {
    AppCredentials {
        app_id: 4242,
        slug: "sourcelens-test".into(),
        client_id: "Iv1.test".into(),
        client_secret: "client-secret".into(),
        webhook_secret: Some(WEBHOOK_SECRET.into()),
        private_key_pem: include_str!("../fixtures/test_only_app_key.pem").into(),
        html_url: "https://github.com/apps/sourcelens-test".into(),
        source: "env",
    }
}

/// An instance at [`PUBLIC_URL`] with its data directory in `tmp`.
pub fn new_state(tmp: &TempDir, configure: impl FnOnce(&mut Config)) -> AppState {
    let mut cfg = Config::new(PUBLIC_URL, tmp.path());
    configure(&mut cfg);
    AppState::new(cfg).unwrap()
}

/// A migrated database in `tmp`, without the rest of the application state.
pub fn temp_db(tmp: &TempDir) -> Db {
    Db::open(&tmp.path().join("test.db")).unwrap()
}

pub fn insert_repo(conn: &Connection, github_id: i64, full_name: &str, state: &str) -> i64 {
    conn.query_row(
        "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, state, created_at)
         VALUES (?1, ?2, ?3, 0, 'main', ?4, 0) RETURNING id",
        rusqlite::params![github_id, INSTALLATION_ID, full_name, state],
        |row| row.get(0),
    )
    .unwrap()
}

pub async fn insert_repo_async(
    app: &AppState,
    github_id: i64,
    full_name: &str,
    state: &str,
) -> i64 {
    let full_name = full_name.to_string();
    let state = state.to_string();
    app.db
        .write(move |conn| {
            Ok::<_, rusqlite::Error>(insert_repo(conn, github_id, &full_name, &state))
        })
        .await
        .unwrap()
}

/// First column of the first row of `sql`.
pub fn count(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

/// [`count`] on a reader connection of `state`.
pub async fn scalar(state: &AppState, sql: &'static str) -> i64 {
    state
        .db
        .read(move |conn| Ok::<_, rusqlite::Error>(count(conn, sql)))
        .await
        .unwrap()
}

// ── Indexing ─────────────────────────────────────────────────────────────────

/// Indexes `(path, content)` pairs through the real store.
pub async fn index_files<P: AsRef<str>, C: AsRef<[u8]>>(
    state: &AppState,
    repo_id: i64,
    files: &[(P, C)],
) {
    let prepared: Vec<store::PreparedFile> = files
        .iter()
        .map(|(path, content)| {
            let content = content.as_ref();
            store::prepare_file(path.as_ref(), &crypto::git_blob_sha1(content), content)
        })
        .collect();
    state
        .db
        .write(move |conn| store::apply_files(conn, repo_id, &prepared))
        .await
        .unwrap();
}

/// A Rust file with `n` functions, each large enough to be its own chunk.
pub fn functions_source(prefix: &str, n: usize) -> String {
    (0..n)
        .map(|i| {
            let body: String = (0..12)
                .map(|j| {
                    format!("    let value_{j} = compute_{prefix}_{i}(input, {j}) * factor_{j};\n")
                })
                .collect();
            format!("pub fn {prefix}_{i}(input: u64) -> u64 {{\n{body}    input\n}}\n\n")
        })
        .collect()
}

/// Four Rust functions in `src/lib.rs` and `frobnicate` in `lib/special.py`.
pub async fn index_sample(state: &AppState, repo_id: i64) {
    index_files(
        state,
        repo_id,
        &[
            ("src/lib.rs", functions_source("alpha", 4)),
            (
                "lib/special.py",
                "def frobnicate(widget):\n    return widget.spin() + 1\n".to_string(),
            ),
        ],
    )
    .await;
}

// ── Users and sessions ───────────────────────────────────────────────────────

/// A user who has signed in before.
pub async fn insert_user(state: &AppState, login: &str, role: &str, github_id: Option<i64>) -> i64 {
    let login = login.to_string();
    let role = role.to_string();
    state
        .db
        .write(move |conn| {
            conn.query_row(
                "INSERT INTO users (github_id, login, role, created_at, last_login_at)
                 VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
                rusqlite::params![github_id, login, role, now()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap()
}

/// Value of the session cookie of a new session of `user_id`.
pub async fn session_for(state: &AppState, user_id: i64) -> String {
    let token = GhToken {
        access_token: "gho_test".into(),
        refresh_token: None,
        expires_at: i64::MAX,
    };
    create_session(state, user_id, &token).await.unwrap()
}

/// Session cookie value of a new owner.
pub async fn owner_session(state: &AppState, login: &str) -> String {
    signed_in(state, login, "owner").await
}

/// Session cookie value of a new member.
pub async fn member_session(state: &AppState, login: &str) -> String {
    signed_in(state, login, "member").await
}

async fn signed_in(state: &AppState, login: &str, role: &str) -> String {
    let github_id = scalar(state, "SELECT count(*) + 1000 FROM users").await;
    let user_id = insert_user(state, login, role, Some(github_id)).await;
    session_for(state, user_id).await
}

// ── HTTP ─────────────────────────────────────────────────────────────────────

/// A request as the web app sends it: non-GET requests carry the public origin, a body is JSON.
pub fn request(
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if method != "GET" {
        builder = builder.header(header::ORIGIN, PUBLIC_URL);
    }
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, format!("{SESSION_COOKIE}={cookie}"));
    }
    match body {
        Some(body) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// The body as JSON; `Null` when it is empty or not JSON.
    pub body: Value,
    pub text: String,
}

pub async fn send(app: &Router, req: Request<Body>) -> Reply {
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let body = serde_json::from_str(&text).unwrap_or(Value::Null);
    Reply {
        status,
        headers,
        body,
        text,
    }
}

/// Sends `req` through the application router.
pub async fn call(state: &AppState, req: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let reply = send(&http::router(state.clone()), req).await;
    (reply.status, reply.headers, reply.body)
}

/// A JSON API call with a session: status, JSON body and raw body.
pub async fn api(
    app: &Router,
    method: &str,
    uri: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let reply = send(app, request(method, uri, Some(cookie), body)).await;
    (reply.status, reply.body, reply.text)
}

/// `code` of an API error body; empty when there is none.
pub fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

/// A `POST /mcp` request, with `Authorization: Bearer <key>` when a key is given.
pub fn mcp_request(key: Option<&str>) -> axum::http::request::Builder {
    let mut builder = Request::post("/mcp")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json, text/event-stream");
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    builder
}

/// `X-Hub-Signature-256` value for `body` under [`WEBHOOK_SECRET`].
pub fn sign_webhook(body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, WEBHOOK_SECRET.as_bytes());
    format!("sha256={}", crypto::hex(hmac::sign(&key, body).as_ref()))
}

/// Serves `router` on a free local port and returns its base URL.
pub async fn serve(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}
