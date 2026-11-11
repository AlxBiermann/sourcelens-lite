use sourcelens::config::Config;
use sourcelens::index::store::{self, camel_parts};
use sourcelens::search::fuse::fuse;
use sourcelens::search::query::plan;
use sourcelens::search::{self, symbols, Filter, SearchRequest, SearchResponse};
use sourcelens::state::AppState;

const USERS_TS: &str = include_str!("fixtures/users.ts");
const ORDERS_PY: &str = include_str!("fixtures/orders.py");

struct Fixture {
    _tmp: tempfile::TempDir,
    state: AppState,
    repo_id: i64,
    /// Line of `export function getUserById`.
    definition_line: u32,
}

async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(Config::new("http://localhost:3000", tmp.path())).unwrap();
    let repo_id = state
        .db
        .write(|conn| {
            let repo_id: i64 = conn.query_row(
                "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, created_at)
                 VALUES (1, 1, 'acme/shop', 0, 'main', 0) RETURNING id",
                [],
                |row| row.get(0),
            )?;
            let files = [
                store::prepare_file("src/users.ts", "u1", USERS_TS.as_bytes()),
                store::prepare_file("app/orders.py", "o1", ORDERS_PY.as_bytes()),
            ];
            store::apply_files(conn, repo_id, &files)?;
            Ok::<_, rusqlite::Error>(repo_id)
        })
        .await
        .unwrap();
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
    let repo = fx.repo_id;
    let (exact, case_insensitive, prefix, substring, qualified, other_repo) = fx
        .state
        .db
        .read(move |conn| {
            Ok::<_, rusqlite::Error>((
                symbols::lookup(conn, &[repo], "getUserById", None, 10)?,
                symbols::lookup(conn, &[repo], "GETUSERBYID", None, 10)?,
                symbols::lookup(conn, &[repo], "listActive", None, 10)?,
                symbols::lookup(conn, &[repo], "serById", None, 10)?,
                symbols::lookup(conn, &[repo], "Order.total", Some("method"), 10)?,
                symbols::lookup(conn, &[repo + 1], "getUserById", None, 10)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].name, "getUserById");
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
