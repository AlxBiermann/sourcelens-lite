use axum::extract::State;
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::http::session::AuthUser;
use crate::http::Body;
use crate::repositories::{self, CONNECTED_IDS};
use crate::search::{self, Filter, SearchHit, SearchRequest};
use crate::state::AppState;

/// `POST /api/search`, `GET /api/search/languages`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/search", post(search_handler))
        .route("/api/search/languages", get(languages))
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
    user: AuthUser,
    Body(body): Body<SearchBody>,
) -> Response {
    if let Err(retry_after) = state.search_rate.check(user.id) {
        return rate_limited(retry_after);
    }
    match run_search(&state, body).await {
        Ok(result) => Json(result).into_response(),
        Err(err) => err.into_response(),
    }
}

/// 429 `rate_limited` with `Retry-After`.
fn rate_limited(retry_after: u64) -> Response {
    let mut response = ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        format!("Too many searches. Try again in {retry_after} s."),
    )
    .into_response();
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from(retry_after));
    response
}

async fn run_search(state: &AppState, body: SearchBody) -> ApiResult<SearchResult> {
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
        state,
        SearchRequest {
            query: body.query,
            filter,
            limit: body.limit,
        },
    )
    .await?;
    Ok(SearchResult {
        mode: response.mode,
        terms: response.terms,
        results: response.hits,
    })
}

#[derive(Serialize)]
struct Languages {
    languages: Vec<String>,
}

/// Languages of the indexed files of connected repositories, sorted.
async fn languages(State(state): State<AppState>, _user: AuthUser) -> ApiResult<Json<Languages>> {
    let languages = state
        .db
        .read(|conn| {
            conn.prepare_cached(&format!(
                "SELECT DISTINCT f.language
                 FROM files f
                 WHERE f.language IS NOT NULL
                   AND f.skip_reason IS NULL
                   AND f.repo_id IN ({CONNECTED_IDS})
                 ORDER BY f.language"
            ))?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()
        })
        .await?;
    Ok(Json(Languages { languages }))
}
