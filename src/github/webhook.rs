use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::Router;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::crypto::hmac_sha256_verify;
use crate::error::{ApiError, ApiResult};
use crate::index::jobs;
use crate::state::{now, AppState};

/// Deliveries older than this are forgotten.
const DELIVERY_RETENTION_SECS: i64 = 7 * 24 * 60 * 60;

/// `POST /webhooks/github`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/webhooks/github", post(receive))
}

async fn receive(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    match handle(&state, &headers, &body).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn handle(state: &AppState, headers: &HeaderMap, body: &[u8]) -> ApiResult<Response> {
    let secret = state
        .github
        .credentials()
        .await?
        .and_then(|creds| creds.webhook_secret)
        .filter(|secret| !secret.is_empty())
        .ok_or_else(|| ApiError::not_found("webhooks_disabled", "Webhooks are not configured."))?;
    let signature = header(headers, "x-hub-signature-256").unwrap_or_default();
    if !hmac_sha256_verify(secret.as_bytes(), body, signature) {
        return Err(ApiError::unauthorized(
            "invalid_signature",
            "The webhook signature does not match.",
        ));
    }
    let (Some(event), Some(delivery)) = (
        header(headers, "x-github-event"),
        header(headers, "x-github-delivery"),
    ) else {
        return Err(ApiError::bad_request(
            "invalid_webhook",
            "X-GitHub-Event and X-GitHub-Delivery headers are required.",
        ));
    };
    let payload: Value = serde_json::from_slice(body).map_err(|err| {
        ApiError::bad_request("invalid_webhook", format!("The payload is not JSON: {err}"))
    })?;

    let (event, delivery) = (event.to_string(), delivery.to_string());
    let received_at = now();
    let recorded = {
        let (event, delivery) = (event.clone(), delivery.clone());
        state
            .db
            .write(move |conn| {
                let inserted = conn.execute(
                    "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, event, received_at) VALUES (?1, ?2, ?3)",
                    params![delivery, event, received_at],
                )?;
                conn.execute(
                    "DELETE FROM webhook_deliveries WHERE received_at < ?1",
                    [received_at - DELIVERY_RETENTION_SECS],
                )?;
                Ok(inserted == 1)
            })
            .await?
    };
    if !recorded {
        return Ok((StatusCode::OK, "duplicate").into_response());
    }

    if let Err(err) = dispatch(state, &event, &payload).await {
        // Let GitHub's redelivery run the handling again.
        state
            .db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM webhook_deliveries WHERE delivery_id = ?1",
                    [delivery],
                )
            })
            .await?;
        return Err(err);
    }
    Ok((StatusCode::ACCEPTED, "accepted").into_response())
}

async fn dispatch(state: &AppState, event: &str, payload: &Value) -> ApiResult<()> {
    let to_enqueue = match event {
        "push" => on_push(state, payload).await?,
        "repository" => on_repository(state, payload).await?,
        "installation" => on_installation(state, payload).await?,
        "installation_repositories" => on_installation_repositories(state, payload).await?,
        _ => Vec::new(),
    };
    for (repo_id, target_commit) in to_enqueue {
        jobs::enqueue(state, repo_id, "webhook", target_commit.as_deref(), false).await?;
    }
    Ok(())
}

type Enqueue = Vec<(i64, Option<String>)>;

async fn on_push(state: &AppState, payload: &Value) -> ApiResult<Enqueue> {
    let Some(github_id) = payload["repository"]["id"].as_i64() else {
        return Ok(Vec::new());
    };
    if payload["deleted"].as_bool() == Some(true) {
        return Ok(Vec::new());
    }
    let git_ref = payload["ref"].as_str().unwrap_or_default().to_string();
    let after = payload["after"]
        .as_str()
        .filter(|sha| !sha.is_empty() && !sha.bytes().all(|b| b == b'0'))
        .map(str::to_string);
    let repo = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT id, COALESCE(branch, default_branch) FROM repositories
                 WHERE github_id = ?1 AND state = 'connected'",
                [github_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
        })
        .await?;
    Ok(match repo {
        Some((repo_id, branch)) if git_ref == format!("refs/heads/{branch}") => {
            vec![(repo_id, after)]
        }
        _ => Vec::new(),
    })
}

