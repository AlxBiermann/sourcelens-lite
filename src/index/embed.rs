//! Embedding settings and the embed phase of index jobs. The provider client is `index::embedder`.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::time::Instant;

use futures_util::{stream, TryStreamExt};
use reqwest::Url;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::crypto::{sha256_hex, SecretKey};
use crate::db::{delete_setting, get_setting, put_setting};
use crate::error::{ApiError, ApiResult};
use crate::index::embedder::Embedder;
use crate::index::{jobs, store};
use crate::model::{JobPhase, JobStatus, Trigger, EMBED_DONE, EMBED_PENDING, EMBED_REJECTED};
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
/// Provider requests in flight per embed phase.
const EMBED_CONCURRENCY: usize = 4;
const CONNECTION_TEST_INPUT: &str = "sourcelens connection test";

/// The settings an owner edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub request_dimensions: Option<u32>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub batch_size: u32,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            model: String::new(),
            request_dimensions: None,
            query_prefix: String::new(),
            document_prefix: String::new(),
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

impl EmbeddingConfig {
    /// Trims the provider fields; the base URL loses its trailing `/`.
    fn normalized(mut self) -> Self {
        self.base_url = normalize_base_url(&self.base_url);
        self.model = self.model.trim().to_string();
        self
    }
}

/// What `settings['embeddings']` holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredSettings {
    #[serde(flatten)]
    config: EmbeddingConfig,
    /// Detected from the provider by the last successful save.
    dimensions: Option<u32>,
}

/// API view of the settings.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettings {
    #[serde(flatten)]
    pub config: EmbeddingConfig,
    pub dimensions: Option<u32>,
    pub api_key_set: bool,
}

/// Body of `PUT /api/settings/embeddings` and `POST /api/settings/embeddings/test`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettingsInput {
    #[serde(flatten)]
    pub config: EmbeddingConfig,
    pub api_key: Option<String>,
    pub clear_api_key: Option<bool>,
}

/// Settings, sealed API key and vector fingerprint, read in one go.
struct Stored {
    settings: StoredSettings,
    api_key: Option<String>,
    fingerprint: Option<Vec<u8>>,
}

impl Stored {
    fn view(self) -> EmbeddingSettings {
        EmbeddingSettings {
            config: self.settings.config,
            dimensions: self.settings.dimensions,
            api_key_set: self.api_key.is_some(),
        }
    }

    fn embedder(&self, state: &AppState) -> Embedder {
        let config = &self.settings.config;
        Embedder::new(
            state.http.clone(),
            &config.base_url,
            &config.model,
            self.api_key.clone(),
            config.request_dimensions,
        )
    }
}

fn parse_settings(json: &[u8]) -> serde_json::Result<StoredSettings> {
    serde_json::from_slice(json)
}

