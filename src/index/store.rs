use std::collections::HashMap;

use rusqlite::Connection;

use crate::index::chunker::{Chunk, Symbol};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: String,
    pub blob_sha: String,
    pub size: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffPlan {
    pub fetch: Vec<TreeEntry>,
    pub delete: Vec<String>,
}

pub fn plan_diff(
    _existing: &HashMap<String, String>,
    _tree: &[TreeEntry],
    _force: bool,
) -> DiffPlan {
    todo!("store-search")
}

#[derive(Debug, Clone)]
pub enum FileOutcome {
    Indexed {
        language: Option<&'static str>,
        content: String,
        line_count: u32,
        chunks: Vec<Chunk>,
        symbols: Vec<Symbol>,
    },
    Skipped(&'static str),
}

#[derive(Debug, Clone)]
pub struct PreparedFile {
    pub path: String,
    pub blob_sha: String,
    pub size: u64,
    pub outcome: FileOutcome,
}

pub fn prepare_file(_path: &str, _blob_sha: &str, _bytes: &[u8]) -> PreparedFile {
    todo!("store-search")
}

pub fn skipped(_path: &str, _blob_sha: &str, _size: u64, _reason: &'static str) -> PreparedFile {
    todo!("store-search")
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub indexed: u32,
    pub skipped: u32,
    pub chunks: u32,
}

pub fn existing_files(
    _conn: &Connection,
    _repo_id: i64,
) -> rusqlite::Result<HashMap<String, String>> {
    todo!("store-search")
}

pub fn apply_files(
    _conn: &mut Connection,
    _repo_id: i64,
    _files: &[PreparedFile],
) -> rusqlite::Result<ApplyStats> {
    todo!("store-search")
}

pub fn delete_paths(
    _conn: &mut Connection,
    _repo_id: i64,
    _paths: &[String],
) -> rusqlite::Result<u32> {
    todo!("store-search")
}

pub fn purge_repository(_conn: &mut Connection, _repo_id: i64) -> rusqlite::Result<()> {
    todo!("store-search")
}

pub fn embedding_input(_path: &str, _symbol_path: &str, _content: &str) -> String {
    todo!("store-search")
}

pub fn vec_table_exists(_conn: &Connection) -> rusqlite::Result<bool> {
    todo!("store-search")
}
