mod common;

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::Redirect;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use common::{
    api, error_code, insert_repo_async, insert_user, mcp_request, member_session, new_state,
    owner_session, request, scalar, send, serve, sso_session, Reply, PUBLIC_URL,
};
use reqwest::Url;
use serde_json::{json, Value};
use sourcelens::crypto::{pkce_challenge, sha256_hex};
use sourcelens::db::put_setting;
use sourcelens::http;
use sourcelens::http::session::{OAUTH_COOKIE, SESSION_COOKIE};
use sourcelens::sso::{resolve_user, Identity, SignIn};
use sourcelens::state::{now, AppState};

const CLIENT_ID: &str = "sl-client";
/// Contains characters that RFC 6749 §2.3.1 form-urlencoding changes.
const CLIENT_SECRET: &str = "s3c%r+t:";
const CLIENT_SECRET_ENCODED: &str = "s3c%25r%2Bt%3A";

// ── Fake identity provider ────────────────────────────────────────────────────

/// What the fake provider answers and what it received at its token endpoint.
#[derive(Default)]
struct Idp {
    base: OnceLock<String>,
    /// Issuer the discovery document reports instead of the root.
    issuer: Mutex<Option<String>>,
    /// `id_token` of the next token response.
    id_token: Mutex<String>,
    /// Form body of the last token request.
    token_form: Mutex<String>,
    /// `Authorization` header of the last token request.
    token_auth: Mutex<Option<String>>,
}

impl Idp {
    fn base(&self) -> &str {
        self.base.get().unwrap()
    }

    /// The value of `name` in the last token request's form.
    fn token_param(&self, name: &str) -> String {
        let form = self.token_form.lock().unwrap().clone();
        let url = Url::parse(&format!("http://form.invalid/?{form}")).unwrap();
        query_param(&url, name)
    }
}

/// Discovery document that names the root (or `Idp::issuer`) as issuer, wherever it is served.
async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let base = idp.base();
    let issuer = idp.issuer.lock().unwrap().clone();
    Json(json!({
        "issuer": issuer.as_deref().unwrap_or(base),
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    }))
}

async fn token(State(idp): State<Arc<Idp>>, headers: HeaderMap, body: String) -> Json<Value> {
    *idp.token_form.lock().unwrap() = body;
    *idp.token_auth.lock().unwrap() = headers
        .get(header::AUTHORIZATION)
        .map(|value| value.to_str().unwrap().to_string());
    Json(json!({
        "access_token": "idp-access-token",
        "token_type": "Bearer",
        "id_token": idp.id_token.lock().unwrap().clone(),
    }))
}

async fn spawn_idp() -> Arc<Idp> {
    let idp = Arc::new(Idp::default());
    let router = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        // Served below the root, so its reported issuer differs from the entered one.
        .route("/other/.well-known/openid-configuration", get(discovery))
        .route("/token", post(token))
        .with_state(idp.clone());
    let base = serve(router).await;
    idp.base.set(base).unwrap();
    idp
}

/// An unsigned JWT with these claims; the signature is not checked.
fn id_token(claims: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(json!({ "alg": "RS256", "typ": "JWT" }).to_string());
    let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("{header}.{payload}.signature")
}

/// Claims of a valid ID token for `sub` with a verified email.
fn claims(idp: &Idp, sub: &str, nonce: &str) -> Value {
    json!({
        "iss": idp.base(),
        "sub": sub,
        "aud": [CLIENT_ID, "other-client"],
        "exp": now() + 300,
        "nonce": nonce,
        "email": "Ada@Example.com",
        "email_verified": true,
        "name": "Ada Lovelace",
    })
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn query_param(url: &Url, name: &str) -> String {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .unwrap_or_else(|| panic!("{url} has no {name}"))
}

/// Value of the cookie `name` set by the response; `None` when absent or cleared.
fn set_cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| {
            value
                .split(';')
                .next()?
                .strip_prefix(&format!("{name}="))
                .map(str::to_string)
        })
        .filter(|value| !value.is_empty())
}

fn location(reply: &Reply) -> &str {
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.text);
    reply.headers[header::LOCATION].to_str().unwrap()
}

fn settings_body(issuer: &str) -> Value {
    json!({
        "enabled": true,
        "name": "Acme login",
        "issuer": issuer,
        "clientId": CLIENT_ID,
        "clientSecret": CLIENT_SECRET,
    })
}

