//! Lexical channels over `chunks_fts`: weighted bm25 over names, path, body and identifier parts.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use crate::search::query::Group;
use crate::search::symbols::{escape_like, placeholders};
use crate::search::Filter;

/// FTS5 expression of one group: `"whole"*` (prefix from three chars), or-ed with all of its parts.
/// Tokens are alphanumeric, so quoting them is safe.
pub fn group_expression(group: &Group) -> String {
    let whole = if group.whole.chars().count() >= 3 {
        format!("\"{}\"*", group.whole)
    } else {
        format!("\"{}\"", group.whole)
    };
    if group.parts.is_empty() {
        return whole;
    }
    let parts: Vec<String> = group
        .parts
        .iter()
        .map(|part| format!("\"{part}\""))
        .collect();
    format!("({whole} OR ({}))", parts.join(" AND "))
}

/// All groups must match; `None` for fewer than two groups.
pub fn and_expression(groups: &[Group]) -> Option<String> {
    (groups.len() >= 2).then(|| join(groups, " AND "))
}

/// Any group may match; `None` without groups.
pub fn or_expression(groups: &[Group]) -> Option<String> {
    (!groups.is_empty()).then(|| join(groups, " OR "))
}

fn join(groups: &[Group], separator: &str) -> String {
    groups
        .iter()
        .map(group_expression)
        .collect::<Vec<_>>()
        .join(separator)
}

/// Chunk ids matching `match_expr` within `filter`, best bm25 first.
pub fn search(
    conn: &Connection,
    match_expr: &str,
    filter: &Filter,
    limit: usize,
) -> rusqlite::Result<Vec<i64>> {
    if filter.repo_ids.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let mut sql = String::from(
        "SELECT c.id FROM chunks_fts
         JOIN chunks c ON c.id = chunks_fts.rowid JOIN files f ON f.id = c.file_id
         WHERE chunks_fts MATCH ?",
    );
    let mut values = vec![Value::from(match_expr.to_string())];
    push_filter(&mut sql, &mut values, filter);
    sql.push_str(" ORDER BY bm25(chunks_fts, 6.0, 3.0, 1.0, 0.6) LIMIT ?");
    values.push(Value::from(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| row.get(0))?;
    rows.collect()
}

/// Appends ` AND …` conditions for `filter` on aliases `c` (chunks) and `f` (files).
pub(crate) fn push_filter(sql: &mut String, values: &mut Vec<Value>, filter: &Filter) {
    sql.push_str(&format!(
        " AND c.repo_id IN ({})",
        placeholders(filter.repo_ids.len())
    ));
    values.extend(filter.repo_ids.iter().map(|&id| Value::from(id)));
    if !filter.languages.is_empty() {
        sql.push_str(&format!(
            " AND f.language IN ({})",
            placeholders(filter.languages.len())
        ));
        values.extend(filter.languages.iter().map(|language| {
            let canonical = crate::index::languages::language_by_name(language)
                .map(|known| known.name.to_string())
                .unwrap_or_else(|| language.trim().to_lowercase());
            Value::from(canonical)
        }));
    }
    let prefix = filter
        .path_prefix
        .as_deref()
        .map(|p| p.trim_start_matches('/'));
    if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
        sql.push_str(" AND f.path LIKE ? ESCAPE '\\'");
        values.push(Value::from(format!("{}%", escape_like(prefix))));
    }
}
