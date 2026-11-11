use std::collections::{HashMap, HashSet};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use futures_util::{StreamExt, TryStreamExt};
use rusqlite::{params, Connection, OptionalExtension};

use crate::crypto::git_blob_sha1;
use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::github::api::{self, RawTreeEntry};
use crate::index::chunker::CHUNKER_VERSION;
use crate::index::store::{self, PreparedFile, TreeEntry};
use crate::index::{embed, select};
use crate::state::{now, AppState};

/// More changed files than this are fetched as one tarball instead of single blobs.
pub const TARBALL_THRESHOLD: usize = 200;
/// Files per `apply_files` transaction.
const BATCH_SIZE: usize = 50;
/// Parallel blob downloads.
const BLOB_CONCURRENCY: usize = 8;
const NO_ACCESS_REASON: &str = "GitHub App has no access";

/// A job row claimed by a worker.
#[derive(Debug, Clone)]
pub struct ClaimedJob {
    pub id: i64,
    pub repo_id: i64,
    pub trigger: String,
    pub target_commit: Option<String>,
    pub force: bool,
}

/// What a finished job changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct JobOutcome {
    pub files_changed: u32,
    pub files_deleted: u32,
    pub files_skipped: u32,
    pub embedded: u32,
}

struct Repo {
    id: i64,
    full_name: String,
    installation_id: u64,
    effective_branch: String,
    state: String,
    indexed_commit: Option<String>,
    chunker_version: i64,
}

/// File counters of the job, written to its row after every batch.
#[derive(Debug, Clone, Copy, Default)]
struct Progress {
    total: u32,
    done: u32,
    changed: u32,
    skipped: u32,
}

/// Which archive entries an archive pass indexes.
enum Selection {
    /// Exactly these tree entries (by path); entries missing from the archive are recorded as skipped.
    Fetch(HashMap<String, TreeEntry>),
    /// Every eligible entry whose blob SHA differs from `existing` (truncated tree).
    All {
        existing: HashMap<String, String>,
        force: bool,
    },
}

/// Removes the temporary tarball when dropped, also on errors.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!("cannot remove {}: {err}", self.0.display());
            }
        }
    }
}

pub async fn run_job(state: &AppState, job: ClaimedJob) -> Result<JobOutcome, ApiError> {
    let mut repo = load_repo(&state.db, job.repo_id)
        .await?
        .ok_or_else(disconnected)?;
    if repo.state != "connected" {
        return Err(disconnected());
    }
    let token = access_token(state, &mut repo).await?;

    let head = match &job.target_commit {
        Some(commit) => commit.clone(),
        None => {
            let head = api::branch_head(
                &state.github,
                &token,
                &repo.full_name,
                &repo.effective_branch,
            )
            .await
            .map_err(|err| forget_rejected_token(state, &repo, err))?;
            // The Activity table shows which commit a manual, connect or startup job worked on.
            let (job_id, commit) = (job.id, head.clone());
            state
                .db
                .write(move |conn| {
                    conn.execute(
                        "UPDATE jobs SET target_commit = ?2 WHERE id = ?1",
                        rusqlite::params![job_id, commit],
                    )
                })
                .await?;
            head
        }
    };
    let force = job.force || repo.chunker_version != CHUNKER_VERSION;

    let mut outcome = JobOutcome::default();
    if force || repo.indexed_commit.as_deref() != Some(head.as_str()) {
        index_commit(state, &job, &repo, &token, &head, force, &mut outcome)
            .await
            .map_err(|err| forget_rejected_token(state, &repo, err))?;
    }
    outcome.embedded = embed::embed_pending(state, repo.id, job.id).await?;
    Ok(outcome)
}

