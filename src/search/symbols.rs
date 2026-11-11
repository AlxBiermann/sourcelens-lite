//! Symbol channel: exact, prefix and substring lookup of definitions by name.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OptionalExtension};

use crate::db::sql::{escape_like, placeholders};

/// Minimum name length for prefix and substring lookup (the trigram index needs three chars).
const MIN_FUZZY_CHARS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub id: i64,
    pub repo_id: i64,
    pub file_id: i64,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u32,
}

const COLUMNS: &str =
    "s.id, s.repo_id, s.file_id, s.kind, s.name, s.qualified_name, s.signature, s.start_line, s.end_line, s.depth";

/// Exact name matches first; only when there are none, name prefixes; only then substrings.
pub fn lookup(
    conn: &Connection,
    repo_ids: &[i64],
    candidate: &str,
    kind: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<SymbolRow>> {
    let candidate = candidate.trim();
    let (qualifier, seg) = split_qualifier(candidate);
    if seg.is_empty() || repo_ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let qualified = qualifier
        .map(|_| normalize_qualified(candidate).to_lowercase())
        .unwrap_or_default();

    let exact = run(
        conn,
        repo_ids,
        kind,
        limit,
        "FROM symbols s WHERE s.name = ?",
        vec![Value::from(seg.to_string())],
        "ORDER BY (? <> '' AND lower(substr(s.qualified_name, -length(?))) = ?) DESC,
                  (s.name = ? COLLATE BINARY) DESC, s.depth, s.id",
        vec![
            Value::from(qualified.clone()),
            Value::from(qualified.clone()),
            Value::from(qualified),
            Value::from(seg.to_string()),
        ],
    )?;
    if !exact.is_empty() || seg.chars().count() < MIN_FUZZY_CHARS {
        return Ok(exact);
    }

    let prefix = run(
        conn,
        repo_ids,
        kind,
        limit,
        "FROM symbols s WHERE s.name LIKE ? ESCAPE '\\'",
        vec![Value::from(format!("{}%", escape_like(seg)))],
        "ORDER BY length(s.name), s.depth, s.id",
        Vec::new(),
    )?;
    if !prefix.is_empty() {
        return Ok(prefix);
    }

    run(
        conn,
        repo_ids,
        kind,
        limit,
        "FROM symbols_fts JOIN symbols s ON s.id = symbols_fts.rowid WHERE symbols_fts MATCH ?",
        vec![Value::from(format!("\"{}\"", seg.replace('"', "\"\"")))],
        "ORDER BY length(s.name), s.depth, s.id",
        Vec::new(),
    )
}

/// `(qualifier, last segment)` split at the last `::`, `.`, `#` or `/`.
fn split_qualifier(candidate: &str) -> (Option<&str>, &str) {
    let cut = ["::", ".", "#", "/"]
        .iter()
        .filter_map(|sep| candidate.rfind(sep).map(|at| (at, at + sep.len())))
        .max_by_key(|(at, _)| *at);
    match cut {
        Some((at, end)) => (Some(&candidate[..at]), &candidate[end..]),
        None => (None, candidate),
    }
}

fn normalize_qualified(candidate: &str) -> String {
    candidate.replace("::", ".").replace(['#', '/'], ".")
}

#[allow(clippy::too_many_arguments)]
fn run(
    conn: &Connection,
    repo_ids: &[i64],
    kind: Option<&str>,
    limit: usize,
    from_where: &str,
    where_params: Vec<Value>,
    order_by: &str,
    order_params: Vec<Value>,
) -> rusqlite::Result<Vec<SymbolRow>> {
    let mut sql = format!(
        "SELECT {COLUMNS} {from_where} AND s.repo_id IN ({})",
        placeholders(repo_ids.len())
    );
    let mut values = where_params;
    values.extend(repo_ids.iter().map(|&id| Value::from(id)));
    if let Some(kind) = kind {
        sql.push_str(" AND s.kind = ? COLLATE NOCASE");
        values.push(Value::from(kind.to_string()));
    }
    sql.push(' ');
    sql.push_str(order_by);
    sql.push_str(" LIMIT ?");
    values.extend(order_params);
    values.push(Value::from(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(SymbolRow {
            id: row.get(0)?,
            repo_id: row.get(1)?,
            file_id: row.get(2)?,
            kind: row.get(3)?,
            name: row.get(4)?,
            qualified_name: row.get(5)?,
            signature: row.get(6)?,
            start_line: row.get(7)?,
            end_line: row.get(8)?,
            depth: row.get(9)?,
        })
    })?;
    rows.collect()
}

/// Innermost chunk of the file that contains `line`.
pub fn containing_chunk(
    conn: &Connection,
    file_id: i64,
    line: u32,
) -> rusqlite::Result<Option<i64>> {
    conn.prepare_cached(
        "SELECT id FROM chunks WHERE file_id = ?1 AND start_line <= ?2 AND end_line >= ?2
         ORDER BY start_line DESC LIMIT 1",
    )?
    .query_row(rusqlite::params![file_id, line], |row| row.get(0))
    .optional()
}
