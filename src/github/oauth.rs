use reqwest::header::ACCEPT;
use reqwest::Url;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::db::{self, Db};
use crate::error::{ApiError, ApiResult};
use crate::github::app::{AppCredentials, GitHub};
use crate::model::Role;
use crate::state::{now, AppState};

const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
}

/// The fields of `GET /user` used for sign-in.
#[derive(Debug, Clone, Deserialize)]
pub struct GhUser {
    pub id: i64,
    pub login: String,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

/// Result of matching a signed-in GitHub user against the member list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// `users.id` of the signed-in user.
    User(i64),
    NotInvited,
    SetupRequired,
}

pub fn authorize_url(state: &AppState, creds: &AppCredentials, oauth_state: &str) -> String {
    let redirect = state.cfg.callback_url();
    match Url::parse_with_params(
        AUTHORIZE_URL,
        [
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("state", oauth_state),
        ],
    ) {
        Ok(url) => url.to_string(),
        Err(_) => AUTHORIZE_URL.to_string(),
    }
}

/// Exchanges an authorization code for a user token.
pub async fn exchange_code(
    state: &AppState,
    creds: &AppCredentials,
    code: &str,
) -> ApiResult<GhToken> {
    let body = serde_json::json!({
        "client_id": creds.client_id,
        "client_secret": creds.client_secret,
        "code": code,
        "redirect_uri": state.cfg.callback_url(),
    });
    token_request(state, &body).await.map_err(|err| match err {
        TokenError::OAuth(reason) => ApiError::bad_gateway(
            "github_error",
            format!("GitHub rejected the sign-in: {reason}"),
        ),
        TokenError::Request(err) => err,
    })
}

pub async fn fetch_user(state: &AppState, access_token: &str) -> ApiResult<GhUser> {
    let url = format!("{}/user", state.github.api_base());
    let response = state
        .github
        .send(|| state.http.get(&url).bearer_auth(access_token))
        .await?;
    GitHub::json(response, &url).await
}

/// Applies the sign-in rules in one transaction, in order: a member (or invitation) with this
/// GitHub account id → update profile; no owner and `claim` → new owner; otherwise not invited, or
/// setup required while there is no owner.
pub async fn resolve_user(db: &Db, user: &GhUser, claim: bool) -> ApiResult<Resolution> {
    let user = user.clone();
    let ts = now();
    let resolution = db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let resolution = if let Some(id) = by_github_id(&tx, &user, ts)? {
                Resolution::User(id)
            } else if let Some(id) = by_claim(&tx, &user, claim, ts)? {
                Resolution::User(id)
            } else if db::owner_exists(&tx)? {
                Resolution::NotInvited
            } else {
                Resolution::SetupRequired
            };
            tx.commit()?;
            Ok::<_, rusqlite::Error>(resolution)
        })
        .await?;
    Ok(resolution)
}

/// Rule 1: the account id belongs to a member or an invitation → refresh the profile.
fn by_github_id(tx: &Transaction<'_>, user: &GhUser, ts: i64) -> rusqlite::Result<Option<i64>> {
    let id: Option<i64> = tx
        .query_row(
            "SELECT id FROM users WHERE github_id = ?1",
            [user.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    // GitHub logins are unique at any time, so another row holding this login is out of date (its
    // account was renamed). Mark it until that account signs in again, keeping `login` UNIQUE.
    tx.execute(
        "UPDATE users SET login = login || '~' || id WHERE login = ?1 AND id != ?2",
        params![user.login, id],
    )?;
    tx.execute(
        "UPDATE users SET login = ?1, name = ?2, avatar_url = ?3, last_login_at = ?4
         WHERE id = ?5",
        params![user.login, user.name, user.avatar_url, ts, id],
    )?;
    Ok(Some(id))
}

/// Rule 2: no owner yet and the sign-in presented the setup token → this account becomes the owner.
fn by_claim(
    tx: &Transaction<'_>,
    user: &GhUser,
    claim: bool,
    ts: i64,
) -> rusqlite::Result<Option<i64>> {
    if !claim || db::owner_exists(tx)? {
        return Ok(None);
    }
    tx.execute(
        "INSERT INTO users (github_id, login, name, avatar_url, role, created_at, last_login_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        params![
            user.id,
            user.login,
            user.name,
            user.avatar_url,
            Role::Owner,
            ts
        ],
    )?;
    Ok(Some(tx.last_insert_rowid()))
}

/// Why `POST https://github.com/login/oauth/access_token` produced no token.
#[derive(Debug)]
pub enum TokenError {
    /// GitHub answered with an OAuth `error` (for example `bad_refresh_token`): the grant is gone.
    OAuth(String),
    /// GitHub could not be reached or failed (network, 5xx, rate limit, unexpected answer).
    Request(ApiError),
}

/// Exchanges a refresh token for a new user token.
pub async fn refresh_token(
    state: &AppState,
    creds: &AppCredentials,
    refresh_token: &str,
) -> Result<GhToken, TokenError> {
    let body = serde_json::json!({
        "client_id": creds.client_id,
        "client_secret": creds.client_secret,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    token_request(state, &body).await
}

/// GitHub reports OAuth failures as 200 with an `error` field.
async fn token_request(state: &AppState, body: &serde_json::Value) -> Result<GhToken, TokenError> {
    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: Option<String>,
        refresh_token: Option<String>,
        expires_in: Option<i64>,
        error: Option<String>,
        error_description: Option<String>,
    }
    let response = state
        .github
        .send(|| {
            state
                .http
                .post(ACCESS_TOKEN_URL)
                .header(ACCEPT, "application/json")
                .json(body)
        })
        .await
        .map_err(TokenError::Request)?;
    let parsed: TokenResponse = GitHub::json(response, ACCESS_TOKEN_URL)
        .await
        .map_err(TokenError::Request)?;
    let Some(access_token) = parsed.access_token.filter(|t| !t.is_empty()) else {
        return Err(match parsed.error {
            Some(error) => TokenError::OAuth(parsed.error_description.unwrap_or(error)),
            None => TokenError::Request(ApiError::bad_gateway(
                "github_error",
                "GitHub returned no access token.",
            )),
        });
    };
    let expires_at = match parsed.expires_in {
        Some(seconds) => now().saturating_add(seconds),
        None => i64::MAX,
    };
    Ok(GhToken {
        access_token,
        refresh_token: parsed.refresh_token.filter(|t| !t.is_empty()),
        expires_at,
    })
}
