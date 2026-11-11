use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use sourcelens::config::Config;
use sourcelens::crypto::git_blob_sha1;
use sourcelens::github::app::AppCredentials;
use sourcelens::index::jobs;
use sourcelens::index::pipeline::{archive_threads_running, TARBALL_THRESHOLD};
use sourcelens::model::Trigger;
use sourcelens::state::{now, AppState};
use tempfile::TempDir;

const INSTALLATION_ID: i64 = 42;

/// Held by every test that runs an archive pipeline, so `archive_threads_running` only sees the
/// current test's threads.
static ARCHIVE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── Fake GitHub ──────────────────────────────────────────────────────────────

#[derive(Default)]
struct Fake {
    /// commit sha → path → content
    commits: HashMap<String, BTreeMap<String, Vec<u8>>>,
    /// branch → commit sha
    branches: HashMap<String, String>,
    truncated: bool,
    /// paths left out of tarballs
    archive_omit: HashSet<String>,
    /// serve only the first half of the tarball bytes
    archive_cut: bool,
    /// runs while a tarball is served
    tarball_hook: Option<Box<dyn Fn() + Send>>,
    token_status: Option<StatusCode>,
    installation_status: Option<StatusCode>,
    tree_requests: usize,
    blob_requests: usize,
    tarball_requests: usize,
}

type Shared = Arc<Mutex<Fake>>;

impl Fake {
    fn set_commit(&mut self, sha: &str, files: &[(&str, &[u8])]) {
        let files = files
            .iter()
            .map(|(path, content)| (path.to_string(), content.to_vec()))
            .collect();
        self.commits.insert(sha.to_string(), files);
        self.branches.insert("main".into(), sha.to_string());
    }
}

fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "message": "Not Found" })),
    )
        .into_response()
}

async fn access_token(State(fake): State<Shared>, Path(_id): Path<u64>) -> Response {
    let status = fake.lock().unwrap().token_status;
    match status {
        Some(status) => (status, Json(json!({ "message": "Not Found" }))).into_response(),
        None => (
            StatusCode::CREATED,
            Json(json!({ "token": "ghs_fake", "expires_at": "2099-01-01T00:00:00Z" })),
        )
            .into_response(),
    }
}

async fn installation(
    State(fake): State<Shared>,
    Path((_o, _r)): Path<(String, String)>,
) -> Response {
    let status = fake.lock().unwrap().installation_status;
    match status {
        Some(status) => (status, Json(json!({ "message": "Not Found" }))).into_response(),
        None => Json(json!({ "id": INSTALLATION_ID })).into_response(),
    }
}

async fn branch(
    State(fake): State<Shared>,
    Path((_o, _r, branch)): Path<(String, String, String)>,
) -> Response {
    let fake = fake.lock().unwrap();
    match fake.branches.get(&branch) {
        Some(sha) => Json(json!({ "name": branch, "commit": { "sha": sha } })).into_response(),
        None => not_found(),
    }
}

async fn branch_list(
    State(fake): State<Shared>,
    Path((_o, _r)): Path<(String, String)>,
) -> Response {
    let fake = fake.lock().unwrap();
    let names: BTreeSet<&String> = fake.branches.keys().collect();
    Json(
        names
            .into_iter()
            .map(|name| json!({ "name": name }))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

async fn tree(
    State(fake): State<Shared>,
    Path((_o, _r, sha)): Path<(String, String, String)>,
) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.tree_requests += 1;
    let Some(files) = fake.commits.get(&sha) else {
        return not_found();
    };
    let mut entries = Vec::new();
    let mut dirs = BTreeSet::new();
    for (path, content) in files {
        let mut prefix = String::new();
        for part in path.split('/').collect::<Vec<_>>().split_last().unwrap().1 {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            dirs.insert(prefix.clone());
        }
        entries.push(json!({
            "path": path, "mode": "100644", "type": "blob",
            "sha": git_blob_sha1(content), "size": content.len()
        }));
    }
    for dir in dirs {
        entries
            .push(json!({ "path": dir, "mode": "040000", "type": "tree", "sha": "d".repeat(40) }));
    }
    // A symlink and a submodule are never indexed.
    entries.push(json!({ "path": "link", "mode": "120000", "type": "blob", "sha": "e".repeat(40), "size": 7 }));
    entries
        .push(json!({ "path": "sub", "mode": "160000", "type": "commit", "sha": "f".repeat(40) }));
    if fake.truncated {
        entries.truncate(1);
    }
    Json(json!({ "sha": sha, "tree": entries, "truncated": fake.truncated })).into_response()
}

async fn blob(
    State(fake): State<Shared>,
    Path((_o, _r, sha)): Path<(String, String, String)>,
) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.blob_requests += 1;
    let found = fake
        .commits
        .values()
        .flat_map(|files| files.values())
        .find(|content| git_blob_sha1(content) == sha)
        .cloned();
    match found {
        Some(content) => content.into_response(),
        None => not_found(),
    }
}

