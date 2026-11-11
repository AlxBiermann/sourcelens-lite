mod common;

use axum::body::Body;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use common::{index_files, insert_repo_async as insert_repo, mcp_request, new_state, PUBLIC_URL};
use serde_json::{json, Value};
use sourcelens::http::keys::{self, NewKey};
use sourcelens::mcp::authenticate;
use sourcelens::state::{now, AppState};
use std::time::Duration;
use tempfile::TempDir;

const COMMIT: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
/// 2026-11-11 00:00:00 UTC.
const INDEXED_AT: i64 = 1_794_355_200;

struct Env {
    _tmp: TempDir,
    state: AppState,
    app: Router,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    Env {
        _tmp: tmp,
        state,
        app,
    }
}

async fn mark_indexed(state: &AppState, repo_id: i64) {
    state
        .db
        .write(move |c| {
            c.execute(
                "UPDATE repositories SET indexed_commit = ?1, indexed_at = ?2,
                   file_count = (SELECT count(*) FROM files WHERE repo_id = ?3 AND skip_reason IS NULL)
                 WHERE id = ?3",
                rusqlite::params![COMMIT, INDEXED_AT, repo_id],
            )
        })
        .await
        .unwrap();
}

/// Indexes files through the real store.
async fn index(state: &AppState, repo_id: i64, files: Vec<(String, Vec<u8>)>) {
    index_files(state, repo_id, files.as_slice()).await;
    mark_indexed(state, repo_id).await;
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("tests/fixtures/mcp/{name}")).unwrap()
}

/// Inserts a `files` row directly; `get_code` reads nothing else.
async fn insert_file(
    state: &AppState,
    repo_id: i64,
    path: &str,
    skip_reason: Option<&str>,
    content: Option<String>,
) {
    let path = path.to_string();
    let skip_reason = skip_reason.map(str::to_string);
    state
        .db
        .write(move |c| {
            let size = content.as_ref().map_or(10, String::len) as i64;
            let line_count = content.as_ref().map_or(0, |s| s.lines().count()) as i64;
            c.execute(
                "INSERT INTO files (repo_id, path, blob_sha, language, size, line_count, skip_reason, content)
                 VALUES (?1, ?2, 'sha', NULL, ?3, ?4, ?5, ?6)",
                rusqlite::params![repo_id, path, size, line_count, skip_reason, content.map(String::into_bytes)],
            )
        })
        .await
        .unwrap();
}

async fn create_key(state: &AppState, repository_ids: Option<Vec<i64>>) -> String {
    let input = NewKey {
        name: "agent".into(),
        expires_in_days: None,
        repository_ids,
    };
    keys::create_key(state, input, None).await.unwrap().1
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let reply = common::send(app, req).await;
    (reply.status, reply.headers, reply.text.into_bytes())
}

async fn post_mcp(
    app: &Router,
    key: Option<&str>,
    body: impl Into<Body>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let req = mcp_request(key).body(body.into()).unwrap();
    send(app, req).await
}

async fn rpc(app: &Router, key: &str, method: &str, params: Value) -> Value {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let (status, _, bytes) = post_mcp(app, Some(key), body.to_string()).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["jsonrpc"], "2.0");
    assert_eq!(value["id"], 1);
    value
}

/// `tools/call` result object.
async fn call(app: &Router, key: &str, tool: &str, arguments: Value) -> Value {
    let response = rpc(
        app,
        key,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    )
    .await;
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

// ── Transport and protocol ───────────────────────────────────────────────────

#[tokio::test]
async fn missing_or_invalid_key_is_unauthorized() {
    let env = env();
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();

    for key in [None, Some("sl_0000"), Some("not-a-key")] {
        let (status, headers, body) = post_mcp(&env.app, key, ping.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let challenge = headers[header::WWW_AUTHENTICATE].to_str().unwrap();
        assert!(challenge.starts_with("Bearer"), "{challenge}");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], -32001);
    }
}

