use reqwest::header::ACCEPT;
use reqwest::Url;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::db::Db;
use crate::error::{ApiError, ApiResult};
use crate::github::app::AppCredentials;
use crate::http::session::AuthUser;
use crate::state::{now, AppState};

const AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
/// Refresh this many seconds before the access token expires.
const REFRESH_MARGIN: i64 = 60;

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

pub fn callback_url(state: &AppState) -> String {
    format!("{}/auth/github/callback", state.cfg.public_url)
}

pub fn authorize_url(state: &AppState, creds: &AppCredentials, oauth_state: &str) -> String {
    let redirect = callback_url(state);
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
        "redirect_uri": callback_url(state),
    });
    token_request(state, &body).await
}

pub async fn fetch_user(state: &AppState, access_token: &str) -> ApiResult<GhUser> {
    let url = format!("{}/user", state.github.api_base());
    let response = state
        .github
        .send(|| state.http.get(&url).bearer_auth(access_token))
        .await?;
    response.json().await.map_err(ApiError::internal)
}

/// Applies the four sign-in rules in one transaction: known `github_id` → update profile; invited login →
/// bind `github_id`; no owner and `claim` → new owner; otherwise not invited / setup required.
pub async fn resolve_user(db: &Db, user: &GhUser, claim: bool) -> ApiResult<Resolution> {
    let user = user.clone();
    let ts = now();
    let resolution = db
        .write(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let known: Option<i64> = tx
                .query_row(
                    "SELECT id FROM users WHERE github_id = ?1",
                    [user.id],
                    |row| row.get(0),
                )
                .optional()?;
            let resolution = if let Some(id) = known {
                // A stale invitation for the (possibly renamed) login would collide with the UNIQUE login.
                tx.execute(
                    "DELETE FROM users WHERE login = ?1 AND github_id IS NULL",
                    [&user.login],
                )?;
                tx.execute(
                    "UPDATE users SET login = ?1, name = ?2, avatar_url = ?3, last_login_at = ?4 WHERE id = ?5",
                    params![user.login, user.name, user.avatar_url, ts, id],
                )?;
                Resolution::User(id)
            } else {
                let invited: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM users WHERE login = ?1 AND github_id IS NULL",
                        [&user.login],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(id) = invited {
                    tx.execute(
                        "UPDATE users SET github_id = ?1, login = ?2, name = ?3, avatar_url = ?4, last_login_at = ?5 WHERE id = ?6",
                        params![user.id, user.login, user.name, user.avatar_url, ts, id],
                    )?;
                    Resolution::User(id)
                } else {
                    let owner_exists: bool = tx.query_row(
                        "SELECT EXISTS (SELECT 1 FROM users WHERE role = 'owner')",
                        [],
                        |row| row.get(0),
                    )?;
                    match (owner_exists, claim) {
                        (false, true) => {
                            tx.execute(
                                "INSERT INTO users (github_id, login, name, avatar_url, role, created_at, last_login_at)
                                 VALUES (?1, ?2, ?3, ?4, 'owner', ?5, ?5)",
                                params![user.id, user.login, user.name, user.avatar_url, ts],
                            )?;
                            Resolution::User(tx.last_insert_rowid())
                        }
                        (false, false) => Resolution::SetupRequired,
                        (true, _) => Resolution::NotInvited,
                    }
                }
            };
            tx.commit()?;
            Ok(resolution)
        })
        .await?;
    Ok(resolution)
}

/// The signed-in user's GitHub token, refreshed when it expires within a minute.
pub async fn user_token(state: &AppState, user: &AuthUser) -> ApiResult<String> {
    let hash = user.session_hash.clone();
    let sealed: Option<Vec<u8>> = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT gh_token FROM sessions WHERE id_hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .optional()
        })
        .await?;
    let token = sealed
        .and_then(|sealed| state.key.open(&sealed))
        .and_then(|plain| serde_json::from_slice::<GhToken>(&plain).ok());
    let Some(token) = token else {
        return Err(reauth_required(state, user).await);
    };
    if token.expires_at.saturating_sub(REFRESH_MARGIN) >= now() {
        return Ok(token.access_token);
    }
    let Some(refresh_token) = token.refresh_token else {
        return Err(reauth_required(state, user).await);
    };
    let Some(creds) = state.github.credentials().await? else {
        return Err(reauth_required(state, user).await);
    };
    let body = serde_json::json!({
        "client_id": creds.client_id,
        "client_secret": creds.client_secret,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    let refreshed = match token_request(state, &body).await {
        Ok(refreshed) => refreshed,
        Err(err) => {
            tracing::warn!(user = %user.login, "GitHub token refresh failed: {err}");
            return Err(reauth_required(state, user).await);
        }
    };
    let sealed = state
        .key
        .seal(&serde_json::to_vec(&refreshed).map_err(ApiError::internal)?);
    let hash = user.session_hash.clone();
    state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE sessions SET gh_token = ?1 WHERE id_hash = ?2",
                params![sealed, hash],
            )
        })
        .await?;
    Ok(refreshed.access_token)
}

/// `POST https://github.com/login/oauth/access_token`; GitHub reports OAuth failures as 200 with an `error` field.
async fn token_request(state: &AppState, body: &serde_json::Value) -> ApiResult<GhToken> {
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
        .await?;
    let parsed: TokenResponse = response.json().await.map_err(ApiError::internal)?;
    let Some(access_token) = parsed.access_token.filter(|t| !t.is_empty()) else {
        let reason = parsed
            .error_description
            .or(parsed.error)
            .unwrap_or_else(|| "no access token returned".to_string());
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "github_error",
            format!("GitHub rejected the sign-in: {reason}"),
        ));
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

async fn reauth_required(state: &AppState, user: &AuthUser) -> ApiError {
    let hash = user.session_hash.clone();
    if let Err(err) = state
        .db
        .write(move |conn| conn.execute("DELETE FROM sessions WHERE id_hash = ?1", [hash]))
        .await
    {
        tracing::error!("cannot delete session: {err}");
    }
    ApiError::unauthorized("github_reauth_required", "Sign in with GitHub again.")
}
