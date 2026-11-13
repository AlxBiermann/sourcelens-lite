mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::{index_files, insert_repo_async, new_state};
use sourcelens::search::grep::{self, GrepRequest, GrepResponse, DEFAULT_LIMIT};
use sourcelens::search::Filter;
use sourcelens::state::AppState;
use tempfile::TempDir;

const USERS_TS: &str = include_str!("fixtures/mcp/users.ts");
const ORDERS_PY: &str = include_str!("fixtures/mcp/orders.py");

struct Fixture {
    _tmp: TempDir,
    state: AppState,
    api: i64,
    orders: i64,
}

/// `acme/api` with `src/users.ts`, `acme/orders` with `orders.py`.
async fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let api = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let orders = insert_repo_async(&state, 2, "acme/orders", "connected").await;
    index_files(&state, api, &[("src/users.ts", USERS_TS)]).await;
    index_files(&state, orders, &[("orders.py", ORDERS_PY)]).await;
    Fixture {
        _tmp: tmp,
        state,
        api,
        orders,
    }
}

/// A literal, case-sensitive request with the default limit.
fn request(
    pattern: &str,
    repo_ids: Vec<i64>,
    languages: &[&str],
    path_prefix: Option<&str>,
) -> GrepRequest {
    GrepRequest {
        pattern: pattern.to_string(),
        regex: false,
        case_sensitive: true,
        filter: Filter::new(repo_ids, languages, path_prefix).unwrap(),
        limit: DEFAULT_LIMIT,
    }
}

async fn run(state: &AppState, req: GrepRequest) -> GrepResponse {
    grep::grep(state, req).await.unwrap()
}

/// `(repository, path, line)` of every returned match.
fn locations(response: &GrepResponse) -> Vec<(&str, &str, u32)> {
    response
        .matches
        .iter()
        .map(|m| (m.repository.as_str(), m.path.as_str(), m.line))
        .collect()
}

#[tokio::test]
async fn literal_finds_every_line_in_repository_path_and_line_order() {
    let f = fixture().await;
    let all = vec![f.orders, f.api];

    let response = run(&f.state, request("getUserById", all.clone(), &[], None)).await;
    assert_eq!(
        locations(&response),
        [
            ("acme/api", "src/users.ts", 9),
            ("acme/api", "src/users.ts", 33)
        ]
    );
    assert_eq!(
        response.matches[0].text,
        "export function getUserById(id: string): User | undefined {"
    );
    assert_eq!(response.total_matches, 2);
    assert_eq!(response.files_with_matches, 1);
    assert_eq!(response.files_searched, 2);
    assert!(!response.truncated);
    assert!(!response.timed_out);

    let response = run(&f.state, request("return", all, &[], None)).await;
    assert_eq!(
        locations(&response),
        [
            ("acme/api", "src/users.ts", 13),
            ("acme/api", "src/users.ts", 15),
            ("acme/api", "src/users.ts", 26),
            ("acme/api", "src/users.ts", 29),
            ("acme/orders", "orders.py", 6),
            ("acme/orders", "orders.py", 10),
        ]
    );
}

#[tokio::test]
async fn literal_is_not_a_regex_and_anchors_apply_per_line() {
    let f = fixture().await;
    let all = vec![f.api, f.orders];

    let literal = run(&f.state, request("users.get(id)", all.clone(), &[], None)).await;
    assert_eq!(locations(&literal), [("acme/api", "src/users.ts", 11)]);

    let as_regex = GrepRequest {
        regex: true,
        ..request("users.get(id)", all.clone(), &[], None)
    };
    assert!(run(&f.state, as_regex).await.matches.is_empty());

    let anchored = GrepRequest {
        regex: true,
        ..request(r"^\s+return \w+;$", all, &[], None)
    };
    let lines: Vec<u32> = run(&f.state, anchored)
        .await
        .matches
        .iter()
        .map(|m| {
            assert_eq!(m.path, "src/users.ts");
            m.line
        })
        .collect();
    assert_eq!(lines, [13, 15, 26, 29]);
}

#[tokio::test]
async fn case_sensitivity_is_a_flag() {
    let f = fixture().await;
    let all = vec![f.api, f.orders];

    let sensitive = run(&f.state, request("userservice", all.clone(), &[], None)).await;
    assert!(sensitive.matches.is_empty());

    let insensitive = GrepRequest {
        case_sensitive: false,
        ..request("userservice", all, &[], None)
    };
    let response = run(&f.state, insensitive).await;
    assert_eq!(locations(&response), [("acme/api", "src/users.ts", 18)]);
}