#[tokio::test]
async fn expired_key_is_unauthorized() {
    let env = env();
    let key = create_key(&env.state, None).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let (status, _, _) = post_mcp(&env.app, Some(&key), ping.clone()).await;
    assert_eq!(status, StatusCode::OK);

    env.state
        .db
        .write(|c| c.execute("UPDATE api_keys SET expires_at = ?1", [now() - 1]))
        .await
        .unwrap();
    let (status, _, _) = post_mcp(&env.app, Some(&key), ping).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn initialize_negotiates_protocol_version() {
    let env = env();
    let key = create_key(&env.state, None).await;

    let response = rpc(
        &env.app,
        &key,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "1" },
        }),
    )
    .await;
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "sourcelens");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(result["instructions"]
        .as_str()
        .unwrap()
        .contains("search_code"));

    let response = rpc(
        &env.app,
        &key,
        "initialize",
        json!({ "protocolVersion": "1999-01-01" }),
    )
    .await;
    assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
}

#[tokio::test]
async fn notification_ping_and_unknown_method() {
    let env = env();
    let key = create_key(&env.state, None).await;

    let notification = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let (status, _, body) = post_mcp(&env.app, Some(&key), notification.to_string()).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert!(body.is_empty());

    let response = rpc(&env.app, &key, "ping", Value::Null).await;
    assert_eq!(response["result"], json!({}));

    let response = rpc(&env.app, &key, "resources/list", json!({})).await;
    assert_eq!(response["error"]["code"], -32601);
}

#[tokio::test]
async fn tools_list_has_the_five_read_only_tools() {
    let env = env();
    let key = create_key(&env.state, None).await;
    let response = rpc(&env.app, &key, "tools/list", json!({})).await;
    let tools = response["result"]["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "find_symbol",
            "get_code",
            "get_file_outline",
            "list_repositories",
            "search_code"
        ]
    );
    for tool in tools {
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{tool}");
        assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{tool}");
        assert!(!tool["description"].as_str().unwrap().is_empty());
    }
}

#[tokio::test]
async fn unknown_tool_and_invalid_arguments_are_invalid_params() {
    let env = env();
    let key = create_key(&env.state, None).await;

    let unknown = rpc(
        &env.app,
        &key,
        "tools/call",
        json!({ "name": "delete_everything", "arguments": {} }),
    )
    .await;
    assert_eq!(unknown["error"]["code"], -32602);

    let missing = rpc(
        &env.app,
        &key,
        "tools/call",
        json!({ "name": "search_code", "arguments": {} }),
    )
    .await;
    assert_eq!(missing["error"]["code"], -32602);
    assert!(missing["error"]["message"]
        .as_str()
        .unwrap()
        .contains("query"));

    let extra = rpc(
        &env.app,
        &key,
        "tools/call",
        json!({
            "name": "get_code", "arguments": { "repository": "a/b", "path": "x", "colour": "red" },
        }),
    )
    .await;
    assert_eq!(extra["error"]["code"], -32602);
    assert!(extra["error"]["message"]
        .as_str()
        .unwrap()
        .contains("colour"));

    let wrong_type = rpc(&env.app, &key, "tools/call", json!({
        "name": "get_code", "arguments": { "repository": "a/b", "path": "x", "startLine": "ten" },
    }))
    .await;
    assert_eq!(wrong_type["error"]["code"], -32602);
    assert!(wrong_type["error"]["message"]
        .as_str()
        .unwrap()
        .contains("startLine"));
}

#[tokio::test]
async fn transport_rejects_wrong_method_origin_and_body() {
    let env = env();
    let key = create_key(&env.state, None).await;

    for method in [Method::GET, Method::DELETE] {
        let req = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(send(&env.app, req).await.0, StatusCode::METHOD_NOT_ALLOWED);
    }

    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let foreign = mcp_request(Some(&key))
        .header(header::ORIGIN, "https://evil.example")
        .body(Body::from(ping.clone()))
        .unwrap();
    assert_eq!(send(&env.app, foreign).await.0, StatusCode::FORBIDDEN);
    let own = mcp_request(Some(&key))
        .header(header::ORIGIN, PUBLIC_URL)
        .body(Body::from(ping))
        .unwrap();
    assert_eq!(send(&env.app, own).await.0, StatusCode::OK);

    let (status, _, body) = post_mcp(&env.app, Some(&key), "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], -32700);

    for invalid in [
        json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]),
        json!({ "id": 1, "method": "ping" }),
    ] {
        let (status, _, body) = post_mcp(&env.app, Some(&key), invalid.to_string()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], -32600);
    }
}

