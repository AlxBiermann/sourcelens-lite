//! Exhaustive line search: every line of the indexed text files that contains an exact text or
//! matches a regular expression.

use std::time::{Duration, Instant};

use regex::bytes::{Regex, RegexBuilder};
use rusqlite::params_from_iter;
use rusqlite::types::{Value, ValueRef};
use serde::Serialize;

use crate::error::{ApiError, ApiResult};
use crate::repositories;
use crate::search::{cap_line, lexical, Filter};
use crate::state::AppState;

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;
pub const MAX_PATTERN_CHARS: usize = 1_000;
/// Matched lines are cut to this length (worst case 200 lines × 240 chars stays near 50k chars).
pub const MAX_LINE_CHARS: usize = 240;
pub const TIME_BUDGET: Duration = Duration::from_secs(5);
const REGEX_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct GrepRequest {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub filter: Filter,
    /// Clamped to `1..=MAX_LIMIT`.
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepMatch {
    pub repository: String,
    pub path: String,
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GrepResponse {
    pub matches: Vec<GrepMatch>,
    pub total_matches: u64,
    pub files_with_matches: u64,
    pub files_searched: u64,
    pub truncated: bool,
    pub timed_out: bool,
}

/// The matcher for `pattern`: escaped unless `regex`, linear-time, at most [`REGEX_SIZE_LIMIT`]
/// compiled. Every failure is 422 (`empty_pattern`, `invalid_pattern`).
pub fn compile(pattern: &str, regex: bool, case_sensitive: bool) -> ApiResult<Regex> {
    if pattern.is_empty() {
        return Err(ApiError::unprocessable("empty_pattern", "Enter a pattern."));
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(ApiError::unprocessable(
            "invalid_pattern",
            "The pattern is longer than 1000 characters.",
        ));
    }
    if !regex && pattern.contains(['\n', '\r']) {
        return Err(ApiError::unprocessable(
            "invalid_pattern",
            "The pattern contains a line break. grep_code matches line by line; search for one line of it.",
        ));
    }
    let source = if regex {
        pattern.to_string()
    } else {
        regex::escape(pattern)
    };
    RegexBuilder::new(&source)
        .case_insensitive(!case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|err| {
            ApiError::unprocessable(
                "invalid_pattern",
                format!(
                    "Invalid regular expression. Set regex to false to search for the text literally.\n{err}"
                ),
            )
        })
}

/// [`grep_within`] the [`TIME_BUDGET`].
pub async fn grep(state: &AppState, req: GrepRequest) -> ApiResult<GrepResponse> {
    grep_within(state, req, TIME_BUDGET).await
}

/// Every matching line of the files in `req.filter`, by repository name, path and line; the
/// first `limit` are returned, the rest only counted. Stops before the next file once `budget`
/// has passed.
pub async fn grep_within(
    state: &AppState,
    req: GrepRequest,
    budget: Duration,
) -> ApiResult<GrepResponse> {
    let re = compile(&req.pattern, req.regex, req.case_sensitive)?;
    let limit = req.limit.clamp(1, MAX_LIMIT);
    let filter = req.filter;
    let started = Instant::now();
    let response = state
        .db
        .read(move |conn| -> rusqlite::Result<GrepResponse> {
            let mut response = GrepResponse {
                matches: Vec::new(),
                total_matches: 0,
                files_with_matches: 0,
                files_searched: 0,
                truncated: false,
                timed_out: false,
            };
            if filter.repo_ids.is_empty() {
                return Ok(response);
            }
            let mut repos: Vec<(i64, String)> = repositories::full_names(conn, &filter.repo_ids)?
                .into_iter()
                .collect();
            repos.sort_by_cached_key(|(id, name)| (name.to_lowercase(), *id));

            'repos: for (repo_id, repository) in &repos {
                let mut sql = String::from(
                    "SELECT f.path, f.content FROM files f \
                     WHERE f.repo_id = ? AND f.skip_reason IS NULL AND f.content IS NOT NULL",
                );
                let mut values = vec![Value::from(*repo_id)];
                lexical::push_file_filter(&mut sql, &mut values, &filter);
                sql.push_str(" ORDER BY f.path");
                let mut stmt = conn.prepare(&sql)?;
                let mut rows = stmt.query(params_from_iter(values))?;
                while let Some(row) = rows.next()? {
                    if started.elapsed() >= budget {
                        response.timed_out = true;
                        break 'repos;
                    }
                    response.files_searched += 1;
                    let path = bytes(row.get_ref(0)?);
                    let content = bytes(row.get_ref(1)?);
                    let content = content.strip_suffix(b"\n").unwrap_or(content);
                    if content.is_empty() {
                        continue;
                    }
                    let mut file_matched = false;
                    for (index, line) in content.split(|&b| b == b'\n').enumerate() {
                        if !re.is_match(line) {
                            continue;
                        }
                        file_matched = true;
                        response.total_matches += 1;
                        if response.matches.len() < limit {
                            response.matches.push(GrepMatch {
                                repository: repository.clone(),
                                path: String::from_utf8_lossy(path).into_owned(),
                                line: u32::try_from(index + 1).unwrap_or(u32::MAX),
                                text: cap_line(&String::from_utf8_lossy(line), MAX_LINE_CHARS)
                                    .into_owned(),
                            });
                        }
                    }
                    if file_matched {
                        response.files_with_matches += 1;
                    }
                }
            }
            response.truncated = response.total_matches > response.matches.len() as u64;
            Ok(response)
        })
        .await?;
    Ok(response)
}

/// The bytes of a text or blob column; nothing for other types.
fn bytes(value: ValueRef<'_>) -> &[u8] {
    match value {
        ValueRef::Blob(b) | ValueRef::Text(b) => b,
        _ => &[],
    }
}
