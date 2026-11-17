//! Dev mode (`SOURCELENS_DEV_MODE`): sign-in without GitHub and demo data, so every screen can be
//! looked at without a GitHub App. Nothing here is routed or run unless `cfg.dev_mode` is set, and
//! `Config::from_env` allows it only on this computer.
//!
//! The dev accounts are SSO-type accounts (`sso_sub` set): GitHub-type accounts without a GitHub
//! token are asked to sign in again.

use axum::extract::{Query, State};
use axum::response::{AppendHeaders, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::Router;
use rusqlite::{params, Connection, TransactionBehavior};
use serde::Deserialize;

use crate::crypto::{git_blob_sha1, random_hex, sha256_hex};
use crate::error::ApiResult;
use crate::http::auth::{safe_next, session_header, HOME};
use crate::http::keys::KEY_PREFIX;
use crate::http::session;
use crate::index::chunker::CHUNKER_VERSION;
use crate::index::pipeline::NO_ACCESS_REASON;
use crate::index::store::{self, PreparedFile};
use crate::model::{JobPhase, JobStatus, RepoState, Role, Trigger};
use crate::repositories::{self, NewRepository};
use crate::state::{now, AppState, SECS_PER_DAY};

/// `users.sso_issuer` of every dev and demo SSO account.
const ISSUER: &str = "dev";
/// Installation id stored for the demo repositories; no such installation exists.
const DEMO_INSTALLATION_ID: u64 = 1;
const HOUR: i64 = 3600;

/// An account the dev sign-in signs in as.
struct DevAccount {
    sub: &'static str,
    login: &'static str,
    name: &'static str,
    role: Role,
}

const ADMIN: DevAccount = DevAccount {
    sub: "admin",
    login: "dev-admin@example.com",
    name: "Dev Admin",
    role: Role::Owner,
};

const MEMBER: DevAccount = DevAccount {
    sub: "member",
    login: "dev-member@example.com",
    name: "Dev Member",
    role: Role::Member,
};

/// `GET /auth/dev/login`.
pub fn routes() -> Router<AppState> {
    Router::new().route("/auth/dev/login", get(login))
}

/// Start of a dev-mode instance: makes sure both dev accounts exist and, on an empty database,
/// adds demo members, repositories, jobs, MCP clients and an API key. One transaction; a second
/// start changes nothing.
pub async fn prepare(state: &AppState) -> ApiResult<()> {
    let seeded = state
        .db
        .write(|conn| -> rusqlite::Result<bool> {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let empty: bool = tx.query_row(
                "SELECT NOT EXISTS (SELECT 1 FROM users) AND NOT EXISTS (SELECT 1 FROM repositories)",
                [],
                |row| row.get(0),
            )?;
            let admin = ensure_account(&tx, &ADMIN)?;
            let member = ensure_account(&tx, &MEMBER)?;
            if empty {
                seed(&tx, admin, member, now())?;
            }
            tx.commit()?;
            Ok(empty)
        })
        .await?;
    if seeded {
        tracing::info!("dev mode: demo data added to the empty database");
    }
    Ok(())
}

/// Inserts the account, or gives an existing one its role back and approves it, so each dev
/// sign-in button always signs in with the role it names.
fn ensure_account(conn: &Connection, account: &DevAccount) -> rusqlite::Result<i64> {
    conn.query_row(
        "INSERT INTO users (login, name, email, role, status, sso_issuer, sso_sub, created_at)
         VALUES (?1, ?2, ?1, ?3, 'active', ?4, ?5, ?6)
         ON CONFLICT (sso_issuer, sso_sub) WHERE sso_sub IS NOT NULL
           DO UPDATE SET role = excluded.role, status = 'active'
         RETURNING id",
        params![
            account.login,
            account.name,
            account.role,
            ISSUER,
            account.sub,
            now()
        ],
        |row| row.get(0),
    )
}

#[derive(Deserialize)]
struct LoginQuery {
    /// `admin` (default) or `member`.
    #[serde(rename = "as")]
    account: Option<String>,
    next: Option<String>,
}

async fn login(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    let account = match query.account.as_deref() {
        None | Some("admin") => &ADMIN,
        Some("member") => &MEMBER,
        Some(_) => return Redirect::to("/login").into_response(),
    };
    let target = safe_next(query.next).unwrap_or_else(|| HOME.to_string());
    match sign_in(&state, account).await {
        Ok(session_value) => (
            AppendHeaders([session_header(&state, &session_value)]),
            Redirect::to(&target),
        )
            .into_response(),
        Err(err) => {
            tracing::error!("dev sign-in failed: {err}");
            Redirect::to("/login").into_response()
        }
    }
}

/// Session cookie value of a new session of the dev account.
async fn sign_in(state: &AppState, account: &'static DevAccount) -> ApiResult<String> {
    let id = state
        .db
        .write(move |conn| -> rusqlite::Result<i64> {
            let id = ensure_account(conn, account)?;
            conn.execute(
                "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
                params![id, now()],
            )?;
            Ok(id)
        })
        .await?;
    session::create_session(state, id, None).await
}

// ── Demo data ────────────────────────────────────────────────────────────────

/// A demo account; `sso_sub` set → SSO account of [`ISSUER`], else a GitHub account.
struct DemoUser {
    login: &'static str,
    name: Option<&'static str>,
    email: Option<&'static str>,
    role: Role,
    /// `pending` = waits for approval.
    status: &'static str,
    github_id: Option<i64>,
    sso_sub: Option<&'static str>,
    created_ago: i64,
    /// `None` = never signed in (invited, or approved and not back yet).
    login_ago: Option<i64>,
}

const DEMO_USERS: [DemoUser; 7] = [
    DemoUser {
        login: "octocat",
        name: Some("The Octocat"),
        email: None,
        role: Role::Owner,
        status: "active",
        github_id: Some(583_231),
        sso_sub: None,
        created_ago: 30 * SECS_PER_DAY,
        login_ago: Some(2 * HOUR),
    },
    DemoUser {
        login: "hubot",
        name: Some("Hubot"),
        email: None,
        role: Role::Member,
        status: "active",
        github_id: Some(480_938),
        sso_sub: None,
        created_ago: 21 * SECS_PER_DAY,
        login_ago: Some(3 * SECS_PER_DAY),
    },
    DemoUser {
        login: "monalisa",
        name: None,
        email: None,
        role: Role::Member,
        status: "active",
        github_id: Some(2),
        sso_sub: None,
        created_ago: 4 * SECS_PER_DAY,
        login_ago: None,
    },
    DemoUser {
        login: "jane.doe@example.com",
        name: Some("Jane Doe"),
        email: Some("jane.doe@example.com"),
        role: Role::Member,
        status: "pending",
        github_id: None,
        sso_sub: Some("demo-jane"),
        created_ago: 5 * HOUR,
        login_ago: None,
    },
    DemoUser {
        login: "maximilian.alexander.featherstonehaugh-montgomery@subsidiary.example.com",
        name: Some("Maximilian Alexander Featherstonehaugh-Montgomery"),
        email: Some("maximilian.alexander.featherstonehaugh-montgomery@subsidiary.example.com"),
        role: Role::Member,
        status: "pending",
        github_id: None,
        sso_sub: Some("demo-maximilian"),
        created_ago: 40 * 60,
        login_ago: None,
    },
    DemoUser {
        login: "sam.lee@example.com",
        name: Some("Sam Lee"),
        email: Some("sam.lee@example.com"),
        role: Role::Member,
        status: "active",
        github_id: None,
        sso_sub: Some("demo-sam"),
        created_ago: 2 * SECS_PER_DAY,
        login_ago: None,
    },
    DemoUser {
        login: "ops-admin@example.com",
        name: Some("Ops Admin"),
        email: Some("ops-admin@example.com"),
        role: Role::Owner,
        status: "active",
        github_id: None,
        sso_sub: Some("demo-ops"),
        created_ago: 12 * SECS_PER_DAY,
        login_ago: Some(SECS_PER_DAY),
    },
];

/// A finished demo job; `commit` is `None` for jobs that failed before resolving the branch head.
struct DemoJob {
    trigger: Trigger,
    status: JobStatus,
    commit: Option<String>,
    files_total: i64,
    files_changed: i64,
    files_deleted: i64,
    files_skipped: i64,
    error: Option<String>,
    created_ago: i64,
    /// Seconds from start to finish.
    duration: i64,
}

impl DemoJob {
    fn succeeded(trigger: Trigger, commit: &str, created_ago: i64, duration: i64) -> Self {
        Self {
            trigger,
            status: JobStatus::Succeeded,
            commit: Some(commit.to_string()),
            files_total: 0,
            files_changed: 0,
            files_deleted: 0,
            files_skipped: 0,
            error: None,
            created_ago,
            duration,
        }
    }

    fn changed(mut self, total: i64, changed: i64, deleted: i64, skipped: i64) -> Self {
        (
            self.files_total,
            self.files_changed,
            self.files_deleted,
            self.files_skipped,
        ) = (total, changed, deleted, skipped);
        self
    }
}

fn seed(conn: &Connection, admin: i64, member: i64, ts: i64) -> rusqlite::Result<()> {
    let mut setup_admin = None;
    for user in &DEMO_USERS {
        let id: i64 = conn.query_row(
            "INSERT INTO users (github_id, login, name, email, role, status, sso_issuer, sso_sub,
                                created_at, last_login_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             RETURNING id",
            params![
                user.github_id,
                user.login,
                user.name,
                user.email,
                user.role,
                user.status,
                user.sso_sub.map(|_| ISSUER),
                user.sso_sub,
                ts - user.created_ago,
                user.login_ago.map(|ago| ts - ago),
            ],
            |row| row.get(0),
        )?;
        setup_admin.get_or_insert(id);
    }
    let added_by = setup_admin;

    // acme/api: connected, indexed by a connect job, a push and a scheduled sync.
    let api = insert_repo(
        conn,
        9_000_001,
        "acme/api",
        true,
        added_by,
        ts - 14 * SECS_PER_DAY,
    )?;
    let api_files = prepare_files(API_FILES);
    let indexed = index(conn, api, &api_files)?;
    let commits = demo_commits("acme/api", 3);
    record_jobs(
        conn,
        api,
        ts,
        &[
            DemoJob::succeeded(Trigger::Connect, &commits[0], 14 * SECS_PER_DAY, 41)
                .changed(indexed, indexed, 0, 0),
            DemoJob::succeeded(Trigger::Webhook, &commits[1], 2 * SECS_PER_DAY, 6)
                .changed(2, 2, 0, 0),
            DemoJob::succeeded(Trigger::Schedule, &commits[2], 3 * HOUR, 5).changed(1, 1, 1, 0),
        ],
    )?;
    repositories::record_index(conn, api, &commits[2], ts - 3 * HOUR + 5, CHUNKER_VERSION)?;

    // acme/web: connected, one generated file skipped, a manual sync that found nothing new.
    let web = insert_repo(
        conn,
        9_000_002,
        "acme/web",
        false,
        added_by,
        ts - 9 * SECS_PER_DAY,
    )?;
    let web_files = prepare_files(WEB_FILES);
    let indexed = index(conn, web, &web_files)?;
    let skipped = i64::try_from(web_files.len()).unwrap_or(i64::MAX) - indexed;
    let commits = demo_commits("acme/web", 1);
    record_jobs(
        conn,
        web,
        ts,
        &[
            DemoJob::succeeded(Trigger::Connect, &commits[0], 9 * SECS_PER_DAY, 27).changed(
                indexed + skipped,
                indexed,
                0,
                skipped,
            ),
            DemoJob::succeeded(Trigger::Manual, &commits[0], SECS_PER_DAY, 2),
        ],
    )?;
    repositories::record_index(
        conn,
        web,
        &commits[0],
        ts - 9 * SECS_PER_DAY + 27,
        CHUNKER_VERSION,
    )?;

    // acme/legacy: indexed once, then the GitHub App lost access to it.
    let legacy = insert_repo(
        conn,
        9_000_003,
        "acme/legacy",
        true,
        added_by,
        ts - 60 * SECS_PER_DAY,
    )?;
    let legacy_files = prepare_files(LEGACY_FILES);
    let indexed = index(conn, legacy, &legacy_files)?;
    let commits = demo_commits("acme/legacy", 1);
    record_jobs(
        conn,
        legacy,
        ts,
        &[
            DemoJob::succeeded(Trigger::Connect, &commits[0], 60 * SECS_PER_DAY, 12)
                .changed(indexed, indexed, 0, 0),
            DemoJob {
                trigger: Trigger::Webhook,
                status: JobStatus::Failed,
                commit: None,
                files_total: 0,
                files_changed: 0,
                files_deleted: 0,
                files_skipped: 0,
                error: Some("The GitHub App has no access to acme/legacy.".into()),
                created_ago: 20 * SECS_PER_DAY,
                duration: 1,
            },
        ],
    )?;
    repositories::record_index(
        conn,
        legacy,
        &commits[0],
        ts - 60 * SECS_PER_DAY + 12,
        CHUNKER_VERSION,
    )?;
    conn.execute(
        "UPDATE repositories SET state = ?2, state_reason = ?3 WHERE id = ?1",
        params![legacy, RepoState::Disconnected, NO_ACCESS_REASON],
    )?;

    insert_grant(
        conn,
        admin,
        "Claude Code",
        ts - 5 * SECS_PER_DAY,
        Some(ts - HOUR),
        ts,
    )?;
    insert_grant(conn, member, "Cursor", ts - 2 * SECS_PER_DAY, None, ts)?;
    insert_api_key(conn, admin, "CI pipeline", ts)?;
    Ok(())
}

fn insert_repo(
    conn: &Connection,
    github_id: i64,
    full_name: &str,
    private: bool,
    added_by: Option<i64>,
    created_at: i64,
) -> rusqlite::Result<i64> {
    repositories::insert(
        conn,
        &NewRepository {
            github_id,
            installation_id: DEMO_INSTALLATION_ID,
            full_name: full_name.to_string(),
            private,
            default_branch: "main".to_string(),
            added_by,
            created_at,
        },
    )
}

fn prepare_files(files: &[(&str, &str)]) -> Vec<PreparedFile> {
    files
        .iter()
        .map(|(path, content)| {
            store::prepare_file(path, &git_blob_sha1(content.as_bytes()), content.as_bytes())
        })
        .collect()
}

/// Writes the files through the real store; returns the number indexed (not skipped).
fn index(conn: &Connection, repo_id: i64, files: &[PreparedFile]) -> rusqlite::Result<i64> {
    let stats = store::write_files(conn, repo_id, files)?;
    Ok(i64::from(stats.indexed))
}

/// `count` made-up commit SHAs of the repository, oldest first.
fn demo_commits(full_name: &str, count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| git_blob_sha1(format!("{full_name} demo commit {n}").as_bytes()))
        .collect()
}

