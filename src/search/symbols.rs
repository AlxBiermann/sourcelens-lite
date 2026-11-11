//! Symbol channel: exact, prefix and substring lookup of definitions by name.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection, OptionalExtension, Row};
use serde::Serialize;

use crate::db::sql::{escape_like, placeholders};
use crate::search::lexical::push_file_filter;
use crate::search::Filter;

/// Minimum name length for prefix and substring lookup (the trigram index needs three chars).
const MIN_FUZZY_CHARS: usize = 3;
/// `kind` argument that selects Markdown headings of every level.
const HEADING_KIND: &str = "heading";
/// Markdown headings (`heading_1` … `heading_6`).
const IS_HEADING: &str = "s.kind LIKE 'heading\\_%' ESCAPE '\\'";
/// Values rank after functions and types of the same name.
const VALUE_KINDS_LAST: &str = "(s.kind IN ('constant', 'property'))";

/// One definition. Serialises as `{ kind, name, qualifiedName, signature, startLine, endLine, depth }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolRow {
    #[serde(skip)]
    pub id: i64,
    #[serde(skip)]
    pub repo_id: i64,
    #[serde(skip)]
    pub file_id: i64,
    /// Path of the file that defines the symbol.
    #[serde(skip)]
    pub path: String,
    pub kind: String,
    pub name: String,
    pub qualified_name: String,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub depth: u32,
}

const COLUMNS: &str = "s.id, s.repo_id, s.file_id, f.path, s.kind, s.name, s.qualified_name, \
                       s.signature, s.start_line, s.end_line, s.depth";

fn symbol_row(row: &Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        file_id: row.get(2)?,
        path: row.get(3)?,
        kind: row.get(4)?,
        name: row.get(5)?,
        qualified_name: row.get(6)?,
        signature: row.get(7)?,
        start_line: row.get(8)?,
        end_line: row.get(9)?,
        depth: row.get(10)?,
    })
}

/// One lookup step: the `FROM … WHERE` part with its values and the `ORDER BY` with its values.
struct Bucket {
    from_where: &'static str,
    where_values: Vec<Value>,
    order_by: String,
    order_values: Vec<Value>,
}

/// Exact name matches first; only when there are none, name prefixes; only then substrings.
/// Each step applies `filter` before `limit`. Headings are left out unless `kind` is a heading
/// kind or `"heading"` (every level).
pub fn lookup(
    conn: &Connection,
    filter: &Filter,
    candidate: &str,
    kind: Option<&str>,
    limit: usize,
) -> rusqlite::Result<Vec<SymbolRow>> {
    let candidate = candidate.trim();
    let (qualifier, seg) = split_qualifier(candidate);
    if seg.is_empty() || filter.repo_ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let qualified = qualifier
        .map(|_| normalize_qualified(candidate).to_lowercase())
        .unwrap_or_default();

    let exact = Bucket {
        from_where: "FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.name = ?",
        where_values: vec![Value::from(seg.to_string())],
        order_by: format!(
            "ORDER BY (? != '' AND LOWER(SUBSTR(s.qualified_name, -LENGTH(?))) = ?) DESC,
                      (s.name = ? COLLATE BINARY) DESC, {VALUE_KINDS_LAST}, s.depth, s.id"
        ),
        order_values: vec![
            Value::from(qualified.clone()),
            Value::from(qualified.clone()),
            Value::from(qualified),
            Value::from(seg.to_string()),
        ],
    };
    let exact = run(conn, filter, kind, limit, exact)?;
    if !exact.is_empty() || seg.chars().count() < MIN_FUZZY_CHARS {
        return Ok(exact);
    }

    let fuzzy_order = format!("ORDER BY {VALUE_KINDS_LAST}, LENGTH(s.name), s.depth, s.id");
    let prefix = Bucket {
        from_where: "FROM symbols s JOIN files f ON f.id = s.file_id
                     WHERE s.name LIKE ? ESCAPE '\\'",
        where_values: vec![Value::from(format!("{}%", escape_like(seg)))],
        order_by: fuzzy_order.clone(),
        order_values: Vec::new(),
    };
    let prefix = run(conn, filter, kind, limit, prefix)?;
    if !prefix.is_empty() {
        return Ok(prefix);
    }

    let substring = Bucket {
        from_where: "FROM symbols_fts
                     JOIN symbols s ON s.id = symbols_fts.rowid
                     JOIN files f ON f.id = s.file_id
                     WHERE symbols_fts MATCH ?",
        where_values: vec![Value::from(format!("\"{}\"", seg.replace('"', "\"\"")))],
        order_by: fuzzy_order,
        order_values: Vec::new(),
    };
    run(conn, filter, kind, limit, substring)
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

fn run(
    conn: &Connection,
    filter: &Filter,
    kind: Option<&str>,
    limit: usize,
    bucket: Bucket,
) -> rusqlite::Result<Vec<SymbolRow>> {
    let mut sql = format!(
        "SELECT {COLUMNS} {} AND s.repo_id IN ({})",
        bucket.from_where,
        placeholders(filter.repo_ids.len())
    );
    let mut values = bucket.where_values;
    values.extend(filter.repo_ids.iter().map(|&id| Value::from(id)));
    push_file_filter(&mut sql, &mut values, filter);
    match kind {
        Some(kind) if kind.eq_ignore_ascii_case(HEADING_KIND) => {
            sql.push_str(&format!(" AND {IS_HEADING}"));
        }
        Some(kind) => {
            sql.push_str(" AND s.kind = ? COLLATE NOCASE");
            values.push(Value::from(kind.to_string()));
        }
        None => sql.push_str(&format!(" AND NOT {IS_HEADING}")),
    }
    sql.push(' ');
    sql.push_str(&bucket.order_by);
    sql.push_str(" LIMIT ?");
    values.extend(bucket.order_values);
    values.push(Value::from(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), symbol_row)?;
    rows.collect()
}

/// Every symbol of a file (headings included), by line.
pub fn outline(conn: &Connection, file_id: i64) -> rusqlite::Result<Vec<SymbolRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM symbols s JOIN files f ON f.id = s.file_id
         WHERE s.file_id = ?1
         ORDER BY s.start_line, s.depth, s.id"
    ))?;
    let rows = stmt.query_map([file_id], symbol_row)?;
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
