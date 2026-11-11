use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use sourcelens::config::Config;
use sourcelens::crypto::git_blob_sha1;
use sourcelens::github::app::AppCredentials;
use sourcelens::github::oauth::GhToken;
use sourcelens::http::session::{create_session, SESSION_COOKIE};
use sourcelens::index::embed::{self, EmbedError, Embedder, EmbeddingSettingsInput};
use sourcelens::index::store;
use sourcelens::search::{self, Filter, SearchRequest};
use sourcelens::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

const ORIGIN: &str = "http://localhost:3000";

// ── fake embedding provider ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Recorded {
    authorization: Option<String>,
    body: Value,
}

impl Recorded {
    fn inputs(&self) -> Vec<String> {
        self.body["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }
}

#[derive(Default)]
struct FakeState {
    requests: Mutex<Vec<Recorded>>,
    /// Responses served before normal handling: (status, Retry-After).
    script: Mutex<VecDeque<(u16, Option<&'static str>)>>,
    /// 413 when a request has more inputs than this.
    max_batch: Mutex<Option<usize>>,
    /// 400 when any input contains this marker.
    reject_marker: Mutex<Option<String>>,
}

struct Fake {
    state: Arc<FakeState>,
    base_url: String,
}

impl Fake {
    async fn start() -> Self {
        let state = Arc::new(FakeState::default());
        let app = Router::new()
            .route("/v1/embeddings", post(fake_embeddings))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            state,
            base_url: format!("http://{addr}/v1"),
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.state.requests.lock().unwrap().clone()
    }

    fn request_count(&self) -> usize {
        self.state.requests.lock().unwrap().len()
    }

    fn script(&self, responses: &[(u16, Option<&'static str>)]) {
        self.state
            .script
            .lock()
            .unwrap()
            .extend(responses.iter().copied());
    }

    fn set_max_batch(&self, n: Option<usize>) {
        *self.state.max_batch.lock().unwrap() = n;
    }

    fn set_reject_marker(&self, marker: Option<&str>) {
        *self.state.reject_marker.lock().unwrap() = marker.map(str::to_string);
    }
}

/// Deterministic, deliberately not normalised vectors. Inputs mentioning "zebra" or "frobnicate"
/// point along the last axis; everything else lies in the plane of the first two axes.
fn fake_vector(input: &str, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    if input.contains("zebra") || input.contains("frobnicate") {
        v[dims - 1] = 2.0;
    } else {
        v[0] = 3.0;
        v[1] = 4.0 + (input.len() % 5) as f32;
    }
    v
}

async fn fake_embeddings(
    State(state): State<Arc<FakeState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let recorded = Recorded {
        authorization: headers
            .get("authorization")
            .map(|v| v.to_str().unwrap().to_string()),
        body: body.clone(),
    };
    let inputs = recorded.inputs();
    state.requests.lock().unwrap().push(recorded);

    let scripted = state.script.lock().unwrap().pop_front();
    if let Some((status, retry_after)) = scripted {
        let mut response = (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({ "error": { "message": format!("scripted failure {status}") } })),
        )
            .into_response();
        if let Some(value) = retry_after {
            response
                .headers_mut()
                .insert("retry-after", value.parse().unwrap());
        }
        return response;
    }
    let max_batch = *state.max_batch.lock().unwrap();
    if max_batch.is_some_and(|max| inputs.len() > max) {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({ "error": { "message": "batch too large" } })),
        )
            .into_response();
    }
    let marker = state.reject_marker.lock().unwrap().clone();
    if let Some(marker) = marker {
        if inputs.iter().any(|i| i.contains(&marker)) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": { "message": "input rejected" } })),
            )
                .into_response();
        }
    }
    let dims = body["dimensions"].as_u64().unwrap_or(4) as usize;
    // Reversed order: clients must sort by `index`.
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .rev()
        .map(|(i, input)| json!({ "object": "embedding", "index": i, "embedding": fake_vector(input, dims) }))
        .collect();
    Json(json!({ "object": "list", "data": data, "model": body["model"] })).into_response()
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn new_state(tmp: &TempDir) -> AppState {
    AppState::new(Config::new("http://localhost:3000", tmp.path())).unwrap()
}

fn settings_input(fake: &Fake) -> EmbeddingSettingsInput {
    EmbeddingSettingsInput {
        enabled: true,
        base_url: fake.base_url.clone(),
        model: "fake-embed".into(),
        request_dimensions: None,
        query_prefix: "query: ".into(),
        document_prefix: "doc: ".into(),
        batch_size: 2,
        api_key: None,
        clear_api_key: None,
    }
}

fn settings_body(fake: &Fake) -> Value {
    json!({
        "enabled": true,
        "baseUrl": fake.base_url,
        "model": "fake-embed",
        "requestDimensions": null,
        "queryPrefix": "query: ",
        "documentPrefix": "doc: ",
        "batchSize": 2
    })
}

fn embedder(fake: &Fake, api_key: Option<&str>, dims: Option<u32>) -> Embedder {
    Embedder::new(
        reqwest::Client::new(),
        &fake.base_url,
        "fake-embed",
        api_key.map(str::to_string),
        dims,
    )
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

async fn insert_repo(state: &AppState, name: &str, repo_state: &str) -> i64 {
    let name = name.to_string();
    let repo_state = repo_state.to_string();
    state
        .db
        .write(move |c| {
            let github_id: i64 = c.query_row("SELECT count(*) + 100 FROM repositories", [], |r| r.get(0))?;
            c.query_row(
                "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, state, created_at)
                 VALUES (?1, 1, ?2, 0, 'main', ?3, 0) RETURNING id",
                rusqlite::params![github_id, name, repo_state],
                |r| r.get(0),
            )
        })
        .await
        .unwrap()
}

async fn index_files(state: &AppState, repo_id: i64, files: &[(&str, String)]) {
    let prepared: Vec<store::PreparedFile> = files
        .iter()
        .map(|(path, content)| {
            store::prepare_file(path, &git_blob_sha1(content.as_bytes()), content.as_bytes())
        })
        .collect();
    state
        .db
        .write(move |c| store::apply_files(c, repo_id, &prepared))
        .await
        .unwrap();
}

/// A Rust file with `n` functions, each large enough to be its own chunk.
fn functions_source(prefix: &str, n: usize) -> String {
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

async fn index_sample(state: &AppState, repo_id: i64) {
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

async fn insert_job(state: &AppState, repo_id: i64) -> i64 {
    state
        .db
        .write(move |c| {
            c.query_row(
                "INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'running', 'manual', 0) RETURNING id",
                [repo_id],
                |r| r.get(0),
            )
        })
        .await
        .unwrap()
}

async fn scalar(state: &AppState, sql: &'static str) -> i64 {
    state
        .db
        .read(move |c| c.query_row(sql, [], |r| r.get(0)))
        .await
        .unwrap()
}

async fn job_row(state: &AppState, job_id: i64) -> (Option<String>, i64, i64) {
    state
        .db
        .read(move |c| {
            c.query_row(
                "SELECT phase, embed_total, embed_done FROM jobs WHERE id = ?1",
                [job_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap()
}

async fn stored_vectors(state: &AppState) -> Vec<Vec<f32>> {
    state
        .db
        .read(|c| {
            let mut stmt = c.prepare("SELECT embedding FROM vec_chunks")?;
            let rows = stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))?;
            rows.map(|blob| {
                blob.map(|b| {
                    b.chunks_exact(4)
                        .map(|x| f32::from_le_bytes([x[0], x[1], x[2], x[3]]))
                        .collect()
                })
            })
            .collect()
        })
        .await
        .unwrap()
}

async fn session_for(state: &AppState, login: &str, role: &str) -> String {
    let login_owned = login.to_string();
    let role = role.to_string();
    let user_id: i64 = state
        .db
        .write(move |c| {
            let github_id: i64 = c.query_row("SELECT count(*) + 1000 FROM users", [], |r| r.get(0))?;
            c.query_row(
                "INSERT INTO users (github_id, login, role, created_at) VALUES (?1, ?2, ?3, 0) RETURNING id",
                rusqlite::params![github_id, login_owned, role],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    let token = GhToken {
        access_token: "gho_test".into(),
        refresh_token: None,
        expires_at: i64::MAX,
    };
    let cookie = create_session(state, user_id, &token).await.unwrap();
    format!("{SESSION_COOKIE}={cookie}")
}

async fn call(
    app: &Router,
    method: &str,
    path: &str,
    cookie: &str,
    body: Option<Value>,
) -> (StatusCode, Value, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("cookie", cookie)
        .header("origin", ORIGIN);
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value, text)
}

// ── Embedder against the fake provider ─────────────────────────────────────

#[tokio::test]
async fn embedder_normalises_and_orders_by_index() {
    let fake = Fake::start().await;
    let vectors = embedder(&fake, None, None)
        .embed(&strings(&["a", "bb", "zebra"]))
        .await
        .unwrap();
    assert_eq!(vectors.len(), 3);
    for v in &vectors {
        assert_eq!(v.len(), 4);
        assert!((norm(v) - 1.0).abs() < 1e-5, "not normalised: {v:?}");
    }
    assert!(
        (vectors[2][3] - 1.0).abs() < 1e-5,
        "zebra vector out of order"
    );
    assert_eq!(vectors[0][3], 0.0);
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn embedder_sends_authorization_and_dimensions_only_when_set() {
    let fake = Fake::start().await;
    embedder(&fake, None, None)
        .embed(&strings(&["x"]))
        .await
        .unwrap();
    let v = embedder(&fake, Some("sk-1"), Some(3))
        .embed(&strings(&["x"]))
        .await
        .unwrap();
    assert_eq!(v[0].len(), 3);
    let requests = fake.requests();
    assert_eq!(requests[0].authorization, None);
    assert!(requests[0].body.get("dimensions").is_none());
    assert_eq!(requests[0].body["model"], "fake-embed");
    assert_eq!(requests[1].authorization.as_deref(), Some("Bearer sk-1"));
    assert_eq!(requests[1].body["dimensions"], 3);
}

#[tokio::test]
async fn embedder_retries_rate_limits_and_server_errors() {
    let fake = Fake::start().await;
    fake.script(&[(429, Some("0")), (503, Some("0"))]);
    let v = embedder(&fake, None, None)
        .embed(&strings(&["x", "y"]))
        .await
        .unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(fake.request_count(), 3);
}

#[tokio::test]
async fn embedder_gives_up_after_five_attempts() {
    let fake = Fake::start().await;
    fake.script(&[(500, Some("0")); 6]);
    let err = embedder(&fake, None, None)
        .embed(&strings(&["x"]))
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Provider(_)), "{err:?}");
    assert_eq!(fake.request_count(), 5);
}

#[tokio::test]
async fn embedder_does_not_retry_auth_errors() {
    let fake = Fake::start().await;
    fake.script(&[(401, None)]);
    let err = embedder(&fake, None, None)
        .embed(&strings(&["x"]))
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Provider(_)), "{err:?}");
    assert_eq!(fake.request_count(), 1);
}

#[tokio::test]
async fn embedder_splits_refused_batches() {
    let fake = Fake::start().await;
    fake.set_max_batch(Some(1));
    let inputs = strings(&["a", "zebra", "ccc", "dddd"]);
    let v = embedder(&fake, None, None).embed(&inputs).await.unwrap();
    assert_eq!(v.len(), 4);
    assert!((v[1][3] - 1.0).abs() < 1e-5, "order lost while splitting");
    let sizes: Vec<usize> = fake.requests().iter().map(|r| r.inputs().len()).collect();
    assert_eq!(sizes, vec![4, 2, 1, 1, 2, 1, 1]);
}

#[tokio::test]
async fn embedder_reports_a_rejected_single_input() {
    let fake = Fake::start().await;
    fake.set_reject_marker(Some("REJECTME"));
    let err = embedder(&fake, None, None)
        .embed(&strings(&["fine", "REJECTME"]))
        .await
        .unwrap_err();
    assert!(matches!(err, EmbedError::Rejected(_)), "{err:?}");
}

// ── embed_pending ───────────────────────────────────────────────────────────

#[tokio::test]
async fn disabled_embeddings_leave_the_job_untouched() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    let job = insert_job(&state, repo).await;

    assert_eq!(embed::embed_pending(&state, repo, job).await.unwrap(), 0);

    let mut disabled = settings_input(&fake);
    disabled.enabled = false;
    let (saved, queued) = embed::apply_settings(&state, disabled).await.unwrap();
    assert!(!saved.enabled);
    assert_eq!(queued, 0);
    assert_eq!(embed::embed_pending(&state, repo, job).await.unwrap(), 0);

    assert_eq!(job_row(&state, job).await, (None, 0, 0));
    assert_eq!(fake.request_count(), 0);
}

#[tokio::test]
async fn embed_pending_embeds_every_chunk_in_batches() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    assert!(total >= 3, "fixture produced {total} chunks");
    let before = fake.request_count();
    let job = insert_job(&state, repo).await;

    let embedded = embed::embed_pending(&state, repo, job).await.unwrap();

    assert_eq!(i64::from(embedded), total);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 1").await,
        total
    );
    assert_eq!(
        job_row(&state, job).await,
        (Some("embed".into()), total, total)
    );
    for v in stored_vectors(&state).await {
        assert_eq!(v.len(), 4);
        assert!((norm(&v) - 1.0).abs() < 1e-5);
    }

    let requests = &fake.requests()[before..];
    assert!(
        requests.iter().all(|r| r.inputs().len() <= 2),
        "batch size exceeded"
    );
    assert!(requests.iter().all(|r| r.body.get("dimensions").is_none()));
    let inputs: Vec<String> = requests.iter().flat_map(Recorded::inputs).collect();
    assert_eq!(inputs.len() as i64, total);
    assert!(inputs.iter().all(|i| i.starts_with("doc: ")));
    let (path, symbol_path, content): (String, String, String) = state
        .db
        .read(|c| {
            c.query_row(
                "SELECT f.path, c.symbol_path, CAST(substr(CAST(f.content AS BLOB), c.start_byte + 1, c.end_byte - c.start_byte) AS TEXT)
                 FROM chunks c JOIN files f ON f.id = c.file_id WHERE f.path = 'lib/special.py' LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
        })
        .await
        .unwrap();
    let expected = format!("doc: {path}\n{symbol_path}\n\n{content}");
    assert!(
        inputs.contains(&expected),
        "missing input {expected:?} in {inputs:?}"
    );

    // Nothing pending → nothing requested.
    let after = fake.request_count();
    assert_eq!(embed::embed_pending(&state, repo, job).await.unwrap(), 0);
    assert_eq!(fake.request_count(), after);
}

#[tokio::test]
async fn embed_pending_requests_dimensions_when_configured() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    let mut input = settings_input(&fake);
    input.request_dimensions = Some(3);
    let (saved, _) = embed::apply_settings(&state, input).await.unwrap();
    assert_eq!(saved.dimensions, Some(3));
    let job = insert_job(&state, repo).await;

    embed::embed_pending(&state, repo, job).await.unwrap();

    assert!(fake.requests().iter().all(|r| r.body["dimensions"] == 3));
    let vectors = stored_vectors(&state).await;
    assert!(!vectors.is_empty());
    assert!(vectors
        .iter()
        .all(|v| v.len() == 3 && (norm(v) - 1.0).abs() < 1e-5));
}

#[tokio::test]
async fn embed_pending_survives_rate_limits_and_splits_batches() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;

    fake.script(&[(429, Some("0"))]);
    fake.set_max_batch(Some(1));
    let before = fake.request_count();
    let job = insert_job(&state, repo).await;
    let embedded = embed::embed_pending(&state, repo, job).await.unwrap();

    assert_eq!(i64::from(embedded), total);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 1").await,
        total
    );
    let sizes: Vec<usize> = fake.requests()[before..]
        .iter()
        .map(|r| r.inputs().len())
        .collect();
    assert!(sizes.contains(&2), "no full batch was attempted: {sizes:?}");
    assert!(sizes.contains(&1), "no halved batch was retried: {sizes:?}");
}