async fn index_commit(
    state: &AppState,
    job: &ClaimedJob,
    repo: &Repo,
    token: &str,
    head: &str,
    force: bool,
    outcome: &mut JobOutcome,
) -> ApiResult<()> {
    let (job_id, repo_id) = (job.id, repo.id);
    state
        .db
        .write(move |conn| conn.execute("UPDATE jobs SET phase = 'index' WHERE id = ?1", [job_id]))
        .await?;

    let (raw_tree, truncated) = api::tree(&state.github, token, &repo.full_name, head).await?;
    let existing = state
        .db
        .read(move |conn| store::existing_files(conn, repo_id))
        .await?;

    let (progress, delete) = if truncated {
        let selection = Selection::All { existing, force };
        index_from_archive(
            state,
            job_id,
            repo,
            token,
            head,
            selection,
            Progress::default(),
        )
        .await?
    } else {
        let max_bytes = state.cfg.max_file_bytes;
        let eligible: Vec<TreeEntry> = raw_tree
            .into_iter()
            .filter(|entry| is_eligible(entry, max_bytes))
            .map(|entry| TreeEntry {
                path: entry.path,
                blob_sha: entry.sha,
                size: entry.size.unwrap_or(0),
            })
            .collect();
        let plan = store::plan_diff(&existing, &eligible, force);
        let progress = Progress {
            total: count(plan.fetch.len()),
            ..Progress::default()
        };
        write_progress_async(&state.db, job_id, progress).await?;
        let progress = if plan.fetch.len() <= TARBALL_THRESHOLD {
            index_from_blobs(state, job_id, repo, token, plan.fetch, progress).await?
        } else {
            let fetch = plan
                .fetch
                .into_iter()
                .map(|entry| (entry.path.clone(), entry))
                .collect();
            let (progress, _) = index_from_archive(
                state,
                job_id,
                repo,
                token,
                head,
                Selection::Fetch(fetch),
                progress,
            )
            .await?;
            progress
        };
        (progress, plan.delete)
    };

    let head = head.to_string();
    let indexed_at = now();
    let deleted = state
        .db
        .write(move |conn| {
            if !repo_connected(conn, repo_id)? {
                return Ok(None);
            }
            let deleted = if delete.is_empty() {
                0
            } else {
                store::delete_paths(conn, repo_id, &delete)?
            };
            conn.execute(
                "UPDATE jobs SET files_deleted = ?2 WHERE id = ?1",
                params![job_id, deleted],
            )?;
            conn.execute(
                "UPDATE repositories SET indexed_commit = ?2, indexed_at = ?3, chunker_version = ?4,
                   file_count = (SELECT count(*) FROM files WHERE repo_id = ?1 AND skip_reason IS NULL),
                   chunk_count = (SELECT count(*) FROM chunks WHERE repo_id = ?1)
                 WHERE id = ?1",
                params![repo_id, head, indexed_at, CHUNKER_VERSION],
            )?;
            Ok(Some(deleted))
        })
        .await?
        .ok_or_else(disconnected)?;

    outcome.files_changed = progress.changed;
    outcome.files_skipped = progress.skipped;
    outcome.files_deleted = deleted;
    Ok(())
}

/// Fetches changed files as single blobs, in batches of [`BATCH_SIZE`].
async fn index_from_blobs(
    state: &AppState,
    job_id: i64,
    repo: &Repo,
    token: &str,
    fetch: Vec<TreeEntry>,
    mut progress: Progress,
) -> ApiResult<Progress> {
    let gh = &*state.github;
    let full_name = repo.full_name.as_str();
    for batch in fetch.chunks(BATCH_SIZE) {
        ensure_connected(&state.db, repo.id).await?;
        let files: Vec<PreparedFile> = futures_util::stream::iter(batch.iter().cloned())
            .map(|entry| async move {
                let bytes = api::blob(gh, token, full_name, &entry.blob_sha).await?;
                tokio::task::spawn_blocking(move || {
                    store::prepare_file(&entry.path, &entry.blob_sha, &bytes)
                })
                .await
                .map_err(ApiError::internal)
            })
            .buffer_unordered(BLOB_CONCURRENCY)
            .try_collect()
            .await?;
        let repo_id = repo.id;
        let applied = state
            .db
            .write(move |conn| apply_batch(conn, job_id, repo_id, &files, progress))
            .await?;
        progress = applied.ok_or_else(disconnected)?;
    }
    Ok(progress)
}

/// Downloads the commit tarball and indexes the selected entries. Returns the progress and,
/// for [`Selection::All`], the existing paths that are no longer in the archive.
async fn index_from_archive(
    state: &AppState,
    job_id: i64,
    repo: &Repo,
    token: &str,
    head: &str,
    selection: Selection,
    progress: Progress,
) -> ApiResult<(Progress, Vec<String>)> {
    let tmp_dir = state.cfg.data_dir.join("tmp");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(ApiError::internal)?;
    let tmp = TempFile(tmp_dir.join(format!("job-{job_id}.tar.gz")));
    api::tarball(&state.github, token, &repo.full_name, head, &tmp.0).await?;

    let db = state.db.clone();
    let path = tmp.0.clone();
    let repo_id = repo.id;
    let max_bytes = state.cfg.max_file_bytes;
    let result = tokio::task::spawn_blocking(move || {
        read_archive(&db, &path, job_id, repo_id, selection, max_bytes, progress)
    })
    .await
    .map_err(ApiError::internal)?;
    drop(tmp);
    result
}

