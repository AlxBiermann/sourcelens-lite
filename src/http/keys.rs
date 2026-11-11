use std::collections::HashMap;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Deserializer, Serialize};

use crate::crypto::{random_hex, sha256_hex};
use crate::error::{ApiError, ApiResult};
use crate::http::session::AuthUser;
use crate::http::{Body, PathId};
use crate::repositories::{self, LISTED_IDS};
use crate::state::{lock, now, AppState};

pub const KEY_PREFIX: &str = "sl_";
const ALLOWED_EXPIRY_DAYS: [i64; 3] = [30, 90, 365];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKey {
    pub id: i64,
    pub name: String,
    pub last4: String,
    pub all_repositories: bool,
    pub repository_ids: Vec<i64>,
    /// Login of the creator.
    pub created_by: Option<String>,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub last_used_at: Option<i64>,
}

/// Body of `POST /api/keys`; also the input of [`create_key`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewKey {
    pub name: String,
    /// 30, 90, 365 or `None` (never expires).
    #[serde(default)]
    pub expires_in_days: Option<i64>,
    /// `None` = all repositories, including future ones.
    #[serde(default)]
    pub repository_ids: Option<Vec<i64>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatedKey {
    pub key: ApiKey,
    pub secret: String,
}

/// Body of `PATCH /api/keys/{id}`. `repositoryIds: null` switches to all repositories; an absent field keeps the scope.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyUpdate {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub repository_ids: Option<Option<Vec<i64>>>,
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `/api/keys[/{id}]`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/keys", get(list).post(create))
        .route(
            "/api/keys/{id}",
            axum::routing::patch(update).delete(remove),
        )
}

async fn list(State(state): State<AppState>, _user: AuthUser) -> ApiResult<Json<Vec<ApiKey>>> {
    let keys = state.db.read(|conn| load_keys(conn, None)).await?;
    Ok(Json(keys))
}

async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Body(input): Body<NewKey>,
) -> ApiResult<Json<CreatedKey>> {
    let (key, secret) = create_key(&state, input, Some(user.id)).await?;
    Ok(Json(CreatedKey { key, secret }))
}

async fn update(
    State(state): State<AppState>,
    _user: AuthUser,
    PathId(id): PathId,
    Body(input): Body<KeyUpdate>,
) -> ApiResult<Json<ApiKey>> {
    let name = match input.name {
        Some(name) => Some(valid_name(&name)?),
        None => None,
    };
    let scope = match input.repository_ids {
        Some(Some(ids)) => Some(Some(valid_repository_ids(ids)?)),
        other => other,
    };
    let key = state
        .db
        .write(move |conn| -> ApiResult<ApiKey> {
            let tx = conn.transaction()?;
            let exists: Option<i64> = tx
                .query_row("SELECT id FROM api_keys WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            if exists.is_none() {
                return Err(key_not_found());
            }
            if let Some(name) = &name {
                tx.execute(
                    "UPDATE api_keys SET name = ?1 WHERE id = ?2",
                    params![name, id],
                )?;
            }
            if let Some(scope) = &scope {
                replace_scope(&tx, id, scope.as_deref())?;
            }
            let key = load_keys(&tx, Some(id))?.pop();
            tx.commit()?;
            key.ok_or_else(key_not_found)
        })
        .await?;
    Ok(Json(key))
}

async fn remove(
    State(state): State<AppState>,
    _user: AuthUser,
    PathId(id): PathId,
) -> ApiResult<StatusCode> {
    let deleted = state
        .db
        .write(move |conn| conn.execute("DELETE FROM api_keys WHERE id = ?1", [id]))
        .await?;
    if deleted == 0 {
        return Err(key_not_found());
    }
    lock(&state.key_touch).remove(&id);
    Ok(StatusCode::NO_CONTENT)
}