async fn stored(state: &AppState) -> ApiResult<Stored> {
    let (json, sealed, fingerprint) = state
        .db
        .read(|conn| {
            Ok::<_, rusqlite::Error>((
                get_setting(conn, SETTINGS_KEY)?,
                get_setting(conn, API_KEY_KEY)?,
                get_setting(conn, FINGERPRINT_KEY)?,
            ))
        })
        .await?;
    let settings = match json {
        Some(bytes) => parse_settings(&bytes).map_err(ApiError::internal)?,
        None => StoredSettings::default(),
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
    Ok(Stored {
        settings,
        api_key,
        fingerprint,
    })
}

/// Current settings (defaults when never saved).
pub async fn settings(state: &AppState) -> ApiResult<EmbeddingSettings> {
    Ok(stored(state).await?.view())
}

/// `None` when embeddings are disabled.
pub async fn load(state: &AppState) -> ApiResult<Option<(EmbeddingConfig, Embedder)>> {
    let stored = stored(state).await?;
    if !stored.settings.config.enabled {
        return Ok(None);
    }
    let embedder = stored.embedder(state);
    Ok(Some((stored.settings.config, embedder)))
}

// ── embed phase ─────────────────────────────────────────────────────────────

struct PendingChunk {
    id: i64,
    input: String,
}

/// Up to `limit` pending chunks of the repository with an id above `after`, in id order. Each
/// file's content is read once per batch and sliced here.
fn pending_batch(
    conn: &Connection,
    repo_id: i64,
    after: i64,
    limit: u32,
    prefix: &str,
) -> rusqlite::Result<Vec<PendingChunk>> {
    let mut stmt = conn.prepare_cached(
        "SELECT c.id, c.file_id, c.start_byte, c.end_byte, f.path, c.symbol_path
         FROM chunks c
         JOIN files f ON f.id = c.file_id
         WHERE c.repo_id = ?1 AND c.embed_state = ?2 AND c.id > ?3
         ORDER BY c.id
         LIMIT ?4",
    )?;
    let rows = stmt
        .query_map(params![repo_id, EMBED_PENDING, after, limit], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut content_stmt = conn.prepare_cached("SELECT content FROM files WHERE id = ?1")?;
    let mut contents: HashMap<i64, Vec<u8>> = HashMap::new();
    let mut batch = Vec::with_capacity(rows.len());
    for (id, file_id, start, end, path, symbol_path) in rows {
        let content = match contents.entry(file_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let bytes = content_stmt
                    .query_row([file_id], |row| {
                        Ok(match row.get_ref(0)? {
                            ValueRef::Blob(bytes) | ValueRef::Text(bytes) => bytes.to_vec(),
                            _ => Vec::new(),
                        })
                    })
                    .optional()?
                    .unwrap_or_default();
                entry.insert(bytes)
            }
        };
        let range = usize::try_from(start).ok().zip(usize::try_from(end).ok());
        let bytes = range.and_then(|(start, end)| content.get(start..end));
        let slice = String::from_utf8_lossy(bytes.unwrap_or_default());
        let full = format!(
            "{prefix}{}",
            store::embedding_input(&path, &symbol_path, &slice)
        );
        let input = match full.char_indices().nth(MAX_INPUT_CHARS) {
            Some((cut, _)) => full[..cut].to_string(),
            None => full,
        };
        batch.push(PendingChunk { id, input });
    }
    Ok(batch)
}

/// What the embed phase started with; a batch is stored only while the database still agrees.
#[derive(Clone)]
struct EmbedRun {
    repo_id: i64,
    job_id: i64,
    fingerprint: Option<Vec<u8>>,
    dimensions: Option<usize>,
}

enum BatchOutcome {
    Stored(u32),
    /// Stored; another job of the repository is queued, so this one hands over.
    StoredAndYield(u32),
    /// Settings were saved since the phase started; nothing was stored.
    SettingsChanged,
}

/// Embeds every pending chunk of the repository. Returns the number of chunks embedded.
///
/// Stops early, with the remaining chunks still pending, when the settings change or another job of
/// the repository is queued; fails with "Repository was disconnected." when the repository leaves
/// the `connected` state.
pub async fn embed_pending(state: &AppState, repo_id: i64, job_id: i64) -> ApiResult<u32> {
    let stored = stored(state).await?;
    if !stored.settings.config.enabled || !state.db.read(store::vec_table_exists).await? {
        return Ok(0);
    }
    let started = state
        .db
        .write(move |conn| {
            if queued_job_exists(conn, repo_id)? {
                return Ok::<_, rusqlite::Error>(false);
            }
            let total: i64 = conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE repo_id = ?1 AND embed_state = ?2",
                params![repo_id, EMBED_PENDING],
                |row| row.get(0),
            )?;
            conn.execute(
                "UPDATE jobs SET phase = ?3, embed_total = ?1, embed_done = 0 WHERE id = ?2",
                params![total, job_id, JobPhase::Embed],
            )?;
            Ok(true)
        })
        .await?;
    if !started {
        return Ok(0);
    }

    let embedder = stored.embedder(state);
    let config = stored.settings.config;
    let run = EmbedRun {
        repo_id,
        job_id,
        fingerprint: stored.fingerprint,
        dimensions: stored.settings.dimensions.map(|d| d as usize),
    };
    let batch_size = config.batch_size.clamp(1, MAX_BATCH_SIZE);
    let prefix = config.document_prefix;
    let reader = state.clone();
    let batches = stream::try_unfold(0i64, move |after| {
        let state = reader.clone();
        let prefix = prefix.clone();
        async move {
            let batch = state
                .db
                .read(move |conn| pending_batch(conn, repo_id, after, batch_size, &prefix))
                .await?;
            Ok::<_, ApiError>(batch.last().map(|last| last.id).map(|next| (batch, next)))
        }
    });
    let embedder = &embedder;
    let embedded_batches = batches
        .map_ok(|batch| async move {
            let (ids, inputs): (Vec<i64>, Vec<String>) =
                batch.into_iter().map(|c| (c.id, c.input)).unzip();
            let results = embedder
                .embed_partial(&inputs)
                .await
                .map_err(provider_error)?;
            Ok::<_, ApiError>((ids, results))
        })
        .try_buffer_unordered(EMBED_CONCURRENCY);
    let mut embedded_batches = std::pin::pin!(embedded_batches);

    let mut embedded = 0u32;
    while let Some((ids, results)) = embedded_batches.try_next().await? {
        let run = run.clone();
        let outcome = state
            .db
            .write(move |conn| store_batch(conn, &run, &ids, &results))
            .await?;
        match outcome {
            BatchOutcome::Stored(n) => embedded += n,
            BatchOutcome::StoredAndYield(n) => return Ok(embedded + n),
            BatchOutcome::SettingsChanged => return Ok(embedded),
        }
    }
    Ok(embedded)
}

