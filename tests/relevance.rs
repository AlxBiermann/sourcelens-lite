//! Retrieval quality of lexical search on a small sample service (`tests/fixtures/relevance`).
//!
//! Each query names the file and symbol a developer would want. A hit is relevant when its path
//! matches and its line range overlaps the symbol's. The test reports MRR@8 and recall@8 per
//! query class and fails when the overall MRR drops below [`FLOOR`]; run it with `--nocapture`
//! to see the rank table.

mod common;

use common::{index_files, insert_repo_async, new_state};
use sourcelens::search::{self, Filter, SearchHit, SearchRequest};
use sourcelens::state::AppState;

/// Results judged per query.
const K: usize = 8;
/// Measured overall MRR@8 (0.883 on 2026-11-17) rounded down to two decimals, minus 0.02.
const FLOOR: f64 = 0.86;

macro_rules! corpus {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_str!(concat!("fixtures/relevance/", $path)))),*]
    };
}

const CORPUS: &[(&str, &str)] = corpus![
    "web/src/api/users.ts",
    "web/src/api/orders.ts",
    "web/src/auth/session.ts",
    "web/src/components/CartSummary.tsx",
    "web/src/utils/retry.ts",
    "services/billing/src/invoice.rs",
    "services/billing/src/payment.rs",
    "services/billing/src/ledger.rs",
    "services/billing/src/webhook.rs",
    "workers/inventory/stock.py",
    "workers/notifications/email_sender.py",
    "workers/reports/sales_report.py",
    "gateway/ratelimit.go",
    "gateway/router.go",
    "docs/architecture.md",
    "docs/runbook.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Identifier,
    Keyword,
    Natural,
}

const CLASSES: [Class; 3] = [Class::Identifier, Class::Keyword, Class::Natural];

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Identifier => "identifier",
            Class::Keyword => "keyword",
            Class::Natural => "natural",
        }
    }
}

struct Case {
    class: Class,
    query: &'static str,
    path: &'static str,
    /// Name in the `symbols` table (heading text for Markdown sections).
    symbol: &'static str,
}

const fn case(class: Class, query: &'static str, path: &'static str, symbol: &'static str) -> Case {
    Case {
        class,
        query,
        path,
        symbol,
    }
}

use Class::{Identifier as Id, Keyword as Kw, Natural as Nl};

/// Identifiers in the defined form, in the other naming convention and as partial names; two-word
/// keyword queries; questions in the words of someone who has not read the code.
#[rustfmt::skip] // one case per line
const CASES: &[Case] = &[
    case(Id, "getUserById", "web/src/api/users.ts", "getUserById"),
    case(Id, "calculate_tax", "services/billing/src/invoice.rs", "calculate_tax"),
    case(Id, "reserve_stock", "workers/inventory/stock.py", "reserve_stock"),
    case(Id, "exponential_backoff", "web/src/utils/retry.ts", "withExponentialBackoff"),
    case(Id, "balanceForAccount", "services/billing/src/ledger.rs", "balance_for_account"),
    case(Id, "updateEmail", "web/src/api/users.ts", "updateUserEmail"),
    case(Id, "verify_signature", "services/billing/src/webhook.rs", "verify_webhook_signature"),
    case(Id, "TokenBucket.Allow", "gateway/ratelimit.go", "Allow"),
    case(Id, "renderTemplate", "workers/notifications/email_sender.py", "render_email_template"),
    case(Id, "OrderHistory", "web/src/api/orders.ts", "fetchOrderHistory"),
    case(Kw, "cancel order", "web/src/api/orders.ts", "cancelOrder"),
    case(Kw, "refund payment", "services/billing/src/payment.rs", "refund_payment"),
    case(Kw, "idempotency key", "web/src/api/orders.ts", "createOrder"),
    case(Kw, "pagination cursor", "web/src/api/orders.ts", "fetchOrderHistory"),
    case(Kw, "retry jitter", "web/src/utils/retry.ts", "withExponentialBackoff"),
    case(Kw, "tax rate", "services/billing/src/invoice.rs", "calculate_tax"),
    case(Kw, "reverse proxy", "gateway/router.go", "RegisterRoutes"),
    case(Kw, "secret rotation", "docs/runbook.md", "Rotating secrets"),
    case(Kw, "access log", "gateway/router.go", "LoggingMiddleware"),
    case(Kw, "stock reservation", "workers/inventory/stock.py", "reserve_stock"),
    case(Nl, "how to charge a credit card", "services/billing/src/payment.rs", "charge_credit_card"),
    case(Nl, "wait longer after each failure", "web/src/utils/retry.ts", "withExponentialBackoff"),
    case(Nl, "send email when order is received", "workers/notifications/email_sender.py", "send_order_confirmation"),
    case(Nl, "limit requests per API key", "gateway/ratelimit.go", "RateLimitMiddleware"),
    case(Nl, "what happens when payment is declined", "docs/runbook.md", "Handling failed payments"),
    case(Nl, "prevent replayed webhook requests", "services/billing/src/webhook.rs", "verify_webhook_signature"),
    case(Nl, "get a new access token", "web/src/auth/session.ts", "refreshAccessToken"),
    case(Nl, "increase inventory when supplier delivers", "workers/inventory/stock.py", "restock_from_shipment"),
    case(Nl, "order states from checkout to shipping", "docs/architecture.md", "Order lifecycle"),
    case(Nl, "record money movement in ledger", "services/billing/src/ledger.rs", "record_transaction"),
];