#[tokio::test]
async fn rejected_chunk_is_marked_failed_and_the_rest_continues() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    index_files(
        &state,
        repo,
        &[("notes/bad.txt", "REJECTME please\n".to_string())],
    )
    .await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    fake.set_reject_marker(Some("REJECTME"));
    let job = insert_job(&state, repo).await;

    let embedded = embed::embed_pending(&state, repo, job).await.unwrap();

    assert_eq!(i64::from(embedded), total - 1);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 2").await,
        1
    );
    let failed_path: String = state
        .db
        .read(|c| {
            c.query_row(
                "SELECT f.path FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.embed_state = 2",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(failed_path, "notes/bad.txt");
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total - 1
    );
    let (_, embed_total, embed_done) = job_row(&state, job).await;
    assert_eq!((embed_total, embed_done), (total, total));
}

#[tokio::test]
async fn provider_failure_fails_the_job_and_keeps_chunks_pending() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    fake.script(&[(500, Some("0")); 5]);
    let before = fake.request_count();
    let job = insert_job(&state, repo).await;

    let err = embed::embed_pending(&state, repo, job).await.unwrap_err();

    assert!(
        err.message.starts_with("Embedding provider error: "),
        "{}",
        err.message
    );
    assert_eq!(fake.request_count() - before, 5);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total
    );
    assert_eq!(scalar(&state, "SELECT count(*) FROM vec_chunks").await, 0);
}

