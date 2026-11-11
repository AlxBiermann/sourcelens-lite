use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::crypto::{random_hex, sha256_hex};
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::github::app::{AppCredentials, GitHub};
use crate::http::members::valid_github_name;
use crate::http::session::current_user;
use crate::http::Body;
use crate::model::Role;
use crate::state::{lock, AppState};

/// `GET /api/state`, `POST /api/setup/manifest`, `GET /auth/github/app-created`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/setup/manifest", post(create_manifest))
        .route("/auth/github/app-created", get(app_created))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateResponse {
    version: &'static str,
    public_url: String,
    setup: SetupState,
    user: Option<StateUser>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupState {
    app_configured: bool,
    owner_exists: bool,
    app_slug: Option<String>,
    install_url: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StateUser {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    role: Role,
}

async fn get_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<StateResponse>> {
    let creds = state.github.credentials().await?;
    let owner_exists = owner_exists(&state).await?;
    let user = match current_user(&headers, &state).await? {
        Some(user) => {
            let id = user.id;
            state
                .db
                .read(move |conn| {
                    conn.query_row(
                        "SELECT id, login, name, avatar_url, role FROM users WHERE id = ?1",
                        [id],
                        |row| {
                            Ok(StateUser {
                                id: row.get(0)?,
                                login: row.get(1)?,
                                name: row.get(2)?,
                                avatar_url: row.get(3)?,
                                role: row.get(4)?,
                            })
                        },
                    )
                })
                .await
                .map(Some)?
        }
        None => None,
    };
    Ok(Json(StateResponse {
        version: env!("CARGO_PKG_VERSION"),
        public_url: state.cfg.public_url.clone(),
        setup: SetupState {
            app_configured: creds.is_some(),
            owner_exists,
            install_url: creds.as_ref().map(|c| install_url(&c.slug)),
            app_slug: creds.map(|c| c.slug),
        },
        user,
    }))
}

pub fn install_url(slug: &str) -> String {
    format!("https://github.com/apps/{slug}/installations/new")
}

pub async fn owner_exists(state: &AppState) -> ApiResult<bool> {
    Ok(state.db.read(db::owner_exists).await?)
}

/// True when `token` is this process's setup token and no owner exists yet.
pub async fn setup_token_valid(state: &AppState, token: &str) -> ApiResult<bool> {
    let matches = sha256_hex(token.trim().as_bytes()) == sha256_hex(state.setup_token.as_bytes());
    Ok(matches && !owner_exists(state).await?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestRequest {
    setup_token: String,
    organization: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestResponse {
    post_url: String,
    manifest: Value,
}

async fn create_manifest(
    State(state): State<AppState>,
    Body(req): Body<ManifestRequest>,
) -> ApiResult<Json<ManifestResponse>> {
    if !setup_token_valid(&state, &req.setup_token).await? {
        return Err(ApiError::forbidden(
            "invalid_setup_token",
            "The setup token is wrong or setup is already finished. Copy the token from the server log.",
        ));
    }
    if state.github.credentials().await?.is_some() {
        return Err(ApiError::conflict(
            "app_already_configured",
            "A GitHub App is already configured.",
        ));
    }
    let organization = req
        .organization
        .map(|org| org.trim().to_string())
        .filter(|org| !org.is_empty());
    if let Some(org) = &organization {
        if !valid_github_name(org) {
            return Err(ApiError::unprocessable(
                "invalid_organization",
                "Enter a valid GitHub organization name.",
            ));
        }
    }

    let manifest_state = random_hex(16);
    *lock(&state.manifest_state) = Some(manifest_state.clone());

    let post_url = match &organization {
        Some(org) => format!(
            "https://github.com/organizations/{org}/settings/apps/new?state={manifest_state}"
        ),
        None => format!("https://github.com/settings/apps/new?state={manifest_state}"),
    };
    let public = &state.cfg.public_url;
    let manifest = json!({
        "name": format!("SourceLens {}", random_hex(2)),
        "url": public,
        "hook_attributes": {
            "url": state.cfg.webhook_url(),
            "active": !state.cfg.public_host_is_local(),
        },
        "redirect_url": format!("{public}/auth/github/app-created"),
        "callback_urls": [state.cfg.callback_url()],
        "setup_url": format!("{public}/repositories?installed=1"),
        "setup_on_update": true,
        "public": false,
        "request_oauth_on_install": false,
        "default_permissions": { "contents": "read", "metadata": "read" },
        "default_events": ["push", "repository"],
    });
    Ok(Json(ManifestResponse { post_url, manifest }))
}

#[derive(Deserialize)]
struct AppCreatedQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn app_created(
    State(state): State<AppState>,
    Query(query): Query<AppCreatedQuery>,
) -> Redirect {
    let expected = lock(&state.manifest_state).take();
    let state_ok = matches!((&expected, &query.state), (Some(e), Some(s)) if e == s);
    if !state_ok {
        return Redirect::to("/setup?error=state");
    }
    let Some(code) = query.code.filter(|c| {
        !c.is_empty()
            && c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    }) else {
        return Redirect::to("/setup?error=conversion");
    };
    match convert_manifest(&state, &code).await {
        Ok(()) => Redirect::to("/setup?step=install"),
        Err(err) => {
            tracing::error!("GitHub App manifest conversion failed: {err}");
            Redirect::to("/setup?error=conversion")
        }
    }
}

async fn convert_manifest(state: &AppState, code: &str) -> ApiResult<()> {
    #[derive(Deserialize)]
    struct Conversion {
        id: u64,
        slug: String,
        client_id: String,
        client_secret: String,
        webhook_secret: Option<String>,
        pem: String,
        html_url: String,
    }
    let url = format!(
        "{}/app-manifests/{code}/conversions",
        state.github.api_base()
    );
    let response = state.github.send(|| state.http.post(&url)).await?;
    let conversion: Conversion = GitHub::json(response, &url).await?;
    state
        .github
        .store_manifest_credentials(&AppCredentials {
            app_id: conversion.id,
            slug: conversion.slug,
            client_id: conversion.client_id,
            client_secret: conversion.client_secret,
            webhook_secret: conversion.webhook_secret.filter(|s| !s.is_empty()),
            private_key_pem: conversion.pem,
            html_url: conversion.html_url,
            source: "manifest",
        })
        .await
}
