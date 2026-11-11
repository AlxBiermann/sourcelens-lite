//! Semantic channel: sqlite-vec KNN over `vec_chunks`, partitioned by repository.

use std::time::Duration;

use rusqlite::{params, Connection};

use crate::index::{embed, store};
use crate::search::{retain_matching, Filter};
use crate::state::AppState;

const QUERY_EMBED_TIMEOUT: Duration = Duration::from_secs(5);

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
        .read(move |conn| {
            let ids: Vec<i64> = knn(conn, &filter.repo_ids, &vector, limit)?
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            retain_matching(conn, &ids, &filter)
        })
        .await;
    match result {
        Ok(ids) => Some(ids),
        Err(err) => {
            tracing::warn!("semantic search skipped: {err}");
            None
        }
    }
}

/// `(chunk_id, distance)`, ascending by distance, at most `k` over all given repositories.
pub fn knn(
    conn: &Connection,
    repo_ids: &[i64],
    vector: &[f32],
    k: usize,
) -> rusqlite::Result<Vec<(i64, f64)>> {
    if k == 0 {
        return Ok(Vec::new());
    }
    let blob = to_blob(vector);
    let k_value = i64::try_from(k).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare_cached(
        "SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 AND k = ?2 AND repo_id = ?3",
    )?;
    let mut hits: Vec<(i64, f64)> = Vec::new();
    for &repo_id in repo_ids {
        let rows = stmt.query_map(params![blob, k_value, repo_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        for row in rows {
            hits.push(row?);
        }
    }
    hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
    hits.truncate(k);
    Ok(hits)
}

/// Drops and recreates `vec_chunks`; the only place with its DDL.
pub fn create_vec_table(conn: &Connection, dimensions: u32) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS vec_chunks;
         CREATE VIRTUAL TABLE vec_chunks USING vec0(
           chunk_id INTEGER PRIMARY KEY, repo_id INTEGER PARTITION KEY, embedding float[{dimensions}]);"
    ))
}

/// Little-endian f32 bytes.
pub fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}