/// Inserts finished jobs, oldest first, so the activity list shows the newest on top.
fn record_jobs(conn: &Connection, repo_id: i64, ts: i64, jobs: &[DemoJob]) -> rusqlite::Result<()> {
    let mut insert = conn.prepare(
        "INSERT INTO jobs (repo_id, status, trigger, target_commit, phase, files_total, files_done,
                           files_changed, files_deleted, files_skipped, error, created_at,
                           started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )?;
    for job in jobs {
        let created_at = ts - job.created_ago;
        let phase = if job.commit.is_some() {
            JobPhase::Index
        } else {
            JobPhase::Resolve
        };
        insert.execute(params![
            repo_id,
            job.status,
            job.trigger,
            job.commit,
            phase,
            job.files_total,
            job.files_changed,
            job.files_deleted,
            job.files_skipped,
            job.error,
            created_at,
            created_at + 1,
            created_at + 1 + job.duration,
        ])?;
    }
    Ok(())
}

/// An MCP client connected with OAuth. Its tokens are hashes of random values nobody knows.
fn insert_grant(
    conn: &Connection,
    user_id: i64,
    client_name: &str,
    created_at: i64,
    last_used_at: Option<i64>,
    ts: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO oauth_grants (user_id, client_hash, client_name, access_hash, access_expires_at,
                                   refresh_hash, refresh_expires_at, created_at, last_used_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            user_id,
            sha256_hex(random_hex(32).as_bytes()),
            client_name,
            sha256_hex(random_hex(32).as_bytes()),
            ts,
            sha256_hex(random_hex(32).as_bytes()),
            ts + 30 * SECS_PER_DAY,
            created_at,
            last_used_at,
        ],
    )?;
    Ok(())
}

