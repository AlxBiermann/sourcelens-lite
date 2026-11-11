use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ring::hmac;
use sourcelens::config::Config;
use sourcelens::crypto::{self, SecretKey};
use sourcelens::db::Db;
use sourcelens::http;
use sourcelens::state::AppState;
use tower::ServiceExt;

fn f32_blob(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[tokio::test]
async fn migration_applies_and_sqlite_vec_is_loaded() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();

    let (version, vec_version, tables) = db
        .read(|conn| {
            let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let vec_version: String = conn.query_row("SELECT vec_version()", [], |r| r.get(0))?;
            let tables: i64 = conn.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('repositories','files','chunks','chunks_fts','symbols','symbols_fts','jobs','api_keys')",
                [],
                |r| r.get(0),
            )?;
            Ok::<_, rusqlite::Error>((version, vec_version, tables))
        })
        .await
        .unwrap();
    assert_eq!(version, 2);
    assert!(
        vec_version.starts_with('v'),
        "vec_version() = {vec_version}"
    );
    assert_eq!(tables, 8);

    // Reopening an existing database does not re-run the migration.
    drop(db);
    let db = Db::open(&tmp.path().join("test.db")).unwrap();
    let foreign_keys: i64 = db
        .read(|c| c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(foreign_keys, 1);
}

#[tokio::test]
async fn vec0_partitioned_knn_orders_by_distance() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();

    let rows = db
        .write(|conn| {
            conn.execute_batch(
                "CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id INTEGER PRIMARY KEY, repo_id INTEGER PARTITION KEY, embedding float[4])",
            )?;
            let vectors: [(i64, i64, [f32; 4]); 4] = [
                (1, 1, [1.0, 0.0, 0.0, 0.0]),
                (2, 1, [0.0, 1.0, 0.0, 0.0]),
                (3, 1, [0.8, 0.6, 0.0, 0.0]),
                (4, 2, [1.0, 0.0, 0.0, 0.0]),
            ];
            for (chunk_id, repo_id, v) in vectors {
                conn.execute(
                    "INSERT INTO vec_chunks (chunk_id, repo_id, embedding) VALUES (?1, ?2, ?3)",
                    rusqlite::params![chunk_id, repo_id, f32_blob(&v)],
                )?;
            }
            let mut stmt = conn.prepare(
                "SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 AND k = 2 AND repo_id = ?2 ORDER BY distance",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![f32_blob(&[1.0, 0.0, 0.0, 0.0]), 1_i64], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok::<_, rusqlite::Error>(rows)
        })
        .await
        .unwrap();

    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, vec![1, 3]);
    assert!(rows[0].1 < rows[1].1);
}

#[tokio::test]
async fn contentless_fts_supports_delete_by_rowid() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();

    let (before, after, other) = db
        .write(|conn| {
            let insert = "INSERT INTO chunks_fts (rowid, names, path, body, idents) VALUES (?1, ?2, ?3, ?4, ?5)";
            conn.execute(insert, rusqlite::params![10_i64, "getUserById", "src/users.ts", "export function getUserById", "get user by id"])?;
            conn.execute(insert, rusqlite::params![11_i64, "listOrders", "src/orders.py", "def list_orders", "list orders"])?;
            let count = |conn: &rusqlite::Connection, query: &str| -> rusqlite::Result<i64> {
                conn.query_row("SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1", [query], |r| r.get(0))
            };
            let before = count(conn, "getuserbyid")?;
            conn.execute("DELETE FROM chunks_fts WHERE rowid = ?1", [10_i64])?;
            let after = count(conn, "getuserbyid")?;
            let other = count(conn, "orders")?;
            Ok::<_, rusqlite::Error>((before, after, other))
        })
        .await
        .unwrap();
    assert_eq!((before, after, other), (1, 0, 1));
}

#[test]
fn crypto_primitives() {
    let key = SecretKey::from_bytes([7u8; 32]);
    let sealed = key.seal(b"top secret");
    assert_ne!(&sealed[12..], b"top secret");
    assert_eq!(key.open(&sealed).as_deref(), Some(&b"top secret"[..]));

    let mut tampered = sealed.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;
    assert_eq!(key.open(&tampered), None);
    assert_eq!(SecretKey::from_bytes([8u8; 32]).open(&sealed), None);
    assert_eq!(key.open(&sealed[..10]), None);

    assert_eq!(
        crypto::git_blob_sha1(b"hello\n"),
        "ce013625030ba8dba906f756967f9e9ca394464a"
    );
    assert_eq!(
        crypto::sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    let secret = b"webhook-secret";
    let body = br#"{"zen":"Keep it logically awesome."}"#;
    let signature =
        crypto::hex(hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret), body).as_ref());
    assert!(crypto::hmac_sha256_verify(secret, body, &signature));
    assert!(!crypto::hmac_sha256_verify(
        b"other-secret",
        body,
        &signature
    ));
    assert!(!crypto::hmac_sha256_verify(secret, b"tampered", &signature));
    assert!(!crypto::hmac_sha256_verify(secret, body, "not hex"));

    let random = crypto::random_hex(16);
    assert_eq!(random.len(), 32);
    assert_eq!(
        crypto::unhex(&random).map(|b| crypto::hex(&b)),
        Some(random)
    );
}

