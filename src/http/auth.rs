use axum::extract::{Query, State};
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::crypto::{pkce_challenge, random_hex, sha256_hex};
use crate::error::{ApiError, ApiResult};
use crate::github::oauth::{self, Resolution};
use crate::http::session::{self, clear_cookie, cookie, oauth_cookie, session_cookie, set_cookie};
use crate::http::setup::setup_token_valid;
use crate::sso::{self, SignIn};
use crate::state::AppState;

/// Bytes of randomness in the OAuth `state` parameter.
const OAUTH_STATE_BYTES: usize = 16;
/// Bytes of randomness in the OpenID Connect `nonce`.
const SSO_NONCE_BYTES: usize = 16;
/// Bytes of randomness in the PKCE code verifier (64 hex characters, within RFC 7636's 43–128).
const PKCE_VERIFIER_BYTES: usize = 32;
const OAUTH_COOKIE_MAX_AGE: i64 = 600;
/// Where a successful sign-in lands unless it continues to the OAuth consent page.
pub(crate) const HOME: &str = "/repositories";
/// The only place a sign-in may continue to: the consent page of an MCP client's request.
const NEXT_PREFIX: &str = "/oauth/authorize?";

/// `GET /auth/github/login`, `GET /auth/github/callback`, `GET /auth/sso/login`,
/// `GET /auth/sso/callback`, `POST /api/logout`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/github/login", get(login))
        .route("/auth/github/callback", get(callback))
        .route("/auth/sso/login", get(sso_login))
        .route("/auth/sso/callback", get(sso_callback))
        .route("/api/logout", post(logout))
}

/// Sealed into the OAuth cookie between GitHub login and callback.
#[derive(Serialize, Deserialize)]
struct OAuthCookie {
    state: String,
    claim: bool,
    /// Where to go after signing in; `None` → [`HOME`].
    next: Option<String>,
}

/// Sealed into the OAuth cookie between SSO login and callback.
#[derive(Serialize, Deserialize)]
struct SsoCookie {
    state: String,
    nonce: String,
    verifier: String,
    /// Where to go after signing in; `None` → [`HOME`].
    next: Option<String>,
}

/// Redirects to `target` and stores `payload`, sealed, in the sign-in cookie.
fn redirect_with_cookie<T: Serialize>(
    state: &AppState,
    payload: &T,
    target: &str,
) -> ApiResult<Response> {
    let json = serde_json::to_vec(payload).map_err(ApiError::internal)?;
    let value = URL_SAFE_NO_PAD.encode(state.key.seal(&json));
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
        Redirect::to(target),
    )
        .into_response())
}

/// The sign-in cookie's payload when it is present, unaltered and of type `T`.
fn open_cookie<T: DeserializeOwned>(state: &AppState, headers: &HeaderMap) -> Option<T> {
    cookie(headers, oauth_cookie(&state.cfg))
        .and_then(|value| URL_SAFE_NO_PAD.decode(value).ok())
        .and_then(|sealed| state.key.open(&sealed))
        .and_then(|plain| serde_json::from_slice(&plain).ok())
}

/// `next` when it returns to the consent page. It ends up in a `Location` header, so only
/// printable ASCII is kept; the UI sends it percent-encoded.
pub(crate) fn safe_next(next: Option<String>) -> Option<String> {
    next.filter(|next| {
        next.starts_with(NEXT_PREFIX) && next.bytes().all(|byte| byte.is_ascii_graphic())
    })
}

#[derive(Deserialize)]
struct LoginQuery {
    setup_token: Option<String>,
    next: Option<String>,
}

async fn login(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    let next = safe_next(query.next);
    match start_login(&state, query.setup_token.as_deref(), next).await {
        Ok(response) => response,
        Err(err) => {
            tracing::error!("cannot start GitHub sign-in: {err}");
            Redirect::to("/login?error=github").into_response()
        }
    }
}

