use axum::extract::{Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::crypto::{random_hex, sha256_hex};
use crate::error::ApiResult;
use crate::github::oauth::{self, Resolution};
use crate::http::session::{self, clear_cookie, cookie, oauth_cookie, session_cookie, set_cookie};
use crate::http::setup::setup_token_valid;
use crate::state::AppState;

/// Bytes of randomness in the OAuth `state` parameter.
const OAUTH_STATE_BYTES: usize = 16;
const OAUTH_COOKIE_MAX_AGE: i64 = 600;

/// `GET /auth/github/login`, `GET /auth/github/callback`, `POST /api/logout`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/github/login", get(login))
        .route("/auth/github/callback", get(callback))
        .route("/api/logout", post(logout))
}

/// Sealed into the OAuth cookie between login and callback.
#[derive(Serialize, Deserialize)]
struct OAuthCookie {
    state: String,
    claim: bool,
}

#[derive(Deserialize)]
struct LoginQuery {
    setup_token: Option<String>,
}

async fn login(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    match start_login(&state, query.setup_token.as_deref()).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!("cannot start GitHub sign-in: {err}");
            Redirect::to("/login?error=github").into_response()
        }
    }
}

async fn start_login(state: &AppState, setup_token: Option<&str>) -> ApiResult<Response> {
    let Some(creds) = state.github.credentials().await? else {
        return Ok(Redirect::to("/setup").into_response());
    };
    let claim = match setup_token {
        Some(token) => setup_token_valid(state, token).await?,
        None => false,
    };
    let oauth_state = random_hex(OAUTH_STATE_BYTES);
    let payload = serde_json::to_vec(&OAuthCookie {
        state: oauth_state.clone(),
        claim,
    })
    .map_err(crate::error::ApiError::internal)?;
    let value = URL_SAFE_NO_PAD.encode(state.key.seal(&payload));
    Ok((
        AppendHeaders([(
            SET_COOKIE,
            set_cookie(
                state,
                oauth_cookie(&state.cfg),
                &value,
                OAUTH_COOKIE_MAX_AGE,
            ),
        )]),
        Redirect::to(&oauth::authorize_url(state, &creds, &oauth_state)),
    )
        .into_response())
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let clear_oauth = (SET_COOKIE, clear_cookie(&state, oauth_cookie(&state.cfg)));
    let saved = cookie(&headers, oauth_cookie(&state.cfg))
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|sealed| state.key.open(&sealed))
        .and_then(|plain| serde_json::from_slice::<OAuthCookie>(&plain).ok());
    let (Some(saved), Some(code), Some(returned_state)) = (saved, query.code, query.state) else {
        return (
            AppendHeaders([clear_oauth]),
            Redirect::to("/login?error=state"),
        )
            .into_response();
    };
    if returned_state != saved.state {
        return (
            AppendHeaders([clear_oauth]),
            Redirect::to("/login?error=state"),
        )
            .into_response();
    }

    match finish_login(&state, &code, saved.claim).await {
        Ok(Ok(session_value)) => (
            AppendHeaders([
                clear_oauth,
                (
                    SET_COOKIE,
                    set_cookie(
                        &state,
                        session_cookie(&state.cfg),
                        &session_value,
                        session::SESSION_MAX_AGE,
                    ),
                ),
            ]),
            Redirect::to("/repositories"),
        )
            .into_response(),
        Ok(Err(target)) => (AppendHeaders([clear_oauth]), Redirect::to(target)).into_response(),
        Err(err) => {
            tracing::error!("GitHub sign-in failed: {err}");
            (
                AppendHeaders([clear_oauth]),
                Redirect::to("/login?error=github"),
            )
                .into_response()
        }
    }
}

/// `Ok(Ok(cookie value))` on success, `Ok(Err(redirect target))` when the user may not sign in.
async fn finish_login(
    state: &AppState,
    code: &str,
    claim: bool,
) -> ApiResult<Result<String, &'static str>> {
    let creds = state
        .github
        .credentials()
        .await?
        .ok_or_else(crate::github::app::app_not_configured)?;
    let token = oauth::exchange_code(state, &creds, code).await?;
    let user = oauth::fetch_user(state, &token.access_token).await?;
    match oauth::resolve_user(&state.db, &user, claim).await? {
        Resolution::User(id) => Ok(Ok(session::create_session(state, id, &token).await?)),
        Resolution::NotInvited => Ok(Err("/login?error=not_invited")),
        Resolution::SetupRequired => Ok(Err("/login?error=setup_required")),
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(value) = cookie(&headers, session_cookie(&state.cfg)) {
        session::delete_session(&state, &sha256_hex(value.as_bytes())).await?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        AppendHeaders([(SET_COOKIE, clear_cookie(&state, session_cookie(&state.cfg)))]),
    )
        .into_response())
}
