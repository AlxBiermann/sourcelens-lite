mod common;

use axum::http::StatusCode;
use common::{
    api, call, error_code, index_files, index_sample, insert_repo_async, insert_user,
    member_session, new_state, request, session_for,
};
use rusqlite::params;
use serde_json::{json, Value};
use sourcelens::index::store::{self, camel_parts, to_blob};
use sourcelens::search::fuse::fuse;
use sourcelens::search::lexical::group_expression;
use sourcelens::search::query::{plan, MAX_GROUPS};
use sourcelens::search::{self, symbols, vector, Filter, SearchRequest, SearchResponse};
use sourcelens::state::AppState;
use tempfile::TempDir;
use tower::ServiceExt;

/// Contains `retry` but never the inflected `retries`.
const BACKOFF_RS: &str = r#"/// Calls `attempt` until it succeeds, waiting twice as long after each failure.
pub fn retry_with_backoff<T>(
    mut attempt: impl FnMut() -> Result<T, String>,
    max_attempts: u32,
) -> Result<T, String> {
    let mut delay_ms = 100;
    for _ in 1..max_attempts {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
                delay_ms *= 2;
            }
        }
    }
    attempt()
}
"#;

/// Three functions small enough to be merged into one `block` chunk.
const TINY_TS: &str = "export function tinyAlpha(): number {\n  return 1;\n}\n\n\
export function tinyBeta(): number {\n  return 2;\n}\n\n\
export function tinyGamma(): number {\n  return 3;\n}\n";

const USERS_TS: &str = include_str!("fixtures/users.ts");
const ORDERS_PY: &str = include_str!("fixtures/orders.py");

struct Fixture {
    _tmp: TempDir,
    state: AppState,
    repo_id: i64,
    /// Line of `export function getUserById`.
    definition_line: u32,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo_id = insert_repo_async(&state, 1, "acme/shop", "connected").await;
    index_files(
        &state,
        repo_id,
        &[("src/users.ts", USERS_TS), ("app/orders.py", ORDERS_PY)],
    )
    .await;
    let definition_line = USERS_TS
        .lines()
        .position(|line| line.starts_with("export function getUserById"))
        .unwrap() as u32
        + 1;
    Fixture {
        _tmp: tmp,
        state,
        repo_id,
        definition_line,
    }
}

impl Fixture {
    async fn search(&self, query: &str, filter: Filter) -> SearchResponse {
        search::search(
            &self.state,
            SearchRequest {
                query: query.to_string(),
                filter,
                limit: Some(8),
            },
        )
        .await
        .unwrap()
    }

    fn scope(&self) -> Filter {
        Filter {
            repo_ids: vec![self.repo_id],
            ..Filter::default()
        }
    }

    /// Indexes more files into the fixture repository, in order.
    async fn index(&self, files: &[(&str, &str)]) {
        index_files(&self.state, self.repo_id, files).await;
    }
}

fn defines(fx: &Fixture, hit: &search::SearchHit) -> bool {
    hit.path == "src/users.ts"
        && hit.start_line <= fx.definition_line
        && hit.end_line >= fx.definition_line
        && hit
            .content
            .contains("export function getUserById(id: string)")
}

#[tokio::test]
async fn identifier_query_finds_the_definition_through_the_symbol_channel() {
    let fx = fixture().await;
    let response = fx.search("getUserById", fx.scope()).await;
    // No vec_chunks table → the semantic channel is skipped.
    assert_eq!(response.mode, "lexical");
    let first = response.hits.first().expect("at least one hit");
    assert!(defines(&fx, first), "first hit: {first:?}");
    assert!(first.matched_by.contains(&"symbol"));
    assert_eq!(first.repository, "acme/shop");
    assert_eq!(first.language.as_deref(), Some("typescript"));
}

#[tokio::test]
async fn natural_language_query_matches_identifier_parts() {
    let fx = fixture().await;
    let response = fx.search("user id lookup", fx.scope()).await;
    assert!(
        response.hits.iter().any(|hit| defines(&fx, hit)),
        "hits: {:?}",
        response.hits
    );
    assert!(response.terms.contains(&"user".to_string()));
}

