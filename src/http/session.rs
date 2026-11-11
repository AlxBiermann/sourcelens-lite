use axum::extract::FromRequestParts;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::http::HeaderMap;
use rusqlite::{params, OptionalExtension};

use crate::crypto::{random_hex, sha256_hex};
use crate::error::{ApiError, ApiResult};
use crate::github::oauth::GhToken;
use crate::state::{now, AppState};

pub const SESSION_COOKIE: &str = "sl_session";
pub const SESSION_MAX_AGE: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: i64,
    pub login: String,
    pub role: String,
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
        if user.role != "owner" {
            return Err(ApiError::forbidden(
                "owner_required",
                "Only the owner can do this.",
            ));
        }
        Ok(Owner(user))
    }
}

/// The user of a valid, unexpired session cookie, if any.
pub async fn current_user(headers: &HeaderMap, state: &AppState) -> ApiResult<Option<AuthUser>> {
    let Some(value) = cookie(headers, SESSION_COOKIE) else {
        return Ok(None);
    };
    let hash = sha256_hex(value.as_bytes());
    let ts = now();
    let user = state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT u.id, u.login, u.role FROM sessions s JOIN users u ON u.id = s.user_id
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
    let value = random_hex(32);
    let hash = sha256_hex(value.as_bytes());
    let sealed = state
        .key
        .seal(&serde_json::to_vec(token).map_err(ApiError::internal)?);
    let ts = now();
    state
        .db
        .write(move |conn| {
            conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [ts])?;
            conn.execute(
                "INSERT INTO sessions (id_hash, user_id, gh_token, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![hash, user_id, sealed, ts, ts + SESSION_MAX_AGE],
            )
        })
        .await?;
    Ok(value)
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
