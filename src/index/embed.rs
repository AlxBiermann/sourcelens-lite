use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use reqwest::header::RETRY_AFTER;
use reqwest::Url;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::crypto::sha256_hex;
use crate::db::sql::text_column;
use crate::db::{delete_setting, get_setting, put_setting};
use crate::error::{ApiError, ApiResult};
use crate::index::{jobs, store};
use crate::model::{JobPhase, Trigger, EMBED_DONE, EMBED_PENDING, EMBED_REJECTED};
use crate::repositories;
use crate::state::AppState;

const SETTINGS_KEY: &str = "embeddings";
const API_KEY_KEY: &str = "embeddings_api_key";
const FINGERPRINT_KEY: &str = "vector_fingerprint";

const DEFAULT_BATCH_SIZE: u32 = 32;
const MAX_BATCH_SIZE: u32 = 256;
/// Largest vector sqlite-vec accepts.
const MAX_DIMENSIONS: u32 = 8192;
const MAX_INPUT_CHARS: usize = 8000;
const MAX_ATTEMPTS: u32 = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const CONNECTION_TEST_INPUT: &str = "sourcelens connection test";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct EmbeddingSettings {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub request_dimensions: Option<u32>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub batch_size: u32,
    /// Detected from the provider; read-only.
    pub dimensions: Option<u32>,
    /// Read-only; derived from `settings['embeddings_api_key']`.
    pub api_key_set: bool,
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            model: String::new(),
            request_dimensions: None,
            query_prefix: String::new(),
            document_prefix: String::new(),
            batch_size: DEFAULT_BATCH_SIZE,
            dimensions: None,
            api_key_set: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettingsInput {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub request_dimensions: Option<u32>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub batch_size: u32,
    pub api_key: Option<String>,
    pub clear_api_key: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// The provider refused a single input (400/413).
    #[error("{0}")]
    Rejected(String),
    /// Network error, unexpected status or malformed response after all retries.
    #[error("{0}")]
    Provider(String),
}

/// Client for an OpenAI-compatible `POST {baseUrl}/embeddings` endpoint.
pub struct Embedder {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    request_dimensions: Option<u32>,
}

/// Outcome of one batch request after retries.
enum Failure {
    /// 400 or 413: the batch (or input) was refused; halving may help.
    Refused(String),
    Provider(String),
}

type PartialResult = Result<Vec<Result<Vec<f32>, String>>, String>;

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    index: Option<usize>,
}

impl Embedder {
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        request_dimensions: Option<u32>,
    ) -> Self {
        Self {
            http,
            url: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            request_dimensions,
        }
    }

    /// Embeds `inputs` in order and returns L2-normalised vectors.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let results = self
            .embed_partial(inputs)
            .await
            .map_err(EmbedError::Provider)?;
        results
            .into_iter()
            .map(|r| r.map_err(EmbedError::Rejected))
            .collect()
    }

    /// Like `embed`, but a refused single input yields `Err(message)` in its slot instead of failing
    /// the whole call. Refused batches are split in halves and retried.
    fn embed_partial<'a>(
        &'a self,
        inputs: &'a [String],
    ) -> Pin<Box<dyn Future<Output = PartialResult> + Send + 'a>> {
        Box::pin(async move {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }
            match self.request(inputs).await {
                Ok(vectors) => Ok(vectors.into_iter().map(Ok).collect()),
                Err(Failure::Refused(_)) if inputs.len() > 1 => {
                    let (left, right) = inputs.split_at(inputs.len() / 2);
                    let mut out = self.embed_partial(left).await?;
                    out.extend(self.embed_partial(right).await?);
                    Ok(out)
                }
                Err(Failure::Refused(message)) => Ok(vec![Err(message)]),
                Err(Failure::Provider(message)) => Err(message),
            }
        })
    }

    /// One batch with retries on network errors, 429 and 5xx.
    async fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, Failure> {
        let mut body = serde_json::json!({ "model": self.model, "input": inputs });
        if let Some(dimensions) = self.request_dimensions {
            body["dimensions"] = dimensions.into();
        }
        let mut last_error = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let mut builder = self.http.post(&self.url).json(&body);
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            let retry_after = match builder.send().await {
                Err(err) => {
                    last_error = format!("could not reach {}: {err}", self.url);
                    None
                }
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return parse_response(response, inputs.len()).await;
                    }
                    let retry_after = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let message = error_message(status, &response.text().await.unwrap_or_default());
                    if status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE
                    {
                        return Err(Failure::Refused(message));
                    }
                    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                        return Err(Failure::Provider(message));
                    }
                    last_error = message;
                    retry_after
                }
            };
            if attempt + 1 < MAX_ATTEMPTS {
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
            }
        }
        Err(Failure::Provider(last_error))
    }
}