fn read_archive(
    db: &Db,
    archive_path: &Path,
    job_id: i64,
    repo_id: i64,
    mut selection: Selection,
    max_bytes: u64,
    mut progress: Progress,
) -> ApiResult<(Progress, Vec<String>)> {
    let file = std::fs::File::open(archive_path).map_err(ApiError::internal)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch: Vec<PreparedFile> = Vec::with_capacity(BATCH_SIZE);

    for entry in archive.entries().map_err(archive_error)? {
        let mut entry = entry.map_err(archive_error)?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let Some(path) = entry.path().ok().and_then(|path| repository_path(&path)) else {
            continue;
        };
        match &mut selection {
            Selection::Fetch(fetch) => {
                let Some(tree_entry) = fetch.remove(&path) else {
                    continue;
                };
                let bytes = read_entry(&mut entry, max_bytes)?;
                batch.push(store::prepare_file(&path, &tree_entry.blob_sha, &bytes));
            }
            Selection::All { existing, force } => {
                if !select::path_allowed(&path) || entry.size() > max_bytes {
                    continue;
                }
                let bytes = read_entry(&mut entry, max_bytes)?;
                let blob_sha = git_blob_sha1(&bytes);
                let unchanged = existing.get(&path) == Some(&blob_sha);
                seen.insert(path.clone());
                if unchanged && !*force {
                    continue;
                }
                progress.total += 1;
                batch.push(store::prepare_file(&path, &blob_sha, &bytes));
            }
        }
        if batch.len() >= BATCH_SIZE {
            progress = flush_blocking(db, job_id, repo_id, &batch, progress)?;
            batch.clear();
        }
    }

    let delete = match selection {
        Selection::Fetch(missing) => {
            let mut missing: Vec<TreeEntry> = missing.into_values().collect();
            missing.sort_by(|a, b| a.path.cmp(&b.path));
            for entry in missing {
                // Empty blob SHA: the next run sees the file as changed and fetches it again.
                batch.push(store::skipped(
                    &entry.path,
                    "",
                    entry.size,
                    "not_in_archive",
                ));
                if batch.len() >= BATCH_SIZE {
                    progress = flush_blocking(db, job_id, repo_id, &batch, progress)?;
                    batch.clear();
                }
            }
            Vec::new()
        }
        Selection::All { existing, .. } => {
            let mut gone: Vec<String> = existing
                .into_keys()
                .filter(|path| !seen.contains(path))
                .collect();
            gone.sort();
            gone
        }
    };
    if !batch.is_empty() {
        progress = flush_blocking(db, job_id, repo_id, &batch, progress)?;
    }
    Ok((progress, delete))
}

fn flush_blocking(
    db: &Db,
    job_id: i64,
    repo_id: i64,
    batch: &[PreparedFile],
    progress: Progress,
) -> ApiResult<Progress> {
    db.write_blocking(|conn| apply_batch(conn, job_id, repo_id, batch, progress))?
        .ok_or_else(disconnected)
}

/// Applies one batch under the writer lock unless the repository left the `connected` state.
fn apply_batch(
    conn: &mut Connection,
    job_id: i64,
    repo_id: i64,
    files: &[PreparedFile],
    mut progress: Progress,
) -> rusqlite::Result<Option<Progress>> {
    if !repo_connected(conn, repo_id)? {
        return Ok(None);
    }
    let stats = store::apply_files(conn, repo_id, files)?;
    progress.done += count(files.len());
    progress.changed += stats.indexed;
    progress.skipped += stats.skipped;
    write_progress(conn, job_id, progress)?;
    Ok(Some(progress))
}

fn write_progress(conn: &Connection, job_id: i64, progress: Progress) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE jobs SET files_total = ?2, files_done = ?3, files_changed = ?4, files_skipped = ?5 WHERE id = ?1",
        params![
            job_id,
            progress.total,
            progress.done,
            progress.changed,
            progress.skipped
        ],
    )?;
    Ok(())
}

async fn write_progress_async(db: &Db, job_id: i64, progress: Progress) -> ApiResult<()> {
    db.write(move |conn| write_progress(conn, job_id, progress))
        .await?;
    Ok(())
}

