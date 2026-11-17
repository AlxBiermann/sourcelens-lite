//! Semantic channel: sqlite-vec KNN over `vec_chunks`, partitioned by repository.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;

use crate::index::embed;
use crate::index::embedder::Embedder;
use crate::index::store::{self, knn};
use crate::search::{retain_matching, Filter};
use crate::state::{lock, AppState};

const QUERY_EMBED_TIMEOUT: Duration = Duration::from_secs(5);
/// Neighbours fetched per repository when a language or path filter drops some of them
/// (sqlite-vec allows up to 4096).
const FILTERED_K: usize = 1000;
/// Query vectors kept in [`QueryVectorCache`].
pub const QUERY_VECTOR_CACHE_ENTRIES: usize = 256;

/// Outcome of the semantic channel.
#[derive(Debug, Default)]
pub struct Semantic {
    /// Chunk ids nearest to the query; `None` when the channel did not run.
    pub ids: Option<Vec<i64>>,
    /// Why the channel failed: "timeout" | "provider_error" | "internal". `None` when it ran, when
    /// embeddings are off or when the vector table is missing.
    pub unavailable: Option<&'static str>,
}

impl Semantic {
    fn failed(reason: &'static str) -> Self {
        Self {
            ids: None,
            unavailable: Some(reason),
        }
    }
}

/// Embedded queries by `"{fingerprint}\n{query_prefix}{query}"`; the oldest entry goes first.
#[derive(Default)]
pub struct QueryVectorCache {
    vectors: HashMap<String, Arc<[f32]>>,
    /// Keys of `vectors` in insertion order.
    order: VecDeque<String>,
}

impl QueryVectorCache {
    pub fn get(&self, key: &str) -> Option<Arc<[f32]>> {
        self.vectors.get(key).cloned()
    }

    /// Stores `vector` under `key`, dropping the oldest entry beyond
    /// [`QUERY_VECTOR_CACHE_ENTRIES`].
    pub fn put(&mut self, key: String, vector: Arc<[f32]>) {
        if let Some(slot) = self.vectors.get_mut(&key) {
            *slot = vector;
            return;
        }
        if self.order.len() >= QUERY_VECTOR_CACHE_ENTRIES {
            if let Some(oldest) = self.order.pop_front() {
                self.vectors.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.vectors.insert(key, vector);
    }
}

/// Chunk ids nearest to the embedded query, filtered like the other channels. A query is embedded
/// once per vector fingerprint; later searches take its vector from [`QueryVectorCache`].
pub async fn search(state: &AppState, query: &str, filter: &Filter, limit: usize) -> Semantic {
    match state.db.read(store::vec_table_exists).await {
        Ok(true) => {}
        Ok(false) => return Semantic::default(),
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            return Semantic::failed("internal");
        }
    }
    let loaded = match embed::load(state).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return Semantic::default(),
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            return Semantic::failed("internal");
        }
    };
    let input = format!("{}{}", loaded.config.query_prefix, query);
    let key = loaded
        .fingerprint
        .as_ref()
        .map(|fingerprint| format!("{fingerprint}\n{input}"));
    let cached = key
        .as_deref()
        .and_then(|key| lock(&state.query_vectors).get(key));
    let vector = match cached {
        Some(vector) => vector,
        None => match embed_query(&loaded.embedder, input).await {
            Ok(vector) => {
                if let Some(key) = key {
                    lock(&state.query_vectors).put(key, vector.clone());
                }
                vector
            }
            Err(reason) => return Semantic::failed(reason),
        },
    };
    let filter = filter.clone();
    let result = state
        .db
        .read(move |conn| nearest(conn, &filter, &vector, limit))
        .await;
    match result {
        Ok(ids) => Semantic {
            ids: Some(ids),
            unavailable: None,
        },
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            Semantic::failed("internal")
        }
    }
}

/// The query's vector, or the [`Semantic::unavailable`] reason.
async fn embed_query(embedder: &Embedder, input: String) -> Result<Arc<[f32]>, &'static str> {
    // One attempt: a provider that fails would otherwise hold the search for the whole timeout.
    let embedded = tokio::time::timeout(QUERY_EMBED_TIMEOUT, embedder.embed_once(&[input])).await;
    match embedded {
        Ok(Ok(vectors)) => match vectors.into_iter().next() {
            Some(vector) => Ok(vector.into()),
            None => {
                tracing::warn!("semantic search skipped: the provider returned no vector");
                Err("provider_error")
            }
        },
        Ok(Err(err)) => {
            tracing::warn!("semantic search skipped: {err}");
            Err("provider_error")
        }
        Err(_) => {
            tracing::warn!("semantic search skipped: embedding the query timed out");
            Err("timeout")
        }
    }
}

/// Up to `limit` chunk ids nearest to `vector` within `filter`, nearest first. With a language or
/// path filter, [`FILTERED_K`] neighbours are fetched before filtering, so a narrow filter still
/// finds its chunks.
pub fn nearest(
    conn: &Connection,
    filter: &Filter,
    vector: &[f32],
    limit: usize,
) -> rusqlite::Result<Vec<i64>> {
    let k = if filter.filters_files() {
        FILTERED_K.max(limit)
    } else {
        limit
    };
    let ids: Vec<i64> = knn(conn, &filter.repo_ids, vector, k)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut ids = retain_matching(conn, &ids, filter)?;
    ids.truncate(limit);
    Ok(ids)
}
