//! Lexical channels over `chunks_fts`: weighted bm25 over names, path, body and identifier parts.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};

use crate::db::sql::{escape_like, placeholders};
use crate::search::query::Group;
use crate::search::Filter;

/// bm25 column weights of `chunks_fts`, in column order: names, path, body, idents.
const BM25_WEIGHTS: &str = "6.0, 3.0, 1.0, 0.6";
/// Shorter tokens are matched exactly; a prefix of one or two chars expands to too many terms.
const MIN_PREFIX_CHARS: usize = 3;

/// FTS5 expression of one group: `"whole"*` (prefix from three chars), or-ed with its stemmed
/// variant and with all of its parts. Tokens are alphanumeric, so quoting them is safe.
pub fn group_expression(group: &Group) -> String {
    let mut alternatives = vec![token(&group.whole)];
    alternatives.extend(group.variant.as_deref().map(token));
    if !group.parts.is_empty() {
        let parts: Vec<String> = group
            .parts
            .iter()
            .map(|part| format!("\"{part}\""))
            .collect();
        alternatives.push(format!("({})", parts.join(" AND ")));
    }
    if alternatives.len() == 1 {
        return alternatives.remove(0);
    }
    format!("({})", alternatives.join(" OR "))
}

fn token(word: &str) -> String {
    if word.chars().count() >= MIN_PREFIX_CHARS {
        format!("\"{word}\"*")
    } else {
        format!("\"{word}\"")
    }
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
         JOIN chunks c ON c.id = chunks_fts.rowid",
    );
    push_files_join(&mut sql, filter);
    sql.push_str(" WHERE chunks_fts MATCH ?");
    let mut values = vec![Value::from(match_expr.to_string())];
    push_filter(&mut sql, &mut values, filter);
    sql.push_str(&format!(
        " ORDER BY bm25(chunks_fts, {BM25_WEIGHTS}) LIMIT ?"
    ));
    values.push(Value::from(i64::try_from(limit).unwrap_or(i64::MAX)));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| row.get(0))?;
    rows.collect()
}

/// Appends ` JOIN files f …` on alias `c` (chunks) when [`push_filter`] needs `f`.
pub(crate) fn push_files_join(sql: &mut String, filter: &Filter) {
    if filter.filters_files() {
        sql.push_str(" JOIN files f ON f.id = c.file_id");
    }
}

/// Appends ` AND …` conditions for `filter` (already normalised by [`Filter::new`]) on aliases
/// `c` (chunks) and, when [`Filter::filters_files`], `f` (files).
pub(crate) fn push_filter(sql: &mut String, values: &mut Vec<Value>, filter: &Filter) {
    sql.push_str(&format!(
        " AND c.repo_id IN ({})",
        placeholders(filter.repo_ids.len())
    ));
    values.extend(filter.repo_ids.iter().map(|&id| Value::from(id)));
    push_file_filter(sql, values, filter);
}

/// Appends the language and path conditions of `filter` on alias `f` (files).
pub(crate) fn push_file_filter(sql: &mut String, values: &mut Vec<Value>, filter: &Filter) {
    if !filter.languages.is_empty() {
        sql.push_str(&format!(
            " AND f.language IN ({})",
            placeholders(filter.languages.len())
        ));
        values.extend(filter.languages.iter().cloned().map(Value::from));
    }
    if let Some(prefix) = &filter.path_prefix {
        sql.push_str(" AND f.path LIKE ? ESCAPE '\\'");
        values.push(Value::from(format!("{}%", escape_like(prefix))));
    }
}
