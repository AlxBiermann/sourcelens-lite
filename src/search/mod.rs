pub mod fuse;
pub mod lexical;
pub mod query;
pub mod symbols;
pub mod vector;

use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub repo_ids: Vec<i64>,
    pub languages: Vec<String>,
    pub path_prefix: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub filter: Filter,
    /// Clamped to 1..=20, default 8.
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub repository: String,
    pub path: String,
    pub language: Option<String>,
    pub kind: String,
    pub symbol_path: String,
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

pub async fn search(_state: &AppState, _req: SearchRequest) -> Result<SearchResponse, ApiError> {
    todo!("store-search")
}