fn repo_connected(conn: &Connection, repo_id: i64) -> rusqlite::Result<bool> {
    let state: Option<String> = conn
        .query_row(
            "SELECT state FROM repositories WHERE id = ?1",
            [repo_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(state.as_deref() == Some("connected"))
}

async fn ensure_connected(db: &Db, repo_id: i64) -> ApiResult<()> {
    if db.read(move |conn| repo_connected(conn, repo_id)).await? {
        Ok(())
    } else {
        Err(disconnected())
    }
}

/// Installation token; on 401/404 the installation is looked up again once, and without access
/// the repository becomes `disconnected`.
async fn access_token(state: &AppState, repo: &mut Repo) -> ApiResult<String> {
    match state.github.installation_token(repo.installation_id).await {
        Ok(token) => return Ok(token),
        Err(err) if matches!(err.upstream_status, Some(401 | 404)) => {}
        Err(err) => return Err(err),
    }
    state.github.forget_installation_token(repo.installation_id);
    let retried = match state.github.repo_installation(&repo.full_name).await {
        Ok(installation_id) => {
            let repo_id = repo.id;
            let stored = i64::try_from(installation_id).map_err(ApiError::internal)?;
            state
                .db
                .write(move |conn| {
                    conn.execute(
                        "UPDATE repositories SET installation_id = ?2 WHERE id = ?1",
                        params![repo_id, stored],
                    )
                })
                .await?;
            repo.installation_id = installation_id;
            state.github.installation_token(installation_id).await
        }
        Err(err) => Err(err),
    };
    match retried {
        Ok(token) => Ok(token),
        Err(err) if is_access_denied(&err) => {
            let repo_id = repo.id;
            state
                .db
                .write(move |conn| {
                    conn.execute(
                        "UPDATE repositories SET state = 'disconnected', state_reason = ?2
                         WHERE id = ?1 AND state = 'connected'",
                        params![repo_id, NO_ACCESS_REASON],
                    )
                })
                .await?;
            Err(ApiError::forbidden(
                "github_no_access",
                format!("The GitHub App has no access to {}.", repo.full_name),
            ))
        }
        Err(err) => Err(err),
    }
}

fn is_access_denied(err: &ApiError) -> bool {
    err.code != "github_rate_limited" && matches!(err.upstream_status, Some(401 | 403 | 404))
}

/// Drops a cached installation token GitHub no longer accepts, so the next job requests a new one.
fn forget_rejected_token(state: &AppState, repo: &Repo, err: ApiError) -> ApiError {
    if err.upstream_status == Some(401) {
        state.github.forget_installation_token(repo.installation_id);
    }
    err
}

async fn load_repo(db: &Db, repo_id: i64) -> ApiResult<Option<Repo>> {
    let repo = db
        .read(move |conn| {
            conn.query_row(
                "SELECT id, full_name, installation_id, COALESCE(branch, default_branch), state,
                        indexed_commit, chunker_version
                 FROM repositories WHERE id = ?1",
                [repo_id],
                |row| {
                    Ok(Repo {
                        id: row.get(0)?,
                        full_name: row.get(1)?,
                        installation_id: column_u64(row, 2)?,
                        effective_branch: row.get(3)?,
                        state: row.get(4)?,
                        indexed_commit: row.get(5)?,
                        chunker_version: row.get(6)?,
                    })
                },
            )
            .optional()
        })
        .await?;
    Ok(repo)
}

/// Reads a non-negative INTEGER column (GitHub ids) as `u64`.
pub(crate) fn column_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(err),
        )
    })
}

/// Regular or executable blobs (no symlinks or submodules) on allowed paths within the size limit.
fn is_eligible(entry: &RawTreeEntry, max_bytes: u64) -> bool {
    entry.r#type == "blob"
        && matches!(entry.mode.as_str(), "100644" | "100755")
        && entry.size.is_some_and(|size| size <= max_bytes)
        && select::path_allowed(&entry.path)
}

/// Archive path without the leading `owner-repo-sha/` directory, `/`-separated.
fn repository_path(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for (index, component) in path.components().enumerate() {
        let Component::Normal(part) = component else {
            return None;
        };
        if index > 0 {
            parts.push(part.to_str()?);
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn read_entry(entry: &mut impl Read, max_bytes: u64) -> ApiResult<Vec<u8>> {
    let mut bytes = Vec::new();
    entry
        .take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(archive_error)?;
    Ok(bytes)
}

fn archive_error(err: std::io::Error) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "archive_unreadable",
        format!("Reading the repository archive failed: {err}"),
    )
}

fn disconnected() -> ApiError {
    ApiError::conflict("repository_disconnected", "Repository was disconnected.")
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::repository_path;

    #[test]
    fn archive_paths_lose_their_top_directory() {
        assert_eq!(
            repository_path(Path::new("acme-api-1a2b3c4/src/lib.rs")).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(repository_path(Path::new("acme-api-1a2b3c4")), None);
        assert_eq!(repository_path(Path::new("acme-api-1a2b3c4/../x")), None);
    }
}
