use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{ApiError, ApiResult};
use crate::index::pipeline::{self, ClaimedJob, JobOutcome};
use crate::index::store;
use crate::model::{JobStatus, RepoState, Trigger};
use crate::repositories::{self, CONNECTED_IDS};
use crate::state::{now, AppState};

/// How long an idle worker waits for a notification before looking for jobs again.
const IDLE_POLL: Duration = Duration::from_secs(5);
/// Finished jobs kept per repository.
const KEEP_FINISHED_JOBS: i64 = 50;
/// A purge waits at most this long for a running job of the repository to stop.
const PURGE_WAIT: Duration = Duration::from_secs(900);
const PURGE_POLL: Duration = Duration::from_millis(250);
/// Retries of a failed write of a job's result; the pause doubles from `FINISH_FIRST_PAUSE`.
const FINISH_RETRIES: u32 = 5;
const FINISH_FIRST_PAUSE: Duration = Duration::from_secs(1);
/// Error of a job whose real result could not be written.
const RESULT_NOT_SAVED: &str = "The job result could not be saved.";

/// Queues a job; a repository has at most one queued job, a newer request replaces its target.
pub async fn enqueue(
    state: &AppState,
    repo_id: i64,
    trigger: Trigger,
    target_commit: Option<&str>,
    force: bool,
) -> ApiResult<i64> {
    let target_commit = target_commit.map(str::to_string);
    let id = state
        .db
        .write(move |conn| {
            conn.query_row(
                "INSERT INTO jobs (repo_id, trigger, target_commit, force, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (repo_id) WHERE status = 'queued' DO UPDATE SET
                   trigger = excluded.trigger,
                   target_commit = excluded.target_commit,
                   force = max(force, excluded.force),
                   created_at = excluded.created_at
                 RETURNING id",
                params![repo_id, trigger, target_commit, force, now()],
                |row| row.get(0),
            )
        })
        .await?;
    state.job_notify.notify_waiters();
    Ok(id)
}

/// Startup: fails interrupted jobs, queues a `startup` job per connected repository and starts
/// pending deletions in the background. A failure for one repository does not stop the others.
pub async fn recover_and_reconcile(state: &AppState) -> ApiResult<()> {
    let finished_at = now();
    let (deleting, connected) = state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET status = ?2, error = 'Interrupted by a restart.', finished_at = ?1
                 WHERE status = ?3",
                params![finished_at, JobStatus::Failed, JobStatus::Running],
            )?;
            let deleting = repositories::ids_in_state(conn, RepoState::Deleting)?;
            let connected = repositories::connected_ids(conn)?;
            Ok::<_, rusqlite::Error>((deleting, connected))
        })
        .await?;
    for repo_id in connected {
        if let Err(err) = enqueue(state, repo_id, Trigger::Startup, None, false).await {
            tracing::error!(repo_id, "queueing the startup job failed: {err}");
        }
    }
    for repo_id in deleting {
        let task_state = state.clone();
        tokio::spawn(async move {
            if let Err(err) = purge_repository(&task_state, repo_id).await {
                tracing::error!(repo_id, "deleting repository index failed: {err}");
            }
        });
    }
    Ok(())
}

/// Starts `cfg.index_concurrency` workers that run queued jobs until the process exits.
pub fn spawn_workers(state: AppState) {
    for worker in 0..state.cfg.index_concurrency {
        let state = state.clone();
        tokio::spawn(async move {
            // A job whose result could not be written stays `running` and blocks its repository
            // until this worker manages to mark it failed.
            let mut unfinished: Option<i64> = None;
            loop {
                if let Some(job_id) = unfinished {
                    if let Err(err) = mark_failed(&state, job_id).await {
                        tracing::error!(worker, job_id, "recording the job failure failed: {err}");
                        tokio::time::sleep(IDLE_POLL).await;
                        continue;
                    }
                    unfinished = None;
                }
                let mut notified = std::pin::pin!(state.job_notify.notified());
                notified.as_mut().enable();
                match step(&state).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = tokio::time::timeout(IDLE_POLL, notified).await;
                    }
                    Err(err) => {
                        tracing::error!(worker, "index worker failed: {}", err.error);
                        unfinished = err.unfinished_job;
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                }
            }
        });
    }
}

/// Claims one queued job, runs it and records the result. `false` when nothing was claimable.
pub async fn run_next(state: &AppState) -> ApiResult<bool> {
    step(state).await.map_err(|err| err.error)
}

struct StepError {
    error: ApiError,
    /// The claimed job whose result could not be written.
    unfinished_job: Option<i64>,
}

impl From<ApiError> for StepError {
    fn from(error: ApiError) -> Self {
        Self {
            error,
            unfinished_job: None,
        }
    }
}