// ── Keys and scope ───────────────────────────────────────────────────────────

#[tokio::test]
async fn key_scope_excludes_disconnected_and_foreign_repositories() {
    let env = env();
    let api = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let orders = insert_repo(&env.state, 2, "acme/orders", "connected").await;
    let gone = insert_repo(&env.state, 3, "acme/gone", "disconnected").await;
    mark_indexed(&env.state, api).await;

    let all_key = create_key(&env.state, None).await;
    let scope = authenticate(&env.state, &all_key).await.unwrap();
    assert_eq!(scope.repo_ids, vec![api, orders]);

    let result = call(&env.app, &all_key, "list_repositories", json!({})).await;
    assert!(!is_error(&result));
    let listed = text(&result);
    assert!(
        listed.contains("acme/api  branch main  indexed a1b2c3d (2026-11-11)"),
        "{listed}"
    );
    assert!(
        listed.contains("acme/orders  branch main  not indexed yet"),
        "{listed}"
    );
    assert!(!listed.contains("acme/gone"), "{listed}");
    let names: Vec<&str> = result["structuredContent"]["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["acme/api", "acme/orders"]);

    let key_a = create_key(&env.state, Some(vec![api, gone])).await;
    let scope = authenticate(&env.state, &key_a).await.unwrap();
    assert_eq!(scope.repo_ids, vec![api]);
    let result = call(&env.app, &key_a, "list_repositories", json!({})).await;
    assert!(!text(&result).contains("acme/orders"));

    // A key that was used gets last_used_at (written in the background).
    assert!(last_used_at(&env.state, scope.key_id, true).await.is_some());
}

/// `api_keys.last_used_at`; with `wait`, polls for up to five seconds until it is set.
async fn last_used_at(state: &AppState, key_id: i64, wait: bool) -> Option<i64> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let used: Option<i64> = state
            .db
            .read(move |c| {
                c.query_row(
                    "SELECT last_used_at FROM api_keys WHERE id = ?1",
                    [key_id],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap();
        if used.is_some() || !wait || std::time::Instant::now() > deadline {
            return used;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn last_used_at_is_written_without_delaying_the_response() {
    let env = env();
    let key = create_key(&env.state, None).await;
    let key_id = only_key_id(&env.state).await;

    // Hold the writer until the response has arrived.
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let db = env.state.db.clone();
    let holder = tokio::spawn(async move {
        db.write(move |_conn| {
            started_tx.send(()).ok();
            release_rx.recv().ok();
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    });
    tokio::task::spawn_blocking(move || started_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    let (status, _, _) =
        tokio::time::timeout(Duration::from_secs(5), post_mcp(&env.app, Some(&key), ping))
            .await
            .expect("the response does not wait for the writer");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(last_used_at(&env.state, key_id, false).await, None);

    release_tx.send(()).unwrap();
    holder.await.unwrap();
    assert!(last_used_at(&env.state, key_id, true).await.is_some());
}

/// Id of the only key, read directly so that no request touches it.
async fn only_key_id(state: &AppState) -> i64 {
    state
        .db
        .read(|c| c.query_row("SELECT id FROM api_keys", [], |r| r.get(0)))
        .await
        .unwrap()
}

#[tokio::test]
async fn requests_beyond_the_burst_are_rate_limited_per_key() {
    let env = env();
    let key = create_key(&env.state, None).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();

    let mut allowed = 0;
    let mut limited = None;
    for _ in 0..200 {
        let (status, headers, body) = post_mcp(&env.app, Some(&key), ping.clone()).await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            limited = Some((headers, body));
            break;
        }
        assert_eq!(status, StatusCode::OK);
        allowed += 1;
    }
    assert!(allowed >= 20, "the burst allows 20 requests, got {allowed}");
    let (headers, body) = limited.expect("a request beyond the burst is limited");
    let retry_after: u64 = headers[header::RETRY_AFTER]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after >= 1);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], -32000);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("Rate limit exceeded"));

    let other = create_key(&env.state, None).await;
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
    assert_eq!(
        post_mcp(&env.app, Some(&other), ping).await.0,
        StatusCode::OK,
        "keys have separate budgets"
    );
}

// ── get_code ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_code_pages_and_clamps_ranges() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let long: String = (1..=1000).map(|i| format!("line {i}\n")).collect();
    insert_file(&env.state, r1, "src/long.txt", None, Some(long)).await;
    insert_file(&env.state, r1, "assets/logo.png", Some("binary"), None).await;
    mark_indexed(&env.state, r1).await;
    let key = create_key(&env.state, Some(vec![r1])).await;
    let app = &env.app;

    // No range: lines 1-400 and a pointer to the rest.
    let result = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "acme/api", "path": "src/long.txt" }),
    )
    .await;
    assert!(!is_error(&result));
    let body = text(&result);
    assert!(
        body.starts_with("acme/api src/long.txt:1-400 of 1000 lines @ a1b2c3d"),
        "{body}"
    );
    assert!(body.contains("line 400\n"));
    assert!(!body.contains("line 401\n"));
    assert!(body.contains("startLine=401"), "{body}");
    assert_eq!(result["structuredContent"]["startLine"], 1);
    assert_eq!(result["structuredContent"]["endLine"], 400);
    assert_eq!(result["structuredContent"]["lineCount"], 1000);
    assert_eq!(result["structuredContent"]["commit"], COMMIT);

    // Explicit range is returned exactly.
    let result = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "ACME/API", "path": "src/long.txt", "startLine": 10, "endLine": 12 }),
    )
    .await;
    assert_eq!(
        result["structuredContent"]["content"],
        "line 10\nline 11\nline 12"
    );
    assert!(!text(&result).contains("for more"));

    // At most 800 lines per call.
    let result = call(app, &key, "get_code", json!({ "repository": "acme/api", "path": "src/long.txt", "startLine": 1, "endLine": 5000 })).await;
    assert_eq!(result["structuredContent"]["endLine"], 800);
    assert!(text(&result).contains("startLine=801"));

    // The end of the file needs no continuation.
    let result = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "acme/api", "path": "src/long.txt", "startLine": 950 }),
    )
    .await;
    assert_eq!(result["structuredContent"]["startLine"], 950);
    assert_eq!(result["structuredContent"]["endLine"], 1000);
    assert!(!text(&result).contains("for more"));

    let past = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "acme/api", "path": "src/long.txt", "startLine": 2000 }),
    )
    .await;
    assert!(is_error(&past));

    let skipped = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "acme/api", "path": "assets/logo.png" }),
    )
    .await;
    assert!(is_error(&skipped));
    assert!(text(&skipped).contains("binary"));

    let missing = call(
        app,
        &key,
        "get_code",
        json!({ "repository": "acme/api", "path": "src/nope.rs" }),
    )
    .await;
    assert!(is_error(&missing));
    assert!(text(&missing).contains("get_file_outline"));
}