async fn tarball(
    State(fake): State<Shared>,
    Path((owner, repo, sha)): Path<(String, String, String)>,
) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.tarball_requests += 1;
    if let Some(hook) = &fake.tarball_hook {
        hook();
    }
    let Some(files) = fake.commits.get(&sha) else {
        return not_found();
    };
    let prefix = format!("{owner}-{repo}-{}", &sha[..7]);
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut builder = tar::Builder::new(gz);

    let comment = format!("52 comment={sha}\n");
    let mut global = tar::Header::new_ustar();
    global.set_entry_type(tar::EntryType::XGlobalHeader);
    global.set_size(comment.len() as u64);
    global.set_cksum();
    builder
        .append_data(&mut global, "pax_global_header", comment.as_bytes())
        .unwrap();
    let mut dir = tar::Header::new_ustar();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_size(0);
    dir.set_mode(0o755);
    builder
        .append_data(&mut dir, format!("{prefix}/"), std::io::empty())
        .unwrap();
    for (path, content) in files {
        if fake.archive_omit.contains(path) {
            continue;
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, format!("{prefix}/{path}"), content.as_slice())
            .unwrap();
    }
    let mut gz = builder.into_inner().unwrap();
    gz.flush().unwrap();
    let mut bytes = gz.finish().unwrap();
    if fake.archive_cut {
        bytes.truncate(bytes.len() / 2);
    }
    bytes.into_response()
}

async fn start_fake(fake: Shared) -> String {
    let app = Router::new()
        .route("/app/installations/{id}/access_tokens", post(access_token))
        .route("/repos/{o}/{r}/installation", get(installation))
        .route("/repos/{o}/{r}/branches", get(branch_list))
        .route("/repos/{o}/{r}/branches/{b}", get(branch))
        .route("/repos/{o}/{r}/git/trees/{sha}", get(tree))
        .route("/repos/{o}/{r}/git/blobs/{sha}", get(blob))
        .route("/repos/{o}/{r}/tarball/{sha}", get(tarball))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ── Test environment ─────────────────────────────────────────────────────────

struct Env {
    tmp: TempDir,
    state: AppState,
    fake: Shared,
    repo_id: i64,
}

fn credentials() -> AppCredentials {
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/test_only_app_key.pem"
    ))
    .unwrap();
    AppCredentials {
        app_id: 4242,
        slug: "sourcelens-test".into(),
        client_id: "Iv1.test".into(),
        client_secret: "client-secret".into(),
        webhook_secret: Some("hook-secret".into()),
        private_key_pem: pem,
        html_url: "https://github.com/apps/sourcelens-test".into(),
        source: "env",
    }
}

async fn setup() -> Env {
    setup_with(|_| {}).await
}

