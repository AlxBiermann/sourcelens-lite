use rusqlite::{params, Connection};
use sourcelens::db::Db;
use sourcelens::index::store::{
    apply_files, create_vec_table, knn, prepare_file, to_blob, vec_table_exists,
};

fn insert_repo(conn: &Connection, github_id: i64) -> i64 {
    conn.query_row(
        "INSERT INTO repositories (github_id, installation_id, full_name, private, default_branch, created_at)
         VALUES (?1, 1, ?2, 0, 'main', 0) RETURNING id",
        params![github_id, format!("acme/r{github_id}")],
        |row| row.get(0),
    )
    .unwrap()
}

fn insert_vector(conn: &Connection, chunk_id: i64, repo_id: i64, vector: &[f32]) {
    conn.execute(
        "INSERT INTO vec_chunks (chunk_id, repo_id, embedding) VALUES (?1, ?2, ?3)",
        params![chunk_id, repo_id, to_blob(vector)],
    )
    .unwrap();
}

#[test]
fn to_blob_is_little_endian_f32() {
    assert_eq!(to_blob(&[1.0, -2.0]), [0, 0, 128, 63, 0, 0, 0, 192]);
}

#[tokio::test]
async fn knn_orders_by_distance_and_respects_scope_and_k() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();
    let (exists_before, all, only_a, only_b, top_one) = db
        .write(|conn| {
            let exists_before = vec_table_exists(conn)?;
            create_vec_table(conn, 4)?;
            let a = insert_repo(conn, 1);
            let b = insert_repo(conn, 2);
            insert_vector(conn, 1, a, &[1.0, 0.0, 0.0, 0.0]);
            insert_vector(conn, 2, a, &[0.0, 1.0, 0.0, 0.0]);
            insert_vector(conn, 3, b, &[0.8, 0.6, 0.0, 0.0]);
            let query = [1.0, 0.0, 0.0, 0.0];
            Ok::<_, rusqlite::Error>((
                exists_before,
                knn(conn, &[a, b], &query, 10)?,
                knn(conn, &[a], &query, 10)?,
                knn(conn, &[b], &query, 10)?,
                knn(conn, &[a, b], &query, 1)?,
            ))
        })
        .await
        .unwrap();
    assert!(!exists_before);
    let ids = |hits: &[(i64, f64)]| hits.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    assert_eq!(ids(&all), [1, 3, 2]);
    assert!(all.windows(2).all(|pair| pair[0].1 <= pair[1].1));
    assert!(all[0].1.abs() < 1e-6);
    assert_eq!(ids(&only_a), [1, 2]);
    assert_eq!(ids(&only_b), [3]);
    assert_eq!(ids(&top_one), [1]);
}

#[tokio::test]
async fn create_vec_table_replaces_an_existing_table() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();
    let rows = db
        .write(|conn| {
            create_vec_table(conn, 4)?;
            let repo = insert_repo(conn, 1);
            insert_vector(conn, 1, repo, &[1.0, 0.0, 0.0, 0.0]);
            create_vec_table(conn, 8)?;
            insert_vector(conn, 1, repo, &[0.0; 8]);
            conn.query_row("SELECT count(*) FROM vec_chunks", [], |row| {
                row.get::<_, i64>(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(rows, 1);
}

fn function(name: &str, marker: &str) -> String {
    let mut out = format!("pub fn {name}(seed: u32) -> u32 {{\n");
    for i in 0..14 {
        out.push_str(&format!(
            "    let {name}_value_{i} = compute_{name}(seed, {i}, \"{marker}\");\n"
        ));
    }
    out.push_str("    seed\n}\n");
    out
}

#[tokio::test]
async fn unchanged_chunks_keep_their_vectors_when_a_file_is_reapplied() {
    let tmp = tempfile::tempdir().unwrap();
    let db = Db::open(&tmp.path().join("test.db")).unwrap();
    let v1 = format!("{}\n{}", function("alpha", "same"), function("beta", "old"));
    let v2 = format!("{}\n{}", function("alpha", "same"), function("beta", "new"));

    let rows = db
        .write(move |conn| {
            let repo = insert_repo(conn, 1);
            apply_files(
                conn,
                repo,
                &[prepare_file("src/lib.rs", "v1", v1.as_bytes())],
            )?;
            create_vec_table(conn, 4)?;
            let chunk_ids: Vec<i64> = conn
                .prepare("SELECT id FROM chunks ORDER BY start_line")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(chunk_ids.len(), 2, "expected one chunk per function");
            for (i, id) in chunk_ids.iter().enumerate() {
                let mut vector = [0.0f32; 4];
                vector[i] = 1.0;
                insert_vector(conn, *id, repo, &vector);
            }
            conn.execute("UPDATE chunks SET embed_state = 1", [])?;

            apply_files(
                conn,
                repo,
                &[prepare_file("src/lib.rs", "v2", v2.as_bytes())],
            )?;

            let mut stmt = conn.prepare(
                "SELECT c.symbol_path, c.embed_state, v.embedding
                 FROM chunks c LEFT JOIN vec_chunks v ON v.chunk_id = c.id ORDER BY c.start_line",
            )?;
            let rows: Vec<(String, i64, Option<Vec<u8>>)> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let vec_rows: i64 =
                conn.query_row("SELECT count(*) FROM vec_chunks", [], |row| row.get(0))?;
            Ok::<_, rusqlite::Error>((rows, vec_rows))
        })
        .await
        .unwrap();

    let (rows, vec_rows) = rows;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, "alpha");
    assert_eq!(rows[0].1, 1);
    assert_eq!(
        rows[0].2.as_deref(),
        Some(to_blob(&[1.0, 0.0, 0.0, 0.0]).as_slice())
    );
    assert_eq!(rows[1].0, "beta");
    assert_eq!(rows[1].1, 0);
    assert_eq!(rows[1].2, None);
    assert_eq!(vec_rows, 1);
}
