//! Sign-in with one OpenID Connect provider: settings, discovery, the authorization code flow with
//! PKCE and the sign-in rules for SSO accounts.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::header::ACCEPT;
use reqwest::Url;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::is_loopback_host;
use crate::crypto::sha256_hex;
use crate::db::{self, delete_setting, get_setting, put_setting, Db};
use crate::error::{ApiError, ApiResult};
use crate::github::api::percent_encode;
use crate::http::members::{delete_keys, forget_key_touches};
use crate::model::Role;
use crate::state::{lock, now, AppState};

const SETTINGS_KEY: &str = "sso";
const SECRET_KEY: &str = "sso_client_secret";
const DEFAULT_NAME: &str = "SSO";
const MAX_NAME_CHARS: usize = 24;
const SCOPE: &str = "openid email profile";
/// Hex digits of the `sha256(issuer|sub)` suffix that keeps a taken login unique.
const LOGIN_SUFFIX_CHARS: usize = 8;

/// How the client authenticates at the token endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TokenAuth {
    /// `client_secret_basic`: HTTP Basic authentication.
    #[default]
    Basic,
    /// `client_secret_post`: credentials as form fields.
    Post,
}

/// What `settings['sso']` holds; the endpoints come from discovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct StoredSettings {
    enabled: bool,
    name: String,
    issuer: String,
    client_id: String,
    authorization_endpoint: String,
    token_endpoint: String,
    token_auth: TokenAuth,
}

impl Default for StoredSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            name: DEFAULT_NAME.to_string(),
            issuer: String::new(),
            client_id: String::new(),
            authorization_endpoint: String::new(),
            token_endpoint: String::new(),
            token_auth: TokenAuth::Basic,
        }
    }
}

/// Body of `PUT /api/settings/sso`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoInput {
    pub enabled: bool,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    /// Empty or absent keeps the saved secret.
    pub client_secret: Option<String>,
}

/// API view of the settings; the secret is never returned.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsoView {
    pub enabled: bool,
    pub name: String,
    pub issuer: String,
    pub client_id: String,
    pub client_secret_set: bool,
    /// What the admin registers with the provider.
    pub redirect_url: String,
}

/// The signed-in person as the provider's ID token describes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub sub: String,
    pub login: String,
    pub name: Option<String>,
    /// Only a verified address.
    pub email: Option<String>,
}

/// Result of matching an SSO identity against the accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignIn {
    /// `users.id` of an approved account.
    User(i64),
    /// The account waits for approval by an admin.
    Pending,
    /// Nobody has set up the instance yet.
    SetupRequired,
}

// ── settings ────────────────────────────────────────────────────────────────

fn parse_settings(json: Option<Vec<u8>>) -> ApiResult<StoredSettings> {
    match json {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(ApiError::internal),
        None => Ok(StoredSettings::default()),
    }
}

async fn load_settings(state: &AppState) -> ApiResult<StoredSettings> {
    let json = state
        .db
        .read(|conn| get_setting(conn, SETTINGS_KEY))
        .await?;
    parse_settings(json)
}

/// The stored settings JSON and the sealed client secret.
async fn read_stored(state: &AppState) -> ApiResult<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    let stored = state
        .db
        .read(|conn| {
            Ok::<_, rusqlite::Error>((
                get_setting(conn, SETTINGS_KEY)?,
                get_setting(conn, SECRET_KEY)?,
            ))
        })
        .await?;
    Ok(stored)
}

fn open_secret(state: &AppState, sealed: Option<Vec<u8>>) -> ApiResult<Option<String>> {
    match sealed {
        Some(sealed) => {
            let plain = state
                .key
                .open(&sealed)
                .ok_or_else(|| ApiError::internal("cannot decrypt the SSO client secret"))?;
            Ok(Some(String::from_utf8(plain).map_err(ApiError::internal)?))
        }
        None => Ok(None),
    }
}

/// Settings and the decrypted client secret for signing in; unreadable settings fail.
async fn stored(state: &AppState) -> ApiResult<(StoredSettings, Option<String>)> {
    let (json, sealed) = read_stored(state).await?;
    Ok((parse_settings(json)?, open_secret(state, sealed)?))
}

