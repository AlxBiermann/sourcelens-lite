//! Diff planning and the per-file rows of the index (files, chunks, symbols, FTS, vectors).

use std::collections::{HashMap, HashSet};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::crypto::sha256_hex;
use crate::index::chunker::{chunk_file, Chunk, Symbol};
use crate::index::{languages, select};
use crate::model::{SkipReason, EMBED_DONE, EMBED_PENDING};
use crate::repositories;

/// Files deleted per transaction when a whole repository is purged.
const PURGE_BATCH_FILES: i64 = 200;

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

/// `fetch` = new or changed entries (all with `force`); `delete` = indexed paths missing from `tree`.
pub fn plan_diff(existing: &HashMap<String, String>, tree: &[TreeEntry], force: bool) -> DiffPlan {
    let fetch = tree
        .iter()
        .filter(|entry| force || existing.get(&entry.path) != Some(&entry.blob_sha))
        .cloned()
        .collect();
    let in_tree: HashSet<&str> = tree.iter().map(|entry| entry.path.as_str()).collect();
    let mut delete: Vec<String> = existing
        .keys()
        .filter(|path| !in_tree.contains(path.as_str()))
        .cloned()
        .collect();
    delete.sort();
    DiffPlan { fetch, delete }
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
    Skipped(SkipReason),
}

#[derive(Debug, Clone)]
pub struct PreparedFile {
    pub path: String,
    pub blob_sha: String,
    pub size: u64,
    pub outcome: FileOutcome,
}

/// Classifies, normalises and chunks one file. Pure and CPU-bound.
pub fn prepare_file(path: &str, blob_sha: &str, bytes: &[u8]) -> PreparedFile {
    let size = bytes.len() as u64;
    if let Some(reason) = select::content_skip_reason(bytes) {
        return skipped(path, blob_sha, size, reason);
    }
    let content = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let language = languages::language_for_path(path).map(|language| language.name);
    let (chunks, symbols) = chunk_file(path, language, &content);
    let line_count = u32::try_from(content.lines().count()).unwrap_or(u32::MAX);
    PreparedFile {
        path: path.to_string(),
        blob_sha: blob_sha.to_string(),
        size,
        outcome: FileOutcome::Indexed {
            language,
            content,
            line_count,
            chunks,
            symbols,
        },
    }
}

