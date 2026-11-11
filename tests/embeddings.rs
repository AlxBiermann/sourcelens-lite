mod common;

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use common::{
    api, functions_source, index_files, index_sample, insert_repo_async, member_session, new_state,
    owner_session, scalar, serve,
};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use sourcelens::index::embed::{self, EmbeddingConfig, EmbeddingSettingsInput};
use sourcelens::index::embedder::{EmbedError, Embedder};
use sourcelens::repositories::{self, Target};
use sourcelens::search::{self, Filter, SearchRequest};
use sourcelens::state::AppState;
use tempfile::TempDir;

const CONNECTION_TEST_INPUT: &str = "sourcelens connection test";

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
    /// Raw `(status, body)` responses served before everything else.
    raw_script: Mutex<VecDeque<(u16, String)>>,
    /// While set, embedding requests (not the connection test) wait here for a permit.
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// Requests that reached the gate.
    gated: AtomicUsize,
    /// Latency added to every request.
    delay: Mutex<Option<Duration>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

/// Counts a request as in flight until dropped.
struct InFlight<'a>(&'a AtomicUsize);

impl<'a> InFlight<'a> {
    fn enter(state: &'a FakeState) -> Self {
        let now = state.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        state.max_in_flight.fetch_max(now, Ordering::SeqCst);
        Self(&state.in_flight)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
        let base = serve(app).await;
        Self {
            state,
            base_url: format!("{base}/v1"),
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

    fn script_raw(&self, status: u16, body: &str) {
        self.state
            .raw_script
            .lock()
            .unwrap()
            .push_back((status, body.to_string()));
    }

    /// Holds every embedding request until `open_gate`.
    fn close_gate(&self) {
        *self.state.gate.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    }

    /// Releases the held requests and lets later ones through.
    fn open_gate(&self) {
        if let Some(gate) = self.state.gate.lock().unwrap().take() {
            gate.add_permits(10_000);
        }
    }

    fn gated(&self) -> usize {
        self.state.gated.load(Ordering::SeqCst)
    }

    fn set_delay(&self, delay: Option<Duration>) {
        *self.state.delay.lock().unwrap() = delay;
    }

    fn max_in_flight(&self) -> usize {
        self.state.max_in_flight.load(Ordering::SeqCst)
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
    let _in_flight = InFlight::enter(&state);
    let gate = state.gate.lock().unwrap().clone();
    if let Some(gate) = gate {
        if !inputs.iter().any(|i| i == CONNECTION_TEST_INPUT) {
            state.gated.fetch_add(1, Ordering::SeqCst);
            gate.acquire().await.unwrap().forget();
        }
    }
    let delay = *state.delay.lock().unwrap();
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let raw = state.raw_script.lock().unwrap().pop_front();
    if let Some((status, body)) = raw {
        return (StatusCode::from_u16(status).unwrap(), body).into_response();
    }

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

fn settings_input(fake: &Fake) -> EmbeddingSettingsInput {
    EmbeddingSettingsInput {
        config: EmbeddingConfig {
            enabled: true,
            base_url: fake.base_url.clone(),
            model: "fake-embed".into(),
            request_dimensions: None,
            query_prefix: "query: ".into(),
            document_prefix: "doc: ".into(),
            batch_size: 2,
        },
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

/// Starts a job the way a worker claims one: the repository's queued job becomes running; without
/// one, a running job is inserted.
async fn start_job(state: &AppState, repo_id: i64) -> i64 {
    state
        .db
        .write(move |c| {
            let claimed: Option<i64> = c
                .query_row(
                    "UPDATE jobs SET status = 'running' WHERE repo_id = ?1 AND status = 'queued' RETURNING id",
                    [repo_id],
                    |r| r.get(0),
                )
                .optional()?;
            match claimed {
                Some(id) => Ok(id),
                None => c.query_row(
                    "INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'running', 'manual', 0) RETURNING id",
                    [repo_id],
                    |r| r.get(0),
                ),
            }
        })
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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let job = start_job(&state, repo).await;

    assert_eq!(embed::embed_pending(&state, repo, job).await.unwrap(), 0);

    let mut disabled = settings_input(&fake);
    disabled.config.enabled = false;
    let (saved, queued) = embed::apply_settings(&state, disabled).await.unwrap();
    assert!(!saved.config.enabled);
    assert_eq!(queued, 0);
    assert_eq!(embed::embed_pending(&state, repo, job).await.unwrap(), 0);

    assert_eq!(job_row(&state, job).await, (None, 0, 0));
    assert_eq!(fake.request_count(), 0);
}

#[tokio::test]
async fn embed_pending_embeds_every_chunk_in_batches() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    assert!(total >= 3, "fixture produced {total} chunks");
    let before = fake.request_count();
    let job = start_job(&state, repo).await;

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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    let mut input = settings_input(&fake);
    input.config.request_dimensions = Some(3);
    let (saved, _) = embed::apply_settings(&state, input).await.unwrap();
    assert_eq!(saved.dimensions, Some(3));
    let job = start_job(&state, repo).await;

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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;

    fake.script(&[(429, Some("0"))]);
    fake.set_max_batch(Some(1));
    let before = fake.request_count();
    let job = start_job(&state, repo).await;
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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
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
    let job = start_job(&state, repo).await;

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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let total = scalar(&state, "SELECT count(*) FROM chunks").await;
    // The provider is down for every request of every concurrent batch.
    fake.script(&[(500, Some("0")); 100]);
    let before = fake.request_count();
    let job = start_job(&state, repo).await;

    let err = embed::embed_pending(&state, repo, job).await.unwrap_err();

    assert!(
        err.message.starts_with("Embedding provider error: "),
        "{}",
        err.message
    );
    let sent = fake.request_count() - before;
    assert!(
        (5..=5 * IN_FLIGHT).contains(&sent),
        "{sent} requests: one batch must use all five attempts, nothing beyond the batches in flight"
    );
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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let r1 = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let _r2 = insert_repo_async(&state, 2, "acme/web", "connected").await;
    index_sample(&state, r1).await;
    let (_, queued) = embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    assert_eq!(queued, 2);
    let job = start_job(&state, r1).await;
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
    input.config.query_prefix = "search_query: ".into();
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
    input.config.model = "other-model".into();
    let (_, queued) = embed::apply_settings(&state, input.clone()).await.unwrap();
    assert_eq!(queued, 2);
    assert_eq!(scalar(&state, "SELECT count(*) FROM vec_chunks").await, 0);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total
    );

    // Another document prefix: same.
    let job = start_job(&state, r1).await;
    embed::embed_pending(&state, r1, job).await.unwrap();
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
    input.config.document_prefix = "search_document: ".into();
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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;
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

#[tokio::test]
async fn failing_provider_makes_search_lexical_after_one_attempt() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_sample(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;
    embed::embed_pending(&state, repo, job).await.unwrap();

    // Enough failures for every retry an indexing call would make.
    fake.script(&[(503, None); 5]);
    let before = fake.requests().len();
    let started = std::time::Instant::now();
    let response = search::search(
        &state,
        SearchRequest {
            query: "frobnicate".into(),
            filter: Filter {
                repo_ids: vec![repo],
                ..Filter::default()
            },
            limit: Some(8),
        },
    )
    .await
    .unwrap();

    assert_eq!(response.mode, "lexical");
    assert!(!response.hits.is_empty());
    assert_eq!(fake.requests().len() - before, 1);
    // The first retry would wait at least one second.
    assert!(
        started.elapsed() < std::time::Duration::from_millis(900),
        "search took {:?}",
        started.elapsed()
    );
}

// ── HTTP: /api/settings/embeddings ─────────────────────────────────────────

#[tokio::test]
async fn test_endpoint_reports_without_storing() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;

    let (status, body, _) = api(
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
    let (status, body, _) = api(
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
    let (_, body, _) = api(&app, "GET", "/api/settings/embeddings", &owner, None).await;
    assert_eq!(body["enabled"], false);
}

#[tokio::test]
async fn put_enables_embeddings_and_queues_reembedding() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;
    let member = member_session(&state, "sam").await;
    let r1 = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let r2 = insert_repo_async(&state, 2, "acme/web", "connected").await;
    let _r3 = insert_repo_async(&state, 3, "acme/old", "disconnected").await;
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
        api(&app, "PUT", "/api/settings/embeddings", &owner, Some(body)).await;
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

    let (status, got, text) = api(&app, "GET", "/api/settings/embeddings", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["apiKeySet"], true);
    assert!(!text.contains("sk-secret-123"));
    assert!(got["totalChunks"].as_i64().unwrap() > 0);
    assert_eq!(got["embeddedChunks"], 0);
    assert_eq!(got["failedChunks"], 0);

    // Saved key is reused when the body has no apiKey.
    let (status, _, _) = api(
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
    let (status, saved, _) = api(&app, "PUT", "/api/settings/embeddings", &owner, Some(body)).await;
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
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;
    let member = member_session(&state, "sam").await;

    let (status, body, _) = api(
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
    let (status, body, _) = api(
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
    let (status, body, _) = api(
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
    let (status, body, _) = api(
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

// ── embed phase: stop conditions and concurrency ───────────────────────────

/// Waits up to five seconds for `done`.
async fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Twelve function chunks, i.e. six batches at batch size 2. Returns the chunk count.
async fn index_many(state: &AppState, repo_id: i64) -> i64 {
    index_files(
        state,
        repo_id,
        &[("src/lib.rs", functions_source("alpha", 12))],
    )
    .await;
    scalar(state, "SELECT count(*) FROM chunks").await
}

fn spawn_embed(
    state: &AppState,
    repo_id: i64,
    job_id: i64,
) -> tokio::task::JoinHandle<sourcelens::error::ApiResult<u32>> {
    let state = state.clone();
    tokio::spawn(async move { embed::embed_pending(&state, repo_id, job_id).await })
}

async fn finish_job(state: &AppState, job_id: i64) {
    state
        .db
        .write(move |c| {
            c.execute(
                "UPDATE jobs SET status = 'succeeded' WHERE id = ?1",
                [job_id],
            )
        })
        .await
        .unwrap();
}

/// The embed phase keeps four requests in flight; with six batches pending and the provider holding
/// every request, exactly four arrive before anything completes.
const IN_FLIGHT: usize = 4;

#[tokio::test]
async fn settings_saved_during_embedding_stop_the_running_job() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let total = index_many(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;

    fake.close_gate();
    let running = spawn_embed(&state, repo, job);
    wait_until("requests of the old settings", || fake.gated() == IN_FLIGHT).await;

    // Same model and dimensions, another document prefix.
    let mut changed = settings_input(&fake);
    changed.config.document_prefix = "new: ".into();
    let (_, queued) = embed::apply_settings(&state, changed).await.unwrap();
    assert_eq!(queued, 1);
    fake.open_gate();

    assert_eq!(running.await.unwrap().unwrap(), 0);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        0,
        "a vector of the old settings was stored after the change"
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total
    );

    // The queued `settings` job embeds everything with the new prefix.
    finish_job(&state, job).await;
    let next = start_job(&state, repo).await;
    assert_ne!(next, job);
    let trigger: String = state
        .db
        .read(move |c| {
            c.query_row("SELECT trigger FROM jobs WHERE id = ?1", [next], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(trigger, "settings");
    let before = fake.request_count();
    let embedded = embed::embed_pending(&state, repo, next).await.unwrap();
    assert_eq!(i64::from(embedded), total);
    let inputs: Vec<String> = fake.requests()[before..]
        .iter()
        .flat_map(Recorded::inputs)
        .collect();
    assert_eq!(inputs.len() as i64, total);
    assert!(inputs.iter().all(|i| i.starts_with("new: ")), "{inputs:?}");
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
}

#[tokio::test]
async fn disconnect_during_embedding_fails_the_job_and_stops_requests() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let total = index_many(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;

    fake.close_gate();
    let before = fake.request_count();
    let running = spawn_embed(&state, repo, job);
    wait_until("embedding requests", || fake.gated() == IN_FLIGHT).await;
    state
        .db
        .write(move |c| repositories::disconnect(c, Target::Id(repo), "Access removed on GitHub"))
        .await
        .unwrap();
    fake.open_gate();

    let err = running.await.unwrap().unwrap_err();
    assert_eq!(err.message, "Repository was disconnected.");
    let sent = fake.request_count();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fake.request_count(),
        sent,
        "the provider was called after the job failed"
    );
    let batches = (total + 1) / 2;
    assert!(
        ((sent - before) as i64) < batches,
        "every batch was requested"
    );
    assert_eq!(scalar(&state, "SELECT count(*) FROM vec_chunks").await, 0);
}

#[tokio::test]
async fn queued_job_makes_the_embed_phase_hand_over() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let total = index_many(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;

    fake.close_gate();
    let running = spawn_embed(&state, repo, job);
    wait_until("embedding requests", || fake.gated() == IN_FLIGHT).await;
    // A push arrives.
    state
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'queued', 'webhook', 0)",
                [repo],
            )
        })
        .await
        .unwrap();
    fake.open_gate();

    let embedded = i64::from(running.await.unwrap().unwrap());
    assert!(embedded < total, "the phase did not return early");
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 0").await,
        total - embedded
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        embedded
    );
}

#[tokio::test]
async fn batches_run_concurrently_and_each_chunk_is_embedded_once() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let total = index_many(&state, repo).await;
    embed::apply_settings(&state, settings_input(&fake))
        .await
        .unwrap();
    let job = start_job(&state, repo).await;
    fake.set_delay(Some(Duration::from_millis(50)));
    let before = fake.request_count();

    let embedded = embed::embed_pending(&state, repo, job).await.unwrap();

    assert_eq!(i64::from(embedded), total);
    assert!(fake.max_in_flight() > 1, "requests were sequential");
    let inputs: Vec<String> = fake.requests()[before..]
        .iter()
        .flat_map(Recorded::inputs)
        .collect();
    assert_eq!(inputs.len() as i64, total);
    assert_eq!(inputs.iter().collect::<HashSet<_>>().len() as i64, total);
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM vec_chunks").await,
        total
    );
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM chunks WHERE embed_state = 1").await,
        total
    );
}

// ── saved API key and provider errors ──────────────────────────────────────

#[tokio::test]
async fn saved_api_key_is_only_sent_to_its_base_url() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let saved_provider = Fake::start().await;
    let other_provider = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;

    let mut body = settings_body(&saved_provider);
    body["apiKey"] = json!("sk-secret-123");
    let (status, _, text) = api(&app, "PUT", "/api/settings/embeddings", &owner, Some(body)).await;
    assert_eq!(status, StatusCode::OK, "{text}");

    // The saved base URL (a trailing slash does not matter) keeps using the saved key.
    let mut same = settings_body(&saved_provider);
    same["baseUrl"] = json!(format!("{}/", saved_provider.base_url));
    let (status, body, _) = api(
        &app,
        "POST",
        "/api/settings/embeddings/test",
        &owner,
        Some(same),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(
        saved_provider
            .requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer sk-secret-123")
    );

    // A new base URL without a key is refused before any request is sent.
    for (method, path) in [
        ("POST", "/api/settings/embeddings/test"),
        ("PUT", "/api/settings/embeddings"),
    ] {
        let (status, body, _) = api(
            &app,
            method,
            path,
            &owner,
            Some(settings_body(&other_provider)),
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{method} {path}");
        assert_eq!(body["error"]["code"], "api_key_required");
    }
    assert_eq!(other_provider.request_count(), 0);

    // With its own key the new base URL is accepted.
    let mut with_key = settings_body(&other_provider);
    with_key["apiKey"] = json!("sk-other");
    let (status, _, text) = api(
        &app,
        "PUT",
        "/api/settings/embeddings",
        &owner,
        Some(with_key),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        other_provider
            .requests()
            .last()
            .unwrap()
            .authorization
            .as_deref(),
        Some("Bearer sk-other")
    );
}

#[tokio::test]
async fn provider_error_bodies_are_not_echoed() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let fake = Fake::start().await;
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;

    fake.script_raw(404, "<html>admin console of 10.0.0.7</html>");
    let (_, body, _) = api(
        &app,
        "POST",
        "/api/settings/embeddings/test",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(body["ok"], false);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("404"), "{error}");
    assert!(!error.contains("10.0.0.7"), "{error}");

    fake.script_raw(
        401,
        r#"{"error":{"message":"invalid api key"},"detail":"trace id 98765"}"#,
    );
    let (_, body, _) = api(
        &app,
        "POST",
        "/api/settings/embeddings/test",
        &owner,
        Some(settings_body(&fake)),
    )
    .await;
    assert_eq!(body["ok"], false);
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("401") && error.contains("invalid api key"),
        "{error}"
    );
    assert!(!error.contains("98765"), "{error}");
}