/// Settings and the decrypted client secret for the admin; `None` when the stored settings are
/// unreadable, so the admin sees defaults and saving overwrites them.
async fn stored_for_admin(state: &AppState) -> ApiResult<Option<(StoredSettings, Option<String>)>> {
    let (json, sealed) = read_stored(state).await?;
    match parse_settings(json) {
        Ok(settings) => Ok(Some((settings, open_secret(state, sealed)?))),
        Err(err) => {
            tracing::error!("the stored SSO settings are unreadable, showing defaults: {err}");
            Ok(None)
        }
    }
}

fn make_view(state: &AppState, settings: StoredSettings, client_secret_set: bool) -> SsoView {
    SsoView {
        enabled: settings.enabled,
        name: settings.name,
        issuer: settings.issuer,
        client_id: settings.client_id,
        client_secret_set,
        redirect_url: state.cfg.sso_callback_url(),
    }
}

/// Current settings (defaults when never saved or unreadable).
pub async fn view(state: &AppState) -> ApiResult<SsoView> {
    let (settings, secret) = stored_for_admin(state).await?.unwrap_or_default();
    Ok(make_view(state, settings, secret.is_some()))
}

fn invalid(message: &str) -> ApiError {
    ApiError::unprocessable("invalid_sso_settings", message)
}

/// `https`, or `http` on the local machine.
fn allowed_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host().is_some(),
        "http" => is_loopback_host(url.host_str()),
        _ => false,
    }
}

/// Endpoints found through the issuer's discovery document.
struct Discovered {
    authorization_endpoint: String,
    token_endpoint: String,
    token_auth: TokenAuth,
}

/// Reads `{issuer}/.well-known/openid-configuration`; the error is a sentence for the admin.
async fn discover(state: &AppState, issuer: &str) -> Result<Discovered, String> {
    #[derive(Deserialize)]
    struct Document {
        issuer: Option<String>,
        authorization_endpoint: Option<String>,
        token_endpoint: Option<String>,
        token_endpoint_auth_methods_supported: Option<Vec<String>>,
    }
    let url = format!("{issuer}/.well-known/openid-configuration");
    let response = state
        .sso_http
        .get(&url)
        .header(ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| {
            format!("Cannot reach {url}. Check the issuer URL and that this server can reach it.")
        })?;
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "{url} redirects elsewhere. Enter the issuer URL exactly as the provider reports it."
        ));
    }
    if !status.is_success() {
        return Err(format!("{url} answered with status {status}."));
    }
    let document: Document = response
        .json()
        .await
        .map_err(|_| format!("{url} did not return an OpenID Connect discovery document."))?;
    let reported = document.issuer.unwrap_or_default();
    let reported = reported.trim_end_matches('/');
    if reported != issuer {
        return Err(format!(
            "The provider reports the issuer \"{reported}\" instead of \"{issuer}\"."
        ));
    }
    let endpoint = |value: Option<String>, field: &str| {
        let url = value.unwrap_or_default();
        match endpoint_problem(&url, issuer, field) {
            Some(problem) => Err(problem),
            None => Ok(url),
        }
    };
    let authorization_endpoint =
        endpoint(document.authorization_endpoint, "authorization_endpoint")?;
    let token_endpoint = endpoint(document.token_endpoint, "token_endpoint")?;
    let token_auth = match &document.token_endpoint_auth_methods_supported {
        Some(methods)
            if !methods.iter().any(|m| m == "client_secret_basic")
                && methods.iter().any(|m| m == "client_secret_post") =>
        {
            TokenAuth::Post
        }
        _ => TokenAuth::Basic,
    };
    Ok(Discovered {
        authorization_endpoint,
        token_endpoint,
        token_auth,
    })
}

/// Why the discovered endpoint `field` is refused, as a sentence for the admin: it must be
/// `https`, or `http` on the local machine when the issuer is on the local machine as well.
fn endpoint_problem(endpoint: &str, issuer: &str, field: &str) -> Option<String> {
    if !allowed_url(endpoint) {
        return Some(format!(
            "The provider's discovery document has no {field} that is an https URL."
        ));
    }
    let plain_http = Url::parse(endpoint).is_ok_and(|url| url.scheme() == "http");
    let local_issuer = Url::parse(issuer).is_ok_and(|url| is_loopback_host(url.host_str()));
    if plain_http && !local_issuer {
        return Some(format!(
            "The provider's {field} {endpoint} uses http, which is accepted only when the issuer is on this machine too."
        ));
    }
    None
}

