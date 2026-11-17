mod common;

use axum::http::StatusCode;
use common::{api, credentials, error_code, member_session, new_state, owner_session};
use serde_json::{json, Value};
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

// ── HTTP: /api/settings/sync ───────────────────────────────────────────────

#[tokio::test]
async fn sync_settings_default_to_off() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;

    let (status, body, _) = api(&app, "GET", "/api/settings/sync", &member, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], false);
    assert_eq!(body["cron"], "0 * * * *");
    assert!(body["description"].is_string());
    assert_eq!(body["nextRunAt"], Value::Null);
}

#[tokio::test]
async fn sync_settings_reject_invalid_cron() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;

    let cases = [
        (
            "61 * * * *",
            "This is not a valid cron expression. Allowed values: minute 0–59, hour 0–23, \
             day of month 1–31, month 1–12, day of week 0–6.",
        ),
        (
            "0 0 2 * * *",
            "Use five fields: minute hour day-of-month month day-of-week.",
        ),
    ];
    for (cron, message) in cases {
        let body = json!({ "enabled": true, "cron": cron });
        let (status, body, _) = api(&app, "PUT", "/api/settings/sync", &owner, Some(body)).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{cron}");
        assert_eq!(error_code(&body), "invalid_cron", "{cron}");
        assert_eq!(body["error"]["message"], message, "{cron}");
    }
}

#[tokio::test]
async fn sync_settings_need_an_admin() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;

    let body = json!({ "enabled": true, "cron": "0 2 * * *" });
    let (status, body, _) = api(&app, "PUT", "/api/settings/sync", &member, Some(body)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(error_code(&body), "owner_required");
}

#[tokio::test]
async fn sync_settings_are_saved_with_the_next_run() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let owner = owner_session(&state, "alex").await;

    let body = json!({ "enabled": true, "cron": " 0  2 * * * " });
    let (status, body, _) = api(&app, "PUT", "/api/settings/sync", &owner, Some(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["cron"], "0 2 * * *");
    assert!(body["description"].is_string());
    assert!(body["nextRunAt"].as_i64().unwrap() > sourcelens::state::now());

    let (_, body, _) = api(&app, "GET", "/api/settings/sync", &owner, None).await;
    assert_eq!(body["cron"], "0 2 * * *");
    assert!(body["nextRunAt"].as_i64().unwrap() > sourcelens::state::now());
}
