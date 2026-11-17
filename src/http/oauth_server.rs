//! OAuth 2.1 authorization server for `/mcp`: public clients only, PKCE `S256` mandatory, dynamic
//! client registration. A token reads all connected repositories.

use axum::body::Bytes;
use axum::extract::rejection::FormRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::Url;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::{is_loopback_host, Config};
use crate::crypto::{pkce_challenge, random_hex, sha256_hex};
use crate::error::{ApiError, ApiResult};
use crate::http::session::AuthUser;
use crate::http::{Body, PathId};
use crate::model::Role;
use crate::state::{lock, now, AppState, SECS_PER_DAY};

pub const ACCESS_PREFIX: &str = "slo_";
pub const REFRESH_PREFIX: &str = "slr_";
pub const CLIENT_PREFIX: &str = "slc_";
const ACCESS_TTL: i64 = 3600;
const REFRESH_TTL: i64 = 30 * SECS_PER_DAY;
const CODE_TTL: i64 = 60;
const MAX_REDIRECT_URIS: usize = 5;
const MAX_REDIRECT_URI_BYTES: usize = 512;
const MAX_CLIENT_NAME_CHARS: usize = 80;
const DEFAULT_CLIENT_NAME: &str = "MCP client";
/// Bytes of randomness in authorization codes, access tokens and refresh tokens.
const SECRET_BYTES: usize = 32;
/// Bytes of a SHA-256 PKCE code challenge.
const CHALLENGE_BYTES: usize = 32;
const SCOPE: &str = "mcp";
const GRANT_TYPES: [&str; 2] = ["authorization_code", "refresh_token"];
/// Redirect URI schemes that would run or read content in the browser, or start a system handler
/// or another protocol, instead of returning to an app.
const FORBIDDEN_SCHEMES: &[&str] = &[
    "javascript",
    "data",
    "vbscript",
    "file",
    "blob",
    "about",
    "search",
    "search-ms",
    "shell",
    "res",
    "mk",
    "view-source",
    "jar",
    "ftp",
    "ws",
    "wss",
    "mailto",
    "tel",
    "sms",
    "chrome",
    "chrome-extension",
    "moz-extension",
    "filesystem",
    "intent",
    "microsoft-edge",
    "onenote",
    "smb",
    "itms-services",
    "mhtml",
    "its",
    "ldap",
    "telnet",
];
/// Windows protocol handlers (`ms-msdt:`, `ms-officecmd:`, …) are all refused.
const FORBIDDEN_SCHEME_PREFIX: &str = "ms-";

/// Discovery documents, `/oauth/register`, `/oauth/token`, `/api/oauth/authorize` and
/// `/api/oauth/grants[/{id}]`. The consent page `/oauth/authorize` is a UI route.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/token", post(token))
        .route("/api/oauth/authorize", get(authorize_info).post(authorize))
        .route("/api/oauth/grants", get(list_grants))
        .route("/api/oauth/grants/{id}", delete(revoke_grant))
}

/// Protected resource metadata of `/mcp` (RFC 9728).
async fn protected_resource(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "resource": state.cfg.mcp_url(),
        "authorization_servers": [state.cfg.public_url],
        "scopes_supported": [SCOPE],
        "bearer_methods_supported": ["header"],
        "resource_name": "SourceLens",
    }))
}

/// Authorization server metadata (RFC 8414).
async fn authorization_server(State(state): State<AppState>) -> Json<Value> {
    let base = &state.cfg.public_url;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "registration_endpoint": format!("{base}/oauth/register"),
        "scopes_supported": [SCOPE],
        "response_types_supported": ["code"],
        "grant_types_supported": GRANT_TYPES,
        "token_endpoint_auth_methods_supported": ["none"],
        "code_challenge_methods_supported": ["S256"],
    }))
}

/// An error of the registration or token endpoint.
enum Failure {
    /// 400 `{ "error", "error_description" }` (RFC 6749 §5.2, RFC 7591 §3.2.2).
    OAuth(&'static str, String),
    Api(ApiError),
}

fn failure(error: &'static str, description: impl Into<String>) -> Failure {
    Failure::OAuth(error, description.into())
}

impl From<ApiError> for Failure {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<rusqlite::Error> for Failure {
    fn from(err: rusqlite::Error) -> Self {
        Self::Api(err.into())
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        match self {
            Self::OAuth(error, description) => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error, "error_description": description })),
            )
                .into_response(),
            Self::Api(err) => err.into_response(),
        }
    }
}