// ── apply_settings: fingerprint and re-embedding ───────────────────────────

#[tokio::test]
async fn identical_settings_keep_vectors_and_changes_reset_them() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let r1 = insert_repo(&state, "acme/api", "connected").await;
    let _r2 = insert_repo(&state, "acme/web", "connected").await;
    index_sample(&state, r1).await;
    let (_, queued) = embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    assert_eq!(queued, 2);
    let job = insert_job(&state, r1).await;
    embed::embed_pending(&state, r1, job).await.unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );

    // Same settings, and a query prefix change (not part of the fingerprint): nothing re-embeds.
    let (_, queued) = embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    assert_eq!(queued, 0);
    let mut input = settings_input(&fake);
    input.query_prefix = "search_query: ".into();
    let (_, queued) = embed::apply_settings(&state, input).await.unwrap();
    assert_eq!(queued, 0);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 1").await,
        total
    );

    // Another model: table recreated, everything pending again.
    let mut input = settings_input(&fake);
    input.model = "other-model".into();
    let (_, queued) = embed::apply_settings(&state, input.clone()).await.unwrap();
    assert_eq!(queued, 2);
    assert_eq!(scalar(&state, "SELECT count(*) FROM vec_chunks").await, 0);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total
    );

    // Another document prefix: same.
    embed::embed_pending(&state, r1, job).await.unwrap();
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
    input.document_prefix = "search_document: ".into();
    let (_, queued) = embed::apply_settings(&state, input).await.unwrap();
    assert_eq!(queued, 2);
    assert_eq!(scalar(&state, "SELECT count(*) FROM vec_chunks").await, 0);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total
    );
}

