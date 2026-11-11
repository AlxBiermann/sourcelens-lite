use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use futures_util::{StreamExt, TryStreamExt};
use rusqlite::{params, Connection};

use crate::crypto::git_blob_sha1;
use crate::db::Db;
use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::github::api::{self, RawTreeEntry};
use crate::index::chunker::CHUNKER_VERSION;
use crate::index::store::{self, PreparedFile, TreeEntry, DELETE_BATCH_FILES};
use crate::index::{embed, select};
use crate::model::{JobPhase, RepoState, SkipReason, Trigger};
use crate::repositories::{self, RepoRow, Target};
use crate::state::{now, AppState};

/// More changed files than this are fetched as one tarball instead of single blobs.
pub const TARBALL_THRESHOLD: usize = 200;
/// Files per `apply_files` transaction.
const BATCH_SIZE: usize = 50;
/// Parallel blob downloads.
const BLOB_CONCURRENCY: usize = 8;
/// Most `prepare_file` threads of one archive pipeline.
const MAX_ARCHIVE_WORKERS: usize = 4;
/// Archive entries buffered between the reader and the `prepare_file` threads.
const ARCHIVE_ENTRY_BUFFER: usize = 16;
/// Git modes of regular and executable files; symlinks (`120000`) and submodules are not indexed.
const INDEXED_FILE_MODES: [&str; 2] = ["100644", "100755"];
const NO_ACCESS_REASON: &str = "GitHub App has no access";

/// Threads of archive pipelines that are still running.
static ARCHIVE_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Number of archive pipeline threads alive right now. Every pipeline joins its threads before
/// its job ends, so this is 0 whenever no archive is being indexed.
pub fn archive_threads_running() -> usize {
    ARCHIVE_THREADS.load(Ordering::SeqCst)
}

/// A job row claimed by a worker.
#[derive(Debug, Clone)]
pub(crate) struct ClaimedJob {
    pub id: i64,
    pub repo_id: i64,
    pub trigger: Trigger,
    /// The commit a push reported. Only informational: the job indexes the branch head.
    pub target_commit: Option<String>,
    pub force: bool,
}

/// What a finished job changed.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct JobOutcome {
    pub files_changed: u32,
    pub files_deleted: u32,
    pub files_skipped: u32,
    pub embedded: u32,
}

/// File counters of the job, written to its row after every batch.
#[derive(Debug, Clone, Copy, Default)]
struct Progress {
    total: u32,
    done: u32,
    changed: u32,
    skipped: u32,
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

pub(crate) async fn run_job(state: &AppState, job: ClaimedJob) -> ApiResult<JobOutcome> {
    let repo_id = job.repo_id;
    let mut repo = state
        .db
        .read(move |conn| repositories::get(conn, repo_id))
        .await?
        .ok_or_else(disconnected)?;
    if repo.state != RepoState::Connected {
        return Err(disconnected());
    }
    let token = access_token(state, &mut repo).await?;
    let head = resolve_head(state, &job, &repo, &token).await?;
    let force = job.force || repo.chunker_version != CHUNKER_VERSION;

    let mut outcome = if force || repo.indexed_commit.as_deref() != Some(head.as_str()) {
        index_commit(state, job.id, &repo, &token, &head, force)
            .await
            .map_err(|err| forget_rejected_token(state, &repo, err))?
    } else {
        JobOutcome::default()
    };
    outcome.embedded = embed::embed_pending(state, repo.id, job.id).await?;
    Ok(outcome)
}

/// The current head of the tracked branch, recorded on the job for the Activity table. A pushed
/// `target_commit` is not used: deliveries can arrive out of order or be replayed, and indexing
/// them would move the index back to an older commit.
async fn resolve_head(
    state: &AppState,
    job: &ClaimedJob,
    repo: &RepoRow,
    token: &str,
) -> ApiResult<String> {
    let head = api::branch_head(
        &state.github,
        token,
        &repo.full_name,
        repo.effective_branch(),
    )
    .await
    .map_err(|err| forget_rejected_token(state, repo, err))?;
    if let Some(target) = job
        .target_commit
        .as_deref()
        .filter(|target| *target != head)
    {
        tracing::debug!(
            job = job.id,
            "queued for {target}; indexing the branch head {head} instead"
        );
    }
    let (job_id, commit) = (job.id, head.clone());
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET target_commit = ?2 WHERE id = ?1",
                params![job_id, commit],
            )
        })
        .await?;
    Ok(head)
}

