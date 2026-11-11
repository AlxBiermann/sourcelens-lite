use crate::error::ApiResult;
use crate::state::AppState;

pub async fn enqueue(
    _state: &AppState,
    _repo_id: i64,
    _trigger: &str,
    _target_commit: Option<&str>,
    _force: bool,
) -> ApiResult<i64> {
    todo!("repos-jobs")
}

pub async fn recover_and_reconcile(_state: &AppState) -> ApiResult<()> {
    todo!("repos-jobs")
}

pub fn spawn_workers(_state: AppState) {
    todo!("repos-jobs")
}