/// Rank (1-based, within [`K`]) of the first relevant hit of one case.
struct Outcome {
    rank: Option<usize>,
    top: String,
}

impl Outcome {
    fn reciprocal(&self) -> f64 {
        self.rank.map_or(0.0, |rank| 1.0 / rank as f64)
    }
}

/// `(MRR@K, recall@K)` of the outcomes; with one relevant symbol per query, recall is the share
/// of queries that find it at all.
fn metrics<'a>(outcomes: impl Iterator<Item = &'a Outcome>) -> (f64, f64) {
    let (mut n, mut rr, mut found) = (0usize, 0.0, 0usize);
    for outcome in outcomes {
        n += 1;
        rr += outcome.reciprocal();
        found += usize::from(outcome.rank.is_some());
    }
    let n = n.max(1) as f64;
    (rr / n, found as f64 / n)
}

/// Line range of `symbol` in `path`; panics when the fixture does not define it.
async fn symbol_lines(
    state: &AppState,
    repo_id: i64,
    path: &'static str,
    symbol: &'static str,
) -> (u32, u32) {
    state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT s.start_line, s.end_line FROM symbols s JOIN files f ON f.id = s.file_id
                 WHERE f.repo_id = ?1 AND f.path = ?2 AND s.name = ?3
                 ORDER BY s.depth, s.start_line LIMIT 1",
                rusqlite::params![repo_id, path, symbol],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
        })
        .await
        .unwrap_or_else(|err| panic!("{path} defines no symbol {symbol:?}: {err}"))
}

fn label(hit: &SearchHit) -> String {
    let name = if hit.symbol_path.is_empty() {
        hit.names.join(", ")
    } else {
        hit.symbol_path.clone()
    };
    format!("{}:{}-{} {name}", hit.path, hit.start_line, hit.end_line)
}

fn report(outcomes: &[Outcome]) -> String {
    let mut out = String::from("rank | class | query | expected | top hit\n");
    for (case, outcome) in CASES.iter().zip(outcomes) {
        let rank = outcome
            .rank
            .map_or_else(|| "-".to_string(), |rank| rank.to_string());
        out.push_str(&format!(
            "{rank} | {} | {} | {} {} | {}\n",
            case.class.label(),
            case.query,
            case.path,
            case.symbol,
            outcome.top
        ));
    }
    for class in CLASSES {
        let in_class = CASES
            .iter()
            .zip(outcomes)
            .filter(|(case, _)| case.class == class);
        let (mrr, recall) = metrics(in_class.map(|(_, outcome)| outcome));
        out.push_str(&format!(
            "{}: MRR@{K} {mrr:.3}, recall@{K} {recall:.3}\n",
            class.label()
        ));
    }
    let (mrr, recall) = metrics(outcomes.iter());
    out.push_str(&format!(
        "overall: MRR@{K} {mrr:.3}, recall@{K} {recall:.3}\n"
    ));
    out
}

#[tokio::test]
async fn lexical_relevance_floor() {
    assert_eq!(CORPUS.len(), 16);
    assert_eq!(CASES.len(), 30);
    for class in CLASSES {
        assert_eq!(CASES.iter().filter(|case| case.class == class).count(), 10);
    }

    let tmp = tempfile::tempdir().unwrap();
    let state = new_state(&tmp, |_| {});
    let repo_id = insert_repo_async(&state, 1, "shopflow/monorepo", "connected").await;
    index_files(&state, repo_id, CORPUS).await;

    let mut outcomes = Vec::with_capacity(CASES.len());
    for case in CASES {
        let (start, end) = symbol_lines(&state, repo_id, case.path, case.symbol).await;
        let response = search::search(
            &state,
            SearchRequest {
                query: case.query.to_string(),
                filter: Filter {
                    repo_ids: vec![repo_id],
                    ..Filter::default()
                },
                limit: Some(K),
                offset: 0,
            },
        )
        .await
        .unwrap();
        assert_eq!(response.mode, "lexical", "no embeddings are configured");
        let rank = response
            .hits
            .iter()
            .position(|hit| hit.path == case.path && hit.start_line <= end && hit.end_line >= start)
            .map(|index| index + 1);
        let top = response
            .hits
            .first()
            .map_or_else(|| "(no hits)".to_string(), label);
        outcomes.push(Outcome { rank, top });
    }

    let table = report(&outcomes);
    println!("{table}");
    let (mrr, _) = metrics(outcomes.iter());
    assert!(
        mrr >= FLOOR,
        "MRR@{K} {mrr:.3} is below the floor {FLOOR}\n{table}"
    );
}