async fn parse_response(
    response: reqwest::Response,
    expected: usize,
) -> Result<Vec<Vec<f32>>, Failure> {
    let parsed: EmbeddingsResponse = response
        .json()
        .await
        .map_err(|err| Failure::Provider(format!("unreadable response: {err}")))?;
    if parsed.data.len() != expected {
        return Err(Failure::Provider(format!(
            "expected {expected} embeddings, received {}",
            parsed.data.len()
        )));
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (position, item) in parsed.data.into_iter().enumerate() {
        let index = item.index.unwrap_or(position);
        match slots.get_mut(index) {
            Some(slot @ None) => *slot = Some(normalize(item.embedding)),
            _ => {
                return Err(Failure::Provider(format!(
                    "invalid embedding index {index} in response"
                )))
            }
        }
    }
    slots
        .into_iter()
        .map(|slot| {
            slot.filter(|v| !v.is_empty())
                .ok_or_else(|| Failure::Provider("response contains an empty embedding".into()))
        })
        .collect()
}

fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        for x in &mut vector {
            *x /= norm;
        }
    }
    vector
}

fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let base = retry_after.unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(6)));
    let mut jitter = [0u8; 2];
    let jitter_ms = match SystemRandom::new().fill(&mut jitter) {
        Ok(()) => u64::from(u16::from_le_bytes(jitter)) % 251,
        Err(_) => 0,
    };
    base.min(MAX_RETRY_DELAY) + Duration::from_millis(jitter_ms)
}

fn error_message(status: StatusCode, body: &str) -> String {
    let from_json = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            let message = v
                .pointer("/error/message")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .or_else(|| v.get("detail"))?;
            message.as_str().map(str::to_string)
        });
    let message = from_json.unwrap_or_else(|| body.trim().chars().take(300).collect());
    if message.is_empty() {
        format!("HTTP {}", status.as_u16())
    } else {
        format!("HTTP {}: {message}", status.as_u16())
    }
}

/// Stored settings plus the sealed API key, read in one go.
async fn stored(state: &AppState) -> ApiResult<(EmbeddingSettings, Option<String>)> {
    let (json, sealed) = state
        .db
        .read(|conn| {
            Ok::<_, rusqlite::Error>((
                get_setting(conn, SETTINGS_KEY)?,
                get_setting(conn, API_KEY_KEY)?,
            ))
        })
        .await?;
    let mut settings = match json {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(ApiError::internal)?,
        None => EmbeddingSettings::default(),
    };
    let api_key = match sealed {
        Some(sealed) => {
            let plain = state
                .key
                .open(&sealed)
                .ok_or_else(|| ApiError::internal("cannot decrypt the embeddings API key"))?;
            Some(String::from_utf8(plain).map_err(ApiError::internal)?)
        }
        None => None,
    };
    settings.api_key_set = api_key.is_some();
    Ok((settings, api_key))
}

/// Current settings (defaults when never saved).
pub async fn settings(state: &AppState) -> ApiResult<EmbeddingSettings> {
    Ok(stored(state).await?.0)
}