pub fn skipped(path: &str, blob_sha: &str, size: u64, reason: SkipReason) -> PreparedFile {
    PreparedFile {
        path: path.to_string(),
        blob_sha: blob_sha.to_string(),
        size,
        outcome: FileOutcome::Skipped(reason),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub indexed: u32,
    pub skipped: u32,
}

/// Indexed paths of a repository with their blob SHA.
pub fn existing_files(
    conn: &Connection,
    repo_id: i64,
) -> rusqlite::Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, blob_sha FROM files WHERE repo_id = ?1")?;
    let rows = stmt.query_map([repo_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Replaces the rows of every given file in one transaction. Vectors of chunks whose embedding
/// input did not change are carried over.
pub fn apply_files(
    conn: &mut Connection,
    repo_id: i64,
    files: &[PreparedFile],
) -> rusqlite::Result<ApplyStats> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_vec = vec_table_exists(&tx)?;
    let mut stats = ApplyStats::default();
    for file in files {
        let preserved = match file_id(&tx, repo_id, &file.path)? {
            Some(file_id) => delete_file_rows(&tx, file_id, has_vec)?,
            None => HashMap::new(),
        };
        let size = i64::try_from(file.size).unwrap_or(i64::MAX);
        let mut upsert = tx.prepare_cached(
            "INSERT INTO files (repo_id, path, blob_sha, language, size, line_count, skip_reason, content)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (repo_id, path) DO UPDATE SET
               blob_sha = excluded.blob_sha, language = excluded.language, size = excluded.size,
               line_count = excluded.line_count, skip_reason = excluded.skip_reason, content = excluded.content
             RETURNING id",
        )?;
        match &file.outcome {
            FileOutcome::Skipped(reason) => {
                upsert.query_row(
                    params![
                        repo_id,
                        file.path,
                        file.blob_sha,
                        Option::<&str>::None,
                        size,
                        0,
                        reason,
                        Option::<&[u8]>::None
                    ],
                    |row| row.get::<_, i64>(0),
                )?;
                stats.skipped += 1;
            }
            FileOutcome::Indexed {
                language,
                content,
                line_count,
                chunks,
                symbols,
            } => {
                let file_id: i64 = upsert.query_row(
                    params![
                        repo_id,
                        file.path,
                        file.blob_sha,
                        language,
                        size,
                        line_count,
                        Option::<&str>::None,
                        content.as_bytes()
                    ],
                    |row| row.get(0),
                )?;
                insert_chunks(
                    &tx, repo_id, file_id, &file.path, content, chunks, &preserved,
                )?;
                insert_symbols(&tx, repo_id, file_id, symbols)?;
                stats.indexed += 1;
            }
        }
    }
    tx.commit()?;
    Ok(stats)
}

fn insert_chunks(
    conn: &Connection,
    repo_id: i64,
    file_id: i64,
    path: &str,
    content: &str,
    chunks: &[Chunk],
    preserved: &HashMap<String, Vec<u8>>,
) -> rusqlite::Result<()> {
    let mut insert_chunk = conn.prepare_cached(
        "INSERT INTO chunks (repo_id, file_id, kind, symbol_path, start_line, end_line, start_byte, end_byte, embed_hash, embed_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING id",
    )?;
    let mut insert_fts = conn.prepare_cached(
        "INSERT INTO chunks_fts (rowid, names, path, body, idents) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for chunk in chunks {
        let body = content.get(chunk.start_byte..chunk.end_byte).unwrap_or("");
        let symbol_path = chunk.symbol_path.join(" > ");
        let embed_hash = sha256_hex(embedding_input(path, &symbol_path, body).as_bytes());
        let vector = preserved.get(&embed_hash);
        let chunk_id: i64 = insert_chunk.query_row(
            params![
                repo_id,
                file_id,
                chunk.kind,
                symbol_path,
                chunk.start_line,
                chunk.end_line,
                i64::try_from(chunk.start_byte).unwrap_or(i64::MAX),
                i64::try_from(chunk.end_byte).unwrap_or(i64::MAX),
                embed_hash,
                if vector.is_some() {
                    EMBED_DONE
                } else {
                    EMBED_PENDING
                },
            ],
            |row| row.get(0),
        )?;
        insert_fts.execute(params![
            chunk_id,
            names_column(&chunk.names),
            path,
            body,
            idents_column(body)
        ])?;
        if let Some(vector) = vector {
            put_vector(conn, chunk_id, repo_id, vector)?;
        }
    }
    Ok(())
}

fn insert_symbols(
    conn: &Connection,
    repo_id: i64,
    file_id: i64,
    symbols: &[Symbol],
) -> rusqlite::Result<()> {
    let mut insert_symbol = conn.prepare_cached(
        "INSERT INTO symbols (repo_id, file_id, kind, name, qualified_name, signature, start_line, end_line, depth)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
    )?;
    let mut insert_fts =
        conn.prepare_cached("INSERT INTO symbols_fts (rowid, name) VALUES (?1, ?2)")?;
    for symbol in symbols {
        let symbol_id: i64 = insert_symbol.query_row(
            params![
                repo_id,
                file_id,
                symbol.kind,
                symbol.name,
                symbol.qualified_name,
                symbol.signature,
                symbol.start_line,
                symbol.end_line,
                symbol.depth
            ],
            |row| row.get(0),
        )?;
        insert_fts.execute(params![symbol_id, symbol.name])?;
    }
    Ok(())
}

/// Symbol names plus their identifier parts.
fn names_column(names: &[String]) -> String {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        out.push(name.clone());
        let parts = camel_parts(name);
        if parts.len() > 1 {
            out.extend(parts);
        }
    }
    out.join(" ")
}

/// Lower-cased parts of every compound identifier in `body`, once each.
fn idents_column(body: &str) -> String {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let parts = camel_parts(&body[start..i]);
            if parts.len() > 1 {
                for part in parts {
                    if seen.insert(part.clone()) {
                        out.push(part);
                    }
                }
            }
        } else if byte.is_ascii_digit() {
            // Digits continuing a number are not identifier starts.
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out.join(" ")
}

/// Deletes the chunk, symbol, FTS and vector rows of a file (not the `files` row) and returns
/// `embed_hash → embedding` of the deleted vectors.
fn delete_file_rows(
    conn: &Connection,
    file_id: i64,
    has_vec: bool,
) -> rusqlite::Result<HashMap<String, Vec<u8>>> {
    let mut preserved = HashMap::new();
    if has_vec {
        let mut stmt = conn.prepare_cached(
            "SELECT c.embed_hash, v.embedding FROM chunks c JOIN vec_chunks v ON v.chunk_id = c.id WHERE c.file_id = ?1",
        )?;
        let rows = stmt.query_map([file_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (hash, embedding) = row?;
            preserved.insert(hash, embedding);
        }
        conn.prepare_cached(
            "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM chunks WHERE file_id = ?1)",
        )?
        .execute([file_id])?;
    }
    for sql in [
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE file_id = ?1)",
        "DELETE FROM symbols_fts WHERE rowid IN (SELECT id FROM symbols WHERE file_id = ?1)",
        "DELETE FROM chunks WHERE file_id = ?1",
        "DELETE FROM symbols WHERE file_id = ?1",
    ] {
        conn.prepare_cached(sql)?.execute([file_id])?;
    }
    Ok(preserved)
}

/// The `files` row id of `path`.
fn file_id(conn: &Connection, repo_id: i64, path: &str) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached("SELECT id FROM files WHERE repo_id = ?1 AND path = ?2")?
        .query_row(params![repo_id, path], |row| row.get(0))
        .optional()
}