#[test]
fn secret_key_file_is_created_and_reused() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = Config::new("http://localhost:3000", tmp.path());
    let first = SecretKey::load(&cfg).unwrap();
    let sealed = first.seal(b"value");
    let second = SecretKey::load(&cfg).unwrap();
    assert_eq!(second.open(&sealed).as_deref(), Some(&b"value"[..]));
}

#[tokio::test]
async fn healthz_answers_ok() {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(Config::new("http://localhost:3000", tmp.path())).unwrap();
    let app = http::router(state);

    let response = app
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024)
        .await
        .unwrap();
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn api_writes_from_foreign_origins_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let state = AppState::new(Config::new("http://localhost:3000", tmp.path())).unwrap();
    let app = http::router(state);

    for origin in [None, Some("https://evil.example")] {
        let mut request = Request::post("/api/logout");
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "origin {origin:?}"
        );
    }

    let response = app
        .oneshot(
            Request::post("/api/logout")
                .header("origin", "http://localhost:3000")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);
}

#[test]
fn config_helpers() {
    let local = Config::new("http://localhost:3000/", std::path::Path::new("/tmp"));
    assert_eq!(local.public_url, "http://localhost:3000");
    assert_eq!(local.public_origin(), "http://localhost:3000");
    assert!(local.public_host_is_local());
    assert!(!local.public_is_https());
    assert_eq!(local.github_api_url, "https://api.github.com");

    let public = Config::new(
        "https://code.example.com/sourcelens",
        std::path::Path::new("/tmp"),
    );
    assert_eq!(public.public_origin(), "https://code.example.com");
    assert!(!public.public_host_is_local());
    assert!(public.public_is_https());
    assert!(Config::new("http://[::1]:3000", std::path::Path::new("/tmp")).public_host_is_local());
    assert!(
        Config::new("http://127.0.0.1:3000", std::path::Path::new("/tmp")).public_host_is_local()
    );
}

fn index_names(db: &rusqlite::Connection) -> Vec<String> {
    let mut stmt = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

#[tokio::test]
async fn version_1_database_is_migrated_to_version_2() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("old.db");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(include_str!("../migrations/0001_init.sql"))
            .unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 1;
             INSERT INTO users (github_id, login, role, created_at) VALUES (1, 'octocat', 'owner', 0);
             INSERT INTO users (github_id, login, role, created_at) VALUES (NULL, 'invited', 'member', 0);",
        )
        .unwrap();
        assert!(index_names(&conn).contains(&"chunks_pending".to_string()));
    }

    let db = Db::open(&path).unwrap();
    let (version, indexes, logins) = db
        .read(|c| {
            let version: i64 = c.query_row("PRAGMA user_version", [], |r| r.get(0))?;
            let mut stmt = c.prepare("SELECT login FROM users ORDER BY id")?;
            let logins = stmt
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok::<_, rusqlite::Error>((version, index_names(c), logins))
        })
        .await
        .unwrap();
    assert_eq!(version, 2);
    assert!(
        indexes.contains(&"chunks_repo_state".to_string()),
        "{indexes:?}"
    );
    assert!(indexes.contains(&"symbols_repo".to_string()), "{indexes:?}");
    assert!(
        !indexes.contains(&"chunks_pending".to_string()),
        "{indexes:?}"
    );
    assert_eq!(logins, ["octocat"]);

    // A fresh database gets the same schema.
    let fresh = Db::open(&tmp.path().join("fresh.db")).unwrap();
    let fresh_indexes = fresh
        .read(|c| Ok::<_, rusqlite::Error>(index_names(c)))
        .await
        .unwrap();
    assert_eq!(fresh_indexes, indexes);
}

#[cfg(unix)]
#[test]
fn data_dir_is_private_and_tmp_is_emptied_at_start() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;

    let state = AppState::new(Config::new("http://localhost:3000", &data)).unwrap();
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("tmp")), 0o700);
    drop(state);

    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    let leftover = data.join("tmp").join("job-1.tar.gz");
    std::fs::write(&leftover, b"partial archive").unwrap();
    AppState::new(Config::new("http://localhost:3000", &data)).unwrap();
    assert_eq!(mode(&data), 0o700);
    assert!(!leftover.exists());
    assert!(data.join("tmp").is_dir());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_reads_than_reader_connections_complete() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let reads = (0..24).map(|i| {
        let db = db.clone();
        let running = running.clone();
        let peak = peak.clone();
        tokio::spawn(async move {
            db.read(move |c| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(20));
                let value: i64 = c.query_row("SELECT ?1", [i], |r| r.get(0))?;
                running.fetch_sub(1, Ordering::SeqCst);
                Ok::<_, rusqlite::Error>(value)
            })
            .await
        })
    });
    let results = futures_util::future::join_all(reads).await;
    let values: Vec<i64> = results.into_iter().map(|r| r.unwrap().unwrap()).collect();
    assert_eq!(values, (0..24).collect::<Vec<i64>>());
    let peak = peak.load(Ordering::SeqCst);
    assert!((2..=8).contains(&peak), "peak concurrent reads: {peak}");
}