fn queued_job_exists(conn: &Connection, repo_id: i64) -> rusqlite::Result<bool> {
    conn.prepare_cached("SELECT 1 FROM jobs WHERE repo_id = ?1 AND status = ?2")?
        .exists(params![repo_id, JobStatus::Queued])
}

/// Whether the vectors of `run` still match the stored settings: same fingerprint, still enabled.
fn settings_unchanged(conn: &Connection, run: &EmbedRun) -> rusqlite::Result<bool> {
    if get_setting(conn, FINGERPRINT_KEY)? != run.fingerprint {
        return Ok(false);
    }
    Ok(get_setting(conn, SETTINGS_KEY)?
        .and_then(|json| parse_settings(&json).ok())
        .is_some_and(|settings| settings.config.enabled))
}

fn store_batch(
    conn: &mut Connection,
    run: &EmbedRun,
    ids: &[i64],
    results: &[Result<Vec<f32>, String>],
) -> ApiResult<BatchOutcome> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if !settings_unchanged(&tx, run)? {
        return Ok(BatchOutcome::SettingsChanged);
    }
    if !repositories::is_connected(&tx, run.repo_id)? {
        return Err(disconnected());
    }
    if let Some((wrong, expected)) = run.dimensions.and_then(|expected| {
        results
            .iter()
            .flatten()
            .find(|v| v.len() != expected)
            .map(|v| (v.len(), expected))
    }) {
        return Err(provider_error(format!(
            "the model returned {wrong} dimensions instead of {expected}; save the embedding settings again"
        )));
    }
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
                    store::put_vector(&tx, *id, run.repo_id, &store::to_blob(vector))?;
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
        params![ids.len() as i64, run.job_id],
    )?;
    let hand_over = queued_job_exists(&tx, run.repo_id)?;
    tx.commit()?;
    Ok(if hand_over {
        BatchOutcome::StoredAndYield(embedded)
    } else {
        BatchOutcome::Stored(embedded)
    })
}

fn provider_error(message: String) -> ApiError {
    ApiError::bad_gateway(
        "embedding_provider_error",
        format!("Embedding provider error: {message}"),
    )
}

fn disconnected() -> ApiError {
    ApiError::conflict("repository_disconnected", "Repository was disconnected.")
}

// ── settings ────────────────────────────────────────────────────────────────

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::unprocessable("invalid_embedding_settings", message)
}

fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// Checks the fields that must be valid to talk to a provider (`require_provider`) or to be stored.
fn validate(config: &EmbeddingConfig, require_provider: bool) -> ApiResult<()> {
    let base_url = config.base_url.trim();
    if require_provider || !base_url.is_empty() {
        let valid = Url::parse(base_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some());
        if !valid {
            return Err(invalid("Base URL must be an http or https URL."));
        }
    }
    if require_provider && config.model.trim().is_empty() {
        return Err(invalid("Enter the model name."));
    }
    if !(1..=MAX_BATCH_SIZE).contains(&config.batch_size) {
        return Err(invalid(format!(
            "Batch size must be between 1 and {MAX_BATCH_SIZE}."
        )));
    }
    if let Some(d) = config.request_dimensions {
        if !(1..=MAX_DIMENSIONS).contains(&d) {
            return Err(invalid(format!(
                "Dimensions must be between 1 and {MAX_DIMENSIONS}."
            )));
        }
    }
    Ok(())
}