/// An instance with an admin and SSO configured against a fake provider.
struct Instance {
    _tmp: tempfile::TempDir,
    state: AppState,
    app: Router,
    idp: Arc<Idp>,
    owner: String,
}

async fn instance() -> Instance {
    let idp = spawn_idp().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());
    let owner = owner_session(&state, "octocat").await;
    let (status, body, _) = api(
        &app,
        "PUT",
        "/api/settings/sso",
        &owner,
        Some(settings_body(idp.base())),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    Instance {
        _tmp: tmp,
        state,
        app,
        idp,
        owner,
    }
}

/// `GET /auth/sso/login`: the provider URL and the sign-in cookie.
async fn start_sign_in(app: &Router) -> (Url, String) {
    let reply = send(app, request("GET", "/auth/sso/login", None, None)).await;
    let url = Url::parse(location(&reply)).unwrap();
    let cookie = set_cookie_value(&reply.headers, OAUTH_COOKIE).expect("sign-in cookie");
    (url, cookie)
}

async fn finish_sign_in(app: &Router, oauth_state: &str, cookie: &str) -> Reply {
    let req = Request::builder()
        .uri(format!("/auth/sso/callback?code=the-code&state={oauth_state}"))
        .header(header::COOKIE, format!("{OAUTH_COOKIE}={cookie}"))
        .body(Body::empty())
        .unwrap();
    send(app, req).await
}

/// A full sign-in where the provider answers with `claims(nonce)`.
async fn sign_in(inst: &Instance, claims: impl FnOnce(&str) -> Value) -> Reply {
    let (url, cookie) = start_sign_in(&inst.app).await;
    *inst.idp.id_token.lock().unwrap() = id_token(&claims(&query_param(&url, "nonce")));
    finish_sign_in(&inst.app, &query_param(&url, "state"), &cookie).await
}

async fn sso_user_id(state: &AppState, sub: &'static str) -> i64 {
    state
        .db
        .read(move |conn| {
            conn.query_row("SELECT id FROM users WHERE sso_sub = ?1", [sub], |row| {
                row.get(0)
            })
        })
        .await
        .unwrap()
}

async fn user_row(state: &AppState, id: i64) -> (String, Option<String>, String, String) {
    state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT login, email, status, role FROM users WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
        })
        .await
        .unwrap()
}

// ── Settings ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn settings_are_validated_and_discovered() {
    let idp = spawn_idp().await;
    let base = idp.base().to_string();
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());
    let owner = owner_session(&state, "octocat").await;
    let member = member_session(&state, "hubot").await;

    let (status, body, _) = api(&app, "GET", "/api/settings/sso", &owner, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["clientSecretSet"], false);
    assert_eq!(body["redirectUrl"], format!("{PUBLIC_URL}/auth/sso/callback"));

    for (method, input) in [("GET", None), ("PUT", Some(settings_body(&base)))] {
        let (status, body, _) = api(&app, method, "/api/settings/sso", &member, input).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method}");
        assert_eq!(error_code(&body), "owner_required");
    }

    let invalid = [
        json!({ "enabled": true, "name": "", "issuer": "ftp://idp.example", "clientId": CLIENT_ID, "clientSecret": "x" }),
        json!({ "enabled": true, "name": "", "issuer": "http://idp.example", "clientId": CLIENT_ID, "clientSecret": "x" }),
        json!({ "enabled": true, "name": "", "issuer": base, "clientId": " ", "clientSecret": "x" }),
        json!({ "enabled": true, "name": "", "issuer": base, "clientId": CLIENT_ID }),
    ];
    for input in invalid {
        let (status, body, _) = api(&app, "PUT", "/api/settings/sso", &owner, Some(input.clone())).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{input}");
        assert_eq!(error_code(&body), "invalid_sso_settings", "{input}");
    }

    // The provider reports another issuer, has no discovery document there, or cannot be reached.
    let unreachable = "http://127.0.0.1:1".to_string();
    for issuer in [format!("{base}/other"), format!("{base}/missing"), unreachable.clone()] {
        let (status, body, _) =
            api(&app, "PUT", "/api/settings/sso", &owner, Some(settings_body(&issuer))).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{issuer}");
        assert_eq!(error_code(&body), "sso_discovery_failed", "{issuer}");
        if issuer == unreachable {
            assert_eq!(
                body["error"]["message"],
                format!(
                    "Cannot reach {unreachable}/.well-known/openid-configuration. \
                     Check the issuer URL and that this server can reach it."
                )
            );
        }
    }
    let (_, body, _) = call_state(&app).await;
    assert_eq!(body["sso"], Value::Null);

    let (status, body, _) = api(
        &app,
        "PUT",
        "/api/settings/sso",
        &owner,
        Some(settings_body(&format!("{base}/"))),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["name"], "Acme login");
    assert_eq!(body["issuer"], base);
    assert_eq!(body["clientSecretSet"], true);
    assert!(body.get("clientSecret").is_none());
    let (_, body, _) = call_state(&app).await;
    assert_eq!(body["sso"]["name"], "Acme login");

    // Same issuer and client ID without a secret keeps the saved one; the label is cut to 24.
    let renamed = json!({
        "enabled": true,
        "name": "Acme Corporate Single Sign-On (Okta)",
        "issuer": base,
        "clientId": CLIENT_ID,
    });
    let (status, body, _) = api(&app, "PUT", "/api/settings/sso", &owner, Some(renamed)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "Acme Corporate Single Si");
    assert_eq!(body["clientSecretSet"], true);

    // Turning SSO off skips discovery; the changed issuer drops the saved secret.
    let off = json!({ "enabled": false, "name": " ", "issuer": "not a url", "clientId": CLIENT_ID });
    let (status, body, _) = api(&app, "PUT", "/api/settings/sso", &owner, Some(off)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], "SSO");
    assert_eq!(body["clientSecretSet"], false);
    let (_, body, _) = call_state(&app).await;
    assert_eq!(body["sso"], Value::Null);

    let reply = send(&app, request("GET", "/auth/sso/login", None, None)).await;
    assert_eq!(location(&reply), "/login?error=sso");
}

