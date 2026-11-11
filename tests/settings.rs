mod common;

use axum::http::StatusCode;
use common::{api, credentials, member_session, new_state, owner_session};
use serde_json::Value;
use tempfile::TempDir;

// ── HTTP: /api/settings/github ─────────────────────────────────────────────

#[tokio::test]
async fn github_settings_need_credentials() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;
    let (status, body, _) = api(&app, "GET", "/api/settings/github", &owner, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "app_not_configured");
}

#[tokio::test]
async fn github_settings_with_env_credentials() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |cfg| cfg.github = Some(credentials()));
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;

    let (status, body, _) = api(&app, "GET", "/api/settings/github", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["source"], "env");
    assert_eq!(body["slug"], "sourcelens-test");
    assert_eq!(body["appUrl"], "https://github.com/apps/sourcelens-test");
    assert_eq!(
        body["installUrl"],
        "https://github.com/apps/sourcelens-test/installations/new"
    );
    assert_eq!(body["webhookUrl"], "http://localhost:3000/webhooks/github");
    assert_eq!(body["webhookActive"], false);
    assert_eq!(body["lastDeliveryAt"], Value::Null);
    assert_eq!(body["lastDeliveryEvent"], Value::Null);

    state
        .db
        .write(|c| {
            c.execute_batch(
                "INSERT INTO webhook_deliveries (delivery_id, event, received_at) VALUES ('a', 'push', 100), ('b', 'repository', 200)",
            )
        })
        .await
        .unwrap();
    let (_, body, _) = api(&app, "GET", "/api/settings/github", &member, None).await;
    assert_eq!(body["lastDeliveryAt"], 200);
    assert_eq!(body["lastDeliveryEvent"], "repository");
}
