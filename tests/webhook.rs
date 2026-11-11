use axum::body::Body;
use axum::http::{Request, StatusCode};
use ring::hmac;
use serde_json::{json, Value};
use sourcelens::config::Config;
use sourcelens::crypto;
use sourcelens::github::app::AppCredentials;
use sourcelens::http;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;
use tower::ServiceExt;

const SECRET: &str = "hook-secret";
const GITHUB_ID: i64 = 9001;
const INSTALLATION_ID: i64 = 77;

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
        webhook_secret: Some(SECRET.into()),
        private_key_pem: pem,
        html_url: "https://github.com/apps/sourcelens-test".into(),
        source: "env",
    }
}

async fn setup() -> (TempDir, AppState, i64) {
    let tmp = tempfile::tempdir().unwrap();
    let mut cfg = Config::new("http://localhost:3000", tmp.path());
    cfg.github = Some(credentials());
    // Never reached: webhooks make no GitHub calls.
    cfg.github_api_url = "http://127.0.0.1:9".into();
    let state = AppState::new(cfg).unwrap();
    let repo_id = state
        .db
        .write(|conn| {
            conn.query_row(
                "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, created_at)
                 VALUES (?1, ?2, 'acme/api', 0, 'main', ?3) RETURNING id",
                rusqlite::params![GITHUB_ID, INSTALLATION_ID, now()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    (tmp, state, repo_id)
}

fn sign(body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, SECRET.as_bytes());
    format!("sha256={}", crypto::hex(hmac::sign(&key, body).as_ref()))
}

async fn deliver_signed(
    state: &AppState,
    event: &str,
    delivery: &str,
    payload: &Value,
    signature: Option<String>,
) -> StatusCode {
    let body = payload.to_string().into_bytes();
    let signature = signature.unwrap_or_else(|| sign(&body));
    let request = Request::post("/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-hub-signature-256", signature)
        .body(Body::from(body))
        .unwrap();
    http::router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

async fn deliver(state: &AppState, event: &str, delivery: &str, payload: &Value) -> StatusCode {
    deliver_signed(state, event, delivery, payload, None).await
}

fn push(git_ref: &str, after: &str) -> Value {
    json!({ "ref": git_ref, "after": after, "deleted": false, "repository": { "id": GITHUB_ID, "full_name": "acme/api" } })
}

/// (status, target_commit) of every job of the repository.
async fn jobs(state: &AppState, repo_id: i64) -> Vec<(String, Option<String>, String)> {
    state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT status, target_commit, trigger FROM jobs WHERE repo_id = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([repo_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap()
}

async fn repo_row(state: &AppState, repo_id: i64) -> (String, Option<String>, String) {
    state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT state, state_reason, full_name FROM repositories WHERE id = ?1",
                [repo_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn bad_signature_is_rejected() {
    let (_tmp, state, repo_id) = setup().await;
    let payload = push("refs/heads/main", "a".repeat(40).as_str());
    let status = deliver_signed(
        &state,
        "push",
        "d-bad",
        &payload,
        Some(format!("sha256={}", "0".repeat(64))),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let status = deliver_signed(&state, "push", "d-none", &payload, Some(String::new())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(jobs(&state, repo_id).await.is_empty());
}

#[tokio::test]
async fn push_to_tracked_branch_queues_one_job_with_the_newest_commit() {
    let (_tmp, state, repo_id) = setup().await;
    let first = "1".repeat(40);
    let second = "2".repeat(40);

    let status = deliver(&state, "push", "d1", &push("refs/heads/main", &first)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        jobs(&state, repo_id).await,
        vec![("queued".into(), Some(first.clone()), "webhook".into())]
    );

    let status = deliver(&state, "push", "d2", &push("refs/heads/main", &second)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(
        jobs(&state, repo_id).await,
        vec![("queued".into(), Some(second), "webhook".into())]
    );
}

#[tokio::test]
async fn push_to_other_branch_or_branch_deletion_is_ignored() {
    let (_tmp, state, repo_id) = setup().await;
    let status = deliver(
        &state,
        "push",
        "d1",
        &push("refs/heads/feature", &"1".repeat(40)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let mut deleted = push("refs/heads/main", &"0".repeat(40));
    deleted["deleted"] = json!(true);
    deliver(&state, "push", "d2", &deleted).await;
    assert!(jobs(&state, repo_id).await.is_empty());

    // A configured branch replaces the default branch.
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE repositories SET branch = 'feature' WHERE id = ?1",
                [repo_id],
            )
        })
        .await
        .unwrap();
    deliver(
        &state,
        "push",
        "d3",
        &push("refs/heads/main", &"2".repeat(40)),
    )
    .await;
    assert!(jobs(&state, repo_id).await.is_empty());
    deliver(
        &state,
        "push",
        "d4",
        &push("refs/heads/feature", &"3".repeat(40)),
    )
    .await;
    assert_eq!(jobs(&state, repo_id).await.len(), 1);
}

#[tokio::test]
async fn repeated_delivery_has_no_second_effect() {
    let (_tmp, state, repo_id) = setup().await;
    let first = "1".repeat(40);
    let status = deliver(&state, "push", "same", &push("refs/heads/main", &first)).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let status = deliver(
        &state,
        "push",
        "same",
        &push("refs/heads/main", &"2".repeat(40)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        jobs(&state, repo_id).await,
        vec![("queued".into(), Some(first), "webhook".into())]
    );
}

#[tokio::test]
async fn repository_rename_updates_full_name_and_default_branch_change_queues_job() {
    let (_tmp, state, repo_id) = setup().await;
    let payload = json!({
        "action": "renamed",
        "repository": { "id": GITHUB_ID, "full_name": "acme/api-server", "private": true, "default_branch": "main" }
    });
    assert_eq!(
        deliver(&state, "repository", "r1", &payload).await,
        StatusCode::ACCEPTED
    );
    let (repo_state, _, full_name) = repo_row(&state, repo_id).await;
    assert_eq!(
        (repo_state.as_str(), full_name.as_str()),
        ("connected", "acme/api-server")
    );
    assert!(jobs(&state, repo_id).await.is_empty());

    let payload = json!({
        "action": "edited",
        "repository": { "id": GITHUB_ID, "full_name": "acme/api-server", "private": true, "default_branch": "trunk" }
    });
    deliver(&state, "repository", "r2", &payload).await;
    assert_eq!(jobs(&state, repo_id).await.len(), 1);

    let payload = json!({ "action": "deleted", "repository": { "id": GITHUB_ID, "full_name": "acme/api-server" } });
    deliver(&state, "repository", "r3", &payload).await;
    let (repo_state, reason, _) = repo_row(&state, repo_id).await;
    assert_eq!(repo_state, "disconnected");
    assert_eq!(reason.as_deref(), Some("Deleted on GitHub"));
}

#[tokio::test]
async fn installation_events_disconnect_and_reconnect() {
    let (_tmp, state, repo_id) = setup().await;
    let payload = json!({ "action": "suspend", "installation": { "id": INSTALLATION_ID } });
    deliver(&state, "installation", "i1", &payload).await;
    let (repo_state, reason, _) = repo_row(&state, repo_id).await;
    assert_eq!(repo_state, "disconnected");
    assert_eq!(reason.as_deref(), Some("GitHub App suspended"));

    // Disconnected repositories ignore pushes.
    deliver(
        &state,
        "push",
        "p1",
        &push("refs/heads/main", &"1".repeat(40)),
    )
    .await;
    assert!(jobs(&state, repo_id).await.is_empty());

    let payload = json!({ "action": "unsuspend", "installation": { "id": INSTALLATION_ID } });
    deliver(&state, "installation", "i2", &payload).await;
    let (repo_state, reason, _) = repo_row(&state, repo_id).await;
    assert_eq!((repo_state.as_str(), reason), ("connected", None));
    assert_eq!(jobs(&state, repo_id).await.len(), 1);
}

#[tokio::test]
async fn installation_repositories_events_change_access() {
    let (_tmp, state, repo_id) = setup().await;
    let removed = json!({
        "action": "removed",
        "installation": { "id": INSTALLATION_ID },
        "repositories_added": [],
        "repositories_removed": [{ "id": GITHUB_ID, "full_name": "acme/api" }]
    });
    deliver(&state, "installation_repositories", "a1", &removed).await;
    let (repo_state, reason, _) = repo_row(&state, repo_id).await;
    assert_eq!(repo_state, "disconnected");
    assert_eq!(reason.as_deref(), Some("Access removed on GitHub"));

    let added = json!({
        "action": "added",
        "installation": { "id": 88 },
        "repositories_added": [{ "id": GITHUB_ID, "full_name": "acme/api" }],
        "repositories_removed": []
    });
    deliver(&state, "installation_repositories", "a2", &added).await;
    let (repo_state, installation_id) = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT state, installation_id FROM repositories WHERE id = ?1",
                [repo_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!((repo_state.as_str(), installation_id), ("connected", 88));
    assert_eq!(jobs(&state, repo_id).await.len(), 1);
}

#[tokio::test]
async fn webhooks_without_credentials_are_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(Config::new("http://localhost:3000", tmp.path())).unwrap();
    let status = deliver(
        &state,
        "push",
        "x",
        &push("refs/heads/main", &"1".repeat(40)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