/// Validates and stores new settings; enabling runs discovery against the issuer.
pub async fn apply(state: &AppState, input: SsoInput) -> ApiResult<SsoView> {
    let stored = stored_for_admin(state).await?;
    // Unreadable settings say nothing about the provider: the secret is dropped and SSO access ends.
    let readable = stored.is_some();
    let (saved, saved_secret) = stored.unwrap_or_default();
    let name: String = input.name.trim().chars().take(MAX_NAME_CHARS).collect();
    let name = name.trim_end();
    let mut settings = StoredSettings {
        enabled: input.enabled,
        name: if name.is_empty() {
            DEFAULT_NAME.to_string()
        } else {
            name.to_string()
        },
        issuer: input.issuer.trim().trim_end_matches('/').to_string(),
        client_id: input.client_id.trim().to_string(),
        authorization_endpoint: saved.authorization_endpoint,
        token_endpoint: saved.token_endpoint,
        token_auth: saved.token_auth,
    };
    let new_secret = input
        .client_secret
        .map(|secret| secret.trim().to_string())
        .filter(|secret| !secret.is_empty());
    // The saved secret belongs to the saved issuer and client ID and is never sent anywhere else.
    let same_client = settings.issuer == saved.issuer && settings.client_id == saved.client_id;
    let drop_secret = new_secret.is_none() && saved_secret.is_some() && !same_client;
    let client_secret_set = new_secret.is_some() || (saved_secret.is_some() && same_client);

    if settings.enabled {
        if !allowed_url(&settings.issuer) {
            return Err(invalid(
                "Issuer URL must be an https URL (http only for localhost).",
            ));
        }
        if settings.client_id.is_empty() {
            return Err(invalid("Enter the client ID."));
        }
        if drop_secret {
            return Err(invalid(
                "Enter the client secret again when the issuer or client ID changes.",
            ));
        }
        if !client_secret_set {
            return Err(invalid("Enter the client secret."));
        }
        let discovered = discover(state, &settings.issuer)
            .await
            .map_err(|reason| ApiError::unprocessable("sso_discovery_failed", reason))?;
        settings.authorization_endpoint = discovered.authorization_endpoint;
        settings.token_endpoint = discovered.token_endpoint;
        settings.token_auth = discovered.token_auth;
    }
    // Turning SSO off or pointing it at another provider ends the access of every SSO account.
    let end_sso_access = !readable
        || (saved.enabled && !settings.enabled)
        || (!saved.issuer.is_empty() && saved.issuer != settings.issuer);

    let json = serde_json::to_string(&settings).map_err(ApiError::internal)?;
    let sealed = new_secret.map(|secret| state.key.seal(secret.as_bytes()));
    let revoked_keys = state
        .db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            put_setting(&tx, SETTINGS_KEY, &json)?;
            if let Some(sealed) = sealed {
                put_setting(&tx, SECRET_KEY, &sealed)?;
            } else if drop_secret || !readable {
                delete_setting(&tx, SECRET_KEY)?;
            }
            let mut revoked_keys = Vec::new();
            if end_sso_access {
                for table in ["sessions", "oauth_grants"] {
                    tx.execute(
                        &format!(
                            "DELETE FROM {table}
                             WHERE user_id IN (SELECT id FROM users WHERE sso_sub IS NOT NULL)"
                        ),
                        [],
                    )?;
                }
                let sso_users = tx
                    .prepare("SELECT id FROM users WHERE sso_sub IS NOT NULL")?
                    .query_map([], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for user_id in sso_users {
                    revoked_keys.extend(delete_keys(&tx, user_id)?);
                }
            }
            tx.commit()?;
            Ok::<_, rusqlite::Error>(revoked_keys)
        })
        .await?;
    if end_sso_access {
        forget_key_touches(state, revoked_keys);
        // Codes issued within the last minute could still become grants. They live 60 seconds, so all of them
        // are dropped: a client that was just being allowed asks again.
        lock(&state.oauth_codes).clear();
    }
    Ok(make_view(state, settings, client_secret_set))
}

/// Label of the sign-in button; `None` when SSO is off.
pub async fn button_name(state: &AppState) -> ApiResult<Option<String>> {
    let settings = load_settings(state).await?;
    Ok(settings.enabled.then_some(settings.name))
}

// ── sign-in ─────────────────────────────────────────────────────────────────