#[tokio::test]
async fn limit_truncates_the_list_but_counts_every_match() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    let content: String = (1..=30).map(|i| format!("needle {i}\n")).collect();
    index_files(&state, repo, &[("needles.txt", content)]).await;

    let response = run(
        &state,
        GrepRequest {
            limit: 10,
            ..request("needle", vec![repo], &[], None)
        },
    )
    .await;
    let lines: Vec<u32> = response.matches.iter().map(|m| m.line).collect();
    assert_eq!(lines, (1..=10).collect::<Vec<u32>>());
    assert_eq!(response.total_matches, 30);
    assert!(response.truncated);
    assert!(!response.timed_out);
}

#[tokio::test]
async fn filters_narrow_repositories_languages_and_paths() {
    let f = fixture().await;
    let all = vec![f.api, f.orders];

    let orders_only = run(&f.state, request("return", vec![f.orders], &[], None)).await;
    assert_eq!(orders_only.matches.len(), 2);
    assert!(orders_only.matches.iter().all(|m| m.path == "orders.py"));

    let python = run(&f.state, request("return", all.clone(), &["python"], None)).await;
    assert!(!python.matches.is_empty());
    assert!(python.matches.iter().all(|m| m.path == "orders.py"));

    let src = run(&f.state, request("return", all, &[], Some("src/"))).await;
    assert!(!src.matches.is_empty());
    assert!(src.matches.iter().all(|m| m.path == "src/users.ts"));

    let none = run(&f.state, request("return", Vec::new(), &[], None)).await;
    assert!(none.matches.is_empty());
    assert_eq!(none.files_searched, 0);
}

#[tokio::test]
async fn skipped_files_are_not_searched() {
    let f = fixture().await;
    index_files(
        &f.state,
        f.api,
        &[("data.bin", b"getUserById\0\0".as_slice())],
    )
    .await;

    let response = run(&f.state, request("getUserById", vec![f.api], &[], None)).await;
    assert!(response.matches.iter().all(|m| m.path == "src/users.ts"));
    assert_eq!(response.files_searched, 1);
}

#[tokio::test]
async fn line_numbers_match_get_code() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    index_files(&state, repo, &[("a.txt", "one\r\ntwo\r\nneedle")]).await;

    let response = run(&state, request("needle", vec![repo], &[], None)).await;
    assert_eq!(response.matches.len(), 1);
    assert_eq!(response.matches[0].line, 3);
    assert_eq!(response.matches[0].text, "needle");
}

#[tokio::test]
async fn long_matching_lines_are_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo = insert_repo_async(&state, 1, "acme/api", "connected").await;
    // Ten short lines keep the average line below the minified threshold, so the file is indexed.
    let content = format!("needle{}\n{}", "x".repeat(5_000), "short\n".repeat(10));
    index_files(&state, repo, &[("long.txt", content)]).await;

    let response = run(&state, request("needle", vec![repo], &[], None)).await;
    assert_eq!(response.matches.len(), 1);
    let text = &response.matches[0].text;
    assert!(text.ends_with(" chars]"), "{text}");
    assert!(text.contains("… [+"), "{text}");
    assert!(text.chars().count() < 300, "{text}");
}

#[tokio::test]
async fn invalid_patterns_are_rejected() {
    let f = fixture().await;
    let all = vec![f.api, f.orders];
    let long = "a".repeat(1_001);
    let cases = [
        (
            GrepRequest {
                regex: true,
                ..request("foo(", all.clone(), &[], None)
            },
            "invalid_pattern",
        ),
        (request("", all.clone(), &[], None), "empty_pattern"),
        (request("a\nb", all.clone(), &[], None), "invalid_pattern"),
        (request(&long, all, &[], None), "invalid_pattern"),
    ];
    for (req, code) in cases {
        let pattern = req.pattern.clone();
        let err = grep::grep(&f.state, req).await.unwrap_err();
        assert_eq!(err.status, StatusCode::UNPROCESSABLE_ENTITY, "{pattern}");
        assert_eq!(err.code, code, "{pattern}");
    }
}

#[tokio::test]
async fn an_exhausted_time_budget_is_reported() {
    let f = fixture().await;
    let response = grep::grep_within(
        &f.state,
        request("return", vec![f.api, f.orders], &[], None),
        Duration::ZERO,
    )
    .await
    .unwrap();
    assert!(response.timed_out);
    assert_eq!(response.files_searched, 0);
    assert!(response.matches.is_empty());
}