async fn step(state: &AppState) -> Result<bool, StepError> {
    let Some(job) = claim(state).await? else {
        return Ok(false);
    };
    let (job_id, repo_id, trigger) = (job.id, job.repo_id, job.trigger);
    let task_state = state.clone();
    // A separate task so a panic inside the pipeline fails the job instead of killing the worker.
    let result = match tokio::spawn(async move { pipeline::run_job(&task_state, job).await }).await
    {
        Ok(result) => result,
        Err(err) => Err(ApiError::internal(format!(
            "index job {job_id} aborted: {err}"
        ))),
    };
    if let Err(err) = &result {
        tracing::warn!(job_id, repo_id, "index job failed: {}", err.message);
    }
    finish(state, job_id, repo_id, trigger, result)
        .await
        .map_err(|error| StepError {
            error,
            unfinished_job: Some(job_id),
        })?;
    Ok(true)
}

/// Waits until no job of the repository is running, then deletes its index and row, one batch
/// per writer turn so other writes can run in between.
pub async fn purge_repository(state: &AppState, repo_id: i64) -> ApiResult<()> {
    let started = tokio::time::Instant::now();
    loop {
        let running: bool = state
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM jobs WHERE repo_id = ?1 AND status = ?2)",
                    params![repo_id, JobStatus::Running],
                    |row| row.get(0),
                )
            })
            .await?;
        if !running || started.elapsed() >= PURGE_WAIT {
            break;
        }
        tokio::time::sleep(PURGE_POLL).await;
    }
    loop {
        let done = state
            .db
            .write(move |conn| store::purge_batch(conn, repo_id))
            .await?;
        if done {
            return Ok(());
        }
    }
}

async fn claim(state: &AppState) -> ApiResult<Option<ClaimedJob>> {
    let started_at = now();
    let job = state
        .db
        .write(move |conn| {
            conn.query_row(
                &format!(
                    "UPDATE jobs SET status = 'running', started_at = ?1, phase = 'resolve'
                     WHERE id = (
                       SELECT j.id FROM jobs j
                       WHERE j.status = 'queued' AND j.repo_id IN ({CONNECTED_IDS})
                         AND NOT EXISTS (
                           SELECT 1 FROM jobs x WHERE x.repo_id = j.repo_id AND x.status = 'running'
                         )
                       ORDER BY j.id
                       LIMIT 1
                     )
                     RETURNING id, repo_id, trigger, target_commit, force"
                ),
                [started_at],
                |row| {
                    Ok(ClaimedJob {
                        id: row.get(0)?,
                        repo_id: row.get(1)?,
                        trigger: row.get(2)?,
                        target_commit: row.get(3)?,
                        force: row.get(4)?,
                    })
                },
            )
            .optional()
        })
        .await?;
    Ok(job)
}

/// Records the result of a job, retrying the write with growing pauses (a full disk can free up).
async fn finish(
    state: &AppState,
    job_id: i64,
    repo_id: i64,
    trigger: Trigger,
    result: ApiResult<JobOutcome>,
) -> ApiResult<()> {
    let discard = trigger == Trigger::Startup
        && matches!(&result, Ok(o) if o.files_changed == 0 && o.files_deleted == 0 && o.files_skipped == 0 && o.embedded == 0);
    let (status, error) = match result {
        Ok(_) => (JobStatus::Succeeded, None),
        Err(err) => (JobStatus::Failed, Some(err.message)),
    };
    let finished_at = now();
    let mut pause = FINISH_FIRST_PAUSE;
    let mut retries = 0;
    loop {
        let error = error.clone();
        let written = state
            .db
            .write(move |conn| {
                record_result(conn, job_id, repo_id, discard, status, error, finished_at)
            })
            .await;
        match written {
            Ok(()) => break,
            Err(err) if retries < FINISH_RETRIES => {
                tracing::warn!(job_id, "saving the job result failed, retrying: {err}");
                tokio::time::sleep(pause).await;
                pause *= 2;
                retries += 1;
            }
            Err(err) => return Err(err.into()),
        }
    }
    // A queued job of the same repository may have waited for this one.
    state.job_notify.notify_waiters();
    Ok(())
}

fn record_result(
    conn: &Connection,
    job_id: i64,
    repo_id: i64,
    discard: bool,
    status: JobStatus,
    error: Option<String>,
    finished_at: i64,
) -> rusqlite::Result<()> {
    if discard {
        conn.execute("DELETE FROM jobs WHERE id = ?1", [job_id])?;
    } else {
        conn.execute(
            "UPDATE jobs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
            params![job_id, status, error, finished_at],
        )?;
    }
    conn.execute(
        "DELETE FROM jobs
         WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
           AND id NOT IN (
             SELECT id FROM jobs
             WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
             ORDER BY id DESC
             LIMIT ?2
           )",
        params![repo_id, KEEP_FINISHED_JOBS],
    )?;
    Ok(())
}

/// Fallback after `finish` gave up: the smallest write that frees the repository for new jobs.
async fn mark_failed(state: &AppState, job_id: i64) -> ApiResult<()> {
    let finished_at = now();
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET status = ?2, finished_at = ?3, error = COALESCE(error, ?4)
                 WHERE id = ?1 AND status = ?5",
                params![
                    job_id,
                    JobStatus::Failed,
                    finished_at,
                    RESULT_NOT_SAVED,
                    JobStatus::Running
                ],
            )
        })
        .await?;
    state.job_notify.notify_waiters();
    Ok(())
}