async fn on_repository(state: &AppState, payload: &Value) -> ApiResult<Enqueue> {
    let repository = &payload["repository"];
    let Some(github_id) = repository["id"].as_i64() else {
        return Ok(Vec::new());
    };
    if payload["action"].as_str() == Some("deleted") {
        state
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE repositories SET state = 'disconnected', state_reason = 'Deleted on GitHub'
                     WHERE github_id = ?1 AND state != 'deleting'",
                    [github_id],
                )
            })
            .await?;
        return Ok(Vec::new());
    }
    let full_name = repository["full_name"].as_str().map(str::to_string);
    let private = repository["private"].as_bool();
    let default_branch = repository["default_branch"].as_str().map(str::to_string);
    let enqueue = state
        .db
        .write(move |conn| {
            let Some((repo_id, old_default, follows_default, repo_state)) = conn
                .query_row(
                    "SELECT id, default_branch, branch IS NULL, state FROM repositories
                     WHERE github_id = ?1 AND state != 'deleting'",
                    [github_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, bool>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()?
            else {
                return Ok(None);
            };
            conn.execute(
                "UPDATE repositories SET full_name = COALESCE(?2, full_name), private = COALESCE(?3, private),
                   default_branch = COALESCE(?4, default_branch)
                 WHERE id = ?1",
                params![repo_id, full_name, private, default_branch],
            )?;
            let default_changed = default_branch
                .as_deref()
                .is_some_and(|branch| branch != old_default);
            Ok((default_changed && follows_default && repo_state == "connected").then_some(repo_id))
        })
        .await?;
    Ok(enqueue.map(|id| (id, None)).into_iter().collect())
}

async fn on_installation(state: &AppState, payload: &Value) -> ApiResult<Enqueue> {
    let Some(installation_id) = payload["installation"]["id"].as_i64() else {
        return Ok(Vec::new());
    };
    let action = payload["action"].as_str().unwrap_or_default().to_string();
    let added = repository_ids(&payload["repositories"]);
    let reconnected = state
        .db
        .write(move |conn| {
            let reason = match action.as_str() {
                "deleted" => "GitHub App uninstalled",
                "suspend" => "GitHub App suspended",
                "unsuspend" | "created" => {
                    let mut ids = conn
                        .prepare(
                            "UPDATE repositories SET state = 'connected', state_reason = NULL
                             WHERE installation_id = ?1 AND state = 'disconnected' RETURNING id",
                        )?
                        .query_map([installation_id], |row| row.get::<_, i64>(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    // A new installation has a new id; its repositories are listed in the payload.
                    ids.extend(reconnect(conn, installation_id, &added)?);
                    return Ok(ids);
                }
                _ => return Ok(Vec::new()),
            };
            conn.execute(
                "UPDATE repositories SET state = 'disconnected', state_reason = ?2
                 WHERE installation_id = ?1 AND state = 'connected'",
                params![installation_id, reason],
            )?;
            Ok(Vec::new())
        })
        .await?;
    Ok(reconnected.into_iter().map(|id| (id, None)).collect())
}

async fn on_installation_repositories(state: &AppState, payload: &Value) -> ApiResult<Enqueue> {
    let Some(installation_id) = payload["installation"]["id"].as_i64() else {
        return Ok(Vec::new());
    };
    let removed = repository_ids(&payload["repositories_removed"]);
    let added = repository_ids(&payload["repositories_added"]);
    let reconnected = state
        .db
        .write(move |conn| {
            for github_id in &removed {
                conn.execute(
                    "UPDATE repositories SET state = 'disconnected', state_reason = 'Access removed on GitHub'
                     WHERE github_id = ?1 AND state = 'connected'",
                    [github_id],
                )?;
            }
            reconnect(conn, installation_id, &added)
        })
        .await?;
    Ok(reconnected.into_iter().map(|id| (id, None)).collect())
}

/// Marks disconnected repositories with these GitHub ids as connected through `installation_id`.
fn reconnect(
    conn: &Connection,
    installation_id: i64,
    github_ids: &[i64],
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "UPDATE repositories SET state = 'connected', state_reason = NULL, installation_id = ?1
         WHERE github_id = ?2 AND state = 'disconnected' RETURNING id",
    )?;
    let mut ids = Vec::new();
    for github_id in github_ids {
        if let Some(id) = stmt
            .query_row(params![installation_id, github_id], |row| row.get(0))
            .optional()?
        {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn repository_ids(list: &Value) -> Vec<i64> {
    list.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_i64())
                .collect()
        })
        .unwrap_or_default()
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