/// The API key to use for `input`: an explicit new key, none when clearing, the saved key only for
/// the base URL it was saved with.
fn resolve_api_key(input: &EmbeddingSettingsInput, saved: &Stored) -> ApiResult<Option<String>> {
    if input.clear_api_key == Some(true) {
        return Ok(None);
    }
    if let Some(key) = input.api_key.as_deref().map(str::trim) {
        if !key.is_empty() {
            return Ok(Some(key.to_string()));
        }
    }
    match &saved.api_key {
        Some(key)
            if normalize_base_url(&input.config.base_url) == saved.settings.config.base_url =>
        {
            Ok(Some(key.clone()))
        }
        Some(_) => Err(ApiError::unprocessable(
            "api_key_required",
            "Enter the API key for the new base URL.",
        )),
        None => Ok(None),
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
    if let Err(err) = validate(&input.config, true) {
        return Ok(Err(err.message));
    }
    let saved = stored(state).await?;
    let api_key = resolve_api_key(input, &saved)?;
    let config = input.config.clone().normalized();
    let embedder = Embedder::new(
        state.http.clone(),
        &config.base_url,
        &config.model,
        api_key,
        config.request_dimensions,
    );
    let started = Instant::now();
    let result = connection_test(&embedder).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    Ok(result.map(|d| (d, latency_ms)))
}

fn fingerprint(config: &EmbeddingConfig, dimensions: u32) -> String {
    sha256_hex(
        format!(
            "{}\n{}\n{}\n{}",
            config.base_url, config.model, dimensions, config.document_prefix
        )
        .as_bytes(),
    )
}

enum KeyChange {
    Keep,
    Set(Vec<u8>),
    Clear,
}

/// The key `input` asks for and how the stored (sealed) key changes.
fn key_change(
    input: &EmbeddingSettingsInput,
    saved: &Stored,
    key: &SecretKey,
) -> ApiResult<(Option<String>, KeyChange)> {
    let api_key = resolve_api_key(input, saved)?;
    let change = match &api_key {
        None if saved.api_key.is_some() => KeyChange::Clear,
        Some(new) if saved.api_key.as_ref() != Some(new) => {
            KeyChange::Set(key.seal(new.as_bytes()))
        }
        _ => KeyChange::Keep,
    };
    Ok((api_key, change))
}

/// The vector table the saved settings need: dimensions and fingerprint.
type VectorTarget = Option<(u32, String)>;

/// Settings to store for `config`; when enabled, the provider is tested and its dimensions recorded.
async fn tested_settings(
    state: &AppState,
    config: EmbeddingConfig,
    current_dimensions: Option<u32>,
    api_key: Option<String>,
) -> ApiResult<(StoredSettings, VectorTarget)> {
    if !config.enabled {
        let settings = StoredSettings {
            config,
            dimensions: current_dimensions,
        };
        return Ok((settings, None));
    }
    let embedder = Embedder::new(
        state.http.clone(),
        &config.base_url,
        &config.model,
        api_key,
        config.request_dimensions,
    );
    let dimensions = connection_test(&embedder)
        .await
        .map_err(|message| ApiError::unprocessable("embedding_test_failed", message))?;
    let target = Some((dimensions, fingerprint(&config, dimensions)));
    let settings = StoredSettings {
        config,
        dimensions: Some(dimensions),
    };
    Ok((settings, target))
}

/// Writes settings and key; rebuilds the vector table when the fingerprint changed. Returns the
/// repositories to (re-)embed.
fn persist_settings(
    conn: &mut Connection,
    json: &str,
    key_change: KeyChange,
    target: VectorTarget,
    was_enabled: bool,
) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    put_setting(&tx, SETTINGS_KEY, json)?;
    match key_change {
        KeyChange::Keep => {}
        KeyChange::Set(sealed) => put_setting(&tx, API_KEY_KEY, &sealed)?,
        KeyChange::Clear => delete_setting(&tx, API_KEY_KEY)?,
    }
    let mut repos = Vec::new();
    if let Some((dimensions, fingerprint)) = target {
        let unchanged = get_setting(&tx, FINGERPRINT_KEY)?.as_deref()
            == Some(fingerprint.as_bytes())
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
    Ok(repos)
}

/// Validates, tests and stores new settings. Returns the stored settings and the number of
/// repositories queued for (re-)embedding.
pub async fn apply_settings(
    state: &AppState,
    new: EmbeddingSettingsInput,
) -> ApiResult<(EmbeddingSettings, u32)> {
    validate(&new.config, new.config.enabled)?;
    let saved = stored(state).await?;
    let (api_key, key_change) = key_change(&new, &saved, &state.key)?;
    let api_key_set = api_key.is_some();
    let was_enabled = saved.settings.config.enabled;
    let (settings, target) = tested_settings(
        state,
        new.config.normalized(),
        saved.settings.dimensions,
        api_key,
    )
    .await?;

    let json = serde_json::to_string(&settings).map_err(ApiError::internal)?;
    let repos = state
        .db
        .write(move |conn| persist_settings(conn, &json, key_change, target, was_enabled))
        .await?;
    for &repo_id in &repos {
        jobs::enqueue(state, repo_id, Trigger::Settings, None, false).await?;
    }
    let view = EmbeddingSettings {
        config: settings.config,
        dimensions: settings.dimensions,
        api_key_set,
    };
    Ok((view, u32::try_from(repos.len()).unwrap_or(u32::MAX)))
}
