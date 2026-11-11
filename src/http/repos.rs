use std::collections::HashSet;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::github::api;
use crate::github::oauth::user_token;
use crate::http::session::AuthUser;
use crate::index::jobs;
use crate::index::pipeline::column_u64;
use crate::state::{now, AppState};

/// `/api/github/repositories`, `/api/repositories[/{id}[/branches|/sync]]`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/github/repositories", get(github_repositories))
        .route("/api/repositories", get(list).post(connect))
        .route(
            "/api/repositories/{id}",
            get(detail).patch(update_branch).delete(disconnect),
        )
        .route("/api/repositories/{id}/branches", get(branches))
        .route("/api/repositories/{id}/sync", post(sync))
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub id: i64,
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
    pub branch: Option<String>,
    pub effective_branch: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub indexed_commit: Option<String>,
    pub indexed_at: Option<i64>,
    pub file_count: i64,
    pub chunk_count: i64,
    pub embedded_count: i64,
    pub active_job: Option<Job>,
    pub last_job: Option<Job>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: i64,
    pub status: String,
    pub trigger: String,
    pub phase: Option<String>,
    pub target_commit: Option<String>,
    pub files_total: i64,
    pub files_done: i64,
    pub files_changed: i64,
    pub files_deleted: i64,
    pub files_skipped: i64,
    pub embed_total: i64,
    pub embed_done: i64,
    pub error: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallationView {
    installation_id: u64,
    account: AccountView,
    manage_url: String,
    truncated: bool,
    repositories: Vec<GhRepoView>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountView {
    login: String,
    avatar_url: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GhRepoView {
    github_id: u64,
    full_name: String,
    private: bool,
    default_branch: String,
    connected: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryDetail {
    repository: Repository,
    jobs: Vec<Job>,
    skipped: Vec<SkippedCount>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SkippedCount {
    reason: String,
    count: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BranchList {
    default_branch: String,
    branches: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConnectBody {
    full_name: String,
    installation_id: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchBody {
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncBody {
    #[serde(default)]
    force: bool,
}

/// Minimal repository facts the handlers need for GitHub calls.
struct RepoRef {
    full_name: String,
    installation_id: u64,
    default_branch: String,
    state: String,
}

const JOB_COLUMNS: &str = "id, status, trigger, phase, target_commit, files_total, files_done, files_changed,
    files_deleted, files_skipped, embed_total, embed_done, error, created_at, started_at, finished_at";

async fn github_repositories(
    State(state): State<AppState>,
    user: AuthUser,
) -> ApiResult<Json<Vec<InstallationView>>> {
    let token = user_token(&state, &user).await?;
    let installations = api::user_installations(&state.github, &token).await?;
    let listings =
        futures_util::future::try_join_all(installations.iter().map(|installation| {
            api::user_installation_repos(&state.github, &token, installation.id)
        }))
        .await?;
    let connected: HashSet<i64> = state
        .db
        .read(|conn| {
            let mut stmt =
                conn.prepare("SELECT github_id FROM repositories WHERE state != 'deleting'")?;
            let ids = stmt
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<HashSet<i64>>>()?;
            Ok(ids)
        })
        .await?;

    let views = installations
        .into_iter()
        .zip(listings)
        .map(|(installation, (repos, truncated))| InstallationView {
            installation_id: installation.id,
            account: AccountView {
                login: installation.account.login,
                avatar_url: installation.account.avatar_url,
            },
            manage_url: installation.html_url,
            truncated,
            repositories: repos
                .into_iter()
                .map(|repo| GhRepoView {
                    connected: i64::try_from(repo.id).is_ok_and(|id| connected.contains(&id)),
                    github_id: repo.id,
                    full_name: repo.full_name,
                    private: repo.private,
                    default_branch: repo.default_branch,
                })
                .collect(),
        })
        .collect();
    Ok(Json(views))
}

async fn list(State(state): State<AppState>, _user: AuthUser) -> ApiResult<Json<Vec<Repository>>> {
    let repos = state.db.read(|conn| load_repositories(conn, None)).await?;
    Ok(Json(repos))
}

async fn connect(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<ConnectBody>,
) -> ApiResult<(StatusCode, Json<Repository>)> {
    let full_name = body.full_name.trim().to_string();
    if !valid_full_name(&full_name) {
        return Err(ApiError::unprocessable(
            "invalid_repository_name",
            "Use the form owner/repository.",
        ));
    }
    let token = user_token(&state, &user).await?;
    let gh_repo = api::user_repo(&state.github, &token, &full_name).await?;
    let github_id = i64::try_from(gh_repo.id).map_err(ApiError::internal)?;
    let installation_id = i64::try_from(body.installation_id).map_err(ApiError::internal)?;
    let user_id = user.id;

    let inserted = state
        .db
        .write(move |conn| {
            let existing: Option<String> = conn
                .query_row(
                    "SELECT state FROM repositories WHERE github_id = ?1",
                    [github_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(existing) = existing {
                return Ok(Err(existing));
            }
            conn.query_row(
                "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, added_by, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
                params![
                    github_id,
                    installation_id,
                    gh_repo.full_name,
                    gh_repo.private,
                    gh_repo.default_branch,
                    user_id,
                    now()
                ],
                |row| row.get::<_, i64>(0),
            )
            .map(Ok)
        })
        .await?;
    let repo_id = match inserted {
        Ok(id) => id,
        Err(existing_state) if existing_state == "deleting" => {
            return Err(ApiError::conflict(
                "already_connected",
                format!("{full_name} is still being removed. Try again in a moment."),
            ))
        }
        Err(_) => {
            return Err(ApiError::conflict(
                "already_connected",
                format!("{full_name} is already connected."),
            ))
        }
    };
    jobs::enqueue(&state, repo_id, "connect", None, false).await?;
    let repo = repository(&state, repo_id).await?;
    Ok((StatusCode::CREATED, Json(repo)))
}

async fn detail(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<RepositoryDetail>> {
    let detail = state
        .db
        .read(move |conn| {
            let Some(repository) = load_repositories(conn, Some(id))?.pop() else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 ORDER BY id DESC LIMIT 20"
            ))?;
            let jobs = stmt
                .query_map([id], job_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt = conn.prepare(
                "SELECT skip_reason, count(*) FROM files WHERE repo_id = ?1 AND skip_reason IS NOT NULL
                 GROUP BY skip_reason ORDER BY count(*) DESC, skip_reason",
            )?;
            let skipped = stmt
                .query_map([id], |row| {
                    Ok(SkippedCount {
                        reason: row.get(0)?,
                        count: row.get(1)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(Some(RepositoryDetail {
                repository,
                jobs,
                skipped,
            }))
        })
        .await?
        .ok_or_else(repository_not_found)?;
    Ok(Json(detail))
}

async fn branches(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<BranchList>> {
    let repo = repo_ref(&state, id).await?;
    let token = state
        .github
        .installation_token(repo.installation_id)
        .await?;
    let branches = api::branches(&state.github, &token, &repo.full_name).await?;
    Ok(Json(BranchList {
        default_branch: repo.default_branch,
        branches,
    }))
}

async fn update_branch(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<i64>,
    Json(body): Json<BranchBody>,
) -> ApiResult<Json<Repository>> {
    let repo = repo_ref(&state, id).await?;
    require_connected(&repo)?;
    let branch = body
        .branch
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    if let Some(branch) = &branch {
        if branch.len() > 255 || branch.chars().any(char::is_control) {
            return Err(ApiError::unprocessable(
                "branch_not_found",
                "This is not a valid branch name.",
            ));
        }
        let token = state
            .github
            .installation_token(repo.installation_id)
            .await?;
        api::branch_head(&state.github, &token, &repo.full_name, branch).await?;
    }
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE repositories SET branch = ?2 WHERE id = ?1",
                params![id, branch],
            )
        })
        .await?;
    jobs::enqueue(&state, id, "manual", None, false).await?;
    Ok(Json(repository(&state, id).await?))
}

async fn sync(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Json<Job>> {
    let body: SyncBody = if body.iter().all(u8::is_ascii_whitespace) {
        SyncBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|err| {
            ApiError::bad_request(
                "invalid_body",
                format!("The request body is invalid: {err}"),
            )
        })?
    };
    let repo = repo_ref(&state, id).await?;
    require_connected(&repo)?;
    let job_id = jobs::enqueue(&state, id, "manual", None, body.force).await?;
    let job = state
        .db
        .read(move |conn| {
            conn.query_row(
                &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"),
                [job_id],
                job_from_row,
            )
            .optional()
        })
        .await?
        .ok_or_else(|| ApiError::internal(format!("job {job_id} vanished after enqueue")))?;
    Ok(Json(job))
}

async fn disconnect(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let changed = state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE repositories SET state = 'deleting' WHERE id = ?1 AND state != 'deleting'",
                [id],
            )
        })
        .await?;
    if changed == 0 {
        return Err(repository_not_found());
    }
    let task_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = jobs::purge_repository(&task_state, id).await {
            tracing::error!(repo_id = id, "deleting repository index failed: {err}");
        }
    });
    Ok(StatusCode::NO_CONTENT)
}

async fn repository(state: &AppState, id: i64) -> ApiResult<Repository> {
    state
        .db
        .read(move |conn| load_repositories(conn, Some(id)))
        .await?
        .pop()
        .ok_or_else(repository_not_found)
}

async fn repo_ref(state: &AppState, id: i64) -> ApiResult<RepoRef> {
    state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT full_name, installation_id, default_branch, state FROM repositories
                 WHERE id = ?1 AND state != 'deleting'",
                [id],
                |row| {
                    Ok(RepoRef {
                        full_name: row.get(0)?,
                        installation_id: column_u64(row, 1)?,
                        default_branch: row.get(2)?,
                        state: row.get(3)?,
                    })
                },
            )
            .optional()
        })
        .await?
        .ok_or_else(repository_not_found)
}

fn require_connected(repo: &RepoRef) -> ApiResult<()> {
    if repo.state == "connected" {
        Ok(())
    } else {
        Err(ApiError::conflict(
            "repository_disconnected",
            format!(
                "{} is disconnected. Give the GitHub App access to it again first.",
                repo.full_name
            ),
        ))
    }
}

/// Repositories that are not being deleted, ordered by name; `id` narrows to one.
fn load_repositories(conn: &Connection, id: Option<i64>) -> rusqlite::Result<Vec<Repository>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.full_name, r.private, r.default_branch, r.branch, r.state, r.state_reason,
                r.indexed_commit, r.indexed_at, r.file_count, r.chunk_count,
                (SELECT count(*) FROM chunks c WHERE c.file_id IN (SELECT f.id FROM files f WHERE f.repo_id = r.id)
                   AND c.embed_state = 1)
         FROM repositories r
         WHERE r.state != 'deleting' AND (?1 IS NULL OR r.id = ?1)
         ORDER BY r.full_name, r.id",
    )?;
    let mut repos = stmt
        .query_map([id], |row| {
            let default_branch: String = row.get(3)?;
            let branch: Option<String> = row.get(4)?;
            Ok(Repository {
                id: row.get(0)?,
                full_name: row.get(1)?,
                private: row.get(2)?,
                effective_branch: branch.clone().unwrap_or_else(|| default_branch.clone()),
                default_branch,
                branch,
                state: row.get(5)?,
                state_reason: row.get(6)?,
                indexed_commit: row.get(7)?,
                indexed_at: row.get(8)?,
                file_count: row.get(9)?,
                chunk_count: row.get(10)?,
                embedded_count: row.get(11)?,
                active_job: None,
                last_job: None,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut active = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 AND status IN ('queued', 'running')
         ORDER BY status = 'running' DESC, id DESC LIMIT 1"
    ))?;
    let mut last = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
         ORDER BY finished_at DESC, id DESC LIMIT 1"
    ))?;
    for repo in &mut repos {
        repo.active_job = active.query_row([repo.id], job_from_row).optional()?;
        repo.last_job = last.query_row([repo.id], job_from_row).optional()?;
    }
    Ok(repos)
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: row.get(0)?,
        status: row.get(1)?,
        trigger: row.get(2)?,
        phase: row.get(3)?,
        target_commit: row.get(4)?,
        files_total: row.get(5)?,
        files_done: row.get(6)?,
        files_changed: row.get(7)?,
        files_deleted: row.get(8)?,
        files_skipped: row.get(9)?,
        embed_total: row.get(10)?,
        embed_done: row.get(11)?,
        error: row.get(12)?,
        created_at: row.get(13)?,
        started_at: row.get(14)?,
        finished_at: row.get(15)?,
    })
}

/// `owner/repository` with GitHub's name characters only, so it is safe inside API paths.
fn valid_full_name(full_name: &str) -> bool {
    let valid_part = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    matches!(full_name.split_once('/'), Some((owner, name)) if valid_part(owner) && valid_part(name))
}

fn repository_not_found() -> ApiError {
    ApiError::not_found("repository_not_found", "This repository is not connected.")
}
