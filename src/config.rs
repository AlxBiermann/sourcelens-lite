use std::env;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::Url;

use crate::github::app::AppCredentials;

const DEFAULT_BIND: &str = "0.0.0.0:3000";
/// Default listen address in dev mode: this computer only.
const DEV_DEFAULT_BIND: &str = "127.0.0.1:3000";
const DEFAULT_DATA_DIR: &str = "./data";
const DEFAULT_INDEX_CONCURRENCY: usize = 2;
const DEFAULT_MAX_FILE_BYTES: u64 = 1_048_576;
const DEFAULT_MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

const GITHUB_VARS: [&str; 6] = [
    "SOURCELENS_GITHUB_APP_ID",
    "SOURCELENS_GITHUB_APP_SLUG",
    "SOURCELENS_GITHUB_CLIENT_ID",
    "SOURCELENS_GITHUB_CLIENT_SECRET",
    "SOURCELENS_GITHUB_WEBHOOK_SECRET",
    "SOURCELENS_GITHUB_PRIVATE_KEY_PATH",
];

#[derive(Clone)]
pub struct Config {
    pub public_url: String,
    pub bind: String,
    pub data_dir: PathBuf,
    pub secret_key: Option<[u8; 32]>,
    pub index_concurrency: usize,
    pub max_file_bytes: u64,
    /// Largest repository tarball an index job downloads.
    pub max_archive_bytes: u64,
    pub dev_origin: Option<String>,
    /// GitHub App credentials from the environment (`source = "env"`).
    pub github: Option<AppCredentials>,
    /// GitHub REST API base URL without trailing slash; tests point it at a local fake.
    pub github_api_url: String,
    /// `SOURCELENS_DEV_MODE`: sign-in without GitHub and demo data (see `dev`). Only allowed when
    /// both the public URL and the listen address are on this computer.
    pub dev_mode: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();

        let public_url = var("SOURCELENS_PUBLIC_URL").ok_or("SOURCELENS_PUBLIC_URL is required")?;
        let public_url = normalize_public_url(&public_url)?;
        let mut cfg = Self::new(&public_url, Path::new(DEFAULT_DATA_DIR));
        cfg.dev_mode = var("SOURCELENS_DEV_MODE")
            .is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));

        cfg.bind = match var("SOURCELENS_BIND") {
            Some(bind) => bind,
            None if cfg.dev_mode => DEV_DEFAULT_BIND.to_string(),
            None => DEFAULT_BIND.to_string(),
        };
        if cfg.dev_mode {
            check_dev_mode(&cfg.public_url, &cfg.bind)?;
        }
        if let Some(dir) = var("SOURCELENS_DATA_DIR") {
            cfg.data_dir = PathBuf::from(dir);
        }
        if let Some(key) = var("SOURCELENS_SECRET_KEY") {
            cfg.secret_key = Some(decode_secret_key(&key)?);
        }
        if let Some(n) = var("SOURCELENS_INDEX_CONCURRENCY") {
            cfg.index_concurrency = n
                .parse::<usize>()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or("SOURCELENS_INDEX_CONCURRENCY must be a positive integer")?;
        }
        if let Some(n) = var("SOURCELENS_MAX_FILE_BYTES") {
            cfg.max_file_bytes = positive("SOURCELENS_MAX_FILE_BYTES", &n)?;
        }
        if let Some(n) = var("SOURCELENS_MAX_ARCHIVE_BYTES") {
            cfg.max_archive_bytes = positive("SOURCELENS_MAX_ARCHIVE_BYTES", &n)?;
        }
        if let Some(origin) = var("SOURCELENS_DEV_ORIGIN") {
            cfg.dev_origin = Some(origin.trim_end_matches('/').to_string());
        }
        cfg.github = github_from_env()?;
        Ok(cfg)
    }

    pub fn new(public_url: &str, data_dir: &Path) -> Self {
        Self {
            public_url: public_url.trim_end_matches('/').to_string(),
            bind: DEFAULT_BIND.to_string(),
            data_dir: data_dir.to_path_buf(),
            secret_key: None,
            index_concurrency: DEFAULT_INDEX_CONCURRENCY,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
            dev_origin: None,
            github: None,
            github_api_url: DEFAULT_GITHUB_API_URL.to_string(),
            dev_mode: false,
        }
    }

    /// `scheme://host[:port]` of the public URL.
    pub fn public_origin(&self) -> String {
        match Url::parse(&self.public_url) {
            Ok(url) => url.origin().ascii_serialization(),
            Err(_) => self.public_url.clone(),
        }
    }

    pub fn public_host_is_local(&self) -> bool {
        url_host_is_local(&self.public_url)
    }

    pub fn public_is_https(&self) -> bool {
        self.public_url.starts_with("https://")
    }

    /// Where GitHub delivers webhooks.
    pub fn webhook_url(&self) -> String {
        format!("{}/webhooks/github", self.public_url)
    }

    /// OAuth redirect target after GitHub sign-in.
    pub fn callback_url(&self) -> String {
        format!("{}/auth/github/callback", self.public_url)
    }

    /// OpenID Connect redirect target after SSO sign-in.
    pub fn sso_callback_url(&self) -> String {
        format!("{}/auth/sso/callback", self.public_url)
    }

    /// The MCP endpoint, also the OAuth resource identifier of its access tokens.
    pub fn mcp_url(&self) -> String {
        format!("{}/mcp", self.public_url)
    }
}

