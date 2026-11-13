pub mod fuse;
pub mod grep;
pub mod lexical;
pub mod query;
pub mod symbols;
pub mod vector;

use std::borrow::Cow;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use rusqlite::types::{Value, ValueRef};
use rusqlite::{params_from_iter, Connection};
use serde::Serialize;

use crate::db::sql::placeholders;
use crate::error::{ApiError, ApiResult};
use crate::index::languages::{language_by_name, LANGUAGES};
use crate::repositories;
use crate::search::query::QueryPlan;
use crate::state::AppState;

/// Results returned when the request names no limit.
pub const DEFAULT_LIMIT: usize = 8;
/// Most results one request may ask for.
pub const MAX_LIMIT: usize = 20;

const SYMBOL_WEIGHT: f64 = 2.0;
const TEXT_WEIGHT: f64 = 1.0;
const SEMANTIC_WEIGHT: f64 = 1.0;
const SYMBOL_LOOKUP_LIMIT: usize = 10;
const SYMBOL_CHANNEL_LIMIT: usize = 20;
/// Symbol names shown with a hit whose chunk has no symbol path of its own.
const MAX_HIT_NAMES: usize = 3;
const TEXT_CHANNEL_LIMIT: usize = 50;
const SEMANTIC_CHANNEL_LIMIT: usize = 50;
const MAX_HITS_PER_FILE: usize = 3;
const TEST_PATH_PRIOR: f64 = 0.7;
const IMPORTS_PRIOR: f64 = 0.5;

const TEST_DIRECTORIES: &[&str] = &[
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
    "testdata",
    "fixtures",
];

/// Where a search looks. Build it with [`Filter::new`] so languages and the path prefix are normalised.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub repo_ids: Vec<i64>,
    /// Canonical language names (`languages::Language::name`).
    pub languages: Vec<String>,
    /// Without leading `/`; `None` when empty.
    pub path_prefix: Option<String>,
}

impl Filter {
    /// Trims and canonicalises the languages (names, aliases or extensions) and the path prefix.
    /// An unknown language is 422 `unknown_language`.
    pub fn new(
        repo_ids: Vec<i64>,
        languages: impl IntoIterator<Item = impl AsRef<str>>,
        path_prefix: Option<&str>,
    ) -> ApiResult<Self> {
        let mut canonical: Vec<String> = Vec::new();
        for language in languages {
            let name = language.as_ref().trim();
            if name.is_empty() {
                continue;
            }
            let Some(known) = language_by_name(name) else {
                return Err(unknown_language(name));
            };
            if !canonical.iter().any(|c| c == known.name) {
                canonical.push(known.name.to_string());
            }
        }
        let path_prefix = path_prefix
            .map(|prefix| prefix.trim().trim_start_matches('/'))
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_string);
        Ok(Self {
            repo_ids,
            languages: canonical,
            path_prefix,
        })
    }

    /// Whether the language or path conditions need the `files` row of a chunk.
    pub(crate) fn filters_files(&self) -> bool {
        !self.languages.is_empty() || self.path_prefix.is_some()
    }
}

fn unknown_language(name: &str) -> ApiError {
    let mut known: Vec<&str> = LANGUAGES.iter().map(|language| language.name).collect();
    known.sort_unstable();
    ApiError::unprocessable(
        "unknown_language",
        format!("Unknown language \"{name}\". Known: {}.", known.join(", ")),
    )
}