/// An API key for all repositories; only the hash of its secret is stored and the secret is dropped.
fn insert_api_key(conn: &Connection, created_by: i64, name: &str, ts: i64) -> rusqlite::Result<()> {
    let secret = format!("{KEY_PREFIX}{}", random_hex(32));
    conn.execute(
        "INSERT INTO api_keys (name, key_hash, last4, all_repos, created_by, created_at, expires_at,
                               last_used_at)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
        params![
            name,
            sha256_hex(secret.as_bytes()),
            &secret[secret.len() - 4..],
            created_by,
            ts - 6 * SECS_PER_DAY,
            ts + 84 * SECS_PER_DAY,
            ts - 5 * HOUR,
        ],
    )?;
    Ok(())
}

// ── Demo source files ────────────────────────────────────────────────────────

const API_FILES: &[(&str, &str)] = &[
    (
        "README.md",
        r##"# acme/api

Billing and authentication service of the Acme demo shop.

## Running

```sh
cargo run -- --port 8080
```

## Invoices

`calculate_invoice_total` sums the line items, applies the customer discount and adds tax.
Access tokens are issued by `issue_access_token` and checked by `verify_access_token`.
"##,
    ),
    (
        "src/main.rs",
        r##"use std::net::SocketAddr;

mod auth;
mod billing;

use billing::invoice::{calculate_invoice_total, Invoice};

/// Port used when `--port` is not given.
const DEFAULT_PORT: u16 = 8080;

fn main() {
    let addr = SocketAddr::from(([127, 0, 0, 1], parse_port()));
    println!("acme-api listening on {addr}");
    let invoice = Invoice::sample();
    println!("sample invoice total: {}", calculate_invoice_total(&invoice));
}

/// Reads `--port <n>` from the command line.
fn parse_port() -> u16 {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|pair| pair[0] == "--port")
        .and_then(|pair| pair[1].parse().ok())
        .unwrap_or(DEFAULT_PORT)
}
"##,
    ),
    (
        "src/billing/invoice.rs",
        r##"//! Invoices and their totals. Amounts are in cents.

/// VAT rate applied to an invoice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TaxRate {
    Standard,
    Reduced,
    Exempt,
}

