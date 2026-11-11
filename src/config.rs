use std::env;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::Url;

use crate::github::app::AppCredentials;

const DEFAULT_BIND: &str = "0.0.0.0:3000";
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
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        dotenvy::dotenv().ok();

        let public_url = var("SOURCELENS_PUBLIC_URL").ok_or("SOURCELENS_PUBLIC_URL is required")?;
        let public_url = normalize_public_url(&public_url)?;
        let mut cfg = Self::new(&public_url, Path::new(DEFAULT_DATA_DIR));

        if let Some(bind) = var("SOURCELENS_BIND") {
            cfg.bind = bind;
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
        let Ok(url) = Url::parse(&self.public_url) else {
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