async fn index_commit(
    state: &AppState,
    job_id: i64,
    repo: &RepoRow,
    token: &str,
    head: &str,
    force: bool,
) -> ApiResult<JobOutcome> {
    let repo_id = repo.id;
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET phase = ?2 WHERE id = ?1",
                params![job_id, JobPhase::Index],
            )
        })
        .await?;

    let (raw_tree, truncated) = api::tree(&state.github, token, &repo.full_name, head).await?;
    let existing = state
        .db
        .read(move |conn| store::existing_files(conn, repo_id))
        .await?;

    let (progress, delete) = if truncated {
        index_whole_archive(state, job_id, repo, token, head, existing, force).await?
    } else {
        let eligible = eligible_entries(raw_tree, state.cfg.max_file_bytes);
        let plan = store::plan_diff(&existing, &eligible, force);
        let progress = Progress {
            total: saturating_u32(plan.fetch.len()),
            ..Progress::default()
        };
        store_progress(&state.db, job_id, progress).await?;
        let progress = if plan.fetch.len() <= TARBALL_THRESHOLD {
            index_from_blobs(state, job_id, repo, token, plan.fetch, progress).await?
        } else {
            index_selected_from_archive(state, job_id, repo, token, head, plan.fetch, progress)
                .await?
        };
        (progress, plan.delete)
    };

    let deleted = finish_index(state, job_id, repo_id, head, delete).await?;
    Ok(JobOutcome {
        files_changed: progress.changed,
        files_deleted: deleted,
        files_skipped: progress.skipped,
        embedded: 0,
    })
}

/// Regular or executable blobs (no symlinks or submodules) on allowed paths within the size limit.
fn eligible_entries(raw: Vec<RawTreeEntry>, max_bytes: u64) -> Vec<TreeEntry> {
    raw.into_iter()
        .filter(|entry| {
            entry.r#type == "blob"
                && INDEXED_FILE_MODES.contains(&entry.mode.as_str())
                && entry.size.is_some_and(|size| size <= max_bytes)
                && select::path_allowed(&entry.path)
        })
        .map(|entry| TreeEntry {
            path: entry.path,
            blob_sha: entry.sha,
            size: entry.size.unwrap_or(0),
        })
        .collect()
}

/// Deletes `delete` in slices of [`DELETE_BATCH_FILES`] paths, one writer turn each, then records
/// the indexed commit. Returns the number of deleted files.
async fn finish_index(
    state: &AppState,
    job_id: i64,
    repo_id: i64,
    head: &str,
    mut delete: Vec<String>,
) -> ApiResult<u32> {
    let mut deleted: u32 = 0;
    while !delete.is_empty() {
        let slice: Vec<String> = delete
            .drain(..delete.len().min(DELETE_BATCH_FILES))
            .collect();
        deleted = state
            .db
            .write(move |conn| -> ApiResult<u32> {
                require_connected(conn, repo_id)?;
                let deleted = deleted + store::delete_paths(conn, repo_id, &slice)?;
                conn.execute(
                    "UPDATE jobs SET files_deleted = ?2 WHERE id = ?1",
                    params![job_id, deleted],
                )?;
                Ok(deleted)
            })
            .await?;
    }
    let head = head.to_string();
    let indexed_at = now();
    state
        .db
        .write(move |conn| -> ApiResult<()> {
            require_connected(conn, repo_id)?;
            repositories::record_index(conn, repo_id, &head, indexed_at, CHUNKER_VERSION)?;
            Ok(())
        })
        .await?;
    Ok(deleted)
}