#[tokio::test]
async fn get_code_outside_the_key_scope_is_a_tool_error() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let r2 = insert_repo(&env.state, 2, "acme/orders", "connected").await;
    insert_file(
        &env.state,
        r2,
        "orders.py",
        None,
        Some("print('x')\n".into()),
    )
    .await;
    let key_a = create_key(&env.state, Some(vec![r1])).await;

    let result = call(
        &env.app,
        &key_a,
        "get_code",
        json!({ "repository": "acme/orders", "path": "orders.py" }),
    )
    .await;
    assert!(is_error(&result));
    assert!(text(&result).contains("acme/api"), "{}", text(&result));
    assert!(result.get("structuredContent").is_none());
}

// ── Tools over an indexed repository ─────────────────────────────────────────

async fn indexed_env() -> (Env, i64, i64) {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let r2 = insert_repo(&env.state, 2, "acme/orders", "connected").await;
    index(
        &env.state,
        r1,
        vec![("src/users.ts".into(), fixture("users.ts"))],
    )
    .await;
    index(
        &env.state,
        r2,
        vec![("orders.py".into(), fixture("orders.py"))],
    )
    .await;
    (env, r1, r2)
}

#[tokio::test]
async fn search_code_reports_unknown_languages() {
    let (env, _, _) = indexed_env().await;
    let key = create_key(&env.state, None).await;

    let result = call(
        &env.app,
        &key,
        "search_code",
        json!({ "query": "getUserById", "languages": ["klingon"] }),
    )
    .await;
    assert!(is_error(&result));
    assert!(
        text(&result).contains("Unknown language \"klingon\""),
        "{}",
        text(&result)
    );
}

