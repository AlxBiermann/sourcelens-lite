use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};

use axum::extract::FromRequestParts;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use rusqlite::{params, OptionalExtension};
use tokio::sync::OwnedMutexGuard;

use crate::config::Config;
use crate::crypto::{random_hex, sha256_hex};
use crate::error::{ApiError, ApiResult};
use crate::github::oauth::{self, GhToken, TokenError};
use crate::model::Role;
use crate::state::{lock, now, AppState, SECS_PER_DAY};

/// Session cookie name with an http public URL.
pub const SESSION_COOKIE: &str = "sl_session";
/// Session cookie name with an https public URL: `__Host-` binds it to this host, so a sibling
/// subdomain cannot set it.
pub const SECURE_SESSION_COOKIE: &str = "__Host-sl_session";
/// Cookie between GitHub sign-in start and callback, http and https variants.
pub const OAUTH_COOKIE: &str = "sl_oauth";
pub const SECURE_OAUTH_COOKIE: &str = "__Host-sl_oauth";
pub const SESSION_MAX_AGE: i64 = 30 * SECS_PER_DAY;
/// Refresh this many seconds before the access token expires.
const REFRESH_MARGIN: i64 = 60;
/// Bytes of randomness in a session cookie value.
const SESSION_ID_BYTES: usize = 32;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub login: String,
    pub role: Role,
    pub session_hash: String,
}

#[derive(Debug, Clone)]
pub struct Owner(pub AuthUser);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        current_user(&parts.headers, state)
            .await?
            .ok_or_else(|| ApiError::unauthorized("unauthenticated", "Sign in to continue."))
    }
}

impl FromRequestParts<AppState> for Owner {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = AuthUser::from_request_parts(parts, state).await?;
        if user.role != Role::Owner {
            return Err(ApiError::forbidden(
                "owner_required",
                "Only the owner can do this.",
            ));
        }
        Ok(Owner(user))
    }
}

/// Name of the session cookie for this instance.
pub fn session_cookie(cfg: &Config) -> &'static str {
    if cfg.public_is_https() {
        SECURE_SESSION_COOKIE
    } else {
        SESSION_COOKIE
    }
}

/// Name of the sign-in state cookie for this instance.
pub fn oauth_cookie(cfg: &Config) -> &'static str {
    if cfg.public_is_https() {
        SECURE_OAUTH_COOKIE
    } else {
        OAUTH_COOKIE
    }
}

/// The user of a valid, unexpired session cookie, if any.
pub async fn current_user(headers: &HeaderMap, state: &AppState) -> ApiResult<Option<AuthUser>> {
    let Some(value) = cookie(headers, session_cookie(&state.cfg)) else {
        return Ok(None);
    };
    let hash = sha256_hex(value.as_bytes());
    let ts = now();
    let user = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT u.id, u.login, u.role
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.id_hash = ?1 AND s.expires_at > ?2",
                params![hash, ts],
                |row| {
                    Ok(AuthUser {
                        id: row.get(0)?,
                        login: row.get(1)?,
                        role: row.get(2)?,
                        session_hash: hash.clone(),
                    })
                },
            )
            .optional()
        })
        .await?;
    Ok(user)
}

/// Inserts the session row and returns the cookie value.
pub async fn create_session(state: &AppState, user_id: i64, token: &GhToken) -> ApiResult<String> {
    let value = random_hex(SESSION_ID_BYTES);
    let hash = sha256_hex(value.as_bytes());
    let sealed = seal_token(state, token)?;
    let ts = now();
    state
        .db
        .write(move |conn| {
            conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [ts])?;
            conn.execute(
                "INSERT INTO sessions (id_hash, user_id, gh_token, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![hash, user_id, sealed, ts, ts + SESSION_MAX_AGE],
            )
        })
        .await?;
    Ok(value)
}

/// Deletes the session with this cookie hash.
pub async fn delete_session(state: &AppState, session_hash: &str) -> ApiResult<()> {
    let hash = session_hash.to_string();
    state
        .db
        .write(move |conn| conn.execute("DELETE FROM sessions WHERE id_hash = ?1", [hash]))
        .await?;
    Ok(())
}

/// The signed-in user's GitHub token, refreshed when it expires within a minute.
pub async fn user_token(state: &AppState, user: &AuthUser) -> ApiResult<String> {
    user_token_with(state, user, |refresh_token| async move {
        let creds = state
            .github
            .credentials()
            .await
            .map_err(TokenError::Request)?
            .ok_or_else(|| TokenError::OAuth("the GitHub App is not configured".into()))?;
        oauth::refresh_token(state, &creds, &refresh_token).await
    })
    .await
}