/// Creates a key and returns it with its secret, which is not stored and cannot be shown again.
pub async fn create_key(
    state: &AppState,
    input: NewKey,
    created_by: Option<i64>,
) -> ApiResult<(ApiKey, String)> {
    let name = valid_name(&input.name)?;
    let expires_at = match input.expires_in_days {
        None => None,
        Some(days) if ALLOWED_EXPIRY_DAYS.contains(&days) => Some(now() + days * 86_400),
        Some(_) => {
            return Err(ApiError::unprocessable(
                "invalid_expiry",
                "Choose an expiry of 30, 90 or 365 days, or none.",
            ))
        }
    };
    let repository_ids = match input.repository_ids {
        Some(ids) => Some(valid_repository_ids(ids)?),
        None => None,
    };
    let secret = format!("{KEY_PREFIX}{}", random_hex(32));
    let key_hash = sha256_hex(secret.as_bytes());
    let last4 = secret[secret.len() - 4..].to_string();
    let key = state
        .db
        .write(move |conn| -> ApiResult<ApiKey> {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO api_keys (name, key_hash, last4, all_repos, created_by, created_at, expires_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)",
                params![name, key_hash, last4, created_by, now(), expires_at],
            )?;
            let id = tx.last_insert_rowid();
            if let Some(ids) = &repository_ids {
                replace_scope(&tx, id, Some(ids))?;
            }
            let key = load_keys(&tx, Some(id))?.pop();
            tx.commit()?;
            key.ok_or_else(key_not_found)
        })
        .await?;
    Ok((key, secret))
}

fn valid_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::unprocessable(
            "name_required",
            "Give the key a name.",
        ));
    }
    Ok(name.to_string())
}

fn valid_repository_ids(mut ids: Vec<i64>) -> ApiResult<Vec<i64>> {
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Err(ApiError::unprocessable(
            "no_repositories",
            "Select at least one repository or allow all repositories.",
        ));
    }
    Ok(ids)
}

/// Sets the key to all repositories (`None`) or to the given repositories; unknown ids are 422.
fn replace_scope(tx: &Transaction<'_>, key_id: i64, ids: Option<&[i64]>) -> ApiResult<()> {
    tx.execute("DELETE FROM api_key_repos WHERE key_id = ?1", [key_id])?;
    let Some(ids) = ids else {
        tx.execute("UPDATE api_keys SET all_repos = 1 WHERE id = ?1", [key_id])?;
        return Ok(());
    };
    let mut insert = tx.prepare("INSERT INTO api_key_repos (key_id, repo_id) VALUES (?1, ?2)")?;
    for &repo_id in ids {
        if !repositories::is_listed(tx, repo_id)? {
            return Err(ApiError::unprocessable(
                "unknown_repository",
                format!("Repository {repo_id} does not exist."),
            ));
        }
        insert.execute([key_id, repo_id])?;
    }
    tx.execute("UPDATE api_keys SET all_repos = 0 WHERE id = ?1", [key_id])?;
    Ok(())
}

/// All keys (newest first), or the key with `only_id`.
fn load_keys(conn: &Connection, only_id: Option<i64>) -> rusqlite::Result<Vec<ApiKey>> {
    let mut scopes: HashMap<i64, Vec<i64>> = HashMap::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT key_id, repo_id FROM api_key_repos
             WHERE (?1 IS NULL OR key_id = ?1) AND repo_id IN ({LISTED_IDS}) ORDER BY repo_id"
        ))?;
        let rows = stmt.query_map([only_id], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?)))?;
        for row in rows {
            let (key_id, repo_id) = row?;
            scopes.entry(key_id).or_default().push(repo_id);
        }
    }
    let mut stmt = conn.prepare(
        "SELECT k.id, k.name, k.last4, k.all_repos, u.login, k.created_at, k.expires_at, k.last_used_at
         FROM api_keys k LEFT JOIN users u ON u.id = k.created_by
         WHERE ?1 IS NULL OR k.id = ?1 ORDER BY k.id DESC",
    )?;
    let keys = stmt
        .query_map([only_id], |r| {
            let id: i64 = r.get(0)?;
            let all_repositories: bool = r.get(3)?;
            Ok(ApiKey {
                id,
                name: r.get(1)?,
                last4: r.get(2)?,
                all_repositories,
                repository_ids: if all_repositories {
                    Vec::new()
                } else {
                    scopes.get(&id).cloned().unwrap_or_default()
                },
                created_by: r.get(4)?,
                created_at: r.get(5)?,
                expires_at: r.get(6)?,
                last_used_at: r.get(7)?,
            })
        })?
        .collect();
    keys
}

fn key_not_found() -> ApiError {
    ApiError::not_found("key_not_found", "This API key does not exist.")
}