/// Where the browser starts the sign-in; `None` when SSO is off.
pub async fn authorize_url(
    state: &AppState,
    oauth_state: &str,
    nonce: &str,
    challenge: &str,
) -> ApiResult<Option<String>> {
    let settings = load_settings(state).await?;
    if !settings.enabled {
        return Ok(None);
    }
    let mut url = Url::parse(&settings.authorization_endpoint).map_err(ApiError::internal)?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &settings.client_id)
        .append_pair("redirect_uri", &state.cfg.sso_callback_url())
        .append_pair("scope", SCOPE)
        .append_pair("state", oauth_state)
        .append_pair("nonce", nonce)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(Some(url.to_string()))
}

fn sso_error(message: impl Into<String>) -> ApiError {
    ApiError::bad_gateway("sso_error", message)
}

/// Exchanges the authorization code and reads the person from the ID token.
///
/// The ID token signature is not verified: the token comes straight from the token endpoint over
/// TLS with client authentication, which OpenID Connect Core §3.1.3.7 permits in place of
/// signature validation. The claims are still checked: issuer, audience, expiry, nonce, subject.
pub async fn exchange(
    state: &AppState,
    code: &str,
    verifier: &str,
    nonce: &str,
) -> ApiResult<Identity> {
    #[derive(Deserialize)]
    struct TokenResponse {
        id_token: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }
    let (settings, secret) = stored(state).await?;
    if !settings.enabled {
        return Err(sso_error("Single sign-on is turned off."));
    }
    let secret = secret.ok_or_else(|| sso_error("No SSO client secret is saved."))?;
    let redirect_uri = state.cfg.sso_callback_url();
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier),
    ];
    let mut request = state
        .sso_http
        .post(&settings.token_endpoint)
        .header(ACCEPT, "application/json");
    match settings.token_auth {
        // RFC 6749 §2.3.1: client ID and secret are form-urlencoded before Basic encoding.
        TokenAuth::Basic => {
            request = request.basic_auth(
                percent_encode(&settings.client_id),
                Some(percent_encode(&secret)),
            );
        }
        TokenAuth::Post => {
            form.push(("client_id", settings.client_id.as_str()));
            form.push(("client_secret", secret.as_str()));
        }
    }
    let response = request
        .form(&form)
        .send()
        .await
        .map_err(|err| sso_error(format!("Cannot reach the token endpoint: {err}")))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| sso_error(format!("Cannot read the token response: {err}")))?;
    let token = serde_json::from_slice::<TokenResponse>(&bytes).ok();
    if !status.is_success() {
        let reason = token
            .and_then(|t| t.error_description.or(t.error))
            .map_or_else(String::new, |reason| format!(": {reason}"));
        return Err(sso_error(format!(
            "The token endpoint answered {status}{reason}."
        )));
    }
    let id_token = token
        .and_then(|t| t.id_token)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| sso_error("The token endpoint returned no ID token."))?;
    identity(&settings, &id_token, nonce)
}

/// `aud` is one string or a list.
#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, client_id: &str) -> bool {
        match self {
            Self::One(aud) => aud == client_id,
            Self::Many(auds) => auds.iter().any(|aud| aud == client_id),
        }
    }
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: f64,
    nonce: Option<String>,
    email: Option<String>,
    email_verified: Option<Value>,
    name: Option<String>,
    preferred_username: Option<String>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Checks the claims of `id_token` and derives the identity.
fn identity(settings: &StoredSettings, id_token: &str, nonce: &str) -> ApiResult<Identity> {
    let malformed = || sso_error("The ID token is malformed.");
    let mut segments = id_token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(malformed());
    };
    let json = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| malformed())?;
    let claims: Claims = serde_json::from_slice(&json).map_err(|_| malformed())?;

    if claims.iss.trim_end_matches('/') != settings.issuer {
        return Err(sso_error(format!(
            "The ID token was issued by \"{}\", not by \"{}\".",
            claims.iss, settings.issuer
        )));
    }
    if !claims.aud.contains(&settings.client_id) {
        return Err(sso_error("The ID token is meant for another client."));
    }
    if claims.exp <= now() as f64 {
        return Err(sso_error("The ID token has expired."));
    }
    if claims.nonce.as_deref() != Some(nonce) {
        return Err(sso_error("The ID token nonce does not match the sign-in."));
    }
    if claims.sub.is_empty() {
        return Err(sso_error("The ID token has no subject."));
    }

    // Only an explicit "not verified" disqualifies the address; providers differ in its type.
    let unverified = match &claims.email_verified {
        Some(Value::Bool(false)) => true,
        Some(Value::String(value)) => value == "false",
        _ => false,
    };
    let email = non_empty(claims.email).filter(|_| !unverified);
    let login = match &email {
        Some(email) => email.to_lowercase(),
        None => non_empty(claims.preferred_username).unwrap_or_else(|| claims.sub.clone()),
    };
    Ok(Identity {
        issuer: settings.issuer.clone(),
        sub: claims.sub,
        login,
        name: non_empty(claims.name),
        email,
    })
}