// ── Client registration ──────────────────────────────────────────────────────

/// Client metadata of `POST /oauth/register` (RFC 7591); other fields are ignored.
#[derive(Deserialize)]
struct Registration {
    redirect_uris: Option<Vec<String>>,
    client_name: Option<String>,
}

/// What a `client_id` carries, sealed with the instance key.
#[derive(Serialize, Deserialize)]
struct SealedClient {
    #[serde(rename = "n")]
    name: String,
    #[serde(rename = "r")]
    redirect_uris: Vec<String>,
    #[serde(rename = "t")]
    issued_at: i64,
}

/// A registered client, read back from its `client_id`.
pub struct Client {
    pub name: String,
    pub redirect_uris: Vec<String>,
}

/// The endpoint is unauthenticated, so it writes nothing: the client's metadata is sealed into
/// its `client_id`.
async fn register(State(state): State<AppState>, body: Bytes) -> Response {
    match registered_client(&state, &body) {
        Ok(client) => (StatusCode::CREATED, Json(client)).into_response(),
        Err(failure) => failure.into_response(),
    }
}

fn registered_client(state: &AppState, body: &[u8]) -> Result<Value, Failure> {
    let input: Registration = serde_json::from_slice(body).map_err(|_| {
        failure(
            "invalid_client_metadata",
            "The body must be a JSON object of client metadata.",
        )
    })?;
    let redirect_uris = input.redirect_uris.unwrap_or_default();
    if redirect_uris.is_empty() || redirect_uris.len() > MAX_REDIRECT_URIS {
        return Err(failure(
            "invalid_redirect_uri",
            format!("Register between 1 and {MAX_REDIRECT_URIS} redirect URIs."),
        ));
    }
    if let Some(bad) = redirect_uris.iter().find(|uri| !valid_redirect_uri(uri)) {
        return Err(failure(
            "invalid_redirect_uri",
            format!(
                "{bad} is not an allowed redirect URI: use https, http on localhost, or an app's own scheme."
            ),
        ));
    }
    let name = input
        .client_name
        .map(|name| {
            name.trim()
                .chars()
                .take(MAX_CLIENT_NAME_CHARS)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_NAME.to_string());
    let sealed = SealedClient {
        name,
        redirect_uris,
        issued_at: now(),
    };
    let plain = serde_json::to_vec(&sealed).map_err(ApiError::internal)?;
    let client_id = format!(
        "{CLIENT_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(state.key.seal(&plain))
    );
    Ok(json!({
        "client_id": client_id,
        "client_id_issued_at": sealed.issued_at,
        "client_name": sealed.name,
        "redirect_uris": sealed.redirect_uris,
        "grant_types": GRANT_TYPES,
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    }))
}

/// https, http only on the loopback host, or a native app's own scheme; never a fragment.
/// `Url` lowercases the scheme, so the comparison ignores case.
fn valid_redirect_uri(raw: &str) -> bool {
    if raw.len() > MAX_REDIRECT_URI_BYTES {
        return false;
    }
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    if url.fragment().is_some() {
        return false;
    }
    match url.scheme() {
        "https" => true,
        "http" => is_loopback_host(url.host_str()),
        scheme => {
            !FORBIDDEN_SCHEMES.contains(&scheme) && !scheme.starts_with(FORBIDDEN_SCHEME_PREFIX)
        }
    }
}

/// The client a `client_id` of this instance describes.
pub fn open_client(state: &AppState, client_id: &str) -> Option<Client> {
    let sealed = URL_SAFE_NO_PAD
        .decode(client_id.strip_prefix(CLIENT_PREFIX)?)
        .ok()?;
    let plain = state.key.open(&sealed)?;
    let client: SealedClient = serde_json::from_slice(&plain).ok()?;
    Some(Client {
        name: client.name,
        redirect_uris: client.redirect_uris,
    })
}

// ── Authorization ────────────────────────────────────────────────────────────

/// Authorization request parameters, as the client put them on the consent page URL.
#[derive(Deserialize)]
pub struct AuthorizeParams {
    pub response_type: Option<String>,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub state: Option<String>,
    pub code_challenge: Option<String>,
    pub code_challenge_method: Option<String>,
    pub resource: Option<String>,
}

/// Body of `POST /api/oauth/authorize`.
#[derive(Deserialize)]
struct Decision {
    #[serde(flatten)]
    params: AuthorizeParams,
    approve: bool,
}

/// The client of an authorization request and its registered redirect URI.
struct Target {
    client: Client,
    client_id: String,
    redirect: Url,
}

/// An authorization code waiting to be redeemed at the token endpoint.
pub struct PendingCode {
    /// sha256 hex of the `client_id`.
    client_hash: String,
    user_id: i64,
    /// `users.created_at` of `user_id`: ids are reused, so the id alone does not name the account.
    user_created_at: i64,
    redirect_uri: String,
    code_challenge: String,
    pub expires_at: i64,
}

/// `redirect` with `pairs` and the client's `state` appended to its query.
fn redirect_with(redirect: &Url, pairs: &[(&str, &str)], state: Option<&str>) -> String {
    let mut url = redirect.clone();
    {
        let mut query = url.query_pairs_mut();
        query.extend_pairs(pairs);
        if let Some(state) = state {
            query.append_pair("state", state);
        }
    }
    url.into()
}

/// `resource` absent, or naming `/mcp` (a trailing `/` is ignored).
fn valid_resource(cfg: &Config, resource: Option<&str>) -> bool {
    resource.is_none_or(|resource| resource.trim_end_matches('/') == cfg.mcp_url())
}

/// The client and redirect URI of a request. Failing that, an error for the consent page alone:
/// a redirect URI that is not registered is not trusted with a redirect.
fn target(state: &AppState, params: &AuthorizeParams) -> ApiResult<Target> {
    let client_id = params.client_id.as_deref().unwrap_or_default();
    let client = open_client(state, client_id).ok_or_else(|| {
        ApiError::bad_request(
            "invalid_client",
            "Unknown client. Remove the server from your MCP client and add it again.",
        )
    })?;
    // Checked again: a client registered before a scheme was forbidden keeps its client_id.
    let redirect = params
        .redirect_uri
        .as_deref()
        .filter(|uri| {
            client
                .redirect_uris
                .iter()
                .any(|registered| registered == *uri)
        })
        .filter(|uri| valid_redirect_uri(uri))
        .and_then(|uri| Url::parse(uri).ok())
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_redirect_uri",
                "The redirect URI is not registered for this client.",
            )
        })?;
    Ok(Target {
        client,
        client_id: client_id.to_string(),
        redirect,
    })
}