/// Deletes a file with all of its rows.
fn delete_file(tx: &Transaction<'_>, file_id: i64, has_vec: bool) -> rusqlite::Result<()> {
    delete_file_rows(tx, file_id, has_vec)?;
    tx.prepare_cached("DELETE FROM files WHERE id = ?1")?
        .execute([file_id])?;
    Ok(())
}

/// Removes the given paths of a repository; returns the number of deleted files.
pub fn delete_paths(
    conn: &mut Connection,
    repo_id: i64,
    paths: &[String],
) -> rusqlite::Result<u32> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_vec = vec_table_exists(&tx)?;
    let mut deleted = 0;
    for path in paths {
        if let Some(file_id) = file_id(&tx, repo_id, path)? {
            delete_file(&tx, file_id, has_vec)?;
            deleted += 1;
        }
    }
    tx.commit()?;
    Ok(deleted)
}

/// Deletes up to 200 files of a repository in one transaction; when none are left, deletes the
/// repository row and returns `true`. Call it until it returns `true`, one writer turn per call.
pub fn purge_batch(conn: &mut Connection, repo_id: i64) -> rusqlite::Result<bool> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let has_vec = vec_table_exists(&tx)?;
    let file_ids: Vec<i64> = tx
        .prepare("SELECT id FROM files WHERE repo_id = ?1 LIMIT ?2")?
        .query_map(params![repo_id, PURGE_BATCH_FILES], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let done = file_ids.is_empty();
    if done {
        repositories::delete(&tx, repo_id)?;
    }
    for file_id in file_ids {
        delete_file(&tx, file_id, has_vec)?;
    }
    tx.commit()?;
    Ok(done)
}

/// Text sent to the embedding model for one chunk (without the document prefix).
pub fn embedding_input(path: &str, symbol_path: &str, content: &str) -> String {
    format!("{path}\n{symbol_path}\n\n{content}")
}

pub fn vec_table_exists(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'vec_chunks')",
        [],
        |row| row.get(0),
    )
}

/// Drops and recreates `vec_chunks`; the only place with its DDL.
pub fn create_vec_table(conn: &Connection, dimensions: u32) -> rusqlite::Result<()> {
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS vec_chunks;
         CREATE VIRTUAL TABLE vec_chunks USING vec0(
           chunk_id INTEGER PRIMARY KEY, repo_id INTEGER PARTITION KEY, embedding float[{dimensions}]);"
    ))
}

/// Stores the vector of a chunk (`embedding` as [`to_blob`] bytes).
pub fn put_vector(
    conn: &Connection,
    chunk_id: i64,
    repo_id: i64,
    embedding: &[u8],
) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO vec_chunks (chunk_id, repo_id, embedding) VALUES (?1, ?2, ?3)",
    )?
    .execute(params![chunk_id, repo_id, embedding])?;
    Ok(())
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

/// Little-endian f32 bytes, the encoding of every `vec_chunks` insert and query.
pub fn to_blob(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Lower-cased identifier parts: splits at non-alphanumerics, lower|digit→Upper and acronym→Word
/// (`HTTPServer` → `http`, `server`; `get_user` → `get`, `user`). Shared by the `idents` column
/// and the query planner so both tokenise identifiers the same way.
pub fn camel_parts(identifier: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for run in identifier.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = run.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let prev = chars[i - 1];
            let cur = chars[i];
            let lower_to_upper = (prev.is_lowercase() || prev.is_numeric()) && cur.is_uppercase();
            let acronym_to_word = prev.is_uppercase()
                && cur.is_uppercase()
                && chars.get(i + 1).is_some_and(|next| next.is_lowercase());
            if lower_to_upper || acronym_to_word {
                parts.push(lower(&chars[start..i]));
                start = i;
            }
        }
        if start < chars.len() {
            parts.push(lower(&chars[start..]));
        }
    }
    parts
}

fn lower(chars: &[char]) -> String {
    chars.iter().flat_map(|c| c.to_lowercase()).collect()
}
