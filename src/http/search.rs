use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::ApiResult;
use crate::http::session::AuthUser;
use crate::http::Body;
use crate::repositories;
use crate::search::{self, Filter, SearchHit, SearchRequest};
use crate::state::AppState;

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
    Body(body): Body<SearchBody>,
) -> ApiResult<Json<SearchResult>> {
    let connected = state.db.read(repositories::connected_ids).await?;
    let repo_ids = match body.repository_ids {
        Some(wanted) => connected
            .into_iter()
            .filter(|id| wanted.contains(id))
            .collect(),
        None => connected,
    };
    let filter = Filter::new(repo_ids, &body.languages, body.path_prefix.as_deref())?;
    let response = search::search(
        &state,
        SearchRequest {
            query: body.query,
            filter,
            limit: body.limit,
        },
    )
    .await?;
    Ok(Json(SearchResult {
        mode: response.mode,
        terms: response.terms,
        results: response.hits,
    }))
}