#[tokio::test]
async fn search_code_finds_the_definition() {
    let (env, r1, _) = indexed_env().await;
    let key_a = create_key(&env.state, Some(vec![r1])).await;

    let result = call(
        &env.app,
        &key_a,
        "search_code",
        json!({ "query": "getUserById" }),
    )
    .await;
    assert!(!is_error(&result), "{}", text(&result));
    let body = text(&result);
    assert!(body.contains("users.ts:"), "{body}");
    assert!(body.contains("function getUserById"), "{body}");
    let first = &result["structuredContent"]["results"][0];
    assert_eq!(first["repository"], "acme/api");
    assert_eq!(first["path"], "src/users.ts");
    assert!(first["matchedBy"]
        .as_array()
        .unwrap()
        .contains(&json!("symbol")));
    assert_eq!(result["structuredContent"]["mode"], "lexical");

    let foreign = call(
        &env.app,
        &key_a,
        "search_code",
        json!({ "query": "order", "repositories": ["acme/orders"] }),
    )
    .await;
    assert!(is_error(&foreign));

    let empty = call(&env.app, &key_a, "search_code", json!({ "query": "   " })).await;
    assert!(is_error(&empty));
}

#[tokio::test]
async fn search_code_clamps_the_limit() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let files = (0..30)
        .map(|i| {
            let source = format!("def widget_handler_{i}():\n    return \"widget {i}\"\n");
            (format!("handlers/widget_{i}.py"), source.into_bytes())
        })
        .collect();
    index(&env.state, r1, files).await;
    let key = create_key(&env.state, None).await;

    let result = call(
        &env.app,
        &key,
        "search_code",
        json!({ "query": "widget", "limit": 50 }),
    )
    .await;
    assert!(!is_error(&result), "{}", text(&result));
    assert_eq!(
        result["structuredContent"]["results"]
            .as_array()
            .unwrap()
            .len(),
        20
    );
    assert!(text(&result).starts_with("20 results for \"widget\""));
}

#[tokio::test]
async fn get_file_outline_indents_by_depth() {
    let (env, _, _) = indexed_env().await;
    let key = create_key(&env.state, None).await;

    let result = call(
        &env.app,
        &key,
        "get_file_outline",
        json!({ "repository": "acme/api", "path": "src/users.ts" }),
    )
    .await;
    assert!(!is_error(&result), "{}", text(&result));
    let lines: Vec<&str> = text(&result).lines().collect();
    let class = lines
        .iter()
        .find(|l| l.contains("class UserService"))
        .unwrap();
    assert!(class.starts_with('L'), "{class}");
    let method = lines
        .iter()
        .find(|l| l.contains("method findByEmail"))
        .unwrap();
    assert!(method.starts_with("  L"), "{method}");
    assert!(method.contains("findByEmail(email: string)"), "{method}");
    let class_pos = lines
        .iter()
        .position(|l| l.contains("class UserService"))
        .unwrap();
    let method_pos = lines
        .iter()
        .position(|l| l.contains("method findByEmail"))
        .unwrap();
    assert!(class_pos < method_pos);

    let symbols = result["structuredContent"]["symbols"].as_array().unwrap();
    let find = symbols.iter().find(|s| s["name"] == "findByEmail").unwrap();
    assert_eq!(find["depth"], 1);
    assert_eq!(find["qualifiedName"], "UserService.findByEmail");
}

#[tokio::test]
async fn find_symbol_scopes_and_reports_unknown_repositories() {
    let (env, r1, _) = indexed_env().await;
    let all_key = create_key(&env.state, None).await;

    let result = call(
        &env.app,
        &all_key,
        "find_symbol",
        json!({ "name": "getUserById" }),
    )
    .await;
    assert!(!is_error(&result), "{}", text(&result));
    let first = &result["structuredContent"]["symbols"][0];
    assert_eq!(first["repository"], "acme/api");
    assert_eq!(first["path"], "src/users.ts");
    assert_eq!(first["kind"], "function");
    assert!(
        text(&result).contains("src/users.ts:9-16"),
        "{}",
        text(&result)
    );

    let unknown = call(
        &env.app,
        &all_key,
        "find_symbol",
        json!({ "name": "getUserById", "repositories": ["acme/nope"] }),
    )
    .await;
    assert!(is_error(&unknown));
    assert!(text(&unknown).contains("acme/api"));
    assert!(text(&unknown).contains("acme/orders"));

    let key_a = create_key(&env.state, Some(vec![r1])).await;
    let scoped = call(
        &env.app,
        &key_a,
        "find_symbol",
        json!({ "name": "OrderRepository" }),
    )
    .await;
    assert!(!is_error(&scoped));
    assert_eq!(scoped["structuredContent"]["symbols"], json!([]));
}

