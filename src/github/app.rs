use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::crypto::SecretKey;
use crate::db::Db;
use crate::error::ApiResult;

#[derive(Clone)]
pub struct AppCredentials {
    pub app_id: u64,
    pub slug: String,
    pub client_id: String,
    pub client_secret: String,
    pub webhook_secret: Option<String>,
    pub private_key_pem: String,
    pub html_url: String,
    /// "manifest" | "env"
    pub source: &'static str,
}

#[allow(dead_code)]
pub struct GitHub {
    http: reqwest::Client,
    db: Db,
    key: Arc<SecretKey>,
    env: Option<AppCredentials>,
    api_base: String,
    token_cache: Mutex<HashMap<u64, (String, Instant)>>,
}

impl GitHub {
    pub fn new(
        http: reqwest::Client,
        db: Db,
        key: Arc<SecretKey>,
        env: Option<AppCredentials>,
        api_base: String,
    ) -> Self {
        Self {
            http,
            db,
            key,
            env,
            api_base,
            token_cache: Mutex::new(HashMap::new()),
        }
    }

    /// GitHub REST API base URL without trailing slash.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    pub async fn credentials(&self) -> ApiResult<Option<AppCredentials>> {
        todo!("github-auth")
    }

    pub async fn store_manifest_credentials(&self, _c: &AppCredentials) -> ApiResult<()> {
        todo!("github-auth")
    }

    pub fn app_jwt(_c: &AppCredentials) -> ApiResult<String> {
        todo!("github-auth")
    }

    pub async fn installation_token(&self, _installation_id: u64) -> ApiResult<String> {
        todo!("github-auth")
    }

    pub fn forget_installation_token(&self, _installation_id: u64) {
        todo!("github-auth")
    }

    pub async fn repo_installation(&self, _full_name: &str) -> ApiResult<u64> {
        todo!("github-auth")
    }

    pub async fn send(
        &self,
        _build: impl Fn() -> reqwest::RequestBuilder,
    ) -> ApiResult<reqwest::Response> {
        todo!("github-auth")
    }
}
