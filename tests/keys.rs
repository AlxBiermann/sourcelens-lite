mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::{api, insert_repo_async, mcp_request, member_session, new_state, send};
use serde_json::json;
use sourcelens::crypto;
use sourcelens::http::keys::{self, NewKey};
use sourcelens::mcp::authenticate;
use sourcelens::state::now;

#[tokio::test]
async fn create_key_validates_input() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;

    let blank = NewKey {
        name: "  ".into(),
        expires_in_days: None,
        repository_ids: None,
    };
    assert_eq!(
        keys::create_key(&state, blank, None)
            .await
            .unwrap_err()
            .code,
        "name_required"
    );
    let empty = NewKey {
        name: "ci".into(),
        expires_in_days: None,
        repository_ids: Some(vec![]),
    };
    assert_eq!(
        keys::create_key(&state, empty, None)
            .await
            .unwrap_err()
            .code,
        "no_repositories"
    );
    let odd_expiry = NewKey {
        name: "ci".into(),
        expires_in_days: Some(7),
        repository_ids: None,
    };
    assert_eq!(
        keys::create_key(&state, odd_expiry, None)
            .await
            .unwrap_err()
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let input = NewKey {
        name: "ci".into(),
        expires_in_days: Some(90),
        repository_ids: Some(vec![repo]),
    };
    let (key, secret) = keys::create_key(&state, input, None).await.unwrap();
    assert!(secret.starts_with("sl_"));
    assert_eq!(secret.len(), 67);
    assert_eq!(key.last4, secret[63..]);
    assert!(!key.all_repositories);
    assert_eq!(key.repository_ids, vec![repo]);
    let expires_at = key.expires_at.unwrap();
    assert!((expires_at - (now() + 90 * 86_400)).abs() < 60);

    let stored: String = state
        .db
        .read(|c| c.query_row("SELECT key_hash FROM api_keys", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(stored, crypto::sha256_hex(secret.as_bytes()));
}

#[tokio::test]
async fn keys_api_through_the_router() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let r1 = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let r2 = insert_repo_async(&state, 2, "acme/orders", "connected").await;
    let cookie = member_session(&state, "alex").await;
    let app = &sourcelens::http::router(state.clone());

    let (status, body, _) = api(
        app,
        "POST",
        "/api/keys",
        &cookie,
        Some(json!({ "name": " ", "expiresInDays": null, "repositoryIds": null })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "name_required");
    let (status, body, _) = api(
        app,
        "POST",
        "/api/keys",
        &cookie,
        Some(json!({ "name": "ci", "expiresInDays": null, "repositoryIds": [] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "no_repositories");

    let (status, created, _) = api(
        app,
        "POST",
        "/api/keys",
        &cookie,
        Some(json!({ "name": "ci", "expiresInDays": 30, "repositoryIds": [r1] })),
    )
    .await;
    assert!(status.is_success(), "{created}");
    let secret = created["secret"].as_str().unwrap().to_string();
    let id = created["key"]["id"].as_i64().unwrap();
    assert_eq!(created["key"]["createdBy"], "alex");
    assert_eq!(created["key"]["allRepositories"], false);
    assert_eq!(created["key"]["repositoryIds"], json!([r1]));

    let (status, listed, raw) = api(app, "GET", "/api/keys", &cookie, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert!(
        !raw.contains(&secret),
        "the secret must only appear in the create response"
    );
    assert!(!raw.contains(&secret[3..]));

    assert_eq!(
        authenticate(&state, &secret).await.unwrap().repo_ids,
        vec![r1]
    );

    let (status, patched, _) = api(
        app,
        "PATCH",
        &format!("/api/keys/{id}"),
        &cookie,
        Some(json!({ "repositoryIds": [r2] })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(patched["repositoryIds"], json!([r2]));
    assert_eq!(
        authenticate(&state, &secret).await.unwrap().repo_ids,
        vec![r2]
    );

    let (_, patched, _) = api(
        app,
        "PATCH",
        &format!("/api/keys/{id}"),
        &cookie,
        Some(json!({ "name": "renamed" })),
    )
    .await;
    assert_eq!(patched["name"], "renamed");
    assert_eq!(patched["repositoryIds"], json!([r2]));

    let (_, patched, _) = api(
        app,
        "PATCH",
        &format!("/api/keys/{id}"),
        &cookie,
        Some(json!({ "repositoryIds": null })),
    )
    .await;
    assert_eq!(patched["allRepositories"], true);
    assert_eq!(
        authenticate(&state, &secret).await.unwrap().repo_ids,
        vec![r1, r2]
    );

    let (status, _, raw) = api(app, "DELETE", &format!("/api/keys/{id}"), &cookie, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(raw.is_empty());
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let request = mcp_request(Some(&secret)).body(Body::from(ping)).unwrap();
    assert_eq!(send(app, request).await.status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = api(app, "DELETE", &format!("/api/keys/{id}"), &cookie, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
