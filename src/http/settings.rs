use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::{json, Value};

use crate::error::{ApiError, ApiResult};
use crate::http::session::{AuthUser, Owner};
use crate::index::embed::{self, EmbeddingSettings, EmbeddingSettingsInput};
use crate::state::AppState;

/// `/api/settings/github`, `/api/settings/embeddings[/test]`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/settings/github", get(github_settings))
        .route(
            "/api/settings/embeddings",
            get(get_embeddings).put(put_embeddings),
        )
        .route("/api/settings/embeddings/test", post(test_embeddings))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GithubSettings {
    source: &'static str,
    slug: String,
    app_url: String,
    install_url: String,
    webhook_url: String,
    webhook_active: bool,
    last_delivery_at: Option<i64>,
    last_delivery_event: Option<String>,
}

async fn github_settings(
    State(state): State<AppState>,
    _user: AuthUser,
) -> ApiResult<Json<GithubSettings>> {
    let creds = state.github.credentials().await?.ok_or_else(|| {
        ApiError::not_found("app_not_configured", "No GitHub App is configured yet.")
    })?;
    let last = state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT received_at, event FROM webhook_deliveries ORDER BY received_at DESC, rowid DESC LIMIT 1",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
        })
        .await?;
    let has_secret = creds
        .webhook_secret
        .as_deref()
        .is_some_and(|s| !s.is_empty());
    Ok(Json(GithubSettings {
        source: creds.source,
        install_url: format!("https://github.com/apps/{}/installations/new", creds.slug),
        slug: creds.slug,
        app_url: creds.html_url,
        webhook_url: format!("{}/webhooks/github", state.cfg.public_url),
        webhook_active: has_secret && !state.cfg.public_host_is_local(),
        last_delivery_at: last.as_ref().map(|(at, _)| *at),
        last_delivery_event: last.map(|(_, event)| event),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EmbeddingsView {
    #[serde(flatten)]
    settings: EmbeddingSettings,
    embedded_chunks: i64,
    total_chunks: i64,
    failed_chunks: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    reembed_repositories: Option<u32>,
}

async fn embeddings_view(
    state: &AppState,
    settings: EmbeddingSettings,
    reembed_repositories: Option<u32>,
) -> ApiResult<EmbeddingsView> {
    let (embedded_chunks, total_chunks, failed_chunks) = state
        .db
        .read(|conn| {
            conn.query_row(
                "SELECT coalesce(sum(embed_state = 1), 0), count(*), coalesce(sum(embed_state = 2), 0) FROM chunks",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })
        .await?;
    Ok(EmbeddingsView {
        settings,
        embedded_chunks,
        total_chunks,
        failed_chunks,
        reembed_repositories,
    })
}

async fn get_embeddings(
    State(state): State<AppState>,
    _user: AuthUser,
) -> ApiResult<Json<EmbeddingsView>> {
    let settings = embed::settings(&state).await?;
    Ok(Json(embeddings_view(&state, settings, None).await?))
}

async fn put_embeddings(
    State(state): State<AppState>,
    _owner: Owner,
    Json(input): Json<EmbeddingSettingsInput>,
) -> ApiResult<Json<EmbeddingsView>> {
    let (settings, queued) = embed::apply_settings(&state, input).await?;
    Ok(Json(embeddings_view(&state, settings, Some(queued)).await?))
}

async fn test_embeddings(
    State(state): State<AppState>,
    _owner: Owner,
    Json(input): Json<EmbeddingSettingsInput>,
) -> ApiResult<Json<Value>> {
    Ok(Json(match embed::test_connection(&state, &input).await? {
        Ok((dimensions, latency_ms)) => {
            json!({ "ok": true, "dimensions": dimensions, "latencyMs": latency_ms })
        }
        Err(error) => json!({ "ok": false, "error": error }),
    }))
}