/// `None` when embeddings are disabled.
pub async fn load(state: &AppState) -> Result<Option<(EmbeddingSettings, Embedder)>, ApiError> {
    let (settings, api_key) = stored(state).await?;
    if !settings.enabled {
        return Ok(None);
    }
    let embedder = Embedder::new(
        state.http.clone(),
        &settings.base_url,
        &settings.model,
        api_key,
        settings.request_dimensions,
    );
    Ok(Some((settings, embedder)))
}

struct PendingChunk {
    id: i64,
    input: String,
}

fn pending_batch(
    conn: &Connection,
    repo_id: i64,
    limit: u32,
    prefix: &str,
) -> rusqlite::Result<Vec<PendingChunk>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, f.path, c.symbol_path,
                substr(CAST(f.content AS BLOB), c.start_byte + 1, c.end_byte - c.start_byte)
         FROM chunks c JOIN files f ON f.id = c.file_id
         WHERE c.repo_id = ?1 AND c.embed_state = ?3
         ORDER BY c.id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![repo_id, limit, EMBED_PENDING], |row| {
        let path: String = row.get(1)?;
        let symbol_path: String = row.get(2)?;
        let content = text_column(row, 3)?;
        let full = format!(
            "{prefix}{}",
            store::embedding_input(&path, &symbol_path, &content)
        );
        let input = match full.char_indices().nth(MAX_INPUT_CHARS) {
            Some((cut, _)) => full[..cut].to_string(),
            None => full,
        };
        Ok(PendingChunk {
            id: row.get(0)?,
            input,
        })
    })?;
    rows.collect()
}