async fn setup_with(configure: impl FnOnce(&mut Config)) -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let fake: Shared = Arc::default();
    let base = start_fake(fake.clone()).await;
    let mut cfg = Config::new("http://localhost:3000", tmp.path());
    cfg.github = Some(credentials());
    cfg.github_api_url = base;
    configure(&mut cfg);
    let state = AppState::new(cfg).unwrap();
    let repo_id = state
        .db
        .write(|conn| {
            conn.query_row(
                "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, created_at)
                 VALUES (1001, ?1, 'acme/api', 0, 'main', ?2) RETURNING id",
                rusqlite::params![INSTALLATION_ID, now()],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    Env {
        tmp,
        state,
        fake,
        repo_id,
    }
}

#[derive(Debug, Default)]
struct JobRow {
    status: String,
    error: Option<String>,
    files_total: i64,
    files_done: i64,
    files_changed: i64,
    files_deleted: i64,
    files_skipped: i64,
}

impl Env {
    fn fake(&self) -> std::sync::MutexGuard<'_, Fake> {
        self.fake.lock().unwrap()
    }

    /// Queues a job, runs it through `run_next` and returns its row (`None` when it was discarded).
    async fn run(&self, trigger: &str, force: bool) -> Option<JobRow> {
        self.run_target(trigger, None, force).await
    }

    /// [`Env::run`] with a pushed `target_commit` on the queued job.
    async fn run_target(&self, trigger: &str, target: Option<&str>, force: bool) -> Option<JobRow> {
        let trigger = Trigger::parse(trigger).expect("known trigger");
        let job_id = jobs::enqueue(&self.state, self.repo_id, trigger, target, force)
            .await
            .unwrap();
        assert!(jobs::run_next(&self.state).await.unwrap());
        self.state
            .db
            .read(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(
                    "SELECT status, error, files_total, files_done, files_changed, files_deleted, files_skipped
                     FROM jobs WHERE id = ?1",
                    [job_id],
                    |row| {
                        Ok(JobRow {
                            status: row.get(0)?,
                            error: row.get(1)?,
                            files_total: row.get(2)?,
                            files_done: row.get(3)?,
                            files_changed: row.get(4)?,
                            files_deleted: row.get(5)?,
                            files_skipped: row.get(6)?,
                        })
                    },
                )
                .optional()
            })
            .await
            .unwrap()
    }

    async fn query<T: rusqlite::types::FromSql + Send + 'static>(&self, sql: &'static str) -> T {
        let repo_id = self.repo_id;
        self.state
            .db
            .read(move |conn| conn.query_row(sql, [repo_id], |row| row.get(0)))
            .await
            .unwrap()
    }

    /// path → skip_reason of every file row.
    async fn files(&self) -> BTreeMap<String, Option<String>> {
        let repo_id = self.repo_id;
        self.state
            .db
            .read(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT path, skip_reason FROM files WHERE repo_id = ?1")?;
                let rows = stmt
                    .query_map([repo_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .unwrap()
    }

    async fn fts_hits(&self, token: &'static str) -> i64 {
        self.state
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                    [token],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap()
    }
}

const LIB_RS: &[u8] = b"pub fn alpha_marker() -> u32 {\n    41 + 1\n}\n\npub struct Widget {\n    pub size: u32,\n}\n";
const UTIL_V1: &[u8] = b"def helper(value):\n    return \"quokkaword\" + str(value)\n";
const UTIL_V2: &[u8] = b"def helper(value):\n    return \"zebracorn\" + str(value)\n";
const README: &[u8] = b"# Acme API\n\nService that answers requests.\n";
const LOGO: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01";
const VENDORED: &[u8] = b"module.exports = function leftPad() {};\n";

fn first_commit(fake: &mut Fake) {
    fake.set_commit(
        &"1".repeat(40),
        &[
            ("src/lib.rs", LIB_RS),
            ("src/util.py", UTIL_V1),
            ("README.md", README),
            ("assets/logo.png", LOGO),
            ("node_modules/left-pad/index.js", VENDORED),
        ],
    );
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn first_index_records_files_counts_and_skips() {
    let env = setup().await;
    first_commit(&mut env.fake());

    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(
        (
            job.files_total,
            job.files_done,
            job.files_changed,
            job.files_skipped,
            job.files_deleted
        ),
        (4, 4, 3, 1, 0)
    );
    assert_eq!(env.fake().blob_requests, 4);

    let files = env.files().await;
    assert_eq!(files.len(), 4, "{files:?}");
    assert_eq!(files["assets/logo.png"].as_deref(), Some("binary"));
    assert_eq!(files["src/lib.rs"], None);
    assert!(!files.contains_key("node_modules/left-pad/index.js"));
    assert!(!files.contains_key("link"));

    let commit: Option<String> = env
        .query("SELECT indexed_commit FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(commit, Some("1".repeat(40)));
    // A job without a pushed target records the branch head it resolved (shown in the Activity table).
    let job_commit: Option<String> = env
        .query("SELECT target_commit FROM jobs WHERE repo_id = ?1 ORDER BY id DESC LIMIT 1")
        .await;
    assert_eq!(job_commit, Some("1".repeat(40)));
    let file_count: i64 = env
        .query("SELECT file_count FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(file_count, 3);
    let chunk_count: i64 = env
        .query("SELECT chunk_count FROM repositories WHERE id = ?1")
        .await;
    let chunks: i64 = env
        .query("SELECT count(*) FROM chunks WHERE repo_id = ?1")
        .await;
    assert!(chunks > 0);
    assert_eq!(chunk_count, chunks);
    let version: i64 = env
        .query("SELECT chunker_version FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(version, sourcelens::index::chunker::CHUNKER_VERSION);
}

#[tokio::test]
async fn second_commit_fetches_only_the_changed_file() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();
    assert_eq!(env.fts_hits("quokkaword").await, 1);

    {
        let mut fake = env.fake();
        fake.set_commit(
            &"2".repeat(40),
            &[
                ("src/lib.rs", LIB_RS),
                ("src/util.py", UTIL_V2),
                ("assets/logo.png", LOGO),
                ("node_modules/left-pad/index.js", VENDORED),
            ],
        );
        fake.blob_requests = 0;
    }
    let job = env.run("webhook", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!((job.files_changed, job.files_deleted), (1, 1));
    assert_eq!(env.fake().blob_requests, 1);
    assert_eq!(env.fts_hits("zebracorn").await, 1);
    assert_eq!(env.fts_hits("quokkaword").await, 0);
    assert!(!env.files().await.contains_key("README.md"));
    let commit: Option<String> = env
        .query("SELECT indexed_commit FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(commit, Some("2".repeat(40)));
}

#[tokio::test]
async fn unchanged_head_makes_no_tree_request() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();
    let trees = env.fake().tree_requests;

    let job = env.run("manual", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(env.fake().tree_requests, trees);
    assert_eq!(job.files_changed, 0);
}

#[tokio::test]
async fn many_changed_files_use_the_tarball() {
    let _archive = ARCHIVE_LOCK.lock().await;
    let env = setup().await;
    let contents: Vec<(String, Vec<u8>)> = (0..TARBALL_THRESHOLD + 10)
        .map(|i| {
            (
                format!("src/gen/mod_{i}.rs"),
                format!("pub fn generated_{i}() -> u32 {{\n    {i}\n}}\n").into_bytes(),
            )
        })
        .collect();
    let missing: &[u8] = b"pub fn missing_from_archive() {}\n";
    {
        let mut fake = env.fake();
        let mut files: Vec<(&str, &[u8])> = contents
            .iter()
            .map(|(path, content)| (path.as_str(), content.as_slice()))
            .collect();
        files.push(("src/missing.rs", missing));
        fake.set_commit(&"1".repeat(40), &files);
        fake.archive_omit.insert("src/missing.rs".into());
    }

    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    let total = (TARBALL_THRESHOLD + 11) as i64;
    assert_eq!(
        (
            job.files_total,
            job.files_done,
            job.files_changed,
            job.files_skipped
        ),
        (total, total, total - 1, 1)
    );
    {
        let fake = env.fake();
        assert_eq!((fake.tarball_requests, fake.blob_requests), (1, 0));
    }
    let files = env.files().await;
    assert_eq!(files.len(), TARBALL_THRESHOLD + 11);
    assert_eq!(files["src/missing.rs"].as_deref(), Some("not_in_archive"));
    assert_eq!(files["src/gen/mod_0.rs"], None);
    let leftovers: Vec<_> = std::fs::read_dir(env.tmp.path().join("tmp"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // The next commit plans the missing file as changed and fetches it as a blob.
    {
        let mut fake = env.fake();
        let files = fake.commits[&"1".repeat(40)].clone();
        fake.commits.insert("2".repeat(40), files);
        fake.branches.insert("main".into(), "2".repeat(40));
        fake.archive_omit.clear();
    }
    let job = env.run("webhook", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(job.files_changed, 1);
    assert_eq!(env.fake().blob_requests, 1);
    assert_eq!(env.files().await["src/missing.rs"], None);
}

#[tokio::test]
async fn truncated_tree_takes_files_and_deletions_from_the_archive() {
    let _archive = ARCHIVE_LOCK.lock().await;
    let env = setup().await;
    env.fake().set_commit(
        &"1".repeat(40),
        &[
            ("a.py", b"def a():\n    return 1\n"),
            ("b.py", b"def b():\n    return 2\n"),
            ("c.py", b"def c():\n    return 3\n"),
        ],
    );
    env.run("connect", false).await.unwrap();

    {
        let mut fake = env.fake();
        fake.set_commit(
            &"2".repeat(40),
            &[
                ("a.py", b"def a():\n    return 'changedalpha'\n"),
                ("c.py", b"def c():\n    return 3\n"),
                ("d.py", b"def d():\n    return 4\n"),
            ],
        );
        fake.truncated = true;
        fake.blob_requests = 0;
    }
    let job = env.run("webhook", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!((job.files_changed, job.files_deleted), (2, 1));
    {
        let fake = env.fake();
        assert_eq!((fake.tarball_requests, fake.blob_requests), (1, 0));
    }
    let files: Vec<String> = env.files().await.into_keys().collect();
    assert_eq!(files, vec!["a.py", "c.py", "d.py"]);
    assert_eq!(env.fts_hits("changedalpha").await, 1);
}

#[tokio::test]
async fn force_refetches_every_file() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();
    env.fake().blob_requests = 0;

    let job = env.run("manual", true).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(env.fake().blob_requests, 4);
    assert_eq!((job.files_changed, job.files_skipped), (3, 1));
}

#[tokio::test]
async fn lost_app_access_disconnects_the_repository() {
    let env = setup().await;
    first_commit(&mut env.fake());
    {
        let mut fake = env.fake();
        fake.token_status = Some(StatusCode::NOT_FOUND);
        fake.installation_status = Some(StatusCode::NOT_FOUND);
    }
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.is_some_and(|e| !e.is_empty()));
    let state: String = env
        .query("SELECT state FROM repositories WHERE id = ?1")
        .await;
    let reason: Option<String> = env
        .query("SELECT state_reason FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(state, "disconnected");
    assert_eq!(reason.as_deref(), Some("GitHub App has no access"));
}

#[tokio::test]
async fn idle_startup_job_is_discarded_and_failures_keep_the_message() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();

    assert!(env.run("startup", false).await.is_none());

    let repo_id = env.repo_id;
    env.state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE repositories SET branch = 'gone' WHERE id = ?1",
                [repo_id],
            )
        })
        .await
        .unwrap();
    let job = env.run("manual", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.is_some_and(|e| e.contains("gone")));
}

// ── Queue behaviour (no GitHub involved) ─────────────────────────────────────

#[tokio::test]
async fn enqueue_keeps_one_queued_job_per_repository() {
    let env = setup().await;
    let first = jobs::enqueue(&env.state, env.repo_id, Trigger::Webhook, Some("a"), true)
        .await
        .unwrap();
    let second = jobs::enqueue(&env.state, env.repo_id, Trigger::Manual, Some("b"), false)
        .await
        .unwrap();
    assert_eq!(first, second);
    let repo_id = env.repo_id;
    let (count, target, force, trigger): (i64, String, bool, String) = env
        .state
        .db
        .read(move |conn| {
            conn.query_row(
                "SELECT count(*), max(target_commit), max(force), max(trigger) FROM jobs WHERE repo_id = ?1",
                [repo_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(
        (count, target.as_str(), force, trigger.as_str()),
        (1, "b", true, "manual")
    );
}

#[tokio::test]
async fn claim_skips_busy_and_disconnected_repositories() {
    let env = setup().await;
    assert!(!jobs::run_next(&env.state).await.unwrap());

    let repo_id = env.repo_id;
    env.state
        .db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'running', 'manual', 0)",
                [repo_id],
            )
        })
        .await
        .unwrap();
    jobs::enqueue(&env.state, repo_id, Trigger::Manual, None, false)
        .await
        .unwrap();
    assert!(
        !jobs::run_next(&env.state).await.unwrap(),
        "a repository runs one job at a time"
    );

    env.state
        .db
        .write(move |conn| {
            conn.execute("DELETE FROM jobs WHERE status = 'running'", [])?;
            conn.execute(
                "UPDATE repositories SET state = 'disconnected' WHERE id = ?1",
                [repo_id],
            )
        })
        .await
        .unwrap();
    assert!(
        !jobs::run_next(&env.state).await.unwrap(),
        "disconnected repositories wait"
    );
}

#[tokio::test]
async fn recovery_fails_interrupted_jobs_and_queues_startup_jobs() {
    let env = setup().await;
    let repo_id = env.repo_id;
    let interrupted: i64 = env
        .state
        .db
        .write(move |conn| {
            conn.query_row(
                "INSERT INTO jobs (repo_id, status, trigger, created_at) VALUES (?1, 'running', 'manual', 0) RETURNING id",
                [repo_id],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    jobs::recover_and_reconcile(&env.state).await.unwrap();
    let rows: Vec<(i64, String, String, Option<String>)> = env
        .state
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, status, trigger, error FROM jobs WHERE repo_id = ?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([repo_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, interrupted);
    assert_eq!(rows[0].1, "failed");
    assert!(rows[0].3.is_some());
    assert_eq!(
        (rows[1].1.as_str(), rows[1].2.as_str()),
        ("queued", "startup")
    );
}

#[tokio::test]
async fn recovery_purges_repositories_being_deleted() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();
    let repo_id = env.repo_id;
    env.state
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE repositories SET state = 'deleting' WHERE id = ?1",
                [repo_id],
            )
        })
        .await
        .unwrap();
    jobs::recover_and_reconcile(&env.state).await.unwrap();
    // The purge runs in the background so startup is not blocked by it.
    let mut repos: i64 = 1;
    for _ in 0..400 {
        repos = env
            .query("SELECT count(*) FROM repositories WHERE id = ?1")
            .await;
        if repos == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let files: i64 = env
        .query("SELECT count(*) FROM files WHERE repo_id = ?1")
        .await;
    assert_eq!((repos, files), (0, 0));
    let fts: i64 = env.fts_hits("quokkaword").await;
    assert_eq!(fts, 0);
}

// ── Fixes: push target, app auth, archive limits and the archive pipeline ────

impl Env {
    /// Files left in `<data>/tmp`.
    fn tmp_leftovers(&self) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(self.tmp.path().join("tmp"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    }

    /// Every row of `sql` (one `?1` = repository id), all columns rendered, sorted.
    async fn dump(&self, sql: &'static str) -> Vec<String> {
        let repo_id = self.repo_id;
        let mut rows = self
            .state
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(sql)?;
                let columns = stmt.column_count();
                let rows = stmt
                    .query_map([repo_id], |row| {
                        (0..columns)
                            .map(|i| {
                                row.get::<_, rusqlite::types::Value>(i)
                                    .map(|v| format!("{v:?}"))
                            })
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .map(|values| values.join(" | "))
                    })?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok::<_, rusqlite::Error>(rows)
            })
            .await
            .unwrap();
        rows.sort();
        rows
    }

    async fn repo_state(&self) -> (String, Option<String>) {
        let state: String = self
            .query("SELECT state FROM repositories WHERE id = ?1")
            .await;
        let reason: Option<String> = self
            .query("SELECT state_reason FROM repositories WHERE id = ?1")
            .await;
        (state, reason)
    }
}

fn generated_files(count: usize) -> Vec<(String, Vec<u8>)> {
    (0..count)
        .map(|i| {
            (
                format!("src/gen/mod_{i}.rs"),
                format!(
                    "pub fn generated_{i}(seed: u32) -> u32 {{\n    let value = seed * {i};\n    value + {i}\n}}\n"
                )
                .into_bytes(),
            )
        })
        .collect()
}

fn set_files(fake: &mut Fake, sha: &str, files: &[(String, Vec<u8>)]) {
    let files: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_slice()))
        .collect();
    fake.set_commit(sha, &files);
}

#[tokio::test]
async fn queued_older_target_commit_indexes_the_branch_head() {
    let env = setup().await;
    first_commit(&mut env.fake());
    env.run("connect", false).await.unwrap();
    {
        let mut fake = env.fake();
        let mut files = fake.commits[&"1".repeat(40)].clone();
        files.insert("src/util.py".into(), UTIL_V2.to_vec());
        fake.commits.insert("2".repeat(40), files);
        fake.branches.insert("main".into(), "2".repeat(40));
    }
    // A replayed or late delivery of the first push queues the older commit.
    let job = env
        .run_target("webhook", Some(&"1".repeat(40)), false)
        .await
        .unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    let indexed: Option<String> = env
        .query("SELECT indexed_commit FROM repositories WHERE id = ?1")
        .await;
    let job_commit: Option<String> = env
        .query("SELECT target_commit FROM jobs WHERE repo_id = ?1 ORDER BY id DESC LIMIT 1")
        .await;
    assert_eq!(indexed, Some("2".repeat(40)));
    assert_eq!(job_commit, Some("2".repeat(40)));
    assert_eq!(env.fts_hits("zebracorn").await, 1);
    assert_eq!(env.fts_hits("quokkaword").await, 0);
}

#[tokio::test]
async fn rejected_app_credentials_fail_the_job_and_keep_the_repository_connected() {
    for (token, lookup) in [
        (StatusCode::UNAUTHORIZED, None),
        (StatusCode::NOT_FOUND, Some(StatusCode::UNAUTHORIZED)),
    ] {
        let env = setup().await;
        first_commit(&mut env.fake());
        {
            let mut fake = env.fake();
            fake.token_status = Some(token);
            fake.installation_status = lookup;
        }
        let job = env.run("connect", false).await.unwrap();
        assert_eq!(job.status, "failed", "token {token}, lookup {lookup:?}");
        assert!(job.error.is_some_and(|e| !e.is_empty()));
        assert_eq!(env.repo_state().await, ("connected".to_string(), None));
        let installation: i64 = env
            .query("SELECT installation_id FROM repositories WHERE id = ?1")
            .await;
        assert_eq!(installation, INSTALLATION_ID);
        assert!(env.files().await.is_empty());
    }
}

#[tokio::test]
async fn forbidden_installation_lookup_disconnects_the_repository() {
    let env = setup().await;
    first_commit(&mut env.fake());
    {
        let mut fake = env.fake();
        fake.token_status = Some(StatusCode::NOT_FOUND);
        fake.installation_status = Some(StatusCode::FORBIDDEN);
    }
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert_eq!(env.repo_state().await.0, "disconnected");
}

#[tokio::test]
async fn tarball_is_written_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let env = setup().await;
    env.fake()
        .set_commit(&"1".repeat(40), &[("a.py", b"def a():\n    return 1\n")]);
    let token = env
        .state
        .github
        .installation_token(INSTALLATION_ID as u64)
        .await
        .unwrap();
    let dest = env.tmp.path().join("archive.tar.gz");
    sourcelens::github::api::tarball(
        &env.state.github,
        &token,
        "acme/api",
        &"1".repeat(40),
        &dest,
        u64::MAX,
    )
    .await
    .unwrap();
    let mode = std::fs::metadata(&dest).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn archive_over_the_size_limit_fails_the_job_and_leaves_no_temp_file() {
    let env = setup_with(|cfg| cfg.max_archive_bytes = 1024).await;
    set_files(
        &mut env.fake(),
        &"1".repeat(40),
        &generated_files(TARBALL_THRESHOLD + 10),
    );
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert!(
        job.error
            .as_deref()
            .is_some_and(|e| e.contains("SOURCELENS_MAX_ARCHIVE_BYTES")),
        "{:?}",
        job.error
    );
    assert_eq!(env.fake().tarball_requests, 1);
    assert!(env.tmp_leftovers().is_empty(), "{:?}", env.tmp_leftovers());
    assert!(env.files().await.is_empty());
    let indexed: Option<String> = env
        .query("SELECT indexed_commit FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(indexed, None);
}

#[tokio::test]
async fn archive_and_blobs_index_the_same_rows() {
    let _archive = ARCHIVE_LOCK.lock().await;
    const TS: &[u8] = b"export class UserStore {\n  private users = new Map<string, string>();\n\n  getUserById(id: string): string | undefined {\n    return this.users.get(id);\n  }\n}\n";
    let mut snapshots = Vec::new();
    for truncated in [false, true] {
        let env = setup().await;
        {
            let mut fake = env.fake();
            first_commit(&mut fake);
            let mut files = fake.commits[&"1".repeat(40)].clone();
            files.insert("web/store.ts".into(), TS.to_vec());
            fake.commits.insert("1".repeat(40), files);
            fake.truncated = truncated;
        }
        let job = env.run("connect", false).await.unwrap();
        assert_eq!(job.status, "succeeded", "{:?}", job.error);
        {
            let fake = env.fake();
            let expected = if truncated { (1, 0) } else { (0, 5) };
            assert_eq!((fake.tarball_requests, fake.blob_requests), expected);
        }
        snapshots.push((
            env.dump(
                "SELECT path, blob_sha, language, size, line_count, skip_reason
                 FROM files WHERE repo_id = ?1",
            )
            .await,
            env.dump(
                "SELECT f.path, c.kind, c.symbol_path, c.start_line, c.end_line, c.start_byte,
                        c.end_byte, c.embed_hash
                 FROM chunks c JOIN files f ON f.id = c.file_id WHERE c.repo_id = ?1",
            )
            .await,
            env.dump(
                "SELECT f.path, s.kind, s.qualified_name, s.signature, s.start_line, s.end_line
                 FROM symbols s JOIN files f ON f.id = s.file_id WHERE s.repo_id = ?1",
            )
            .await,
        ));
    }
    let (blobs, archive) = (&snapshots[0], &snapshots[1]);
    assert_eq!(blobs.0.len(), 5);
    assert!(!blobs.1.is_empty() && !blobs.2.is_empty());
    assert_eq!(blobs.0, archive.0);
    assert_eq!(blobs.1, archive.1);
    assert_eq!(blobs.2, archive.2);
}

#[tokio::test]
async fn archive_failure_midway_leaves_no_temp_file_and_no_thread() {
    let _archive = ARCHIVE_LOCK.lock().await;
    let env = setup().await;
    {
        let mut fake = env.fake();
        set_files(
            &mut fake,
            &"1".repeat(40),
            &generated_files(TARBALL_THRESHOLD + 50),
        );
        fake.archive_cut = true;
    }
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.is_some_and(|e| !e.is_empty()));
    assert_eq!(env.fake().tarball_requests, 1);
    assert!(env.tmp_leftovers().is_empty(), "{:?}", env.tmp_leftovers());
    assert_eq!(archive_threads_running(), 0);
    let indexed: Option<String> = env
        .query("SELECT indexed_commit FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(indexed, None);
}

#[tokio::test]
async fn disconnect_during_the_archive_stops_every_stage() {
    let _archive = ARCHIVE_LOCK.lock().await;
    let env = setup().await;
    let db_path = env.tmp.path().join("sourcelens.db");
    let repo_id = env.repo_id;
    {
        let mut fake = env.fake();
        set_files(
            &mut fake,
            &"1".repeat(40),
            &generated_files(TARBALL_THRESHOLD + 50),
        );
        // The repository is disconnected while its archive downloads.
        fake.tarball_hook = Some(Box::new(move || {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            conn.execute(
                "UPDATE repositories SET state = 'disconnected' WHERE id = ?1",
                [repo_id],
            )
            .unwrap();
        }));
    }
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "failed");
    assert!(job.error.is_some_and(|e| !e.is_empty()));
    assert!(env.tmp_leftovers().is_empty(), "{:?}", env.tmp_leftovers());
    assert_eq!(archive_threads_running(), 0);
    assert!(env.files().await.is_empty());
}

#[tokio::test]
async fn deleting_more_than_one_slice_of_files_works() {
    let _archive = ARCHIVE_LOCK.lock().await;
    let env = setup().await;
    let mut files = generated_files(TARBALL_THRESHOLD + 50);
    set_files(&mut env.fake(), &"1".repeat(40), &files);
    let job = env.run("connect", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);

    files.truncate(1);
    set_files(&mut env.fake(), &"2".repeat(40), &files);
    let job = env.run("webhook", false).await.unwrap();
    assert_eq!(job.status, "succeeded", "{:?}", job.error);
    assert_eq!(
        (job.files_deleted, job.files_changed),
        ((TARBALL_THRESHOLD + 49) as i64, 0)
    );
    let left: Vec<String> = env.files().await.into_keys().collect();
    assert_eq!(left, vec!["src/gen/mod_0.rs".to_string()]);
    let chunks: i64 = env
        .query("SELECT count(*) FROM chunks WHERE repo_id = ?1")
        .await;
    let chunk_count: i64 = env
        .query("SELECT chunk_count FROM repositories WHERE id = ?1")
        .await;
    assert_eq!(chunk_count, chunks);
}
