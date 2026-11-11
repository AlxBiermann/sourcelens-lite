//! Semantic channel: sqlite-vec KNN over `vec_chunks`, partitioned by repository.

use std::time::Duration;

use rusqlite::Connection;

use crate::index::embed;
use crate::index::store::{self, knn};
use crate::search::{retain_matching, Filter};
use crate::state::AppState;

const QUERY_EMBED_TIMEOUT: Duration = Duration::from_secs(5);
/// Neighbours fetched per repository when a language or path filter drops some of them
/// (sqlite-vec allows up to 4096).
const FILTERED_K: usize = 1000;

/// Chunk ids nearest to the embedded query, filtered like the other channels. `None` when
/// embeddings are off, the vector table is missing or the query cannot be embedded.
pub async fn search(
    state: &AppState,
    query: &str,
    filter: &Filter,
    limit: usize,
) -> Option<Vec<i64>> {
    match state.db.read(store::vec_table_exists).await {
        Ok(true) => {}
        Ok(false) => return None,
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            return None;
        }
    }
    let (settings, embedder) = match embed::load(state).await {
        Ok(Some(loaded)) => loaded,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            return None;
        }
    };
    let input = format!("{}{}", settings.query_prefix, query);
    let vector = match tokio::time::timeout(QUERY_EMBED_TIMEOUT, embedder.embed(&[input])).await {
        Ok(Ok(vectors)) => match vectors.into_iter().next() {
            Some(vector) => vector,
            None => {
                tracing::warn!("semantic search skipped: the provider returned no vector");
                return None;
            }
        },
        Ok(Err(err)) => {
            tracing::warn!("semantic search skipped: {err}");
            return None;
        }
        Err(_) => {
            tracing::warn!("semantic search skipped: embedding the query timed out");
            return None;
        }
    };
    let filter = filter.clone();
    let result = state
        .db
        .read(move |conn| nearest(conn, &filter, &vector, limit))
        .await;
    match result {
        Ok(ids) => Some(ids),
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            None
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