/// Fetches changed files as single blobs, in batches of [`BATCH_SIZE`].
async fn index_from_blobs(
    state: &AppState,
    job_id: i64,
    repo: &RepoRow,
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
        progress = apply_files_async(&state.db, job_id, repo.id, files, progress).await?;
    }
    Ok(progress)
}

/// Indexes exactly the `fetch` entries from the commit tarball. Entries missing from the archive
/// are recorded as skipped with an empty blob SHA, so the next run fetches them again.
async fn index_selected_from_archive(
    state: &AppState,
    job_id: i64,
    repo: &RepoRow,
    token: &str,
    head: &str,
    fetch: Vec<TreeEntry>,
    progress: Progress,
) -> ApiResult<Progress> {
    let fetch = fetch
        .into_iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let selection = FetchSelection(fetch);
    let (mut progress, FetchSelection(missing)) =
        index_archive(state, job_id, repo, token, head, selection, progress).await?;
    let mut missing: Vec<TreeEntry> = missing.into_values().collect();
    missing.sort_by(|a, b| a.path.cmp(&b.path));
    for batch in missing.chunks(BATCH_SIZE) {
        let files = batch
            .iter()
            .map(|entry| store::skipped(&entry.path, "", entry.size, SkipReason::NotInArchive))
            .collect();
        progress = apply_files_async(&state.db, job_id, repo.id, files, progress).await?;
    }
    Ok(progress)
}

/// Indexes a commit whose tree GitHub truncated: the file list comes from the tarball. Returns the
/// progress and the indexed paths that are no longer in the commit.
async fn index_whole_archive(
    state: &AppState,
    job_id: i64,
    repo: &RepoRow,
    token: &str,
    head: &str,
    existing: HashMap<String, String>,
    force: bool,
) -> ApiResult<(Progress, Vec<String>)> {
    let selection = WholeSelection {
        existing,
        force,
        max_bytes: state.cfg.max_file_bytes,
    };
    let (progress, WholeSelection { existing, .. }) = index_archive(
        state,
        job_id,
        repo,
        token,
        head,
        selection,
        Progress::default(),
    )
    .await?;
    let mut gone: Vec<String> = existing.into_keys().collect();
    gone.sort();
    Ok((progress, gone))
}

/// Downloads the commit tarball to a temporary file (removed on every path) and runs the archive
/// pipeline over it. Returns the progress and the selection with what it did not consume.
async fn index_archive<S: Selection + 'static>(
    state: &AppState,
    job_id: i64,
    repo: &RepoRow,
    token: &str,
    head: &str,
    selection: S,
    progress: Progress,
) -> ApiResult<(Progress, S)> {
    let tmp_dir = state.cfg.data_dir.join("tmp");
    tokio::fs::create_dir_all(&tmp_dir)
        .await
        .map_err(ApiError::internal)?;
    let tmp = TempFile(tmp_dir.join(format!("job-{job_id}.tar.gz")));
    api::tarball(
        &state.github,
        token,
        &repo.full_name,
        head,
        &tmp.0,
        state.cfg.max_archive_bytes,
    )
    .await?;

    let pipeline = ArchivePipeline {
        db: state.db.clone(),
        job_id,
        repo_id: repo.id,
        max_bytes: state.cfg.max_file_bytes,
    };
    let path = tmp.0.clone();
    let result = tokio::task::spawn_blocking(move || pipeline.run(&path, selection, progress))
        .await
        .map_err(ApiError::internal)?;
    drop(tmp);
    result
}

/// One regular file of the archive chosen for indexing.
struct ArchiveEntry {
    path: String,
    blob_sha: String,
    bytes: Vec<u8>,
}

/// Which archive entries an archive pass indexes. Runs on the reader thread.
trait Selection: Send {
    /// Whether every selected entry adds to the job's file total (not known before reading).
    const COUNTS_TOTAL: bool;

