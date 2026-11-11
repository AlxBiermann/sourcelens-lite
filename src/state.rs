use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::crypto::{random_hex, SecretKey};
use crate::db::Db;
use crate::github::app::GitHub;
use crate::rate::RateLimiter;

pub const SECS_PER_DAY: i64 = 86_400;

/// Bytes of randomness in the setup token printed at startup.
const SETUP_TOKEN_BYTES: usize = 16;

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub key: Arc<SecretKey>,
    pub http: reqwest::Client,
    pub github: Arc<GitHub>,
    pub job_notify: Arc<tokio::sync::Notify>,
    pub setup_token: Arc<String>,
    /// Last `api_keys.last_used_at` write per key id; owned by `mcp`.
    pub key_touch: Arc<Mutex<HashMap<i64, Instant>>>,
    /// Pending manifest-flow `state`; owned by `http::setup`.
    pub manifest_state: Arc<Mutex<Option<String>>>,
    /// One lock per session hash while its GitHub token is refreshed; owned by `http::session`.
    pub refresh_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Request budget per API key on `/mcp`.
    pub mcp_rate: Arc<RateLimiter>,
    /// Request budget per user on `POST /api/search`.
    pub search_rate: Arc<RateLimiter>,
}

impl AppState {
    /// Creates the data directories (mode 0700, `tmp/` emptied), opens the database and loads the
    /// secret key. Does not start workers.
    pub fn new(cfg: Config) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        prepare_data_dir(&cfg.data_dir)?;
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
            setup_token: Arc::new(random_hex(SETUP_TOKEN_BYTES)),
            key_touch: Arc::new(Mutex::new(HashMap::new())),
            manifest_state: Arc::new(Mutex::new(None)),
            refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            mcp_rate: Arc::new(RateLimiter::default()),
            search_rate: Arc::new(RateLimiter::default()),
        })
    }
}

/// The data directory holds private source code and the secret key, so only the server's user may
/// enter it. `tmp/` only holds archives of index jobs, and no job runs before startup finishes, so
/// whatever a killed process left there is removed.
fn prepare_data_dir(data_dir: &Path) -> io::Result<()> {
    create_private_dir(data_dir)?;
    let tmp = data_dir.join("tmp");
    match std::fs::remove_dir_all(&tmp) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    create_private_dir(&tmp)
}

fn create_private_dir(dir: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        builder.mode(0o700);
        builder.create(dir)?;
        // An existing directory keeps its mode on create; tighten it.
        if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            tracing::warn!("cannot restrict {} to mode 0700: {err}", dir.display());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    builder.create(dir)
}

/// Current time in unix seconds.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Locks `mutex`, recovering the data when another thread panicked while holding it.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
