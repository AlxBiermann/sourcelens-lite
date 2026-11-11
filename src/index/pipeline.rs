use crate::error::ApiError;
use crate::state::AppState;

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

pub async fn run_job(_state: &AppState, _job: ClaimedJob) -> Result<JobOutcome, ApiError> {
    todo!("repos-jobs")
}
