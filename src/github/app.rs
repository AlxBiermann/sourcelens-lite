use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use reqwest::header::{ACCEPT, RETRY_AFTER};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::crypto::SecretKey;
use crate::db::{self, Db};
use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::state::{lock, now};

const SETTINGS_KEY: &str = "github_app";
const API_VERSION_HEADER: &str = "x-github-api-version";
const API_VERSION: &str = "2022-11-28";
const ACCEPT_GITHUB_JSON: &str = "application/vnd.github+json";
/// GitHub installation tokens live 60 minutes; reuse them for 50.
const TOKEN_TTL: Duration = Duration::from_secs(50 * 60);
const MAX_ATTEMPTS: u32 = 3;
/// App JWTs are backdated against clock drift and expire before GitHub's 10-minute maximum.
const JWT_BACKDATE_SECS: i64 = 60;
const JWT_LIFETIME_SECS: i64 = 540;
/// Wait reported for a rate limit whose response names no reset time.
const DEFAULT_RATE_LIMIT_WAIT_SECS: i64 = 60;

/// GitHub App credentials. The same record is stored sealed under `settings['github_app']` (camelCase)
/// and read from GitHub's manifest conversion response (snake_case aliases, `id`, `pem`).
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppCredentials {
    #[serde(alias = "id")]
    pub app_id: u64,
    pub slug: String,
    #[serde(alias = "client_id")]
    pub client_id: String,
    #[serde(alias = "client_secret")]
    pub client_secret: String,
    #[serde(alias = "webhook_secret")]
    pub webhook_secret: Option<String>,
    #[serde(alias = "pem")]
    pub private_key_pem: String,
    #[serde(alias = "html_url")]
    pub html_url: String,
    /// "manifest" | "env"; not stored, every stored record comes from the manifest flow.
    #[serde(skip, default = "manifest_source")]
    pub source: &'static str,
}

fn manifest_source() -> &'static str {
    "manifest"
}

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
            api_base: api_base.trim_end_matches('/').to_string(),
            token_cache: Mutex::new(HashMap::new()),
        }
    }

    /// GitHub REST API base URL without trailing slash.
    pub fn api_base(&self) -> &str {
        &self.api_base
    }

    /// The shared HTTP client, for building requests passed to [`GitHub::send`].
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// Environment credentials first, else the ones created through the manifest flow.
    pub async fn credentials(&self) -> ApiResult<Option<AppCredentials>> {
        if let Some(env) = &self.env {
            return Ok(Some(env.clone()));
        }
        let sealed = self
            .db
            .read(|conn| db::get_setting(conn, SETTINGS_KEY))
            .await?;
        let Some(sealed) = sealed else {
            return Ok(None);
        };
        let plain = self
            .key
            .open(&sealed)
            .ok_or_else(|| ApiError::internal("settings['github_app'] cannot be decrypted"))?;
        let creds = serde_json::from_slice(&plain).map_err(ApiError::internal)?;
        Ok(Some(creds))
    }

    pub async fn store_manifest_credentials(&self, c: &AppCredentials) -> ApiResult<()> {
        let json = serde_json::to_vec(c).map_err(ApiError::internal)?;
        let sealed = self.key.seal(&json);
        self.db
            .write(move |conn| db::put_setting(conn, SETTINGS_KEY, sealed))
            .await?;
        Ok(())
    }

    /// RS256 app JWT: `iat = now - 60`, `exp = now + 540`, `iss = app id`.
    pub fn app_jwt(c: &AppCredentials) -> ApiResult<String> {
        let ts = now();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({
            "iat": ts - JWT_BACKDATE_SECS,
            "exp": ts + JWT_LIFETIME_SECS,
            "iss": c.app_id.to_string(),
        });
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signing_input = format!("{header}.{claims}");

        let der = decode_private_key_pem(&c.private_key_pem).ok_or_else(invalid_private_key)?;
        let key_pair = RsaKeyPair::from_pkcs8(&der)
            .or_else(|_| RsaKeyPair::from_der(&der))
            .map_err(|_| invalid_private_key())?;
        let mut signature = vec![0u8; key_pair.public().modulus_len()];
        key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| ApiError::internal("RS256 signing failed"))?;
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }

    pub async fn installation_token(&self, installation_id: u64) -> ApiResult<String> {
        if let Some((token, created)) = self.cache().get(&installation_id) {
            if created.elapsed() < TOKEN_TTL {
                return Ok(token.clone());
            }
        }
        let creds = self.credentials().await?.ok_or_else(app_not_configured)?;
        let jwt = Self::app_jwt(&creds)?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base
        );
        let response = self.send(|| self.http.post(&url).bearer_auth(&jwt)).await?;

        #[derive(Deserialize)]
        struct TokenResponse {
            token: String,
        }
        let body: TokenResponse = Self::json(response, &url).await?;
        self.cache()
            .insert(installation_id, (body.token.clone(), Instant::now()));
        Ok(body.token)
    }

    pub fn forget_installation_token(&self, installation_id: u64) {
        self.cache().remove(&installation_id);
    }

    /// Installation id of the app on `owner/repo` (`GET /repos/{full}/installation` with the app JWT).
    pub async fn repo_installation(&self, full_name: &str) -> ApiResult<u64> {
        let creds = self.credentials().await?.ok_or_else(app_not_configured)?;
        let jwt = Self::app_jwt(&creds)?;
        let url = format!("{}/repos/{full_name}/installation", self.api_base);
        let response = self.send(|| self.http.get(&url).bearer_auth(&jwt)).await?;

        #[derive(Deserialize)]
        struct InstallationResponse {
            id: u64,
        }
        let body: InstallationResponse = Self::json(response, &url).await?;
        Ok(body.id)
    }

    /// Parses a 2xx GitHub response body; a body that does not parse is 502 `github_error`.
    pub async fn json<T: DeserializeOwned>(response: reqwest::Response, url: &str) -> ApiResult<T> {
        response.json::<T>().await.map_err(|err| {
            tracing::warn!("unexpected GitHub response from {url}: {err}");
            ApiError::bad_gateway("github_error", "GitHub sent an unexpected response.")
        })
    }

    /// Single path for every GitHub call: default headers, retries on network errors and 502/503/504,
    /// rate-limit and error mapping with [`ApiError::github`].
    pub async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> ApiResult<reqwest::Response> {
        let mut attempt = 1;
        loop {
            let mut request = build().build().map_err(ApiError::internal)?;
            let headers = request.headers_mut();
            if !headers.contains_key(ACCEPT) {
                headers.insert(ACCEPT, HeaderValue::from_static(ACCEPT_GITHUB_JSON));
            }
            if !headers.contains_key(API_VERSION_HEADER) {
                headers.insert(API_VERSION_HEADER, HeaderValue::from_static(API_VERSION));
            }
            let retryable = match self.http.execute(request).await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    let transient = matches!(status.as_u16(), 502..=504);
                    if !transient || attempt >= MAX_ATTEMPTS {
                        return Err(error_from_response(response).await);
                    }
                    format!("status {status}")
                }
                Err(err) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(ApiError::bad_gateway(
                            "github_unreachable",
                            format!("GitHub could not be reached: {err}"),
                        ));
                    }
                    err.to_string()
                }
            };
            tracing::debug!(attempt, "retrying GitHub request after {retryable}");
            tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
            attempt += 1;
        }
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<u64, (String, Instant)>> {
        lock(&self.token_cache)
    }
}

