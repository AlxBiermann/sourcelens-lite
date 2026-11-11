use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::ApiResult;
use crate::http::session::AuthUser;
use crate::search::{self, Filter, SearchHit, SearchRequest};
use crate::state::AppState;

const DEFAULT_LIMIT: usize = 8;
const MAX_LIMIT: usize = 20;

/// `POST /api/search`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/search", post(search_handler))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchBody {
    query: String,
    repository_ids: Option<Vec<i64>>,
    #[serde(default)]
    languages: Vec<String>,
    path_prefix: Option<String>,
    limit: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchResult {
    mode: &'static str,
    terms: Vec<String>,
    results: Vec<SearchHit>,
}

async fn search_handler(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(body): Json<SearchBody>,
) -> ApiResult<Json<SearchResult>> {
    let connected = state
        .db
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT id FROM repositories WHERE state = 'connected' ORDER BY id")?;
            let ids = stmt.query_map([], |row| row.get::<_, i64>(0))?;
            ids.collect::<rusqlite::Result<Vec<i64>>>()
        })
        .await?;
    let repo_ids = match body.repository_ids {
        Some(wanted) => connected
            .into_iter()
            .filter(|id| wanted.contains(id))
            .collect(),
        None => connected,
    };
    let path_prefix = body
        .path_prefix
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty());
    let languages = body
        .languages
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let response = search::search(
        &state,
        SearchRequest {
            query: body.query,
            filter: Filter {
                repo_ids,
                languages,
                path_prefix,
            },
            limit: body.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
        },
    )
    .await?;
    Ok(Json(SearchResult {
        mode: response.mode,
        terms: response.terms,
        results: response.hits,
    }))
}
