use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::github::api;
use crate::http::session::{user_token, AuthUser};
use crate::http::{Body, PathId};
use crate::index::jobs;
use crate::model::{JobPhase, JobStatus, RepoState, SkipReason, Trigger, EMBED_DONE};
use crate::repositories::{self, NewRepository, RepoRow, Target};
use crate::state::{now, AppState};

/// Longest branch name accepted.
const MAX_BRANCH_NAME_BYTES: usize = 255;
/// Jobs listed in a repository's activity.
const DETAIL_JOBS: i64 = 20;

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
struct Repository {
    id: i64,
    full_name: String,
    private: bool,
    default_branch: String,
    branch: Option<String>,
    effective_branch: String,
    state: RepoState,
    state_reason: Option<String>,
    indexed_commit: Option<String>,
    indexed_at: Option<i64>,
    file_count: i64,
    chunk_count: i64,
    embedded_count: i64,
    active_job: Option<Job>,
    last_job: Option<Job>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Job {
    id: i64,
    status: JobStatus,
    trigger: Trigger,
    phase: Option<JobPhase>,
    target_commit: Option<String>,
    files_total: i64,
    files_done: i64,
    files_changed: i64,
    files_deleted: i64,
    files_skipped: i64,
    embed_total: i64,
    embed_done: i64,
    error: Option<String>,
    created_at: i64,
    started_at: Option<i64>,
    finished_at: Option<i64>,
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
    reason: SkipReason,
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
    let connected = state.db.read(repositories::listed_github_ids).await?;

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
    Body(body): Body<ConnectBody>,
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
    let new = NewRepository {
        github_id: i64::try_from(gh_repo.id).map_err(ApiError::internal)?,
        installation_id: body.installation_id,
        full_name: gh_repo.full_name,
        private: gh_repo.private,
        default_branch: gh_repo.default_branch,
        added_by: Some(user.id),
        created_at: now(),
    };
    let repo_id = state
        .db
        .write(move |conn| -> ApiResult<i64> {
            match repositories::by_github_id(conn, new.github_id)? {
                Some(existing) if existing.state == RepoState::Deleting => Err(ApiError::conflict(
                    "already_connected",
                    format!("{full_name} is still being removed. Try again in a moment."),
                )),
                Some(_) => Err(ApiError::conflict(
                    "already_connected",
                    format!("{full_name} is already connected."),
                )),
                None => Ok(repositories::insert(conn, &new)?),
            }
        })
        .await?;
    jobs::enqueue(&state, repo_id, Trigger::Connect, None, false).await?;
    let repo = repository(&state, repo_id).await?;
    Ok((StatusCode::CREATED, Json(repo)))
}

async fn detail(
    State(state): State<AppState>,
    _user: AuthUser,
    PathId(id): PathId,
) -> ApiResult<Json<RepositoryDetail>> {
    let detail = state
        .db
        .read(move |conn| -> rusqlite::Result<_> {
            let Some(repository) = load_repositories(conn, Some(id))?.pop() else {
                return Ok(None);
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 ORDER BY id DESC LIMIT ?2"
            ))?;
            let jobs = stmt
                .query_map(rusqlite::params![id, DETAIL_JOBS], job_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut stmt = conn.prepare(
                "SELECT skip_reason, count(*) FROM files
                 WHERE repo_id = ?1 AND skip_reason IS NOT NULL
                 GROUP BY skip_reason
                 ORDER BY count(*) DESC, skip_reason",
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
    PathId(id): PathId,
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
    PathId(id): PathId,
    Body(body): Body<BranchBody>,
) -> ApiResult<Json<Repository>> {
    let repo = repo_ref(&state, id).await?;
    let repo = ensure_connected(&state, repo).await?;
    let branch = body
        .branch
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty());
    if let Some(branch) = &branch {
        if branch.len() > MAX_BRANCH_NAME_BYTES || branch.chars().any(char::is_control) {
            return Err(ApiError::unprocessable(
                "invalid_branch_name",
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
        .write(move |conn| repositories::set_branch(conn, id, branch.as_deref()))
        .await?;
    jobs::enqueue(&state, id, Trigger::Manual, None, false).await?;
    Ok(Json(repository(&state, id).await?))
}

async fn sync(
    State(state): State<AppState>,
    _user: AuthUser,
    PathId(id): PathId,
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
    ensure_connected(&state, repo).await?;
    let job_id = jobs::enqueue(&state, id, Trigger::Manual, None, body.force).await?;
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
    PathId(id): PathId,
) -> ApiResult<StatusCode> {
    let marked = state
        .db
        .write(move |conn| repositories::mark_deleting(conn, id))
        .await?;
    if !marked {
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

/// The repository unless it is being deleted.
async fn repo_ref(state: &AppState, id: i64) -> ApiResult<RepoRow> {
    state
        .db
        .read(move |conn| repositories::get(conn, id))
        .await?
        .filter(|repo| repo.state != RepoState::Deleting)
        .ok_or_else(repository_not_found)
}

/// A disconnected repository gets one reconnect attempt: when GitHub confirms that the app can
/// read it, it is connected through that installation again. Callers queue the job themselves, so
/// a reconnect does not queue a second one when a worker claims the first in between.
async fn ensure_connected(state: &AppState, repo: RepoRow) -> ApiResult<RepoRow> {
    if repo.state == RepoState::Connected {
        return Ok(repo);
    }
    let installation_id = match state.github.repo_installation(&repo.full_name).await {
        Ok(installation_id) => installation_id,
        Err(err) if matches!(err.github, Some(GitHubFailure::Status(403 | 404))) => {
            return Err(disconnected(&repo));
        }
        Err(err) => return Err(err),
    };
    let id = repo.id;
    state
        .db
        .write(move |conn| repositories::reconnect(conn, Target::Id(id), installation_id))
        .await?;
    state
        .db
        .read(move |conn| repositories::get(conn, id))
        .await?
        .filter(|current| current.state == RepoState::Connected)
        .ok_or_else(|| disconnected(&repo))
}

/// 409 naming the stored reason why the repository is disconnected.
fn disconnected(repo: &RepoRow) -> ApiError {
    let reason = repo
        .state_reason
        .as_deref()
        .map(|reason| format!(" ({reason})"))
        .unwrap_or_default();
    ApiError::conflict(
        "repository_disconnected",
        format!(
            "{} is disconnected{reason}. Give the GitHub App access to it again first.",
            repo.full_name
        ),
    )
}

/// Repositories that are not being deleted, ordered by name; `id` narrows to one.
fn load_repositories(conn: &Connection, id: Option<i64>) -> rusqlite::Result<Vec<Repository>> {
    let mut embedded =
        conn.prepare("SELECT count(*) FROM chunks WHERE repo_id = ?1 AND embed_state = ?2")?;
    let mut active = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 AND status IN ('queued', 'running')
         ORDER BY status = 'running' DESC, id DESC LIMIT 1"
    ))?;
    let mut last = conn.prepare(&format!(
        "SELECT {JOB_COLUMNS} FROM jobs WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
         ORDER BY finished_at DESC, id DESC LIMIT 1"
    ))?;
    repositories::listed(conn, id)?
        .into_iter()
        .map(|repo| {
            Ok(Repository {
                effective_branch: repo.effective_branch().to_string(),
                embedded_count: embedded
                    .query_row(rusqlite::params![repo.id, EMBED_DONE], |row| row.get(0))?,
                active_job: active.query_row([repo.id], job_from_row).optional()?,
                last_job: last.query_row([repo.id], job_from_row).optional()?,
                id: repo.id,
                full_name: repo.full_name,
                private: repo.private,
                default_branch: repo.default_branch,
                branch: repo.branch,
                state: repo.state,
                state_reason: repo.state_reason,
                indexed_commit: repo.indexed_commit,
                indexed_at: repo.indexed_at,
                file_count: repo.file_count,
                chunk_count: repo.chunk_count,
            })
        })
        .collect()
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