#[tokio::test]
async fn identifier_prefix_finds_the_definition() {
    let fx = fixture().await;
    let response = fx.search("getUser", fx.scope()).await;
    let first = response.hits.first().expect("at least one hit");
    assert!(defines(&fx, first), "first hit: {first:?}");
    assert!(first.matched_by.contains(&"symbol"));
}

#[tokio::test]
async fn symbol_lookup_buckets() {
    let fx = fixture().await;
    let scope = fx.scope();
    let other = Filter {
        repo_ids: vec![fx.repo_id + 1],
        ..Filter::default()
    };
    let (exact, case_insensitive, prefix, substring, qualified, other_repo) = fx
        .state
        .db
        .read(move |conn| {
            Ok::<_, rusqlite::Error>((
                symbols::lookup(conn, &scope, "getUserById", None, 10)?,
                symbols::lookup(conn, &scope, "GETUSERBYID", None, 10)?,
                symbols::lookup(conn, &scope, "listActive", None, 10)?,
                symbols::lookup(conn, &scope, "serById", None, 10)?,
                symbols::lookup(conn, &scope, "Order.total", Some("method"), 10)?,
                symbols::lookup(conn, &other, "getUserById", None, 10)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].name, "getUserById");
    assert_eq!(exact[0].path, "src/users.ts");
    assert_eq!(exact[0].start_line, fx.definition_line);
    assert_eq!(case_insensitive.len(), 1);
    assert_eq!(
        prefix.first().map(|s| s.name.as_str()),
        Some("listActiveUsers")
    );
    assert_eq!(
        substring.first().map(|s| s.name.as_str()),
        Some("getUserById")
    );
    assert_eq!(
        qualified.first().map(|s| s.qualified_name.as_str()),
        Some("Order.total")
    );
    assert!(other_repo.is_empty());
}

#[tokio::test]
async fn language_filter_excludes_other_languages() {
    let fx = fixture().await;
    let filter = Filter {
        languages: vec!["python".to_string()],
        ..fx.scope()
    };
    let response = fx.search("getUserById", filter).await;
    assert!(response.hits.iter().all(|hit| hit.path == "app/orders.py"));

    let python = fx
        .search(
            "create_order",
            Filter {
                languages: vec!["python".to_string()],
                ..fx.scope()
            },
        )
        .await;
    assert!(python
        .hits
        .iter()
        .any(|hit| hit.content.contains("def create_order")));
}

#[tokio::test]
async fn path_prefix_filter() {
    let fx = fixture().await;
    let filter = Filter {
        path_prefix: Some("app/".to_string()),
        ..fx.scope()
    };
    let response = fx.search("getUserById", filter).await;
    assert!(response.hits.iter().all(|hit| hit.path.starts_with("app/")));
}

#[tokio::test]
async fn scope_without_the_repository_returns_nothing() {
    let fx = fixture().await;
    let response = fx
        .search(
            "getUserById",
            Filter {
                repo_ids: vec![fx.repo_id + 100],
                ..Filter::default()
            },
        )
        .await;
    assert!(response.hits.is_empty());
    let empty = fx.search("getUserById", Filter::default()).await;
    assert!(empty.hits.is_empty());
}

#[tokio::test]
async fn empty_query_is_rejected() {
    let fx = fixture().await;
    let err = search::search(
        &fx.state,
        SearchRequest {
            query: "  ? ! ".to_string(),
            filter: fx.scope(),
            limit: Some(8),
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, "empty_query");
    assert_eq!(err.status.as_u16(), 400);
}

#[tokio::test]
async fn hits_are_capped_per_file_and_by_limit() {
    let fx = fixture().await;
    let many: String = (0..40)
        .map(|i| {
            format!(
                "export function widgetHandler{i}(input: string): string {{\n{}  return input;\n}}\n\n",
                "  const widgetValue = input.trim().toLowerCase().replace(\"widget\", \"gadget\");\n".repeat(6)
            )
        })
        .collect();
    let repo = fx.repo_id;
    fx.state
        .db
        .write(move |conn| {
            store::apply_files(
                conn,
                repo,
                &[store::prepare_file("src/widgets.ts", "w", many.as_bytes())],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .await
        .unwrap();
    let response = search::search(
        &fx.state,
        SearchRequest {
            query: "widget".to_string(),
            filter: fx.scope(),
            limit: Some(50),
        },
    )
    .await
    .unwrap();
    let in_file = response
        .hits
        .iter()
        .filter(|hit| hit.path == "src/widgets.ts")
        .count();
    assert_eq!(in_file, 3, "hits from one file are capped at three");
    assert!(response.hits.len() <= 20);
}

#[test]
fn filter_normalises_languages_and_rejects_unknown_ones() {
    let filter = Filter::new(vec![1], ["Rust", " golang ", "", "rust"], Some(" /src/ ")).unwrap();
    assert_eq!(filter.languages, ["rust", "go"]);
    assert_eq!(filter.path_prefix.as_deref(), Some("src/"));
    let empty_prefix = Filter::new(vec![1], Vec::<String>::new(), Some(" / ")).unwrap();
    assert_eq!(empty_prefix.path_prefix, None);

    let err = Filter::new(vec![1], ["klingon"], None).unwrap_err();
    assert_eq!(err.code, "unknown_language");
    assert_eq!(err.status.as_u16(), 422);
    assert!(err.message.contains("\"klingon\""), "{}", err.message);
}

#[test]
fn camel_parts_splits_identifiers() {
    assert_eq!(camel_parts("getUserById"), ["get", "user", "by", "id"]);
    assert_eq!(camel_parts("HTTPServer"), ["http", "server"]);
    assert_eq!(
        camel_parts("parse_http_header"),
        ["parse", "http", "header"]
    );
    assert_eq!(camel_parts("utf8Decode"), ["utf8", "decode"]);
    assert_eq!(camel_parts("plain"), ["plain"]);
}

#[test]
fn query_plan_groups_terms_and_candidates() {
    let p = plan("how does the getUserById work").unwrap();
    let wholes: Vec<&str> = p.groups.iter().map(|g| g.whole.as_str()).collect();
    assert_eq!(wholes, ["getuserbyid", "work"]);
    assert_eq!(p.groups[0].parts, ["get", "user", "by", "id"]);
    assert_eq!(p.symbol_candidates, ["getUserById"]);
    assert!(p.terms.contains(&"user".to_string()));

    let single = plan("Order::total()").unwrap();
    assert_eq!(single.symbol_candidates, ["Order::total"]);

    // Only stop words: they are kept rather than dropping everything.
    let stops = plan("how to").unwrap();
    assert_eq!(stops.groups.len(), 2);

    assert!(plan("   ").is_none());
    assert!(plan("a ! ?").is_none());
}

#[test]
fn fuse_weights_channels() {
    // Rank 1 in `symbol` (weight 2) beats rank 1 in `text_any` alone.
    let fused = fuse(&[("symbol", 2.0, vec![1]), ("text_any", 1.0, vec![2])]);
    assert_eq!(fused[0].0, 1);
    assert_eq!(fused[0].2, ["symbol"]);

    // Present in three channels beats present only in `semantic`, even at rank 1 there.
    let fused = fuse(&[
        ("text_all", 1.0, vec![10, 20]),
        ("text_any", 1.0, vec![11, 20]),
        ("semantic", 1.0, vec![30, 31, 32, 20]),
    ]);
    let position = |id: i64| fused.iter().position(|(chunk, _, _)| *chunk == id).unwrap();
    assert!(position(20) < position(30));
    let twenty = &fused[position(20)];
    assert_eq!(twenty.2, ["text_all", "text_any", "semantic"]);
    assert!((twenty.1 - (1.0 / 62.0 + 1.0 / 62.0 + 1.0 / 64.0)).abs() < 1e-12);
}

#[test]
fn natural_language_words_get_a_stem_and_code_words_do_not() {
    let p = plan("retries handled users parsing").unwrap();
    let variants: Vec<Option<&str>> = p.groups.iter().map(|g| g.variant.as_deref()).collect();
    assert_eq!(
        variants,
        [Some("retry"), Some("handl"), Some("user"), Some("pars")]
    );
    assert!(p.terms.contains(&"retry".to_string()));
    assert_eq!(
        group_expression(&p.groups[0]),
        "(\"retries\"* OR \"retry\"*)"
    );

    for query in ["getUsers", "parse_files", "Order::items", "users.rs"] {
        let p = plan(query).unwrap();
        assert!(
            p.groups.iter().all(|g| g.variant.is_none()),
            "{query}: {:?}",
            p.groups
        );
    }
    let code = plan("parse_files").unwrap();
    assert!(!code.terms.contains(&"file".to_string()));
    assert_eq!(group_expression(&code.groups[1]), "\"files\"*");
}

#[test]
fn a_plan_has_at_most_sixteen_groups() {
    let query: String = (0..100u8)
        .map(|i| format!("x{}{} ", (b'a' + i / 26) as char, (b'a' + i % 26) as char))
        .collect();
    let p = plan(&query).unwrap();
    assert_eq!(MAX_GROUPS, 16);
    assert_eq!(p.groups.len(), MAX_GROUPS);
}

#[tokio::test]
async fn inflected_query_finds_the_stem_in_lexical_mode() {
    let fx = fixture().await;
    fx.index(&[("src/backoff.rs", BACKOFF_RS)]).await;
    let response = fx.search("retries", fx.scope()).await;
    assert_eq!(response.mode, "lexical");
    assert!(response.terms.contains(&"retry".to_string()));
    assert!(
        response
            .hits
            .iter()
            .any(|hit| hit.content.contains("fn retry_with_backoff")),
        "hits: {:?}",
        response.hits
    );
}

#[tokio::test]
async fn path_prefix_applies_before_the_symbol_limit() {
    let fx = fixture().await;
    let mut files: Vec<(String, String)> = (0..12)
        .map(|i| {
            (
                format!("services/s{i}/handler.ts"),
                format!("export function handler(input: string): string {{\n  return \"service {i}\" + input;\n}}\n"),
            )
        })
        .collect();
    files.push((
        "api/users/handler.ts".to_string(),
        "export function handler(input: string): string {\n  return \"users\" + input;\n}\n"
            .to_string(),
    ));
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    fx.index(&files).await;

    let filter = Filter {
        path_prefix: Some("api/".to_string()),
        ..fx.scope()
    };
    let response = fx.search("handler", filter).await;
    let first = response.hits.first().expect("a hit");
    assert_eq!(first.path, "api/users/handler.ts");
    assert!(first.matched_by.contains(&"symbol"), "{first:?}");
}

#[tokio::test]
async fn semantic_channel_filters_before_cutting_to_fifty() {
    let fx = fixture().await;
    let repo = fx.repo_id;
    let (python_chunk, unfiltered, filtered) = fx
        .state
        .db
        .write(move |conn| {
            store::create_vec_table(conn, 4)?;
            let ts_file: i64 = conn.query_row(
                "SELECT id FROM files WHERE path = 'src/users.ts'",
                [],
                |r| r.get(0),
            )?;
            let python_chunk: i64 = conn.query_row(
                "SELECT c.id FROM chunks c JOIN files f ON f.id = c.file_id
                 WHERE f.path = 'app/orders.py' ORDER BY c.id LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            // Sixty TypeScript chunks, all nearer to the query than the Python chunk.
            for i in 0..60u8 {
                let id: i64 = conn.query_row(
                    "INSERT INTO chunks (repo_id, file_id, kind, start_line, end_line, start_byte, end_byte, embed_hash)
                     VALUES (?1, ?2, 'block', 1, 1, 0, 1, 'h') RETURNING id",
                    params![repo, ts_file],
                    |r| r.get(0),
                )?;
                let near = [1.0, f32::from(i) * 0.001, 0.0, 0.0];
                store::put_vector(conn, id, repo, &to_blob(&near))?;
            }
            store::put_vector(conn, python_chunk, repo, &to_blob(&[0.0, 1.0, 0.0, 0.0]))?;

            let query = [1.0, 0.0, 0.0, 0.0];
            let all = Filter {
                repo_ids: vec![repo],
                ..Filter::default()
            };
            let python = Filter {
                languages: vec!["python".to_string()],
                ..all.clone()
            };
            Ok::<_, rusqlite::Error>((
                python_chunk,
                vector::nearest(conn, &all, &query, 50)?,
                vector::nearest(conn, &python, &query, 50)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(unfiltered.len(), 50);
    assert!(!unfiltered.contains(&python_chunk));
    assert_eq!(filtered, [python_chunk]);
}

#[tokio::test]
async fn a_readme_heading_does_not_outrank_a_function() {
    let fx = fixture().await;
    fx.index(&[
        (
            "README.md",
            "# Shop\n\n## Retry and backoff\n\nFailed calls are attempted again after a growing pause.\n",
        ),
        ("src/backoff.rs", BACKOFF_RS),
    ])
    .await;

    let response = fx.search("retry", fx.scope()).await;
    let first = response.hits.first().expect("a hit");
    assert_eq!(first.path, "src/backoff.rs", "hits: {:?}", response.hits);
    assert!(first.matched_by.contains(&"symbol"));
    assert!(response
        .hits
        .iter()
        .filter(|hit| hit.path == "README.md")
        .all(|hit| !hit.matched_by.contains(&"symbol")));

    let scope = fx.scope();
    let (default, any_heading, level) = fx
        .state
        .db
        .read(move |conn| {
            Ok::<_, rusqlite::Error>((
                symbols::lookup(conn, &scope, "Retry and backoff", None, 10)?,
                symbols::lookup(conn, &scope, "Retry and backoff", Some("heading"), 10)?,
                symbols::lookup(conn, &scope, "Retry", Some("heading_2"), 10)?,
            ))
        })
        .await
        .unwrap();
    assert!(default.is_empty(), "{default:?}");
    assert_eq!(any_heading.len(), 1);
    assert_eq!(any_heading[0].kind, "heading_2");
    assert_eq!(level.first().map(|s| s.path.as_str()), Some("README.md"));
}

#[tokio::test]
async fn a_block_hit_carries_the_names_it_contains() {
    let fx = fixture().await;
    fx.index(&[("src/tiny.ts", TINY_TS)]).await;
    let response = fx.search("tinyBeta", fx.scope()).await;
    let hit = response
        .hits
        .iter()
        .find(|hit| hit.path == "src/tiny.ts")
        .expect("a hit in src/tiny.ts");
    assert_eq!(hit.kind, "block", "{hit:?}");
    assert_eq!(hit.symbol_path, "");
    assert_eq!(hit.names, ["tinyAlpha", "tinyBeta", "tinyGamma"]);

    let definition = fx.search("getUserById", fx.scope()).await;
    assert!(definition.hits[0]
        .names
        .contains(&"getUserById".to_string()));
}

#[tokio::test]
async fn exact_lookup_prefers_functions_and_types_over_values() {
    let fx = fixture().await;
    fx.index(&[
        (
            "src/defaults.ts",
            "export const Settings = { attempts: 3, timeoutMs: 5000 };\n",
        ),
        (
            "src/settings.ts",
            "export class Settings {\n  load(): void {}\n}\n",
        ),
    ])
    .await;
    let scope = fx.scope();
    let settings = fx
        .state
        .db
        .read(move |conn| symbols::lookup(conn, &scope, "Settings", None, 10))
        .await
        .unwrap();
    let kinds: Vec<&str> = settings.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, ["class", "constant"]);
}

#[tokio::test]
async fn exact_lookup_honours_qualifiers() {
    let fx = fixture().await;
    fx.index(&[(
        "store/store.go",
        "package store\n\ntype Memory struct{}\n\nfunc (m *Memory) Get(k string) string {\n\treturn k\n}\n\n\
         type Disk struct{}\n\nfunc (d *Disk) Get(k string) string {\n\treturn k\n}\n",
    )])
    .await;
    let scope = fx.scope();
    let (disk, memory) = fx
        .state
        .db
        .read(move |conn| {
            Ok::<_, rusqlite::Error>((
                symbols::lookup(conn, &scope, "Disk.Get", None, 10)?,
                symbols::lookup(conn, &scope, "Memory.Get", None, 10)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(disk[0].qualified_name, "store.Disk.Get");
    assert_eq!(disk[0].kind, "method");
    assert_eq!(memory[0].qualified_name, "store.Memory.Get");
}

// ── HTTP: /api/search ──────────────────────────────────────────────────────

#[tokio::test]
async fn search_endpoint_scopes_to_connected_repositories() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;
    let r1 = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let r2 = insert_repo_async(&state, 2, "acme/old", "disconnected").await;
    index_sample(&state, r1).await;
    index_sample(&state, r2).await;

    let (status, body, text) = api(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "frobnicate" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(body["mode"], "lexical");
    let results = body["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert!(results.iter().all(|r| r["repository"] == "acme/api"));
    assert_eq!(results[0]["path"], "lib/special.py");
    assert!(results[0]["startLine"].is_u64());
    assert!(results[0]["matchedBy"].is_array());

    let (_, body, _) = api(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "frobnicate", "repositoryIds": [r2] })),
    )
    .await;
    assert_eq!(body["results"].as_array().unwrap().len(), 0);

    let (status, body, _) = api(
        &app,
        "POST",
        "/api/search",
        &member,
        Some(json!({ "query": "  " })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "empty_query");
}

#[tokio::test]
async fn search_rejects_unknown_languages() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let owner = insert_user(&state, "octocat", "owner", Some(1)).await;
    let cookie = session_for(&state, owner).await;

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/search",
            Some(&cookie),
            Some(json!({ "query": "parser", "languages": ["klingon"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(error_code(&body), "unknown_language");
    let message = body["error"]["message"].as_str().unwrap_or("");
    assert!(
        message.contains("klingon") && message.contains("rust"),
        "{message}"
    );

    let (status, _, body) = call(
        &state,
        request(
            "POST",
            "/api/search",
            Some(&cookie),
            Some(json!({ "query": "parser", "languages": ["Rust", "golang"] })),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn languages_endpoint_lists_connected_repositories_only() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;
    let r1 = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let r2 = insert_repo_async(&state, 2, "acme/old", "disconnected").await;
    index_sample(&state, r1).await;
    index_files(
        &state,
        r2,
        &[("main.go", "package main\n\nfunc main() {}\n".to_string())],
    )
    .await;

    let (status, body, text) = api(&app, "GET", "/api/search/languages", &member, None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(body, json!({ "languages": ["python", "rust"] }));
}

#[tokio::test]
async fn search_is_rate_limited_per_user() {
    let tmp = TempDir::new().unwrap();
    let state = new_state(&tmp, |_| {});
    let app = sourcelens::http::router(state.clone());
    let member = member_session(&state, "sam").await;

    let mut accepted = 0;
    let mut limited = None;
    for _ in 0..200 {
        let request = request(
            "POST",
            "/api/search",
            Some(&member),
            Some(json!({ "query": "frobnicate" })),
        );
        let response = app.clone().oneshot(request).await.unwrap();
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            limited = Some(response);
            break;
        }
        assert_eq!(response.status(), StatusCode::OK);
        accepted += 1;
    }
    let response = limited.expect("searches were never rate limited");
    assert!(accepted >= 20, "limited after {accepted} searches");
    let retry_after: u64 = response.headers()["retry-after"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(retry_after >= 1);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["code"], "rate_limited");
}