/// The first problem of a request that OAuth reports to the client on its redirect URI:
/// `(error, error_description)`. `None` means the request is valid, its code challenge included.
fn refusal(cfg: &Config, params: &AuthorizeParams) -> Option<(&'static str, &'static str)> {
    if params.response_type.as_deref() != Some("code") {
        return Some((
            "unsupported_response_type",
            "Only response_type=code is supported.",
        ));
    }
    let challenge_valid = params.code_challenge.as_deref().is_some_and(|challenge| {
        URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|bytes| bytes.len() == CHALLENGE_BYTES)
    });
    if !challenge_valid || params.code_challenge_method.as_deref() != Some("S256") {
        return Some((
            "invalid_request",
            "A PKCE code_challenge with code_challenge_method=S256 is required.",
        ));
    }
    if !valid_resource(cfg, params.resource.as_deref()) {
        return Some((
            "invalid_target",
            "This server only issues tokens for its /mcp endpoint.",
        ));
    }
    None
}

/// `redirect` with the refusal's `error`, `error_description` and the client's `state`.
fn refusal_redirect(
    redirect: &Url,
    (error, description): (&str, &str),
    state: Option<&str>,
) -> String {
    redirect_with(
        redirect,
        &[("error", error), ("error_description", description)],
        state,
    )
}