/// Embeds every pending chunk of the repository. Returns the number of chunks embedded.
pub async fn embed_pending(state: &AppState, repo_id: i64, job_id: i64) -> ApiResult<u32> {
    let Some((settings, embedder)) = load(state).await? else {
        return Ok(0);
    };
    if !state.db.read(store::vec_table_exists).await? {
        return Ok(0);
    }
    let dimensions = settings.dimensions.unwrap_or(0) as usize;
    let batch_size = settings.batch_size.clamp(1, MAX_BATCH_SIZE);

    state
        .db
        .write(move |conn| {
            let total: i64 = conn.query_row(
                "SELECT count(*) FROM chunks WHERE repo_id = ?1 AND embed_state = ?2",
                params![repo_id, EMBED_PENDING],
                |row| row.get(0),
            )?;
            conn.execute(
                "UPDATE jobs SET phase = ?3, embed_total = ?1, embed_done = 0 WHERE id = ?2",
                params![total, job_id, JobPhase::Embed],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .await?;

    let mut embedded = 0u32;
    loop {
        let prefix = settings.document_prefix.clone();
        let batch = state
            .db
            .read(move |conn| pending_batch(conn, repo_id, batch_size, &prefix))
            .await?;
        if batch.is_empty() {
            return Ok(embedded);
        }
        let inputs: Vec<String> = batch.iter().map(|c| c.input.clone()).collect();
        let results = embedder
            .embed_partial(&inputs)
            .await
            .map_err(provider_error)?;
        if let Some(wrong) = results
            .iter()
            .flatten()
            .find(|v| dimensions != 0 && v.len() != dimensions)
        {
            return Err(provider_error(format!(
                "the model returned {} dimensions instead of {dimensions}; save the embedding settings again",
                wrong.len()
            )));
        }
        let ids: Vec<i64> = batch.iter().map(|c| c.id).collect();
        embedded += state
            .db
            .write(move |conn| store_batch(conn, repo_id, job_id, &ids, &results))
            .await?;
    }
}

fn store_batch(
    conn: &mut Connection,
    repo_id: i64,
    job_id: i64,
    ids: &[i64],
    results: &[Result<Vec<f32>, String>],
) -> rusqlite::Result<u32> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut embedded = 0u32;
    for (id, result) in ids.iter().zip(results) {
        match result {
            Ok(vector) => {
                // Only chunks that still exist and are still pending get a vector.
                let changed = tx.execute(
                    "UPDATE chunks SET embed_state = ?2 WHERE id = ?1 AND embed_state = ?3",
                    params![id, EMBED_DONE, EMBED_PENDING],
                )?;
                if changed == 1 {
                    tx.execute("DELETE FROM vec_chunks WHERE chunk_id = ?1", [id])?;
                    store::put_vector(&tx, *id, repo_id, &store::to_blob(vector))?;
                    embedded += 1;
                }
            }
            Err(message) => {
                tracing::warn!(
                    chunk_id = id,
                    "embedding provider rejected a chunk: {message}"
                );
                tx.execute(
                    "UPDATE chunks SET embed_state = ?2 WHERE id = ?1 AND embed_state = ?3",
                    params![id, EMBED_REJECTED, EMBED_PENDING],
                )?;
            }
        }
    }
    tx.execute(
        "UPDATE jobs SET embed_done = embed_done + ?1 WHERE id = ?2",
        params![ids.len() as i64, job_id],
    )?;
    tx.commit()?;
    Ok(embedded)
}

fn provider_error(message: String) -> ApiError {
    ApiError::bad_gateway(
        "embedding_provider_error",
        format!("Embedding provider error: {message}"),
    )
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::unprocessable("invalid_embedding_settings", message)
}

/// Checks the fields that must be valid to talk to a provider (`require_provider`) or to be stored.
fn validate(input: &EmbeddingSettingsInput, require_provider: bool) -> ApiResult<()> {
    let base_url = input.base_url.trim();
    if require_provider || !base_url.is_empty() {
        let valid = Url::parse(base_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
        if !valid {
            return Err(invalid("Base URL must be an http or https URL."));
        }
    }
    if require_provider && input.model.trim().is_empty() {
        return Err(invalid("Enter the model name."));
    }
    if !(1..=MAX_BATCH_SIZE).contains(&input.batch_size) {
        return Err(invalid(format!(
            "Batch size must be between 1 and {MAX_BATCH_SIZE}."
        )));
    }
    if let Some(d) = input.request_dimensions {
        if !(1..=MAX_DIMENSIONS).contains(&d) {
            return Err(invalid(format!(
                "Dimensions must be between 1 and {MAX_DIMENSIONS}."
            )));
        }
    }
    Ok(())
}

/// The API key to use for `input`: an explicit new key, none when clearing, else the saved one.
fn resolve_api_key(input: &EmbeddingSettingsInput, saved: Option<String>) -> Option<String> {
    if input.clear_api_key == Some(true) {
        return None;
    }
    match input.api_key.as_deref().map(str::trim) {
        Some(key) if !key.is_empty() => Some(key.to_string()),
        _ => saved,
    }
}

/// Embeds the connection test input and returns the vector length.
async fn connection_test(embedder: &Embedder) -> Result<u32, String> {
    let vectors = embedder
        .embed(&[CONNECTION_TEST_INPUT.to_string()])
        .await
        .map_err(|err| err.to_string())?;
    let dimensions = vectors.first().map_or(0, Vec::len);
    match u32::try_from(dimensions) {
        Ok(d) if (1..=MAX_DIMENSIONS).contains(&d) => Ok(d),
        _ => Err(format!(
            "The provider returned {dimensions} dimensions; SourceLens supports 1 to {MAX_DIMENSIONS}."
        )),
    }
}

/// `POST /api/settings/embeddings/test`: tries the given settings without storing anything.
/// Returns `(dimensions, latency_ms)` or a message for the user.
pub async fn test_connection(
    state: &AppState,
    input: &EmbeddingSettingsInput,
) -> ApiResult<Result<(u32, u64), String>> {
    if let Err(err) = validate(input, true) {
        return Ok(Err(err.message));
    }
    let (_, saved_key) = stored(state).await?;
    let embedder = Embedder::new(
        state.http.clone(),
        input.base_url.trim(),
        input.model.trim(),
        resolve_api_key(input, saved_key),
        input.request_dimensions,
    );
    let started = Instant::now();
    let result = connection_test(&embedder).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(result.map(|d| (d, latency_ms)))
}

fn fingerprint(settings: &EmbeddingSettings, dimensions: u32) -> String {
    sha256_hex(
        format!(
            "{}\n{}\n{}\n{}",
            settings.base_url, settings.model, dimensions, settings.document_prefix
        )
        .as_bytes(),
    )
}

enum KeyChange {
    Keep,
    Set(Vec<u8>),
    Clear,
}

/// Validates, tests and stores new settings. Returns the stored settings and the number of
/// repositories queued for (re-)embedding.
pub async fn apply_settings(
    state: &AppState,
    new: EmbeddingSettingsInput,
) -> ApiResult<(EmbeddingSettings, u32)> {
    validate(&new, new.enabled)?;
    let (current, saved_key) = stored(state).await?;
    let api_key = resolve_api_key(&new, saved_key.clone());
    let key_change = match &api_key {
        None if saved_key.is_some() => KeyChange::Clear,
        Some(key) if saved_key.as_ref() != Some(key) => {
            KeyChange::Set(state.key.seal(key.as_bytes()))
        }
        _ => KeyChange::Keep,
    };

    let mut settings = EmbeddingSettings {
        enabled: new.enabled,
        base_url: new.base_url.trim().trim_end_matches('/').to_string(),
        model: new.model.trim().to_string(),
        request_dimensions: new.request_dimensions,
        query_prefix: new.query_prefix,
        document_prefix: new.document_prefix,
        batch_size: new.batch_size,
        dimensions: current.dimensions,
        api_key_set: api_key.is_some(),
    };

    let mut target = None;
    if settings.enabled {
        let embedder = Embedder::new(
            state.http.clone(),
            &settings.base_url,
            &settings.model,
            api_key,
            settings.request_dimensions,
        );
        let dimensions = connection_test(&embedder)
            .await
            .map_err(|message| ApiError::unprocessable("embedding_test_failed", message))?;
        settings.dimensions = Some(dimensions);
        target = Some((dimensions, fingerprint(&settings, dimensions)));
    }
    let was_enabled = current.enabled;

    let mut stored_json = settings.clone();
    stored_json.api_key_set = false;
    let json = serde_json::to_string(&stored_json).map_err(ApiError::internal)?;
    let repos = state
        .db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            put_setting(&tx, SETTINGS_KEY, &json)?;
            match key_change {
                KeyChange::Keep => {}
                KeyChange::Set(sealed) => put_setting(&tx, API_KEY_KEY, &sealed)?,
                KeyChange::Clear => delete_setting(&tx, API_KEY_KEY)?,
            }
            let mut repos = Vec::new();
            if let Some((dimensions, fingerprint)) = target {
                let stored_fingerprint = get_setting(&tx, FINGERPRINT_KEY)?;
                let unchanged = stored_fingerprint.as_deref() == Some(fingerprint.as_bytes())
                    && store::vec_table_exists(&tx)?;
                if !unchanged {
                    store::create_vec_table(&tx, dimensions)?;
                    tx.execute("UPDATE chunks SET embed_state = ?1", [EMBED_PENDING])?;
                    put_setting(&tx, FINGERPRINT_KEY, &fingerprint)?;
                    repos = repositories::connected_ids(&tx)?;
                } else if !was_enabled {
                    // Chunks indexed while embeddings were off are still pending.
                    repos = repositories::connected_with_pending_embeddings(&tx)?;
                }
            }
            tx.commit()?;
            Ok::<_, rusqlite::Error>(repos)
        })
        .await?;

    for &repo_id in &repos {
        jobs::enqueue(state, repo_id, Trigger::Settings, None, false).await?;
    }
    Ok((settings, u32::try_from(repos.len()).unwrap_or(u32::MAX)))
}