async fn call_state(app: &Router) -> (StatusCode, Value, String) {
    let reply = send(app, request("GET", "/api/state", None, None)).await;
    (reply.status, reply.body, reply.text)
}

// ── Sign-in, approval, roles ──────────────────────────────────────────────────

#[tokio::test]
async fn login_redirects_to_the_provider_with_pkce() {
    let inst = instance().await;
    let (url, cookie) = start_sign_in(&inst.app).await;
    assert!(cookie.len() > 20);
    assert_eq!(
        format!("{}{}", url.origin().ascii_serialization(), url.path()),
        format!("{}/authorize", inst.idp.base())
    );
    assert_eq!(query_param(&url, "response_type"), "code");
    assert_eq!(query_param(&url, "client_id"), CLIENT_ID);
    assert_eq!(
        query_param(&url, "redirect_uri"),
        format!("{PUBLIC_URL}/auth/sso/callback")
    );
    assert_eq!(query_param(&url, "scope"), "openid email profile");
    assert_eq!(query_param(&url, "code_challenge_method"), "S256");
    assert_eq!(query_param(&url, "code_challenge").len(), 43);
    assert_eq!(query_param(&url, "state").len(), 32);
    assert_eq!(query_param(&url, "nonce").len(), 32);

    // A callback whose state does not match the cookie is refused before the provider is asked.
    let reply = finish_sign_in(&inst.app, "forged", &cookie).await;
    assert_eq!(location(&reply), "/login?error=state");
    assert_eq!(*inst.idp.token_form.lock().unwrap(), "");
}

