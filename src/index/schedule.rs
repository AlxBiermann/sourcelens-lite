//! Automatic sync of every connected repository on a cron schedule (UTC).

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};

use crate::db::{get_setting, put_setting};
use crate::error::{ApiError, ApiResult};
use crate::index::jobs;
use crate::model::Trigger;
use crate::state::{now, AppState};

const SETTINGS_KEY: &str = "auto_sync";
const DEFAULT_CRON: &str = "0 * * * *";
/// Pause before the scheduler retries after its settings could not be read.
const RETRY_PAUSE: Duration = Duration::from_secs(60);
/// Longest single sleep, so a changed system clock is noticed within this many seconds.
const MAX_SLEEP_SECS: i64 = 300;
/// Error for an expression with five fields that croner rejects.
const INVALID_CRON: &str = "This is not a valid cron expression. Allowed values: minute 0–59, \
     hour 0–23, day of month 1–31, month 1–12, day of week 0–6.";

/// `settings['auto_sync']`, also the body of `PUT /api/settings/sync`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoSync {
    pub enabled: bool,
    pub cron: String,
}

impl Default for AutoSync {
    fn default() -> Self {
        Self {
            enabled: false,
            cron: DEFAULT_CRON.to_string(),
        }
    }
}

/// Parses a five-field cron expression (minute hour day-of-month month day-of-week).
pub fn parse(cron: &str) -> Result<Cron, String> {
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return Err("Use five fields: minute hour day-of-month month day-of-week.".to_string());
    }
    Cron::from_str(&fields.join(" ")).map_err(|_| INVALID_CRON.to_string())
}

/// First run strictly after `after` (unix seconds, UTC); `None` when the schedule never runs.
pub fn next_run(cron: &Cron, after: i64) -> Option<i64> {
    let start = DateTime::<Utc>::from_timestamp(after, 0)?;
    cron.find_next_occurrence(&start, false)
        .ok()
        .map(|next| next.timestamp())
}

/// Current settings (defaults when never saved).
pub async fn settings(state: &AppState) -> ApiResult<AutoSync> {
    let json = state
        .db
        .read(|conn| get_setting(conn, SETTINGS_KEY))
        .await?;
    match json {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(ApiError::internal),
        None => Ok(AutoSync::default()),
    }
}

/// Validates and stores new settings with the expression normalized, and wakes the scheduler.
pub async fn apply(state: &AppState, input: AutoSync) -> ApiResult<AutoSync> {
    let cron = input.cron.split_whitespace().collect::<Vec<_>>().join(" ");
    let parsed =
        parse(&cron).map_err(|message| ApiError::unprocessable("invalid_cron", message))?;
    if next_run(&parsed, now()).is_none() {
        return Err(ApiError::unprocessable(
            "invalid_cron",
            "This schedule never runs.",
        ));
    }
    let settings = AutoSync {
        enabled: input.enabled,
        cron,
    };
    let json = serde_json::to_string(&settings).map_err(ApiError::internal)?;
    state
        .db
        .write(move |conn| put_setting(conn, SETTINGS_KEY, json))
        .await?;
    state.schedule_notify.notify_one();
    Ok(settings)
}

/// Starts the scheduler task. Runs missed while the process was down are not caught up; the
/// startup jobs cover them.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        loop {
            let settings = match settings(&state).await {
                Ok(settings) => settings,
                Err(err) => {
                    tracing::error!("loading the sync schedule failed: {err}");
                    tokio::time::sleep(RETRY_PAUSE).await;
                    continue;
                }
            };
            let at = if settings.enabled {
                parse(&settings.cron)
                    .ok()
                    .and_then(|cron| next_run(&cron, now()))
            } else {
                None
            };
            let Some(at) = at else {
                state.schedule_notify.notified().await;
                continue;
            };
            if wait_until(&state, at).await {
                match jobs::enqueue_all(&state, Trigger::Schedule).await {
                    Ok(queued) => tracing::info!("scheduled sync queued {queued} jobs"),
                    Err(err) => tracing::error!("scheduled sync failed: {err}"),
                }
            }
        }
    });
}

/// Sleeps until `at`; `false` when the settings changed first.
async fn wait_until(state: &AppState, at: i64) -> bool {
    loop {
        let left = at - now();
        if left <= 0 {
            return true;
        }
        let pause = Duration::from_secs(left.min(MAX_SLEEP_SECS).unsigned_abs());
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            () = state.schedule_notify.notified() => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{next_run, parse};

    #[test]
    fn next_run_follows_the_expression() {
        let cron = parse("*/15 * * * *").unwrap();
        assert_eq!(next_run(&cron, 1_794_355_200), Some(1_794_356_100));
    }

    #[test]
    fn parse_requires_five_fields() {
        assert!(parse("0 0 * * * *").is_err());
        assert!(parse(" 0  2 * * * ").is_ok());
    }

    #[test]
    fn parse_reports_allowed_values() {
        assert_eq!(
            parse("61 * * * *").unwrap_err(),
            "This is not a valid cron expression. Allowed values: minute 0–59, hour 0–23, \
             day of month 1–31, month 1–12, day of week 0–6."
        );
    }
}
