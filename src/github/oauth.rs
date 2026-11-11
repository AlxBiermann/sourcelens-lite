use reqwest::header::ACCEPT;
use reqwest::Url;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
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
    token_request(state, &body).await
}

pub async fn fetch_user(state: &AppState, access_token: &str) -> ApiResult<GhUser> {
    let url = format!("{}/user", state.github.api_base());
    let response = state
        .github
        .send(|| state.http.get(&url).bearer_auth(access_token))
        .await?;
    GitHub::json(response, &url).await
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
                    let owner_exists = db::owner_exists(&tx)?;
                    match (owner_exists, claim) {
                        (false, true) => {
                            tx.execute(
                                "INSERT INTO users (github_id, login, name, avatar_url, role, created_at, last_login_at)
                                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                                params![user.id, user.login, user.name, user.avatar_url, Role::Owner, ts],
                            )?;
                            Resolution::User(tx.last_insert_rowid())
                        }
                        (false, false) => Resolution::SetupRequired,
                        (true, _) => Resolution::NotInvited,
                    }
                }
            };
            tx.commit()?;
            Ok::<_, rusqlite::Error>(resolution)
        })
        .await?;
    Ok(resolution)
}

/// Exchanges a refresh token for a new user token.
pub async fn refresh_token(
    state: &AppState,
    creds: &AppCredentials,
    refresh_token: &str,
) -> ApiResult<GhToken> {
    let body = serde_json::json!({
        "client_id": creds.client_id,
        "client_secret": creds.client_secret,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    token_request(state, &body).await
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
    let parsed: TokenResponse = GitHub::json(response, ACCESS_TOKEN_URL).await?;
    let Some(access_token) = parsed.access_token.filter(|t| !t.is_empty()) else {
        let reason = parsed
            .error_description
            .or(parsed.error)
            .unwrap_or_else(|| "no access token returned".to_string());
        return Err(ApiError::bad_gateway(
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