    /// Decides on one regular file; `read` returns its bytes. `None` skips the entry.
    fn select(
        &mut self,
        path: String,
        size: u64,
        read: impl FnOnce() -> ApiResult<Vec<u8>>,
    ) -> ApiResult<Option<ArchiveEntry>>;
}

/// Exactly these tree entries, by path; what is left afterwards was missing from the archive.
struct FetchSelection(HashMap<String, TreeEntry>);

impl Selection for FetchSelection {
    const COUNTS_TOTAL: bool = false;

    fn select(
        &mut self,
        path: String,
        _size: u64,
        read: impl FnOnce() -> ApiResult<Vec<u8>>,
    ) -> ApiResult<Option<ArchiveEntry>> {
        let Some(tree_entry) = self.0.remove(&path) else {
            return Ok(None);
        };
        Ok(Some(ArchiveEntry {
            bytes: read()?,
            path,
            blob_sha: tree_entry.blob_sha,
        }))
    }
}

/// Every eligible entry whose blob SHA differs from `existing` (all with `force`); what is left of
/// `existing` afterwards is no longer in the commit.
struct WholeSelection {
    existing: HashMap<String, String>,
    force: bool,
    max_bytes: u64,
}

impl Selection for WholeSelection {
    const COUNTS_TOTAL: bool = true;

    fn select(
        &mut self,
        path: String,
        size: u64,
        read: impl FnOnce() -> ApiResult<Vec<u8>>,
    ) -> ApiResult<Option<ArchiveEntry>> {
        if !select::path_allowed(&path) || size > self.max_bytes {
            return Ok(None);
        }
        let bytes = read()?;
        let blob_sha = git_blob_sha1(&bytes);
        let previous = self.existing.remove(&path);
        if !self.force && previous.as_deref() == Some(blob_sha.as_str()) {
            return Ok(None);
        }
        Ok(Some(ArchiveEntry {
            path,
            blob_sha,
            bytes,
        }))
    }
}

/// Reader thread → bounded channel → `prepare_file` threads → this thread, which writes batches.
/// A failure in any stage sets `stop`; dropped channel ends release blocked stages, and every
/// thread is joined before [`ArchivePipeline::run`] returns.
struct ArchivePipeline {
    db: Db,
    job_id: i64,
    repo_id: i64,
    max_bytes: u64,
}

impl ArchivePipeline {
    fn run<S: Selection>(
        &self,
        archive: &Path,
        selection: S,
        progress: Progress,
    ) -> ApiResult<(Progress, S)> {
        let stop = AtomicBool::new(false);
        let total = AtomicU32::new(progress.total);
        let workers = thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(MAX_ARCHIVE_WORKERS);
        thread::scope(|scope| {
            let (entry_tx, entry_rx) = mpsc::sync_channel::<ArchiveEntry>(ARCHIVE_ENTRY_BUFFER);
            let (file_tx, file_rx) = mpsc::sync_channel::<PreparedFile>(BATCH_SIZE);
            let entry_rx = Arc::new(Mutex::new(entry_rx));
            let (stop, total) = (&stop, &total);
            for _ in 0..workers {
                let (entries, files) = (Arc::clone(&entry_rx), file_tx.clone());
                thread::Builder::new()
                    .name("sl-archive-prepare".into())
                    .spawn_scoped(scope, move || {
                        let _alive = StageThread::start();
                        prepare_entries(&entries, &files, stop);
                    })
                    .map_err(ApiError::internal)?;
            }
            drop((entry_rx, file_tx));
            let reader = thread::Builder::new()
                .name("sl-archive-read".into())
                .spawn_scoped(scope, move || {
                    let _alive = StageThread::start();
                    let read = self.read_entries(archive, selection, &entry_tx, total, stop);
                    if read.is_err() {
                        stop.store(true, Ordering::SeqCst);
                    }
                    read
                })
                .map_err(ApiError::internal)?;

            let written = self.write_files(file_rx, progress, total, stop);
            if written.is_err() {
                stop.store(true, Ordering::SeqCst);
            }
            let read = reader
                .join()
                .unwrap_or_else(|_| Err(ApiError::internal("archive reader panicked")));
            let progress = written?;
            Ok((progress, read?))
        })
    }