/// Dev mode lets anyone who reaches the server sign in as admin, so it needs a public URL and a
/// listen address that only this computer can use.
pub fn check_dev_mode(public_url: &str, bind: &str) -> Result<(), String> {
    if !url_host_is_local(public_url) {
        return Err(format!(
            "SOURCELENS_DEV_MODE needs SOURCELENS_PUBLIC_URL on this computer (localhost, 127.0.0.1 \
             or [::1]), not {public_url}: in dev mode anyone who can open the URL signs in as admin."
        ));
    }
    if !bind_is_loopback(bind) {
        return Err(format!(
            "SOURCELENS_DEV_MODE needs a loopback SOURCELENS_BIND such as 127.0.0.1:3000, not \
             {bind}: in dev mode anyone who can reach the port signs in as admin."
        ));
    }
    Ok(())
}

fn url_host_is_local(url: &str) -> bool {
    let Ok(url) = Url::parse(url) else {
        return false;
    };
    match url.host_str() {
        Some(host) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<Ipv4Addr>()
                    .is_ok_and(|ip| ip == Ipv4Addr::LOCALHOST)
                || host
                    .parse::<Ipv6Addr>()
                    .is_ok_and(|ip| ip == Ipv6Addr::LOCALHOST)
        }
        None => false,
    }
}

/// `ip:port` with a loopback IP, or `localhost:port`.
fn bind_is_loopback(bind: &str) -> bool {
    match bind.parse::<SocketAddr>() {
        Ok(addr) => addr.ip().is_loopback(),
        Err(_) => bind
            .rsplit_once(':')
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost")),
    }
}

fn positive(name: &str, value: &str) -> Result<u64, String> {
    value
        .parse::<u64>()
        .ok()
        .filter(|n| *n >= 1)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn normalize_public_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim_end_matches('/');
    let url = Url::parse(trimmed)
        .map_err(|e| format!("SOURCELENS_PUBLIC_URL is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err("SOURCELENS_PUBLIC_URL must be an http or https URL".into());
    }
    Ok(trimmed.to_string())
}

/// Whether a URL host (as returned by `Url::host_str`) is this computer; plain http is accepted only there.
pub fn is_loopback_host(host: Option<&str>) -> bool {
    matches!(host, Some("localhost" | "127.0.0.1" | "[::1]"))
}

fn decode_secret_key(value: &str) -> Result<[u8; 32], String> {
    STANDARD
        .decode(value)
        .ok()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .ok_or_else(|| "SOURCELENS_SECRET_KEY must be base64 of exactly 32 bytes".to_string())
}

fn github_from_env() -> Result<Option<AppCredentials>, String> {
    let values: Vec<Option<String>> = GITHUB_VARS.iter().map(|name| var(name)).collect();
    let missing: Vec<&str> = GITHUB_VARS
        .iter()
        .zip(&values)
        .filter(|(_, v)| v.is_none())
        .map(|(name, _)| *name)
        .collect();
    if missing.len() == GITHUB_VARS.len() {
        return Ok(None);
    }
    if !missing.is_empty() {
        return Err(format!(
            "Missing GitHub App variables: {}",
            missing.join(", ")
        ));
    }
    let [app_id, slug, client_id, client_secret, webhook_secret, key_path] =
        <[Option<String>; 6]>::try_from(values)
            .map_err(|_| "unexpected GitHub variable count".to_string())?
            .map(Option::unwrap_or_default);

    let app_id = app_id
        .parse::<u64>()
        .map_err(|_| "SOURCELENS_GITHUB_APP_ID must be a number".to_string())?;
    let private_key_pem = std::fs::read_to_string(&key_path)
        .map_err(|e| format!("Cannot read SOURCELENS_GITHUB_PRIVATE_KEY_PATH ({key_path}): {e}"))?;
    let html_url = format!("https://github.com/apps/{slug}");
    Ok(Some(AppCredentials {
        app_id,
        slug,
        client_id,
        client_secret,
        webhook_secret: Some(webhook_secret),
        private_key_pem,
        html_url,
        source: "env",
    }))
}

#[cfg(test)]
mod tests {
    use super::check_dev_mode;

    #[test]
    fn dev_mode_is_only_allowed_on_this_computer() {
        assert!(check_dev_mode("https://code.example.com", "127.0.0.1:3000").is_err());
        assert!(check_dev_mode("http://localhost:3000", "0.0.0.0:3000").is_err());
        assert!(check_dev_mode("http://localhost:3000", "[::]:3000").is_err());
        assert!(check_dev_mode("http://localhost:3000", "127.0.0.1:3000").is_ok());
        assert!(check_dev_mode("http://127.0.0.1:3947", "[::1]:3947").is_ok());
        assert!(check_dev_mode("http://[::1]:3000", "localhost:3000").is_ok());
    }
}