// ── search with vectors ─────────────────────────────────────────────────────

#[tokio::test]
async fn vectors_make_search_hybrid_with_semantic_only_hits() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let repo = insert_repo(&state, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = insert_job(&state, repo).await;
    embed::embed_pending(&state, repo, job).await.unwrap();

    let response = search::search(
        &state,
        SearchRequest {
            query: "zebra".into(),
            filter: Filter {
                repo_ids: vec![repo],
                ..Filter::default()
            },
            limit: Some(8),
        },
    )
    .await
    .unwrap();

    assert_eq!(response.mode, "hybrid");
    let first = response.hits.first().expect("no hits");
    assert!(first.content.contains("frobnicate"), "first hit: {first:?}");
    assert_eq!(first.matched_by, vec!["semantic"]);
    let last = fake.requests().last().unwrap().inputs();
    assert_eq!(last, vec!["query: zebra".to_string()]);
}

// ── HTTP: /api/settings/embeddings ─────────────────────────────────────────

#[tokio::test]
async fn test_endpoint_reports_without_storing() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = session_for(&state, "alex", "owner").await;

    let (status, body, _) = call(
        &app,
        "POST",
        "/api/settings/embeddings/test",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["dimensions"], 4);
    assert!(body["latencyMs"].is_u64());

    fake.script(&[(401, None)]);
    let (status, body, _) = call(
        &app,
        "POST",
        "/api/settings/embeddings/test",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));

    assert_eq!(scalar(&state, "SELECT count(*) FROM settings").await, 0);
    let (_, body, _) = call(&app, "GET", "/api/settings/embeddings", &owner, None).await;
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn put_enables_embeddings_and_queues_reembedding() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = session_for(&state, "alex", "owner").await;
    let member = session_for(&state, "sam", "member").await;
    let r1 = insert_repo(&state, "acme/api", "connected").await;
    let r2 = insert_repo(&state, "acme/web", "connected").await;
    let _r3 = insert_repo(&state, "acme/old", "disconnected").await;
    index_sample(&state, r1).await;
    index_files(
        &state,
        r2,
        &[("main.go", "package main\n\nfunc main() {}\n".to_string())],
    )
    .await;
    state
        .db
        .write(move |c| c.execute("UPDATE chunks SET embed_state = 1 WHERE repo_id = ?1", [r1]))
        .await
        .unwrap();

    let mut body = settings_body(&fake);
    body["apiKey"] = json!("sk-secret-123");
    let (status, saved, text) =
        call(&app, "PUT", "/api/settings/embeddings", &owner, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(saved["enabled"], true);
    assert_eq!(saved["dimensions"], 4);
    assert_eq!(saved["apiKeySet"], true);
    assert!(saved.get("apiKey").is_none());
    assert!(!text.contains("sk-secret-123"));
    assert_eq!(saved["reembedRepositories"], 2);
    assert_eq!(
        fake.requests().last().unwrap().authorization.as_deref(),
        Some("Bearer sk-secret-123")
    );

    assert_eq!(
        scalar(
            &state,
            "SELECT count(*) FROM sqlite_master WHERE name = 'vec_chunks'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state <> 0").await,
        0
    );
    let queued: Vec<i64> = state
        .db
        .read(|c| {
            let mut stmt = c.prepare(
                "SELECT repo_id FROM jobs WHERE trigger = 'settings' AND status = 'queued' ORDER BY repo_id",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
        .await
        .unwrap();
    assert_eq!(queued, vec![r1, r2]);

    let (status, got, text) = call(&app, "GET", "/api/settings/embeddings", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["apiKeySet"], true);
    assert!(!text.contains("sk-secret-123"));
    assert!(got["totalChunks"].as_i64().unwrap() > 0);
    assert_eq!(got["embeddedChunks"], 0);
    assert_eq!(got["failedChunks"], 0);

    // Saved key is reused when the body has no apiKey.
    let (status, _, _) = call(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        fake.requests().last().unwrap().authorization.as_deref(),
        Some("Bearer sk-secret-123")
    );

    // clearApiKey removes it; no Authorization header afterwards.
    let mut body = settings_body(&fake);
    body["clearApiKey"] = json!(true);
    let (status, saved, _) =
        call(&app, "PUT", "/api/settings/embeddings", &owner, Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["apiKeySet"], false);
    assert_eq!(fake.requests().last().unwrap().authorization, None);
    assert_eq!(
        scalar(
            &state,
            "SELECT count(*) FROM settings WHERE key = 'embeddings_api_key'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn put_rejects_members_invalid_settings_and_failing_providers() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = session_for(&state, "alex", "owner").await;
    let member = session_for(&state, "sam", "member").await;

    let (status, body, _) = call(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &member,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "owner_required");

    let mut bad_url = settings_body(&fake);
    bad_url["baseUrl"] = json!("ftp://models.example");
    let (status, body, _) = call(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &owner,
        Some(bad_url),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_embedding_settings");

    let mut no_model = settings_body(&fake);
    no_model["model"] = json!("  ");
    let (status, body, _) = call(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &owner,
        Some(no_model),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_embedding_settings");

    fake.script(&[(401, None)]);
    let (status, body, _) = call(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "embedding_test_failed");
    assert!(body["error"]["message"].as_str().unwrap().contains("401"));

    assert_eq!(scalar(&state, "SELECT count(*) FROM settings").await, 0);
}

// ── HTTP: /api/settings/github ─────────────────────────────────────────────

#[tokio::test]
async fn github_settings_need_credentials() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let app = sourcelens::http::router(state.clone());
    let owner = session_for(&state, "alex", "owner").await;
    let (status, body, _) = call(&app, "GET", "/api/settings/github", &owner, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "app_not_configured");
}

#[tokio::test]
async fn github_settings_with_env_credentials() {
    let tmp = TempDir::new().unwrap();
    let mut cfg = Config::new("http://localhost:3000", tmp.path());
    cfg.github = Some(AppCredentials {
        app_id: 42,
        slug: "sourcelens-test".into(),
        client_id: "Iv1.test".into(),
        client_secret: "secret".into(),
        webhook_secret: Some("whsec".into()),
        private_key_pem: include_str!("fixtures/test_only_app_key.pem").into(),
        html_url: "https://github.com/apps/sourcelens-test".into(),
        source: "env",
    });
    let state = AppState::new(cfg).unwrap();
    let app = sourcelens::http::router(state.clone());
    let member = session_for(&state, "sam", "member").await;

    let (status, body, _) = call(&app, "GET", "/api/settings/github", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "env");
    assert_eq!(body["slug"], "sourcelens-test");
    assert_eq!(body["appUrl"], "https://github.com/apps/sourcelens-test");
    assert_eq!(
        body["installUrl"],
        "https://github.com/apps/sourcelens-test/installations/new"
    );
    assert_eq!(body["webhookUrl"], "http://localhost:3000/webhooks/github");
    assert_eq!(body["webhookActive"], false);
    assert_eq!(body["lastDeliveryAt"], Value::Null);
    assert_eq!(body["lastDeliveryEvent"], Value::Null);

    state
        .db
        .write(|c| {
            c.execute_batch(
                "INSERT INTO webhook_deliveries (delivery_id, event, received_at) VALUES ('a', 'push', 100), ('b', 'repository', 200)",
            )
        })
        .await
        .unwrap();
    let (_, body, _) = call(&app, "GET", "/api/settings/github", &member, None).await;
    assert_eq!(body["lastDeliveryAt"], 200);
    assert_eq!(body["lastDeliveryEvent"], "repository");
}

// ── HTTP: /api/search ──────────────────────────────────────────────────────

#[tokio::test]
async fn search_endpoint_scopes_to_connected_repositories() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp);
    let app = sourcelens::http::router(state.clone());
    let member = session_for(&state, "sam", "member").await;
    let r1 = insert_repo(&state, "acme/api", "connected").await;
    let r2 = insert_repo(&state, "acme/old", "disconnected").await;
    index_sample(&state, r1).await;
    index_sample(&state, r2).await;

    let (status, body, text) = call(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "frobnicate" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(body["mode"], "lexical");
    let results = body["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| r["repository"] == "acme/api"));
    assert_eq!(results[0]["path"], "lib/special.py");
    assert!(results[0]["startLine"].is_u64());
    assert!(results[0]["matchedBy"].is_array());

    let (_, body, _) = call(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "frobnicate", "repositoryIds": [r2] })),
    )
    .await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0);

    let (status, body, _) = call(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "empty_query");
}
