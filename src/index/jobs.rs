use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{ApiError, ApiResult};
use crate::index::pipeline::{self, ClaimedJob, JobOutcome};
use crate::index::store;
use crate::state::{now, AppState};

/// How long an idle worker waits for a notification before looking for jobs again.
const IDLE_POLL: Duration = Duration::from_secs(5);
/// Finished jobs kept per repository.
const KEEP_FINISHED_JOBS: i64 = 50;
/// A purge waits at most this long for a running job of the repository to stop.
const PURGE_WAIT: Duration = Duration::from_secs(900);
const PURGE_POLL: Duration = Duration::from_millis(250);

/// Queues a job; a repository has at most one queued job, a newer request replaces its target.
pub async fn enqueue(
    state: &AppState,
    repo_id: i64,
    trigger: &str,
    target_commit: Option<&str>,
    force: bool,
) -> ApiResult<i64> {
    let trigger = trigger.to_string();
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

/// Startup: fails interrupted jobs, finishes pending deletions and queues a `startup` job per connected repository.
pub async fn recover_and_reconcile(state: &AppState) -> ApiResult<()> {
    let finished_at = now();
    let (deleting, connected) = state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE jobs SET status = 'failed', error = 'Interrupted by a restart.', finished_at = ?1
                 WHERE status = 'running'",
                [finished_at],
            )?;
            let deleting = repository_ids(conn, "deleting")?;
            let connected = repository_ids(conn, "connected")?;
            Ok((deleting, connected))
        })
        .await?;
    for repo_id in deleting {
        purge_repository(state, repo_id).await?;
    }
    for repo_id in connected {
        enqueue(state, repo_id, "startup", None, false).await?;
    }
    Ok(())
}

/// Starts `cfg.index_concurrency` workers that run queued jobs until the process exits.
pub fn spawn_workers(state: AppState) {
    for worker in 0..state.cfg.index_concurrency.max(1) {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                let mut notified = std::pin::pin!(state.job_notify.notified());
                notified.as_mut().enable();
                match run_next(&state).await {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = tokio::time::timeout(IDLE_POLL, notified).await;
                    }
                    Err(err) => {
                        tracing::error!(worker, "index worker failed: {err}");
                        tokio::time::sleep(IDLE_POLL).await;
                    }
                }
            }
        });
    }
}

/// Claims one queued job, runs it and records the result. `false` when nothing was claimable.
pub async fn run_next(state: &AppState) -> ApiResult<bool> {
    let Some(job) = claim(state).await? else {
        return Ok(false);
    };
    let (job_id, repo_id, trigger) = (job.id, job.repo_id, job.trigger.clone());
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
    finish(state, job_id, repo_id, &trigger, result).await?;
    Ok(true)
}

/// Waits until no job of the repository is running, then deletes its index and row.
pub async fn purge_repository(state: &AppState, repo_id: i64) -> ApiResult<()> {
    let started = tokio::time::Instant::now();
    loop {
        let running: bool = state
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM jobs WHERE repo_id = ?1 AND status = 'running')",
                    [repo_id],
                    |row| row.get(0),
                )
            })
            .await?;
        if !running || started.elapsed() >= PURGE_WAIT {
            break;
        }
        tokio::time::sleep(PURGE_POLL).await;
    }
    state
        .db
        .write(move |conn| store::purge_repository(conn, repo_id))
        .await?;
    Ok(())
}

async fn claim(state: &AppState) -> ApiResult<Option<ClaimedJob>> {
    let started_at = now();
    let job = state
        .db
        .write(move |conn| {
            conn.query_row(
                "UPDATE jobs SET status = 'running', started_at = ?1, phase = 'resolve'
                 WHERE id = (SELECT j.id FROM jobs j JOIN repositories r ON r.id = j.repo_id
                             WHERE j.status = 'queued' AND r.state = 'connected'
                               AND NOT EXISTS (SELECT 1 FROM jobs x WHERE x.repo_id = j.repo_id AND x.status = 'running')
                             ORDER BY j.id LIMIT 1)
                 RETURNING id, repo_id, trigger, target_commit, force",
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

async fn finish(
    state: &AppState,
    job_id: i64,
    repo_id: i64,
    trigger: &str,
    result: ApiResult<JobOutcome>,
) -> ApiResult<()> {
    let discard = trigger == "startup"
        && matches!(&result, Ok(o) if o.files_changed == 0 && o.files_deleted == 0 && o.files_skipped == 0 && o.embedded == 0);
    let (status, error) = match result {
        Ok(_) => ("succeeded", None),
        Err(err) => ("failed", Some(err.message)),
    };
    let finished_at = now();
    state
        .db
        .write(move |conn| {
            if discard {
                conn.execute("DELETE FROM jobs WHERE id = ?1", [job_id])?;
            } else {
                conn.execute(
                    "UPDATE jobs SET status = ?2, error = ?3, finished_at = ?4 WHERE id = ?1",
                    params![job_id, status, error, finished_at],
                )?;
            }
            conn.execute(
                "DELETE FROM jobs WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
                   AND id NOT IN (SELECT id FROM jobs WHERE repo_id = ?1 AND status IN ('succeeded', 'failed')
                                  ORDER BY id DESC LIMIT ?2)",
                params![repo_id, KEEP_FINISHED_JOBS],
            )?;
            Ok(())
        })
        .await?;
    // A queued job of the same repository may have waited for this one.
    state.job_notify.notify_waiters();
    Ok(())
}

fn repository_ids(conn: &Connection, repo_state: &str) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM repositories WHERE state = ?1 ORDER BY id")?;
    let ids = stmt
        .query_map([repo_state], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(ids)
}
