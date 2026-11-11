mod common;

use axum::body::Body;
use axum::extract::Path;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::{api, credentials, error_code, new_state, owner_session, serve, sign_webhook};
use serde_json::{json, Value};
use sourcelens::http;
use sourcelens::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

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

/// The app is installed on `acme/api` only, through installation 99.
async fn repo_installation(Path((owner, repo)): Path<(String, String)>) -> Response {
    if (owner.as_str(), repo.as_str()) == ("acme", "api") {
        Json(json!({ "id": 99 })).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(json!({ "message": "Not Found" })),
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
        .route("/repos/{o}/{r}/installation", get(repo_installation))
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
    serve(app).await
}

// ── Helpers ──────────────────────────────────────────────────────────────────

struct Env {
    _tmp: TempDir,
    state: AppState,
    cookie: String,
}

async fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = start_fake().await;
    let state = new_state(&tmp, |cfg| {
        cfg.github = Some(credentials());
        cfg.github_api_url = base;
    });
    let cookie = owner_session(&state, "alex").await;
    Env {
        _tmp: tmp,
        state,
        cookie,
    }
}

impl Env {
    async fn call(&self, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let app = http::router(self.state.clone());
        let (status, body, _) = api(&app, method, uri, &self.cookie, body).await;
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
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .unwrap()
    }
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
}

#[tokio::test]
async fn branches_lists_the_default_and_the_repository_branches() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();

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
            Ok::<_, rusqlite::Error>((a, b))
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

// ── Reconnect, disconnect, job bookkeeping ───────────────────────────────────

impl Env {
    /// Marks the repository disconnected the way the pipeline or a webhook does.
    async fn disconnect_by_sql(&self, id: i64, reason: &str) {
        let reason = reason.to_string();
        self.state
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE repositories SET state = 'disconnected', state_reason = ?2 WHERE id = ?1",
                    rusqlite::params![id, reason],
                )?;
                conn.execute("DELETE FROM jobs WHERE repo_id = ?1", [id])
            })
            .await
            .unwrap();
    }

    async fn repo_state(&self, id: i64) -> (String, Option<String>, i64) {
        self.state
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT state, state_reason, installation_id FROM repositories WHERE id = ?1",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            })
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn sync_reconnects_a_disconnected_repository_github_confirms() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();
    env.disconnect_by_sql(id, "GitHub App has no access").await;

    let (status, job) = env
        .call("POST", &format!("/api/repositories/{id}/sync"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["status"], "queued");
    assert_eq!(env.repo_state(id).await, ("connected".into(), None, 99));
    assert_eq!(
        env.jobs(id).await,
        vec![("queued".into(), "manual".into(), false)]
    );
}

#[tokio::test]
async fn branch_change_reconnects_a_disconnected_repository_github_confirms() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();
    env.disconnect_by_sql(id, "GitHub App suspended").await;

    let (status, body) = env
        .call(
            "PATCH",
            &format!("/api/repositories/{id}"),
            Some(json!({ "branch": "develop" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "connected");
    assert_eq!(body["effectiveBranch"], "develop");
    assert_eq!(body["activeJob"]["status"], "queued");
    assert_eq!(env.repo_state(id).await, ("connected".into(), None, 99));
}

#[tokio::test]
async fn sync_without_github_access_reports_the_stored_reason() {
    let env = setup().await;
    let id: i64 = env
        .state
        .db
        .write(|conn| {
            conn.query_row(
                "INSERT INTO repositories
                   (github_id, installation_id, full_name, private, default_branch, state, state_reason, created_at)
                 VALUES (7, 42, 'acme/gone', 0, 'main', 'disconnected', 'Access removed on GitHub', 0)
                 RETURNING id",
                [],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();

    for (method, body) in [("POST", None), ("PATCH", Some(json!({ "branch": "main" })))] {
        let uri = if method == "POST" {
            format!("/api/repositories/{id}/sync")
        } else {
            format!("/api/repositories/{id}")
        };
        let (status, body) = env.call(method, &uri, body).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {body}");
        assert_eq!(error_code(&body), "repository_disconnected");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("Access removed on GitHub"),
            "{body}"
        );
    }
    assert_eq!(
        env.repo_state(id).await,
        (
            "disconnected".into(),
            Some("Access removed on GitHub".into()),
            42
        )
    );
    assert!(env.jobs(id).await.is_empty());
}

#[tokio::test]
async fn webhook_disconnect_removes_the_queued_job() {
    let env = setup().await;
    let (_, body) = env.connect().await;
    let id = body["id"].as_i64().unwrap();
    assert_eq!(body["activeJob"]["status"], "queued");

    let payload =
        json!({ "action": "deleted", "repository": { "id": 5001, "full_name": "acme/api" } })
            .to_string();
    let signature = sign_webhook(payload.as_bytes());
    let request = Request::post("/webhooks/github")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-github-event", "repository")
        .header("x-github-delivery", "deleted-1")
        .header("x-hub-signature-256", signature)
        .body(Body::from(payload))
        .unwrap();
    let response = http::router(env.state.clone())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    assert!(env.jobs(id).await.is_empty());
    let (status, list) = env.call("GET", "/api/repositories", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list[0]["state"], "disconnected");
    assert_eq!(list[0]["stateReason"], "Deleted on GitHub");
    assert_eq!(list[0]["activeJob"], Value::Null);
}