/// `line` cut after `max_chars` characters to `… [+N chars]`.
pub fn cap_line(line: &str, max_chars: usize) -> Cow<'_, str> {
    match line.char_indices().nth(max_chars) {
        None => Cow::Borrowed(line),
        Some((at, _)) => {
            let rest = line[at..].chars().count();
            Cow::Owned(format!("{}… [+{rest} chars]", &line[..at]))
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub filter: Filter,
    /// Clamped to `1..=MAX_LIMIT`; `None` = [`DEFAULT_LIMIT`].
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub repository: String,
    pub path: String,
    pub language: Option<String>,
    pub kind: String,
    pub symbol_path: String,
    /// Up to three symbols that start inside the chunk, by line; for chunks such as `block` that
    /// group several declarations.
    pub names: Vec<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub content: String,
    pub score: f64,
    pub matched_by: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    /// "lexical" | "hybrid"
    pub mode: &'static str,
    pub terms: Vec<String>,
    pub hits: Vec<SearchHit>,
}

/// Symbol, lexical and (when configured) semantic channels, fused, weighted by priors and capped per file.
pub async fn search(state: &AppState, req: SearchRequest) -> Result<SearchResponse, ApiError> {
    let Some(plan) = query::plan(&req.query) else {
        return Err(ApiError::bad_request(
            "empty_query",
            "Enter a search query.",
        ));
    };
    let limit = req.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let terms = plan.terms.clone();
    if req.filter.repo_ids.is_empty() {
        return Ok(SearchResponse {
            mode: "lexical",
            terms,
            hits: Vec::new(),
        });
    }
    let semantic =
        vector::search(state, req.query.trim(), &req.filter, SEMANTIC_CHANNEL_LIMIT).await;
    let mode = if semantic.is_some() {
        "hybrid"
    } else {
        "lexical"
    };
    let filter = req.filter;
    let hits = state
        .db
        .read(move |conn| ranked_hits(conn, &plan, &filter, semantic, limit))
        .await?;
    Ok(SearchResponse { mode, terms, hits })
}

fn ranked_hits(
    conn: &Connection,
    plan: &QueryPlan,
    filter: &Filter,
    semantic: Option<Vec<i64>>,
    limit: usize,
) -> rusqlite::Result<Vec<SearchHit>> {
    let mut lists: Vec<(&'static str, f64, Vec<i64>)> = Vec::new();

    let mut symbol_ids: Vec<i64> = Vec::new();
    for candidate in &plan.symbol_candidates {
        for row in symbols::lookup(conn, filter, candidate, None, SYMBOL_LOOKUP_LIMIT)? {
            if let Some(chunk_id) = symbols::containing_chunk(conn, row.file_id, row.start_line)? {
                if !symbol_ids.contains(&chunk_id) {
                    symbol_ids.push(chunk_id);
                }
            }
        }
    }
    symbol_ids.truncate(SYMBOL_CHANNEL_LIMIT);
    lists.push(("symbol", SYMBOL_WEIGHT, symbol_ids));

    if let Some(expr) = lexical::and_expression(&plan.groups) {
        let ids = lexical::search(conn, &expr, filter, TEXT_CHANNEL_LIMIT)?;
        lists.push(("text_all", TEXT_WEIGHT, ids));
    }
    if let Some(expr) = lexical::or_expression(&plan.groups) {
        let ids = lexical::search(conn, &expr, filter, TEXT_CHANNEL_LIMIT)?;
        lists.push(("text_any", TEXT_WEIGHT, ids));
    }
    if let Some(ids) = semantic {
        lists.push(("semantic", SEMANTIC_WEIGHT, ids));
    }

    let fused = fuse::fuse(&lists);
    let ids: Vec<i64> = fused.iter().map(|(id, _, _)| *id).collect();
    let meta = chunk_meta(conn, &ids)?;
    let exempt_tests = plan
        .terms
        .iter()
        .any(|term| matches!(term.as_str(), "test" | "tests" | "spec"));

    let mut ranked: Vec<(i64, f64, Vec<&'static str>)> = fused
        .into_iter()
        .filter_map(|(id, mut score, channels)| {
            let chunk = meta.get(&id)?;
            if !exempt_tests && is_test_path(&chunk.path) {
                score *= TEST_PATH_PRIOR;
            }
            if matches!(chunk.kind.as_str(), "imports" | "package") {
                score *= IMPORTS_PRIOR;
            }
            Some((id, score, channels))
        })
        .collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

    let mut per_file: HashMap<i64, usize> = HashMap::new();
    let mut selected: Vec<(&ChunkMeta, f64, Vec<&'static str>)> = Vec::new();
    for (id, score, channels) in ranked {
        let Some(chunk) = meta.get(&id) else {
            continue;
        };
        let count = per_file.entry(chunk.file_id).or_insert(0);
        if *count >= MAX_HITS_PER_FILE {
            continue;
        }
        *count += 1;
        selected.push((chunk, score, channels));
        if selected.len() == limit {
            break;
        }
    }

    let mut repo_ids: Vec<i64> = selected.iter().map(|(chunk, _, _)| chunk.repo_id).collect();
    repo_ids.sort_unstable();
    repo_ids.dedup();
    let repository_names = repositories::full_names(conn, &repo_ids)?;
    // Each file's content is read once, however many of its chunks are hits.
    let mut contents: HashMap<i64, Vec<u8>> = HashMap::new();
    let mut hits = Vec::with_capacity(selected.len());
    for (chunk, score, matched_by) in selected {
        let Some(repository) = repository_names.get(&chunk.repo_id) else {
            continue;
        };
        let bytes = match contents.entry(chunk.file_id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => entry.insert(file_content(conn, chunk.file_id)?),
        };
        let content = slice(bytes, chunk.start_byte, chunk.end_byte);
        hits.push(SearchHit {
            repository: repository.clone(),
            path: chunk.path.clone(),
            language: chunk.language.clone(),
            kind: chunk.kind.clone(),
            symbol_path: chunk.symbol_path.clone(),
            names: names_in(conn, chunk)?,
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            content,
            score,
            matched_by,
        });
    }
    Ok(hits)
}

/// Everything about a hit's chunk except its content.
struct ChunkMeta {
    repo_id: i64,
    file_id: i64,
    path: String,
    language: Option<String>,
    kind: String,
    symbol_path: String,
    start_line: u32,
    end_line: u32,
    start_byte: i64,
    end_byte: i64,
}

fn chunk_meta(conn: &Connection, ids: &[i64]) -> rusqlite::Result<HashMap<i64, ChunkMeta>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT c.id, c.repo_id, c.file_id, f.path, f.language, c.kind, c.symbol_path,
                c.start_line, c.end_line, c.start_byte, c.end_byte
         FROM chunks c JOIN files f ON f.id = c.file_id
         WHERE c.id IN ({})",
        placeholders(ids.len())
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(ids), |row| {
        Ok((
            row.get(0)?,
            ChunkMeta {
                repo_id: row.get(1)?,
                file_id: row.get(2)?,
                path: row.get(3)?,
                language: row.get(4)?,
                kind: row.get(5)?,
                symbol_path: row.get(6)?,
                start_line: row.get(7)?,
                end_line: row.get(8)?,
                start_byte: row.get(9)?,
                end_byte: row.get(10)?,
            },
        ))
    })?;
    rows.collect()
}

fn file_content(conn: &Connection, file_id: i64) -> rusqlite::Result<Vec<u8>> {
    conn.prepare_cached("SELECT content FROM files WHERE id = ?1")?
        .query_row([file_id], |row| {
            Ok(match row.get_ref(0)? {
                ValueRef::Blob(bytes) | ValueRef::Text(bytes) => bytes.to_vec(),
                _ => Vec::new(),
            })
        })
}

/// `bytes[start..end]` (clamped) as text, replacing invalid UTF-8.
fn slice(bytes: &[u8], start: i64, end: i64) -> String {
    let offset = |at: i64| usize::try_from(at).unwrap_or(0).min(bytes.len());
    let end = offset(end);
    let start = offset(start).min(end);
    String::from_utf8_lossy(&bytes[start..end]).into_owned()
}

fn names_in(conn: &Connection, chunk: &ChunkMeta) -> rusqlite::Result<Vec<String>> {
    let limit = i64::try_from(MAX_HIT_NAMES).unwrap_or(i64::MAX);
    conn.prepare_cached(
        "SELECT name FROM symbols
         WHERE file_id = ?1 AND start_line BETWEEN ?2 AND ?3
         ORDER BY start_line, depth, id
         LIMIT ?4",
    )?
    .query_map(
        rusqlite::params![chunk.file_id, chunk.start_line, chunk.end_line, limit],
        |row| row.get(0),
    )?
    .collect()
}

fn is_test_path(path: &str) -> bool {
    let mut segments: Vec<&str> = path.split('/').collect();
    let file_name = segments.pop().unwrap_or(path);
    segments
        .iter()
        .any(|segment| TEST_DIRECTORIES.contains(segment))
        || file_name.contains("_test.")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.starts_with("test_")
}

/// The ids (in their order) whose chunk lies within `filter`'s repositories, languages and path prefix.
pub(crate) fn retain_matching(
    conn: &Connection,
    ids: &[i64],
    filter: &Filter,
) -> rusqlite::Result<Vec<i64>> {
    if ids.is_empty() || filter.repo_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut sql = String::from("SELECT c.id FROM chunks c");
    lexical::push_files_join(&mut sql, filter);
    sql.push_str(&format!(" WHERE c.id IN ({})", placeholders(ids.len())));
    let mut values: Vec<Value> = ids.iter().map(|&id| Value::from(id)).collect();
    lexical::push_filter(&mut sql, &mut values, filter);
    let mut stmt = conn.prepare(&sql)?;
    let matching: HashSet<i64> = stmt
        .query_map(params_from_iter(values), |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids
        .iter()
        .copied()
        .filter(|id| matching.contains(id))
        .collect())
}