/// Where a redirect URI sends the browser, for display, and whether that is an app on this
/// computer: the host (and an explicit port) for https, `host:port` for loopback http,
/// `scheme://host` or `scheme:` for an app's own scheme.
fn redirect_target(redirect: &Url) -> (String, bool) {
    let host = redirect.host_str();
    match (redirect.scheme(), host) {
        ("https", Some(name)) => match redirect.port() {
            Some(port) => (format!("{name}:{port}"), false),
            None => (name.to_string(), false),
        },
        ("http", Some(name)) if is_loopback_host(host) => {
            let port = redirect.port_or_known_default().unwrap_or(80);
            (format!("{name}:{port}"), true)
        }
        (scheme, Some(name)) => (format!("{scheme}://{name}"), false),
        (scheme, None) => (format!("{scheme}:"), false),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeInfo {
    client_name: String,
    redirect_target: String,
    redirect_local: bool,
    /// Description of a request error that OAuth reports to the client.
    error: Option<&'static str>,
    /// The client's redirect URI carrying that error; the page only goes there on request.
    redirect_to: Option<String>,
}

/// What the consent page shows: the client, where it sends the browser back to, and a request
/// error to return to the client with.
async fn authorize_info(
    State(state): State<AppState>,
    _user: AuthUser,
    Query(params): Query<AuthorizeParams>,
) -> ApiResult<Json<AuthorizeInfo>> {
    let target = target(&state, &params)?;
    let refused = refusal(&state.cfg, &params);
    let (redirect_target, redirect_local) = redirect_target(&target.redirect);
    Ok(Json(AuthorizeInfo {
        client_name: target.client.name,
        redirect_target,
        redirect_local,
        error: refused.map(|(_, description)| description),
        redirect_to: refused
            .map(|refused| refusal_redirect(&target.redirect, refused, params.state.as_deref())),
    }))
}

/// Allow or deny: answers where the browser goes next, with a code or `access_denied`.
async fn authorize(
    State(state): State<AppState>,
    user: AuthUser,
    Body(decision): Body<Decision>,
) -> ApiResult<Json<Value>> {
    let params = decision.params;
    let target = target(&state, &params)?;
    if let Some(refused) = refusal(&state.cfg, &params) {
        let to = refusal_redirect(&target.redirect, refused, params.state.as_deref());
        return Ok(Json(json!({ "redirectTo": to })));
    }
    if !decision.approve {
        let to = refusal_redirect(
            &target.redirect,
            ("access_denied", "The user denied access."),
            params.state.as_deref(),
        );
        return Ok(Json(json!({ "redirectTo": to })));
    }
    let user_id = user.id;
    let user_created_at: i64 = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT created_at FROM users WHERE id = ?1",
                [user_id],
                |row| row.get(0),
            )
        })
        .await?;
    let code = random_hex(SECRET_BYTES);
    let pending = PendingCode {
        client_hash: sha256_hex(target.client_id.as_bytes()),
        user_id,
        user_created_at,
        redirect_uri: params.redirect_uri.unwrap_or_default(),
        code_challenge: params.code_challenge.unwrap_or_default(),
        expires_at: now() + CODE_TTL,
    };
    {
        let mut codes = lock(&state.oauth_codes);
        let ts = now();
        codes.retain(|_, waiting| waiting.expires_at > ts);
        codes.insert(sha256_hex(code.as_bytes()), pending);
    }
    let to = redirect_with(
        &target.redirect,
        &[("code", code.as_str())],
        params.state.as_deref(),
    );
    Ok(Json(json!({ "redirectTo": to })))
}

// ── Token endpoint ───────────────────────────────────────────────────────────

/// Form body of `POST /oauth/token`; which fields count depends on `grant_type`.
#[derive(Deserialize)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
    resource: Option<String>,
}