async fn start_login(
    state: &AppState,
    setup_token: Option<&str>,
    next: Option<String>,
) -> ApiResult<Response> {
    let Some(creds) = state.github.credentials().await? else {
        return Ok(Redirect::to("/setup").into_response());
    };
    let claim = match setup_token {
        Some(token) => setup_token_valid(state, token).await?,
        None => false,
    };
    let oauth_state = random_hex(OAUTH_STATE_BYTES);
    let target = oauth::authorize_url(state, &creds, &oauth_state);
    redirect_with_cookie(
        state,
        &OAuthCookie {
            state: oauth_state,
            claim,
            next,
        },
        &target,
    )
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// The saved cookie and the code when the callback's `state` matches the saved one.
fn checked_callback<T: DeserializeOwned>(
    state: &AppState,
    headers: &HeaderMap,
    query: CallbackQuery,
    saved_state: impl Fn(&T) -> &str,
) -> Option<(T, String)> {
    let saved = open_cookie::<T>(state, headers)?;
    let (code, returned_state) = (query.code?, query.state?);
    (returned_state == saved_state(&saved)).then_some((saved, code))
}

/// `Set-Cookie` header that starts the session with this cookie value.
pub(crate) fn session_header(state: &AppState, session_value: &str) -> (HeaderName, String) {
    (
        SET_COOKIE,
        set_cookie(
            state,
            session_cookie(&state.cfg),
            session_value,
            session::SESSION_MAX_AGE,
        ),
    )
}

/// Ends a sign-in callback: clears the sign-in cookie, then sets the session cookie and goes to
/// `target`, goes to the refusal redirect, or logs the error and goes to `failure`.
fn finish_callback(
    state: &AppState,
    outcome: ApiResult<Result<String, &'static str>>,
    target: &str,
    failure: &'static str,
    provider: &str,
) -> Response {
    let clear_oauth = (SET_COOKIE, clear_cookie(state, oauth_cookie(&state.cfg)));
    match outcome {
        Ok(Ok(session_value)) => (
            AppendHeaders([clear_oauth, session_header(state, &session_value)]),
            Redirect::to(target),
        )
            .into_response(),
        Ok(Err(refusal)) => (AppendHeaders([clear_oauth]), Redirect::to(refusal)).into_response(),
        Err(err) => {
            tracing::error!("{provider} sign-in failed: {err}");
            (AppendHeaders([clear_oauth]), Redirect::to(failure)).into_response()
        }
    }
}

fn state_mismatch(state: &AppState) -> Response {
    (
        AppendHeaders([(SET_COOKIE, clear_cookie(state, oauth_cookie(&state.cfg)))]),
        Redirect::to("/login?error=state"),
    )
        .into_response()
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some((saved, code)) =
        checked_callback::<OAuthCookie>(&state, &headers, query, |saved| saved.state.as_str())
    else {
        return state_mismatch(&state);
    };
    let outcome = finish_login(&state, &code, saved.claim).await;
    let target = saved.next.as_deref().unwrap_or(HOME);
    finish_callback(&state, outcome, target, "/login?error=github", "GitHub")
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
        Resolution::User(id) => Ok(Ok(session::create_session(state, id, Some(&token)).await?)),
        Resolution::NotInvited => Ok(Err("/login?error=not_invited")),
        Resolution::SetupRequired => Ok(Err("/login?error=setup_required")),
    }
}

#[derive(Deserialize)]
struct SsoLoginQuery {
    next: Option<String>,
}

async fn sso_login(State(state): State<AppState>, Query(query): Query<SsoLoginQuery>) -> Response {
    match start_sso_login(&state, safe_next(query.next)).await {
        Ok(Some(response)) => response,
        Ok(None) => Redirect::to("/login?error=sso").into_response(),
        Err(err) => {
            tracing::error!("cannot start SSO sign-in: {err}");
            Redirect::to("/login?error=sso").into_response()
        }
    }
}

/// `None` when SSO is off.
async fn start_sso_login(state: &AppState, next: Option<String>) -> ApiResult<Option<Response>> {
    let saved = SsoCookie {
        state: random_hex(OAUTH_STATE_BYTES),
        nonce: random_hex(SSO_NONCE_BYTES),
        verifier: random_hex(PKCE_VERIFIER_BYTES),
        next,
    };
    let challenge = pkce_challenge(&saved.verifier);
    let Some(target) = sso::authorize_url(state, &saved.state, &saved.nonce, &challenge).await?
    else {
        return Ok(None);
    };
    redirect_with_cookie(state, &saved, &target).map(Some)
}

async fn sso_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some((saved, code)) =
        checked_callback::<SsoCookie>(&state, &headers, query, |saved| saved.state.as_str())
    else {
        return state_mismatch(&state);
    };
    let outcome = finish_sso_login(&state, &code, &saved).await;
    let target = saved.next.as_deref().unwrap_or(HOME);
    finish_callback(&state, outcome, target, "/login?error=sso", "SSO")
}

/// `Ok(Ok(cookie value))` on success, `Ok(Err(redirect target))` when the user may not sign in.
async fn finish_sso_login(
    state: &AppState,
    code: &str,
    saved: &SsoCookie,
) -> ApiResult<Result<String, &'static str>> {
    let identity = sso::exchange(state, code, &saved.verifier, &saved.nonce).await?;
    match sso::resolve_user(&state.db, &identity).await? {
        SignIn::User(id) => Ok(Ok(session::create_session(state, id, None).await?)),
        SignIn::Pending => Ok(Err("/login?error=pending_approval")),
        SignIn::SetupRequired => Ok(Err("/login?error=setup_required")),
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
