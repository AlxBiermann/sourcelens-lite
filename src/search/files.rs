//! Directory listings over the indexed files of one repository.

use rusqlite::types::Value;
use rusqlite::{params_from_iter, Connection};
use serde::Serialize;

use crate::db::sql::column_u64;
use crate::search::{lexical, Filter};

/// An indexed file directly under the listed directory.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// Full repository path.
    pub path: String,
    pub language: Option<String>,
    pub line_count: u64,
}

/// The direct children of a directory, each sorted by path.
#[derive(Debug, Clone, Default)]
pub struct DirListing {
    /// Full repository path ending with `/`, and the number of indexed files below it.
    pub directories: Vec<(String, u64)>,
    pub files: Vec<FileEntry>,
}

/// Indexed files and directories directly under `prefix` (empty for the top level, otherwise
/// ending with `/`) in repository `repo_id`. The prefix is matched literally and case-sensitively.
pub fn list_dir(conn: &Connection, repo_id: i64, prefix: &str) -> rusqlite::Result<DirListing> {
    let mut sql = String::from(
        "SELECT f.path, f.language, f.line_count FROM files f
         WHERE f.repo_id = ? AND f.skip_reason IS NULL",
    );
    let mut values = vec![Value::from(repo_id)];
    let filter = Filter {
        path_prefix: (!prefix.is_empty()).then(|| prefix.to_string()),
        ..Filter::default()
    };
    lexical::push_file_filter(&mut sql, &mut values, &filter);
    sql.push_str(" ORDER BY f.path");
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
        Ok(FileEntry {
            path: row.get(0)?,
            language: row.get(1)?,
            line_count: column_u64(row, 2)?,
        })
    })?;

    let mut listing = DirListing::default();
    for row in rows {
        let entry = row?;
        // `LIKE` ignores ASCII case; the listing does not.
        let Some(rest) = entry.path.strip_prefix(prefix) else {
            continue;
        };
        // Length of `{prefix}{first segment}/` when the file lies in a subdirectory.
        let Some(dir_len) = rest.find('/').map(|at| prefix.len() + at + 1) else {
            listing.files.push(entry);
            continue;
        };
        let dir = &entry.path[..dir_len];
        // Paths below one directory are adjacent in path order.
        match listing.directories.last_mut() {
            Some((last, count)) if last.as_str() == dir => *count += 1,
            _ => listing.directories.push((dir.to_string(), 1)),
        }
    }
    Ok(listing)
}