    /// Reader stage: decompresses the archive and sends the selected entries.
    fn read_entries<S: Selection>(
        &self,
        archive: &Path,
        mut selection: S,
        entries: &SyncSender<ArchiveEntry>,
        total: &AtomicU32,
        stop: &AtomicBool,
    ) -> ApiResult<S> {
        let file = std::fs::File::open(archive).map_err(ApiError::internal)?;
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(BufReader::new(file)));
        for entry in archive.entries().map_err(archive_error)? {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let mut entry = entry.map_err(archive_error)?;
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let Some(path) = entry.path().ok().and_then(|path| repository_path(&path)) else {
                continue;
            };
            let size = entry.size();
            let selected =
                selection.select(path, size, || read_entry(&mut entry, self.max_bytes))?;
            let Some(selected) = selected else {
                continue;
            };
            if S::COUNTS_TOTAL {
                total.fetch_add(1, Ordering::SeqCst);
            }
            if entries.send(selected).is_err() {
                // Every prepare thread stopped; the writer reports why.
                break;
            }
        }
        Ok(selection)
    }

    /// Writer stage: applies prepared files in batches of [`BATCH_SIZE`] until every prepare
    /// thread finished. Returning drops `files`, which releases prepare threads blocked on it.
    fn write_files(
        &self,
        files: Receiver<PreparedFile>,
        mut progress: Progress,
        total: &AtomicU32,
        stop: &AtomicBool,
    ) -> ApiResult<Progress> {
        let mut batch: Vec<PreparedFile> = Vec::with_capacity(BATCH_SIZE);
        for file in files.iter() {
            if stop.load(Ordering::SeqCst) {
                return Ok(progress);
            }
            batch.push(file);
            if batch.len() >= BATCH_SIZE {
                progress = self.flush(&mut batch, progress, total)?;
            }
        }
        if stop.load(Ordering::SeqCst) {
            return Ok(progress);
        }
        self.flush(&mut batch, progress, total)
    }

    fn flush(
        &self,
        batch: &mut Vec<PreparedFile>,
        mut progress: Progress,
        total: &AtomicU32,
    ) -> ApiResult<Progress> {
        if batch.is_empty() {
            return Ok(progress);
        }
        progress.total = total.load(Ordering::SeqCst);
        let (job_id, repo_id) = (self.job_id, self.repo_id);
        progress = self
            .db
            .write_blocking(|conn| apply_batch(conn, job_id, repo_id, batch, progress))?;
        batch.clear();
        Ok(progress)
    }
}