impl TaxRate {
    pub fn percent(self) -> u64 {
        match self {
            TaxRate::Standard => 19,
            TaxRate::Reduced => 7,
            TaxRate::Exempt => 0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LineItem {
    pub description: String,
    pub quantity: u32,
    pub unit_price_cents: u64,
}

#[derive(Debug, Clone)]
pub struct Invoice {
    pub customer_id: u64,
    pub items: Vec<LineItem>,
    pub discount_percent: u64,
    pub tax_rate: TaxRate,
}

impl Invoice {
    pub fn sample() -> Self {
        Invoice {
            customer_id: 42,
            items: vec![LineItem {
                description: "Pro plan".into(),
                quantity: 3,
                unit_price_cents: 1_900,
            }],
            discount_percent: 10,
            tax_rate: TaxRate::Standard,
        }
    }
}

/// Sum of the line items after the discount, plus tax.
pub fn calculate_invoice_total(invoice: &Invoice) -> u64 {
    let subtotal: u64 = invoice
        .items
        .iter()
        .map(|item| u64::from(item.quantity) * item.unit_price_cents)
        .sum();
    let discounted = apply_discount(subtotal, invoice.discount_percent);
    discounted + discounted * invoice.tax_rate.percent() / 100
}

/// `amount` reduced by `percent`, never below zero.
pub fn apply_discount(amount: u64, percent: u64) -> u64 {
    amount - amount * percent.min(100) / 100
}
"##,
    ),
    (
        "src/auth/token.rs",
        r##"//! Signed access tokens for the shop's API clients.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds an access token stays valid.
pub const TOKEN_LIFETIME_SECS: u64 = 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct TokenClaims {
    pub subject: String,
    pub scopes: Vec<String>,
    pub expires_at: u64,
}

#[derive(Debug, PartialEq)]
pub enum TokenError {
    Malformed,
    Expired,
    BadSignature,
}

/// Issues a token for `subject` with the given scopes.
pub fn issue_access_token(subject: &str, scopes: &[&str], secret: &[u8]) -> String {
    let claims = TokenClaims {
        subject: subject.to_string(),
        scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
        expires_at: unix_now() + TOKEN_LIFETIME_SECS,
    };
    let payload = format!("{}|{}|{}", claims.subject, claims.scopes.join(","), claims.expires_at);
    format!("{payload}.{}", sign_payload(&payload, secret))
}

/// Checks the signature and expiry of a token from `issue_access_token`.
pub fn verify_access_token(token: &str, secret: &[u8]) -> Result<TokenClaims, TokenError> {
    let (payload, signature) = token.rsplit_once('.').ok_or(TokenError::Malformed)?;
    if sign_payload(payload, secret) != signature {
        return Err(TokenError::BadSignature);
    }
    let mut parts = payload.split('|');
    let subject = parts.next().ok_or(TokenError::Malformed)?.to_string();
    let scopes = parts.next().ok_or(TokenError::Malformed)?;
    let expires_at: u64 = parts.next().and_then(|v| v.parse().ok()).ok_or(TokenError::Malformed)?;
    if expires_at <= unix_now() {
        return Err(TokenError::Expired);
    }
    Ok(TokenClaims {
        subject,
        scopes: scopes.split(',').map(str::to_string).collect(),
        expires_at,
    })
}

fn sign_payload(payload: &str, secret: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in secret.iter().chain(payload.as_bytes()) {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
"##,
    ),
    (
        "migrations/001_invoices.sql",
        r##"CREATE TABLE customers (
  id INTEGER PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  discount_percent INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE invoices (
  id INTEGER PRIMARY KEY,
  customer_id INTEGER NOT NULL REFERENCES customers(id),
  total_cents INTEGER NOT NULL,
  issued_at INTEGER NOT NULL
);

CREATE INDEX invoices_customer ON invoices(customer_id, issued_at DESC);
"##,
    ),
    (
        "Cargo.toml",
        r##"[package]
name = "acme-api"
version = "0.4.2"
edition = "2021"

[dependencies]
"##,
    ),
];

const WEB_FILES: &[(&str, &str)] = &[
    (
        "src/api/client.ts",
        r##"export type Invoice = {
  id: number;
  customerId: number;
  totalCents: number;
  issuedAt: string;
};

/** Thin wrapper around fetch for the acme API. */
export class ApiClient {
  constructor(private readonly baseUrl: string, private readonly token: string) {}

  async fetchInvoices(customerId: number): Promise<Invoice[]> {
    return retryWithBackoff(() => this.get<Invoice[]>(`/customers/${customerId}/invoices`));
  }

  private async get<T>(path: string): Promise<T> {
    const response = await fetch(`${this.baseUrl}${path}`, {
      headers: { Authorization: `Bearer ${this.token}` },
    });
    if (!response.ok) throw new Error(`GET ${path} failed with ${response.status}`);
    return (await response.json()) as T;
  }
}

/** Runs `request` up to `attempts` times, doubling the pause after each failure. */
export async function retryWithBackoff<T>(request: () => Promise<T>, attempts = 3, pauseMs = 200): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      return await request();
    } catch (error) {
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, pauseMs * 2 ** attempt));
    }
  }
  throw lastError;
}
"##,
    ),
    (
        "src/components/InvoiceTable.tsx",
        r##"import type { Invoice } from '../api/client';
import { useInvoices } from '../hooks/useInvoices';

/** Cents as a euro amount, for example 5130 → "€51.30". */
export function formatCurrency(cents: number): string {
  return new Intl.NumberFormat('en-IE', { style: 'currency', currency: 'EUR' }).format(cents / 100);
}

function InvoiceRow({ invoice }: { invoice: Invoice }) {
  return (
    <tr>
      <td>#{invoice.id}</td>
      <td>{new Date(invoice.issuedAt).toLocaleDateString()}</td>
      <td className="amount">{formatCurrency(invoice.totalCents)}</td>
    </tr>
  );
}

export function InvoiceTable({ customerId }: { customerId: number }) {
  const { invoices, loading, error } = useInvoices(customerId);
  if (loading) return <p>Loading invoices…</p>;
  if (error) return <p role="alert">{error.message}</p>;
  return (
    <table>
      <tbody>
        {invoices.map((invoice) => (
          <InvoiceRow key={invoice.id} invoice={invoice} />
        ))}
      </tbody>
    </table>
  );
}
"##,
    ),
    (
        "src/hooks/useInvoices.ts",
        r##"import { useEffect, useState } from 'react';
import { ApiClient, type Invoice } from '../api/client';

const client = new ApiClient(import.meta.env.VITE_API_URL, import.meta.env.VITE_API_TOKEN);

/** Loads the invoices of one customer. */
export function useInvoices(customerId: number) {
  const [invoices, setInvoices] = useState<Invoice[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<Error | null>(null);

  useEffect(() => {
    setLoading(true);
    client
      .fetchInvoices(customerId)
      .then(setInvoices)
      .catch((e: Error) => setError(e))
      .finally(() => setLoading(false));
  }, [customerId]);

  return { invoices, loading, error };
}
"##,
    ),
    (
        "src/generated/schema.ts",
        r##"// Code generated by schema-gen. DO NOT EDIT.
export const SCHEMA_VERSION = 7;
export type CustomerRow = { id: number; email: string; discount_percent: number };
"##,
    ),
    (
        "scripts/export_report.py",
        r##""""Monthly revenue report as CSV."""

import csv
import sys
from dataclasses import dataclass


@dataclass
class InvoiceLine:
    customer_email: str
    total_cents: int


class ReportWriter:
    """Writes one CSV row per customer with the month's revenue."""

    def __init__(self, out):
        self.writer = csv.writer(out)

    def write_header(self):
        self.writer.writerow(["customer", "revenue_eur"])

    def write_line(self, line: InvoiceLine):
        self.writer.writerow([line.customer_email, f"{line.total_cents / 100:.2f}"])


def export_monthly_report(lines, out=sys.stdout):
    """Sums the invoices per customer and writes them sorted by revenue."""
    totals = {}
    for line in lines:
        totals[line.customer_email] = totals.get(line.customer_email, 0) + line.total_cents
    writer = ReportWriter(out)
    writer.write_header()
    for email, cents in sorted(totals.items(), key=lambda item: -item[1]):
        writer.write_line(InvoiceLine(email, cents))


if __name__ == "__main__":
    export_monthly_report([InvoiceLine("demo@example.com", 5130)])
"##,
    ),
    (
        "src/styles/table.css",
        r##"table {
  width: 100%;
  border-collapse: collapse;
}

td.amount {
  text-align: right;
  font-variant-numeric: tabular-nums;
  color: #1f2328;
}
"##,
    ),
];

