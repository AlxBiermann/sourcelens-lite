use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::crypto::{random_hex, SecretKey};
use crate::db::Db;
use crate::github::app::GitHub;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub key: Arc<SecretKey>,
    pub http: reqwest::Client,
    pub github: Arc<GitHub>,
    pub job_notify: Arc<tokio::sync::Notify>,
    pub setup_token: Arc<String>,
    /// Last `api_keys.last_used_at` write per key id.
    pub key_touch: Arc<Mutex<HashMap<i64, Instant>>>,
    /// Pending manifest-flow `state`.
    pub manifest_state: Arc<Mutex<Option<String>>>,
}

impl AppState {
    /// Creates the data directories, opens the database and loads the secret key. Does not start workers.
    pub fn new(cfg: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        std::fs::create_dir_all(cfg.data_dir.join("tmp"))?;
        let db = Db::open(&cfg.data_dir.join("sourcelens.db"))?;
        let key = Arc::new(SecretKey::load(&cfg)?);
        let http = reqwest::Client::builder()
            .user_agent("sourcelens")
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        let github = Arc::new(GitHub::new(
            http.clone(),
            db.clone(),
            key.clone(),
            cfg.github.clone(),
            cfg.github_api_url.clone(),
        ));
        Ok(Self {
            cfg: Arc::new(cfg),
            db,
            key,
            http,
            github,
            job_notify: Arc::new(tokio::sync::Notify::new()),
            setup_token: Arc::new(random_hex(16)),
            key_touch: Arc::new(Mutex::new(HashMap::new())),
            manifest_state: Arc::new(Mutex::new(None)),
        })
    }
}

/// Current time in unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