async fn error_from_response(response: reqwest::Response) -> ApiError {
    let status = response.status();
    let code = status.as_u16();
    if matches!(code, 403 | 429) {
        if let Some(seconds) = rate_limit_wait(response.headers()) {
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "github_rate_limited",
                format!("GitHub rate limit reached. Try again in {seconds} seconds."),
            )
            .with_github(Some(GitHubFailure::RateLimited));
        }
    }
    let text = response.text().await.unwrap_or_default();
    let message = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|body| body.get("message")?.as_str().map(str::to_string))
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("").to_string());
    ApiError::bad_gateway(
        "github_error",
        format!("GitHub rejected the request: {code} {message}")
            .trim_end()
            .to_string(),
    )
    .with_github(Some(GitHubFailure::Status(code)))
}

/// Seconds to wait when the response is a rate-limit answer, else `None`.
fn rate_limit_wait(headers: &HeaderMap) -> Option<i64> {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    };
    let retry_after = header(RETRY_AFTER.as_str());
    let exhausted = header("x-ratelimit-remaining") == Some("0");
    if retry_after.is_none() && !exhausted {
        return None;
    }
    if let Some(seconds) = retry_after.and_then(|v| v.parse::<i64>().ok()) {
        return Some(seconds.max(1));
    }
    let wait = header("x-ratelimit-reset")
        .and_then(|v| v.parse::<i64>().ok())
        .map(|reset| reset - now())
        .unwrap_or(DEFAULT_RATE_LIMIT_WAIT_SECS);
    Some(wait.max(1))
}

/// DER bytes of a PKCS#1 (`RSA PRIVATE KEY`) or PKCS#8 (`PRIVATE KEY`) PEM.
fn decode_private_key_pem(pem: &str) -> Option<Vec<u8>> {
    let pem = pem.trim();
    let encoded = [
        ("-----BEGIN PRIVATE KEY-----", "-----END PRIVATE KEY-----"),
        (
            "-----BEGIN RSA PRIVATE KEY-----",
            "-----END RSA PRIVATE KEY-----",
        ),
    ]
    .into_iter()
    .find_map(|(begin, end)| pem.strip_prefix(begin)?.strip_suffix(end))?;
    let compact: Vec<u8> = encoded
        .bytes()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    STANDARD.decode(compact).ok().filter(|der| !der.is_empty())
}

fn invalid_private_key() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "invalid_app_key",
        "The GitHub App private key cannot be read.",
    )
}

pub(crate) fn app_not_configured() -> ApiError {
    ApiError::conflict(
        "app_not_configured",
        "The GitHub App is not configured yet.",
    )
}