const LEGACY_FILES: &[(&str, &str)] = &[
    (
        "cmd/worker/main.go",
        r##"package main

import (
	"log"
	"time"
)

// Job is one queued e-mail reminder.
type Job struct {
	ID        int
	Recipient string
	DueAt     time.Time
}

func main() {
	queue := make(chan Job, 16)
	go processQueue(queue)
	queue <- Job{ID: 1, Recipient: "billing@example.com", DueAt: time.Now()}
	close(queue)
	time.Sleep(time.Second)
}

// processQueue sends a reminder for every job that is due.
func processQueue(queue <-chan Job) {
	for job := range queue {
		if time.Now().Before(job.DueAt) {
			continue
		}
		log.Printf("sending reminder %d to %s", job.ID, job.Recipient)
	}
}
"##,
    ),
    (
        "lib/mailer.rb",
        r##"# Sends invoice e-mails through the legacy SMTP relay.
class Mailer
  def initialize(relay_host)
    @relay_host = relay_host
  end

  def send_invoice_reminder(customer_email, invoice_id)
    deliver(customer_email, "Reminder: invoice ##{invoice_id} is due")
  end

  private

  def deliver(to, subject)
    puts "via #{@relay_host}: #{subject} -> #{to}"
  end
end
"##,
    ),
];