/// Prepare stage: runs `prepare_file` on entries until the reader is done or a stage failed.
fn prepare_entries(
    entries: &Mutex<Receiver<ArchiveEntry>>,
    files: &SyncSender<PreparedFile>,
    stop: &AtomicBool,
) {
    loop {
        let entry = {
            let Ok(entries) = entries.lock() else {
                return;
            };
            match entries.recv() {
                Ok(entry) => entry,
                Err(_) => return,
            }
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let file = store::prepare_file(&entry.path, &entry.blob_sha, &entry.bytes);
        if files.send(file).is_err() {
            return;
        }
    }
}

/// Counts a running archive pipeline thread for [`archive_threads_running`].
struct StageThread;

impl StageThread {
    fn start() -> Self {
        ARCHIVE_THREADS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for StageThread {
    fn drop(&mut self) {
        ARCHIVE_THREADS.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn apply_files_async(
    db: &Db,
    job_id: i64,
    repo_id: i64,
    files: Vec<PreparedFile>,
    progress: Progress,
) -> ApiResult<Progress> {
    db.write(move |conn| apply_batch(conn, job_id, repo_id, &files, progress))
        .await
}

/// Applies one batch under the writer lock unless the repository left the `connected` state.
fn apply_batch(
    conn: &mut Connection,
    job_id: i64,
    repo_id: i64,
    files: &[PreparedFile],
    mut progress: Progress,
) -> ApiResult<Progress> {
    require_connected(conn, repo_id)?;
    let stats = store::apply_files(conn, repo_id, files)?;
    progress.done += saturating_u32(files.len());
    progress.changed += stats.indexed;
    progress.skipped += stats.skipped;
    write_progress(conn, job_id, progress)?;
    Ok(progress)
}

fn write_progress(conn: &Connection, job_id: i64, progress: Progress) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE jobs
         SET files_total = ?2, files_done = ?3, files_changed = ?4, files_skipped = ?5
         WHERE id = ?1",
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

async fn store_progress(db: &Db, job_id: i64, progress: Progress) -> ApiResult<()> {
    db.write(move |conn| write_progress(conn, job_id, progress))
        .await?;
    Ok(())
}

fn require_connected(conn: &Connection, repo_id: i64) -> ApiResult<()> {
    if repositories::is_connected(conn, repo_id)? {
        Ok(())
    } else {
        Err(disconnected())
    }
}

async fn ensure_connected(db: &Db, repo_id: i64) -> ApiResult<()> {
    db.read(move |conn| require_connected(conn, repo_id)).await
}

/// Installation token. A 404 means the installation moved, so it is looked up again once; 404 or
/// 403 for that lookup means the app lost access and the repository becomes `disconnected`. A 401
/// from these app-JWT calls means GitHub rejected the app itself: the job fails and the
/// repository state stays as it is.
async fn access_token(state: &AppState, repo: &mut RepoRow) -> ApiResult<String> {
    match state.github.installation_token(repo.installation_id).await {
        Ok(token) => return Ok(token),
        Err(err) if err.github == Some(GitHubFailure::Status(404)) => {}
        Err(err) => return Err(app_auth_error(err)),
    }
    state.github.forget_installation_token(repo.installation_id);
    let retried = async {
        let installation_id = state.github.repo_installation(&repo.full_name).await?;
        let repo_id = repo.id;
        state
            .db
            .write(move |conn| repositories::set_installation(conn, repo_id, installation_id))
            .await?;
        repo.installation_id = installation_id;
        state.github.installation_token(installation_id).await
    }
    .await;
    match retried {
        Ok(token) => Ok(token),
        Err(err) if matches!(err.github, Some(GitHubFailure::Status(403 | 404))) => {
            let repo_id = repo.id;
            state
                .db
                .write(move |conn| {
                    repositories::disconnect(conn, Target::Id(repo_id), NO_ACCESS_REASON)
                })
                .await?;
            Err(ApiError::forbidden(
                "github_no_access",
                format!("The GitHub App has no access to {}.", repo.full_name),
            ))
        }
        Err(err) => Err(app_auth_error(err)),
    }
}

/// Replaces a 401 from an app-JWT call with `github_app_auth_failed`; other errors pass through.
fn app_auth_error(err: ApiError) -> ApiError {
    if err.github == Some(GitHubFailure::Status(401)) {
        ApiError::bad_gateway(
            "github_app_auth_failed",
            "GitHub rejected the app's credentials. Check the private key and the server clock.",
        )
        .with_github(err.github)
    } else {
        err
    }
}

/// Drops a cached installation token GitHub no longer accepts, so the next job requests a new one.
fn forget_rejected_token(state: &AppState, repo: &RepoRow, err: ApiError) -> ApiError {
    if err.github == Some(GitHubFailure::Status(401)) {
        state.github.forget_installation_token(repo.installation_id);
    }
    err
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
    ApiError::bad_gateway(
        "archive_unreadable",
        format!("Reading the repository archive failed: {err}"),
    )
}

fn disconnected() -> ApiError {
    ApiError::conflict("repository_disconnected", "Repository was disconnected.")
}

fn saturating_u32(n: usize) -> u32 {
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