#[tokio::test]
async fn search_code_cuts_long_lines_in_text_and_structured_content() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let mut notebook = String::new();
    for i in 0..1000 {
        notebook.push_str(&format!("cell {i}\n"));
        if i == 500 {
            // One 420 KB line; the file's average line stays far below the minified threshold.
            notebook.push_str(&"pixel ".repeat(70_000));
            notebook.push('\n');
        }
    }
    index(
        &env.state,
        r1,
        vec![("notes/analysis.txt".into(), notebook.into_bytes())],
    )
    .await;
    let key = create_key(&env.state, None).await;

    let result = call(&env.app, &key, "search_code", json!({ "query": "pixel" })).await;
    assert!(!is_error(&result), "{}", text(&result));
    let body = text(&result);
    assert!(body.len() < 30_000, "text has {} bytes", body.len());
    assert!(body.contains("… [+"), "{body}");
    let results = result["structuredContent"]["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let contents: Vec<&str> = results
        .iter()
        .map(|hit| hit["content"].as_str().unwrap())
        .collect();
    assert!(contents.iter().any(|content| content.contains("… [+")));
    for content in contents {
        assert!(
            content.len() < 10_000,
            "content has {} bytes",
            content.len()
        );
    }
}

#[tokio::test]
async fn search_code_names_the_symbols_of_a_block() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    let tiny = "export function tinyAlpha(): number {\n  return 1;\n}\n\n\
                export function tinyBeta(): number {\n  return 2;\n}\n";
    index(&env.state, r1, vec![("src/tiny.ts".into(), tiny.into())]).await;
    let key = create_key(&env.state, None).await;

    let result = call(
        &env.app,
        &key,
        "search_code",
        json!({ "query": "tinyBeta" }),
    )
    .await;
    assert!(!is_error(&result), "{}", text(&result));
    assert!(
        text(&result).contains("src/tiny.ts:1-7  block tinyAlpha, tinyBeta"),
        "{}",
        text(&result)
    );
    assert_eq!(
        result["structuredContent"]["results"][0]["names"],
        json!(["tinyAlpha", "tinyBeta"])
    );
}

#[tokio::test]
async fn find_symbol_returns_headings_only_when_asked() {
    let env = env();
    let r1 = insert_repo(&env.state, 1, "acme/api", "connected").await;
    index(
        &env.state,
        r1,
        vec![
            (
                "README.md".into(),
                b"# Shop\n\n## Retry and backoff\n\nFailed calls are attempted again.\n".to_vec(),
            ),
            (
                "src/retry.ts".into(),
                b"export function retryRequest(url: string): string {\n  return url;\n}\n".to_vec(),
            ),
        ],
    )
    .await;
    let key = create_key(&env.state, None).await;

    let kinds = |result: &Value| -> Vec<String> {
        result["structuredContent"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["kind"].as_str().unwrap().to_string())
            .collect()
    };
    let default = call(&env.app, &key, "find_symbol", json!({ "name": "Retry" })).await;
    assert_eq!(kinds(&default), ["function"], "{}", text(&default));

    let headings = call(
        &env.app,
        &key,
        "find_symbol",
        json!({ "name": "Retry", "kind": "heading" }),
    )
    .await;
    assert_eq!(kinds(&headings), ["heading_2"], "{}", text(&headings));
    assert_eq!(
        headings["structuredContent"]["symbols"][0]["path"],
        "README.md"
    );

    let level = call(
        &env.app,
        &key,
        "find_symbol",
        json!({ "name": "Retry", "kind": "heading_2" }),
    )
    .await;
    assert_eq!(kinds(&level), ["heading_2"]);
}