// ── accounts ────────────────────────────────────────────────────────────────

/// Applies the sign-in rules in one transaction, in order: an account with this issuer and subject
/// → update the profile (signed in when approved, else still pending); no admin yet → setup
/// required; otherwise a new account that waits for approval.
pub async fn resolve_user(db: &Db, identity: &Identity) -> ApiResult<SignIn> {
    let identity = identity.clone();
    let ts = now();
    let sign_in = db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(i64, bool)> = tx
                .query_row(
                    "SELECT id, status = 'active' FROM users WHERE sso_issuer = ?1 AND sso_sub = ?2",
                    params![identity.issuer, identity.sub],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let sign_in = match existing {
                Some((id, active)) => {
                    let login = free_login(&tx, &identity, Some(id))?;
                    tx.execute(
                        "UPDATE users SET login = ?1, name = ?2, email = ?3 WHERE id = ?4",
                        params![login, identity.name, identity.email, id],
                    )?;
                    if active {
                        tx.execute(
                            "UPDATE users SET last_login_at = ?1 WHERE id = ?2",
                            params![ts, id],
                        )?;
                        SignIn::User(id)
                    } else {
                        SignIn::Pending
                    }
                }
                None if !db::owner_exists(&tx)? => SignIn::SetupRequired,
                None => {
                    let login = free_login(&tx, &identity, None)?;
                    tx.execute(
                        "INSERT INTO users (login, name, email, role, status, sso_issuer, sso_sub, created_at)
                         VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7)",
                        params![
                            login,
                            identity.name,
                            identity.email,
                            Role::Member,
                            identity.issuer,
                            identity.sub,
                            ts
                        ],
                    )?;
                    SignIn::Pending
                }
            };
            tx.commit()?;
            Ok::<_, rusqlite::Error>(sign_in)
        })
        .await?;
    Ok(sign_in)
}

/// The identity's login, or — when another account (not `own_id`) holds it — the login with a
/// suffix derived from issuer and subject, so `users.login` stays unique.
fn free_login(
    tx: &Transaction<'_>,
    identity: &Identity,
    own_id: Option<i64>,
) -> rusqlite::Result<String> {
    let taken: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE login = ?1 AND id IS NOT ?2)",
        params![identity.login, own_id],
        |row| row.get(0),
    )?;
    if !taken {
        return Ok(identity.login.clone());
    }
    let digest = sha256_hex(format!("{}|{}", identity.issuer, identity.sub).as_bytes());
    Ok(format!(
        "{}~{}",
        identity.login,
        &digest[..LOGIN_SUFFIX_CHARS]
    ))
}

#[cfg(test)]
mod tests {
    use super::endpoint_problem;

    #[test]
    fn http_endpoints_need_an_issuer_on_this_machine() {
        let local = "http://127.0.0.1:9000";
        let remote = "https://login.example.com";
        assert_eq!(
            endpoint_problem("http://127.0.0.1:9000/token", local, "token_endpoint"),
            None
        );
        assert_eq!(
            endpoint_problem("http://localhost/token", local, "token_endpoint"),
            None
        );
        assert_eq!(
            endpoint_problem("https://login.example.com/token", remote, "token_endpoint"),
            None
        );
        assert_eq!(
            endpoint_problem("http://127.0.0.1:9000/token", remote, "token_endpoint").as_deref(),
            Some(
                "The provider's token_endpoint http://127.0.0.1:9000/token uses http, which is \
                 accepted only when the issuer is on this machine too."
            )
        );
        assert_eq!(
            endpoint_problem("http://login.example.com/authorize", remote, "authorization_endpoint")
                .as_deref(),
            Some("The provider's discovery document has no authorization_endpoint that is an https URL.")
        );
        assert!(endpoint_problem("", local, "token_endpoint").is_some());
    }
}