/// [`user_token`] with the refresh request passed in. Refreshes of one session run one at a time:
/// GitHub accepts each refresh token once, so a second concurrent refresh would be rejected.
pub async fn user_token_with<F, Fut>(
    state: &AppState,
    user: &AuthUser,
    refresh: F,
) -> ApiResult<String>
where
    F: FnOnce(String) -> Fut,
    Fut: Future<Output = Result<GhToken, TokenError>>,
{
    let Some(token) = load_token(state, &user.session_hash).await? else {
        return Err(reauth_required(state, user).await);
    };
    if is_fresh(&token) {
        return Ok(token.access_token);
    }
    let _gate = RefreshGate::enter(&state.refresh_locks, &user.session_hash).await;
    // Another request of this session may have refreshed the token while this one waited.
    let Some(token) = load_token(state, &user.session_hash).await? else {
        return Err(reauth_required(state, user).await);
    };
    if is_fresh(&token) {
        return Ok(token.access_token);
    }
    let Some(refresh_token) = token.refresh_token else {
        return Err(reauth_required(state, user).await);
    };
    let refreshed = match refresh(refresh_token).await {
        Ok(refreshed) => refreshed,
        Err(TokenError::OAuth(reason)) => {
            tracing::warn!(user = %user.login, "GitHub rejected the token refresh: {reason}");
            return Err(reauth_required(state, user).await);
        }
        Err(TokenError::Request(err)) => {
            tracing::warn!(user = %user.login, "GitHub token refresh failed: {err}");
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "github_unavailable",
                "GitHub could not renew your sign-in right now. Try again in a moment.",
            ));
        }
    };
    let sealed = seal_token(state, &refreshed)?;
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

fn is_fresh(token: &GhToken) -> bool {
    token.expires_at.saturating_sub(REFRESH_MARGIN) >= now()
}

/// The session's GitHub token; `None` when the row is gone or cannot be decrypted.
async fn load_token(state: &AppState, session_hash: &str) -> ApiResult<Option<GhToken>> {
    let hash = session_hash.to_string();
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
    Ok(sealed
        .and_then(|sealed| state.key.open(&sealed))
        .and_then(|plain| serde_json::from_slice::<GhToken>(&plain).ok()))
}

fn seal_token(state: &AppState, token: &GhToken) -> ApiResult<Vec<u8>> {
    let json = serde_json::to_vec(token).map_err(ApiError::internal)?;
    Ok(state.key.seal(&json))
}

type RefreshLocks = Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

/// Holds a session's refresh lock; on drop (also when the request is cancelled) it releases the lock
/// and removes the map entry once nobody else waits for it.
struct RefreshGate<'a> {
    locks: &'a RefreshLocks,
    session_hash: &'a str,
    mutex: Arc<tokio::sync::Mutex<()>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl<'a> RefreshGate<'a> {
    async fn enter(locks: &'a RefreshLocks, session_hash: &'a str) -> Self {
        let mutex = lock(locks)
            .entry(session_hash.to_string())
            .or_default()
            .clone();
        let guard = mutex.clone().lock_owned().await;
        Self {
            locks,
            session_hash,
            mutex,
            guard: Some(guard),
        }
    }
}

impl Drop for RefreshGate<'_> {
    fn drop(&mut self) {
        self.guard.take();
        let mut locks = lock(self.locks);
        // Only the map and this gate still hold the mutex: no request waits for it.
        if Arc::strong_count(&self.mutex) <= 2 {
            locks.remove(self.session_hash);
        }
    }
}

async fn reauth_required(state: &AppState, user: &AuthUser) -> ApiError {
    if let Err(err) = delete_session(state, &user.session_hash).await {
        tracing::error!("cannot delete session: {err}");
    }
    ApiError::unauthorized("github_reauth_required", "Sign in with GitHub again.")
}

/// Value of the cookie `name` from the request's `Cookie` headers.
pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (key, value) = pair.trim().split_once('=')?;
            (key == name).then(|| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
}

/// `Set-Cookie` value for an HttpOnly, SameSite=Lax cookie on `/`; `Secure` when the public URL is https.
pub fn set_cookie(state: &AppState, name: &str, value: &str, max_age: i64) -> String {
    let secure = if state.cfg.public_is_https() {
        "; Secure"
    } else {
        ""
    };
    format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure}")
}

pub fn clear_cookie(state: &AppState, name: &str) -> String {
    set_cookie(state, name, "", 0)
}
