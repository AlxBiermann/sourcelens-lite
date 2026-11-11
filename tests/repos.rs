use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use sourcelens::config::Config;
use sourcelens::github::app::AppCredentials;
use sourcelens::github::oauth::GhToken;
use sourcelens::http;
use sourcelens::http::session::create_session;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

const PUBLIC_URL: &str = "http://localhost:3000";

// ── Fake GitHub ──────────────────────────────────────────────────────────────

async fn user_repo(Path((owner, repo)): Path<(String, String)>) -> Response {
    match (owner.as_str(), repo.as_str()) {
        ("acme", "api") => Json(json!({
            "id": 5001, "full_name": "acme/api", "private": true, "default_branch": "main"
        }))
        .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
        )
            .into_response(),
    }
}

async fn branch(Path((_o, _r, branch)): Path<(String, String, String)>) -> Response {
    if branch == "main" || branch == "develop" {
        Json(json!({ "name": branch, "commit": { "sha": "a".repeat(40) } })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Branch not found" })),
        )
            .into_response()
    }
}

async fn start_fake() -> String {
    let app = Router::new()
        .route(
            "/app/installations/{id}/access_tokens",
            post(|| async { (StatusCode::CREATED, Json(json!({ "token": "ghs_fake" }))) }),
        )
        .route("/repos/{o}/{r}", get(user_repo))
        .route(
            "/repos/{o}/{r}/branches",
            get(|| async { Json(json!([{ "name": "main" }, { "name": "develop" }])) }),
        )
        .route("/repos/{o}/{r}/branches/{b}", get(branch))
        .route(
            "/user/installations",
            get(|| async {
                Json(json!({ "total_count": 1, "installations": [{
                    "id": 42,
                    "account": { "login": "acme", "avatar_url": "https://avatars.example/acme" },
                    "html_url": "https://github.com/organizations/acme/settings/installations/42"
                }] }))
            }),
        )
        .route(
            "/user/installations/{id}/repositories",
            get(|| async {
                Json(json!({ "total_count": 1, "repositories": [
                    { "id": 5001, "full_name": "acme/api", "private": true, "default_branch": "main" }
                ] }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn credentials() -> AppCredentials {
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

struct Env {
    _tmp: TempDir,
    state: AppState,
    cookie: String,
}

async fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::new(PUBLIC_URL, tmp.path());
    cfg.github = Some(credentials());
    cfg.github_api_url = start_fake().await;
    let state = AppState::new(cfg).unwrap();
    let user_id: i64 = state
        .db
        .write(|conn| {
            conn.query_row(
                "INSERT INTO users (github_id, login, role, created_at) VALUES (1, 'alex', 'owner', ?1) RETURNING id",
                [now()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    let token = GhToken {
        access_token: "gho_user".into(),
        refresh_token: None,
        expires_at: i64::MAX,
    };
    let cookie = create_session(&state, user_id, &token).await.unwrap();
    Env {
        _tmp: tmp,
        state,
        cookie,
    }
}

impl Env {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::COOKIE, format!("sl_session={}", self.cookie));
        if method != "GET" {
            builder = builder.header(header::ORIGIN, PUBLIC_URL);
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        let response = http::router(self.state.clone())
            .oneshot(request)
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    async fn connect(&self) -> (StatusCode, Value) {
        self.call(
            "POST",
            "/api/repositories",
            Some(json!({ "fullName": "acme/api", "installationId": 42 })),
        )
        .await
    }

    async fn jobs(&self, repo_id: i64) -> Vec<(String, String, bool)> {
        self.state
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT status, trigger, force FROM jobs WHERE repo_id = ?1 ORDER BY id",
                )?;
                let rows = stmt
                    .query_map([repo_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap()
    }
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_default()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn github_listing_marks_connected_repositories() {
    let env = setup().await;
    let (status, body) = env.call("GET", "/api/github/repositories", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["installationId"], 42);
    assert_eq!(body[0]["account"]["login"], "acme");
    assert_eq!(body[0]["truncated"], false);
    assert_eq!(body[0]["repositories"][0]["fullName"], "acme/api");
    assert_eq!(body[0]["repositories"][0]["connected"], false);

    env.connect().await;
    let (_, body) = env.call("GET", "/api/github/repositories", None).await;
    assert_eq!(body[0]["repositories"][0]["connected"], true);
}

#[tokio::test]
async fn connect_inserts_repository_and_queues_job() {
    let env = setup().await;
    let (status, body) = env.connect().await;
    assert!(status.is_success(), "{status} {body}");
    assert_eq!(body["fullName"], "acme/api");
    assert_eq!(body["private"], true);
    assert_eq!(body["effectiveBranch"], "main");
    assert_eq!(body["activeJob"]["status"], "queued");
    let repo_id = body["id"].as_i64().unwrap();
    assert_eq!(
        env.jobs(repo_id).await,
        vec![("queued".into(), "connect".into(), false)]
    );

    let (status, body) = env.connect().await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "already_connected");
}

#[tokio::test]
async fn connect_without_access_is_not_found() {
    let env = setup().await;
    let (status, body) = env
        .call(
            "POST",
            "/api/repositories",
            Some(json!({ "fullName": "acme/secret", "installationId": 42 })),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(error_code(&body), "repository_not_accessible");
}

#[tokio::test]
async fn requests_without_session_are_rejected() {
    let env = setup().await;
    let request = Request::get("/api/repositories")
        .body(Body::empty())
        .unwrap();
    let response = http::router(env.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn branch_change_is_verified_and_queues_a_job() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();

    let (status, body) = env
        .call(
            "PATCH",
            &format!("/api/repositories/{id}"),
            Some(json!({ "branch": "nope" })),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "branch_not_found");

    let (status, body) = env
        .call(
            "PATCH",
            &format!("/api/repositories/{id}"),
            Some(json!({ "branch": "develop" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        (body["branch"].as_str(), body["effectiveBranch"].as_str()),
        (Some("develop"), Some("develop"))
    );

    let (status, body) = env
        .call(
            "PATCH",
            &format!("/api/repositories/{id}"),
            Some(json!({ "branch": null })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (body["branch"].clone(), body["effectiveBranch"].as_str()),
        (Value::Null, Some("main"))
    );

    let (status, body) = env
        .call("GET", &format!("/api/repositories/{id}/branches"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "defaultBranch": "main", "branches": ["main", "develop"] })
    );
}

#[tokio::test]
async fn sync_with_force_queues_a_forced_job() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();

    let (status, job) = env
        .call(
            "POST",
            &format!("/api/repositories/{id}/sync"),
            Some(json!({ "force": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["status"], "queued");
    assert_eq!(job["trigger"], "manual");
    assert_eq!(
        env.jobs(id).await,
        vec![("queued".into(), "manual".into(), true)]
    );

    let (status, _) = env
        .call("POST", &format!("/api/repositories/{id}/sync"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(env.jobs(id).await.len(), 1);
}

#[tokio::test]
async fn delete_hides_the_repository() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();

    let (status, _) = env
        .call("DELETE", &format!("/api/repositories/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, list) = env.call("GET", "/api/repositories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list, json!([]));
    let (status, _) = env
        .call("GET", &format!("/api/repositories/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .call("DELETE", &format!("/api/repositories/{id}"), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn list_computes_effective_branch_and_jobs() {
    let env = setup().await;
    let (a, b) = env
        .state
        .db
        .write(|conn| {
            let insert = "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, branch, indexed_commit, created_at)
                          VALUES (?1, 42, ?2, 0, 'main', ?3, ?4, 0) RETURNING id";
            let a: i64 = conn.query_row(insert, rusqlite::params![1, "acme/b-repo", None::<String>, "c0ffee"], |r| r.get(0))?;
            let b: i64 = conn.query_row(insert, rusqlite::params![2, "acme/a-repo", Some("release"), None::<String>], |r| r.get(0))?;
            conn.execute(
                "INSERT INTO jobs (repo_id, status, trigger, created_at, finished_at) VALUES (?1, 'succeeded', 'connect', 1, 2)",
                [a],
            )?;
            conn.execute(
                "INSERT INTO jobs (repo_id, status, trigger, created_at, finished_at, error) VALUES (?1, 'failed', 'manual', 3, 4, 'boom')",
                [a],
            )?;
            conn.execute("INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'queued', 'webhook', 5)", [a])?;
            Ok((a, b))
        })
        .await
        .unwrap();

    let (status, list) = env.call("GET", "/api/repositories", None).await;
    assert_eq!(status, StatusCode::OK);
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 2);
    // Ordered by full name.
    assert_eq!(list[0]["id"].as_i64(), Some(b));
    assert_eq!(list[0]["effectiveBranch"], "release");
    assert_eq!(list[0]["activeJob"], Value::Null);
    assert_eq!(list[0]["lastJob"], Value::Null);
    assert_eq!(list[1]["id"].as_i64(), Some(a));
    assert_eq!(list[1]["effectiveBranch"], "main");
    assert_eq!(list[1]["indexedCommit"], "c0ffee");
    assert_eq!(list[1]["activeJob"]["trigger"], "webhook");
    assert_eq!(list[1]["lastJob"]["status"], "failed");
    assert_eq!(list[1]["lastJob"]["error"], "boom");
    assert_eq!(list[1]["embeddedCount"], 0);

    let (status, detail) = env
        .call("GET", &format!("/api/repositories/{a}"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["repository"]["id"].as_i64(), Some(a));
    assert_eq!(detail["jobs"].as_array().unwrap().len(), 3);
    assert_eq!(detail["skipped"], json!([]));
}