/// Every answer carries `Cache-Control: no-store` (RFC 6749 §5.1).
async fn token(
    State(state): State<AppState>,
    form: Result<Form<TokenForm>, FormRejection>,
) -> Response {
    let outcome = match form {
        Ok(Form(form)) => {
            let grant_type = form.grant_type.clone();
            match grant_type.as_deref() {
                Some("authorization_code") => redeem_code(&state, form).await,
                Some("refresh_token") => refresh(&state, form).await,
                _ => Err(failure(
                    "unsupported_grant_type",
                    "Use grant_type authorization_code or refresh_token.",
                )),
            }
        }
        Err(_) => Err(failure(
            "invalid_request",
            "The token request must be a form-encoded body.",
        )),
    };
    let mut response = match outcome {
        Ok(tokens) => Json(tokens).into_response(),
        Err(failure) => failure.into_response(),
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn invalid_grant(description: &str) -> Failure {
    failure("invalid_grant", description)
}

/// A new access and refresh token pair: the secrets, and the stored hashes.
struct Tokens {
    access: String,
    refresh: String,
}

impl Tokens {
    fn new() -> Self {
        Self {
            access: format!("{ACCESS_PREFIX}{}", random_hex(SECRET_BYTES)),
            refresh: format!("{REFRESH_PREFIX}{}", random_hex(SECRET_BYTES)),
        }
    }

    fn hashes(&self) -> (String, String) {
        (
            sha256_hex(self.access.as_bytes()),
            sha256_hex(self.refresh.as_bytes()),
        )
    }

    fn response(&self) -> Value {
        json!({
            "access_token": self.access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TTL,
            "refresh_token": self.refresh,
            "scope": SCOPE,
        })
    }
}

async fn redeem_code(state: &AppState, form: TokenForm) -> Result<Value, Failure> {
    // Removed before any check: a code is spent by its first use, successful or not.
    let pending = form
        .code
        .as_deref()
        .and_then(|code| lock(&state.oauth_codes).remove(&sha256_hex(code.as_bytes())));
    if !valid_resource(&state.cfg, form.resource.as_deref()) {
        return Err(failure(
            "invalid_target",
            "This server only issues tokens for its /mcp endpoint.",
        ));
    }
    let client_id = form.client_id.unwrap_or_default();
    let verifier = form.code_verifier.unwrap_or_default();
    let pending = pending
        .filter(|pending| pending.expires_at > now())
        .filter(|pending| pending.client_hash == sha256_hex(client_id.as_bytes()))
        .filter(|pending| form.redirect_uri.as_deref() == Some(pending.redirect_uri.as_str()))
        .filter(|pending| pkce_challenge(&verifier) == pending.code_challenge)
        .ok_or_else(|| {
            invalid_grant("The authorization code is invalid, expired or already used.")
        })?;
    let client = open_client(state, &client_id)
        .ok_or_else(|| invalid_grant("The client is not known to this server."))?;

    let tokens = Tokens::new();
    let (access_hash, refresh_hash) = tokens.hashes();
    state
        .db
        .write(move |conn| -> Result<(), Failure> {
            let ts = now();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let user_exists = tx
                .query_row(
                    "SELECT 1 FROM users WHERE id = ?1 AND created_at = ?2",
                    params![pending.user_id, pending.user_created_at],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !user_exists {
                return Err(invalid_grant(
                    "The user who allowed this client no longer exists.",
                ));
            }
            tx.execute(
                "DELETE FROM oauth_grants WHERE refresh_expires_at <= ?1",
                [ts],
            )?;
            tx.execute(
                "DELETE FROM oauth_grants WHERE user_id = ?1 AND client_hash = ?2",
                params![pending.user_id, pending.client_hash],
            )?;
            tx.execute(
                "INSERT INTO oauth_grants (user_id, client_hash, client_name, access_hash,
                   access_expires_at, refresh_hash, refresh_expires_at, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    pending.user_id,
                    pending.client_hash,
                    client.name,
                    access_hash,
                    ts + ACCESS_TTL,
                    refresh_hash,
                    ts + REFRESH_TTL,
                    ts
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(tokens.response())
}

/// What a presented refresh token is to the grants of its client.
enum Presented {
    /// The current, unexpired refresh token of the grant.
    Current(i64),
    /// The refresh token the grant's last rotation replaced.
    Replaced(i64),
}

/// Rotates both tokens of the grant; the old refresh token stops working. A replaced refresh
/// token that comes back means someone else holds a copy (OAuth 2.1 §4.3.1): the grant is
/// revoked, so neither the client nor the copy keeps access.
async fn refresh(state: &AppState, form: TokenForm) -> Result<Value, Failure> {
    let (Some(refresh_token), Some(client_id)) = (form.refresh_token, form.client_id) else {
        return Err(invalid_grant("refresh_token and client_id are required."));
    };
    let old_hash = sha256_hex(refresh_token.as_bytes());
    let client_hash = sha256_hex(client_id.as_bytes());
    let rejected = || invalid_grant("The refresh token is invalid, expired or revoked.");
    // Looked up on a reader: the endpoint is unauthenticated, and requests that match no grant
    // must not wait for the writer.
    let presented = state
        .db
        .read({
            let old_hash = old_hash.clone();
            move |conn| {
                conn.query_row(
                    "SELECT id, refresh_hash = ?1 FROM oauth_grants
                     WHERE client_hash = ?2
                       AND ((refresh_hash = ?1 AND refresh_expires_at > ?3)
                            OR prev_refresh_hash = ?1)",
                    params![old_hash, client_hash, now()],
                    |row| {
                        let id: i64 = row.get(0)?;
                        let current: bool = row.get(1)?;
                        Ok(if current {
                            Presented::Current(id)
                        } else {
                            Presented::Replaced(id)
                        })
                    },
                )
                .optional()
            }
        })
        .await?
        .ok_or_else(rejected)?;
    let grant_id = match presented {
        Presented::Current(grant_id) => grant_id,
        Presented::Replaced(grant_id) => {
            state
                .db
                .write(move |conn| {
                    conn.execute(
                        "DELETE FROM oauth_grants WHERE id = ?1 AND prev_refresh_hash = ?2",
                        params![grant_id, old_hash],
                    )
                })
                .await?;
            lock(&state.key_touch).remove(&-grant_id);
            return Err(invalid_grant(
                "This refresh token was already used, so the client was disconnected. Connect it again.",
            ));
        }
    };
    let tokens = Tokens::new();
    let (access_hash, refresh_hash) = tokens.hashes();
    let rotated = state
        .db
        .write(move |conn| {
            let ts = now();
            conn.execute(
                "UPDATE oauth_grants SET access_hash = ?1, access_expires_at = ?2,
                   refresh_hash = ?3, refresh_expires_at = ?4, prev_refresh_hash = ?6
                 WHERE id = ?5 AND refresh_hash = ?6 AND refresh_expires_at > ?7",
                params![
                    access_hash,
                    ts + ACCESS_TTL,
                    refresh_hash,
                    ts + REFRESH_TTL,
                    grant_id,
                    old_hash,
                    ts
                ],
            )
        })
        .await?;
    if rotated == 0 {
        return Err(rejected());
    }
    Ok(tokens.response())
}

// ── Grants ───────────────────────────────────────────────────────────────────

/// A client connected with OAuth.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub id: i64,
    pub client_name: String,
    /// Login of the user who allowed the client.
    pub user: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// The caller's own grants; admins see all. Grants whose refresh token expired are left out.
async fn list_grants(State(state): State<AppState>, user: AuthUser) -> ApiResult<Json<Vec<Grant>>> {
    let all = user.role == Role::Owner;
    let grants = state
        .db
        .read(move |conn| {
            conn.prepare(
                "SELECT g.id, g.client_name, u.login, g.created_at, g.last_used_at
                 FROM oauth_grants g JOIN users u ON u.id = g.user_id
                 WHERE (?1 OR g.user_id = ?2) AND g.refresh_expires_at > ?3
                 ORDER BY g.created_at DESC, g.id DESC",
            )?
            .query_map(params![all, user.id, now()], |row| {
                Ok(Grant {
                    id: row.get(0)?,
                    client_name: row.get(1)?,
                    user: row.get(2)?,
                    created_at: row.get(3)?,
                    last_used_at: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(Json(grants))
}

/// Revokes one of the caller's grants (any grant for admins); its tokens stop working at once.
async fn revoke_grant(
    State(state): State<AppState>,
    user: AuthUser,
    PathId(id): PathId,
) -> ApiResult<StatusCode> {
    let all = user.role == Role::Owner;
    let deleted = state
        .db
        .write(move |conn| {
            conn.execute(
                "DELETE FROM oauth_grants WHERE id = ?1 AND (?2 OR user_id = ?3)",
                params![id, all, user.id],
            )
        })
        .await?;
    if deleted == 0 {
        return Err(ApiError::not_found(
            "grant_not_found",
            "This MCP client is not connected.",
        ));
    }
    lock(&state.key_touch).remove(&-id);
    Ok(StatusCode::NO_CONTENT)
}
