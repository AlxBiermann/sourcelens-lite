use rusqlite::Connection;

use crate::search::Filter;
use crate::state::AppState;

pub async fn search(
    _state: &AppState,
    _query: &str,
    _filter: &Filter,
    _limit: usize,
) -> Option<Vec<i64>> {
    todo!("store-search")
}

/// `(chunk_id, distance)`, ascending by distance.
pub fn knn(
    _conn: &Connection,
    _repo_ids: &[i64],
    _vector: &[f32],
    _k: usize,
) -> rusqlite::Result<Vec<(i64, f64)>> {
    todo!("store-search")
}

/// Drops and recreates `vec_chunks`; the only place with its DDL.
pub fn create_vec_table(_conn: &Connection, _dimensions: u32) -> rusqlite::Result<()> {
    todo!("store-search")
}

/// Little-endian f32 bytes.
pub fn to_blob(_vector: &[f32]) -> Vec<u8> {
    todo!("store-search")
}