#[tokio::test]
async fn sso_accounts_wait_for_approval_and_can_become_admins() {
    let inst = instance().await;
    let app = &inst.app;

    // First sign-in: a pending account, no session.
    let reply = sign_in(&inst, |nonce| claims(&inst.idp, "user-1", nonce)).await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    assert_eq!(set_cookie_value(&reply.headers, SESSION_COOKIE), None);
    let id = sso_user_id(&inst.state, "user-1").await;
    assert_eq!(
        user_row(&inst.state, id).await,
        (
            "ada@example.com".to_string(),
            Some("Ada@Example.com".to_string()),
            "pending".to_string(),
            "member".to_string()
        )
    );

    // The token request carried the code and HTTP Basic client authentication, with client ID and
    // secret form-urlencoded first (RFC 6749 §2.3.1).
    assert_eq!(inst.idp.token_param("grant_type"), "authorization_code");
    assert_eq!(inst.idp.token_param("code"), "the-code");
    assert_eq!(
        inst.idp.token_param("redirect_uri"),
        format!("{PUBLIC_URL}/auth/sso/callback")
    );
    assert_eq!(
        inst.idp.token_auth.lock().unwrap().as_deref(),
        Some(basic(CLIENT_SECRET_ENCODED).as_str())
    );

    // A wrong nonce fails the sign-in.
    let reply = sign_in(&inst, |_| claims(&inst.idp, "user-1", "another-nonce")).await;
    assert_eq!(location(&reply), "/login?error=sso");
    assert_eq!(set_cookie_value(&reply.headers, SESSION_COOKIE), None);

    // Still pending until an admin approves.
    let reply = sign_in(&inst, |nonce| claims(&inst.idp, "user-1", nonce)).await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    let (status, body, _) = api(
        app,
        "POST",
        &format!("/api/members/{id}/approve"),
        &inst.owner,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let reply = sign_in(&inst, |nonce| claims(&inst.idp, "user-1", nonce)).await;
    assert_eq!(location(&reply), "/repositories");
    let session = set_cookie_value(&reply.headers, SESSION_COOKIE).expect("session cookie");
    assert_eq!(set_cookie_value(&reply.headers, OAUTH_COOKIE), None);

    let (status, body, _) = api(app, "GET", "/api/state", &session, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["user"]["id"], id);
    assert_eq!(body["user"]["login"], "ada@example.com");
    assert_eq!(body["user"]["provider"], "sso");
    assert_eq!(body["user"]["role"], "member");
    assert_eq!(body["user"]["avatarUrl"], Value::Null);

    // An SSO member syncs repositories but has no API keys and no admin rights.
    let (status, body, _) = api(app, "GET", "/api/keys", &session, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "api_keys_unavailable");
    let (status, body, _) = api(app, "POST", "/api/repositories/sync", &session, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = api(app, "GET", "/api/members", &session, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "owner_required");

    // An admin makes the SSO member an admin.
    let (status, body, _) = api(
        app,
        "PATCH",
        &format!("/api/members/{id}"),
        &inst.owner,
        Some(json!({ "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _, _) = api(app, "GET", "/api/members", &session, None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = api(app, "GET", "/api/keys", &session, None).await;
    assert_eq!(status, StatusCode::OK);

    // GitHub accounts never change role, and nobody changes their own.
    let github_member = insert_user(&inst.state, "hubot", "member", Some(77)).await;
    let (status, body, _) = api(
        app,
        "PATCH",
        &format!("/api/members/{github_member}"),
        &session,
        Some(json!({ "role": "owner" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "role_not_changeable");
    let (status, body, _) = api(
        app,
        "PATCH",
        &format!("/api/members/{id}"),
        &session,
        Some(json!({ "role": "member" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(error_code(&body), "cannot_change_own_role");
}

#[tokio::test]
async fn id_token_claims_are_checked() {
    let inst = instance().await;
    let issuer = inst.idp.base().to_string();
    let cases: [(&str, Box<dyn Fn(&mut Value)>); 5] = [
        ("issuer", Box::new(|c: &mut Value| c["iss"] = json!("https://evil.example"))),
        ("audience", Box::new(|c: &mut Value| c["aud"] = json!("other-client"))),
        ("expiry", Box::new(|c: &mut Value| c["exp"] = json!(now() - 1))),
        ("subject", Box::new(|c: &mut Value| c["sub"] = json!(""))),
        ("nonce", Box::new(|c: &mut Value| c["nonce"] = Value::Null)),
    ];
    for (name, change) in cases {
        let reply = sign_in(&inst, |nonce| {
            let mut claims = claims(&inst.idp, "user-x", nonce);
            change(&mut claims);
            claims
        })
        .await;
        assert_eq!(location(&reply), "/login?error=sso", "{name}");
    }
    assert_eq!(
        scalar(&inst.state, "SELECT count(*) FROM users WHERE sso_sub IS NOT NULL").await,
        0
    );

    // An unverified email is ignored: the login falls back to the preferred username.
    let reply = sign_in(&inst, |nonce| {
        json!({
            "iss": format!("{issuer}/"),
            "sub": "user-2",
            "aud": CLIENT_ID,
            "exp": now() + 300,
            "nonce": nonce,
            "email": "grace@example.com",
            "email_verified": "false",
            "preferred_username": "Grace",
        })
    })
    .await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    let id = sso_user_id(&inst.state, "user-2").await;
    let (login, email, status, _) = user_row(&inst.state, id).await;
    assert_eq!((login.as_str(), email, status.as_str()), ("Grace", None, "pending"));
}

#[tokio::test]
async fn pkce_verifier_matches_the_challenge() {
    let inst = instance().await;
    let (url, cookie) = start_sign_in(&inst.app).await;
    let nonce = query_param(&url, "nonce");
    *inst.idp.id_token.lock().unwrap() = id_token(&claims(&inst.idp, "user-3", &nonce));
    let reply = finish_sign_in(&inst.app, &query_param(&url, "state"), &cookie).await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    assert_eq!(
        pkce_challenge(&inst.idp.token_param("code_verifier")),
        query_param(&url, "code_challenge")
    );
}

// ── Changing the provider ─────────────────────────────────────────────────────

/// `Authorization` header of a token request from `CLIENT_ID` with the encoded `secret`.
fn basic(encoded_secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{CLIENT_ID}:{encoded_secret}")))
}

async fn put_settings(inst: &Instance, input: Value) -> (StatusCode, Value) {
    let (status, body, _) = api(&inst.app, "PUT", "/api/settings/sso", &inst.owner, Some(input)).await;
    (status, body)
}

#[tokio::test]
async fn a_new_issuer_or_client_id_needs_the_secret_again() {
    let inst = instance().await;
    let other = spawn_idp().await;

    // The saved secret is never reused for another provider or client.
    for (issuer, client_id) in [(other.base(), CLIENT_ID), (inst.idp.base(), "other-client")] {
        let input = json!({ "enabled": true, "name": "Acme login", "issuer": issuer, "clientId": client_id });
        let (status, body) = put_settings(&inst, input).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{client_id}");
        assert_eq!(error_code(&body), "invalid_sso_settings");
        assert_eq!(
            body["error"]["message"],
            "Enter the client secret again when the issuer or client ID changes."
        );
    }

    // Turned off and pointed elsewhere, the configuration drops the secret, so turning it on
    // again needs one.
    let off = json!({ "enabled": false, "name": "Acme login", "issuer": other.base(), "clientId": CLIENT_ID });
    let (status, body) = put_settings(&inst, off).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["clientSecretSet"], false);
    let on = json!({ "enabled": true, "name": "Acme login", "issuer": other.base(), "clientId": CLIENT_ID });
    let (status, body) = put_settings(&inst, on).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["message"], "Enter the client secret.");
    assert_eq!(
        scalar(&inst.state, "SELECT count(*) FROM settings WHERE key = 'sso_client_secret'").await,
        0
    );
    assert_eq!(*other.token_form.lock().unwrap(), "");
    assert_eq!(*other.token_auth.lock().unwrap(), None);

    // With its own secret, the other provider receives that one.
    let mut input = settings_body(other.base());
    input["clientSecret"] = json!("other-secret");
    let (status, body) = put_settings(&inst, input).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (url, cookie) = start_sign_in(&inst.app).await;
    *other.id_token.lock().unwrap() = id_token(&claims(&other, "user-9", &query_param(&url, "nonce")));
    let reply = finish_sign_in(&inst.app, &query_param(&url, "state"), &cookie).await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    assert_eq!(
        other.token_auth.lock().unwrap().as_deref(),
        Some(basic("other-secret").as_str())
    );
    assert_eq!(*inst.idp.token_auth.lock().unwrap(), None);
}

/// Session of the approved SSO account `sub`.
async fn approved_session(inst: &Instance, sub: &'static str) -> (i64, String) {
    let reply = sign_in(inst, |nonce| claims(&inst.idp, sub, nonce)).await;
    assert_eq!(location(&reply), "/login?error=pending_approval");
    let id = sso_user_id(&inst.state, sub).await;
    let uri = format!("/api/members/{id}/approve");
    let (status, body, _) = api(&inst.app, "POST", &uri, &inst.owner, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reply = sign_in(inst, |nonce| claims(&inst.idp, sub, nonce)).await;
    assert_eq!(location(&reply), "/repositories");
    let session = set_cookie_value(&reply.headers, SESSION_COOKIE).expect("session cookie");
    (id, session)
}

async fn insert_grant(state: &AppState, user_id: i64, tag: &'static str) {
    state
        .db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO oauth_grants (user_id, client_hash, client_name, access_hash,
                     access_expires_at, refresh_hash, refresh_expires_at, created_at)
                 VALUES (?1, 'client', 'Test client', ?2, ?3, ?4, ?3, ?5)",
                rusqlite::params![
                    user_id,
                    format!("access-{tag}"),
                    now() + 3600,
                    format!("refresh-{tag}"),
                    now()
                ],
            )
        })
        .await
        .unwrap();
}

async fn signed_in_user(app: &Router, session: &str) -> Value {
    let (status, body, _) = api(app, "GET", "/api/state", session, None).await;
    assert_eq!(status, StatusCode::OK);
    body["user"].clone()
}

/// Secret of a new API key for all repositories, created by the signed-in `session`.
async fn create_key(app: &Router, session: &str) -> String {
    let input = json!({ "name": "agent", "expiresInDays": null, "repositoryIds": null });
    let (status, created, _) = api(app, "POST", "/api/keys", session, Some(input)).await;
    assert!(status.is_success(), "{created}");
    created["secret"].as_str().unwrap().to_string()
}

/// Status of an MCP `ping` with the API key `secret`.
async fn ping(app: &Router, secret: &str) -> StatusCode {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    send(app, mcp_request(Some(secret)).body(Body::from(body)).unwrap())
        .await
        .status
}

#[tokio::test]
async fn turning_sso_off_or_changing_the_issuer_ends_sso_access() {
    const SSO_GRANTS: &str =
        "SELECT count(*) FROM oauth_grants WHERE user_id IN (SELECT id FROM users WHERE sso_sub IS NOT NULL)";
    let inst = instance().await;
    let app = &inst.app;
    insert_repo_async(&inst.state, 1, "acme/api", "connected").await;
    let owner_id = scalar(&inst.state, "SELECT id FROM users WHERE login = 'octocat'").await;
    insert_grant(&inst.state, owner_id, "github").await;
    let github_key = create_key(app, &inst.owner).await;
    let (id, session) = approved_session(&inst, "user-5").await;
    insert_grant(&inst.state, id, "sso-1").await;
    assert_eq!(signed_in_user(app, &session).await["id"], id);
    let sso_admin = sso_session(&inst.state, "kim", "owner").await;
    let sso_key = create_key(app, &sso_admin).await;
    assert_eq!(ping(app, &sso_key).await, StatusCode::OK);

    // Turning SSO off ends sessions, grants and API keys of SSO accounts only.
    let off = json!({ "enabled": false, "name": "Acme login", "issuer": inst.idp.base(), "clientId": CLIENT_ID });
    let (status, body) = put_settings(&inst, off).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(signed_in_user(app, &session).await, Value::Null);
    assert_eq!(signed_in_user(app, &inst.owner).await["id"], owner_id);
    assert_eq!(scalar(&inst.state, SSO_GRANTS).await, 0);
    assert_eq!(scalar(&inst.state, "SELECT count(*) FROM oauth_grants").await, 1);
    assert_eq!(ping(app, &sso_key).await, StatusCode::UNAUTHORIZED);
    assert_eq!(ping(app, &github_key).await, StatusCode::OK);
    assert_eq!(scalar(&inst.state, "SELECT count(*) FROM api_keys").await, 1);

    // Turned on again with the saved secret, the approved account signs in again.
    let on = json!({ "enabled": true, "name": "Acme login", "issuer": inst.idp.base(), "clientId": CLIENT_ID });
    let (status, body) = put_settings(&inst, on).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reply = sign_in(&inst, |nonce| claims(&inst.idp, "user-5", nonce)).await;
    assert_eq!(location(&reply), "/repositories");
    let session = set_cookie_value(&reply.headers, SESSION_COOKIE).expect("session cookie");
    insert_grant(&inst.state, id, "sso-2").await;
    let sso_admin = sso_session(&inst.state, "lee", "owner").await;
    let sso_key = create_key(app, &sso_admin).await;
    assert_eq!(ping(app, &sso_key).await, StatusCode::OK);

    // Another issuer, still enabled, ends them as well.
    let other = spawn_idp().await;
    let (status, body) = put_settings(&inst, settings_body(other.base())).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(signed_in_user(app, &session).await, Value::Null);
    assert_eq!(signed_in_user(app, &inst.owner).await["id"], owner_id);
    assert_eq!(scalar(&inst.state, SSO_GRANTS).await, 0);
    assert_eq!(scalar(&inst.state, "SELECT count(*) FROM oauth_grants").await, 1);
    assert_eq!(ping(app, &sso_key).await, StatusCode::UNAUTHORIZED);
    assert_eq!(ping(app, &github_key).await, StatusCode::OK);
    assert_eq!(scalar(&inst.state, "SELECT count(*) FROM api_keys").await, 1);
}

#[tokio::test]
async fn unreadable_settings_can_be_saved_again() {
    let idp = spawn_idp().await;
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());
    let owner = owner_session(&state, "octocat").await;
    state
        .db
        .write(|conn| put_setting(conn, "sso", "not json"))
        .await
        .unwrap();

    let (status, body, _) = api(&app, "GET", "/api/settings/sso", &owner, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
    // Sign-in stays closed while the settings are unreadable.
    let reply = send(&app, request("GET", "/auth/sso/login", None, None)).await;
    assert_eq!(location(&reply), "/login?error=sso");

    let (status, body, _) =
        api(&app, "PUT", "/api/settings/sso", &owner, Some(settings_body(idp.base()))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body, _) = api(&app, "GET", "/api/settings/sso", &owner, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
    assert_eq!(body["name"], "Acme login");
    assert_eq!(body["issuer"], idp.base());
    assert_eq!(body["clientId"], CLIENT_ID);
    assert_eq!(body["clientSecretSet"], true);
}

#[tokio::test]
async fn discovery_does_not_follow_redirects() {
    let idp = spawn_idp().await;
    let target = format!("{}/.well-known/openid-configuration", idp.base());
    let redirector = serve(Router::new().route(
        "/.well-known/openid-configuration",
        get(move || {
            let target = target.clone();
            async move { Redirect::temporary(&target) }
        }),
    ))
    .await;
    // Followed, the redirect would succeed: the provider reports the redirecting URL as issuer.
    *idp.issuer.lock().unwrap() = Some(redirector.clone());
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = http::router(state.clone());
    let owner = owner_session(&state, "octocat").await;
    let (status, body, _) =
        api(&app, "PUT", "/api/settings/sso", &owner, Some(settings_body(&redirector))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(error_code(&body), "sso_discovery_failed");
}

// ── Account rules ─────────────────────────────────────────────────────────────

fn identity(login: &str, sub: &str) -> Identity {
    Identity {
        issuer: "https://idp.example".into(),
        sub: sub.into(),
        login: login.into(),
        name: Some("Somebody".into()),
        email: None,
    }
}

#[tokio::test]
async fn sign_in_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});

    // Nobody has set up the instance: no account is created.
    let result = resolve_user(&state.db, &identity("octocat", "s-1")).await.unwrap();
    assert_eq!(result, SignIn::SetupRequired);
    assert_eq!(scalar(&state, "SELECT count(*) FROM users").await, 0);

    // A login held by a GitHub account gets a suffix from issuer and subject.
    owner_session(&state, "octocat").await;
    let result = resolve_user(&state.db, &identity("octocat", "s-1")).await.unwrap();
    assert_eq!(result, SignIn::Pending);
    let id = sso_user_id(&state, "s-1").await;
    let suffix = &sha256_hex(b"https://idp.example|s-1")[..8];
    assert_eq!(user_row(&state, id).await.0, format!("octocat~{suffix}"));

    // Signing in again keeps the account pending and its login stable.
    let result = resolve_user(&state.db, &identity("octocat", "s-1")).await.unwrap();
    assert_eq!(result, SignIn::Pending);
    assert_eq!(user_row(&state, id).await.0, format!("octocat~{suffix}"));
    assert_eq!(scalar(&state, "SELECT count(*) FROM users").await, 2);

    // Once approved, the account signs in and records the time.
    state
        .db
        .write(move |conn| conn.execute("UPDATE users SET status = 'active' WHERE id = ?1", [id]))
        .await
        .unwrap();
    let result = resolve_user(&state.db, &identity("ada", "s-1")).await.unwrap();
    assert_eq!(result, SignIn::User(id));
    assert_eq!(user_row(&state, id).await.0, "ada");
    assert_eq!(
        scalar(&state, "SELECT count(*) FROM users WHERE last_login_at IS NOT NULL AND sso_sub = 's-1'").await,
        1
    );
}
