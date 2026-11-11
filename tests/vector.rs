mod common;

use common::{insert_repo, temp_db};
use rusqlite::{params, Connection};
use sourcelens::index::store::{
    apply_files, create_vec_table, delete_paths, knn, prepare_file, purge_batch, to_blob,
    vec_table_exists, DELETE_VECTOR_SQL, SELECT_VECTOR_SQL,
};

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
    let db = temp_db(&tmp);
    let (exists_before, all, only_a, only_b, top_one) = db
        .write(|conn| {
            let exists_before = vec_table_exists(conn)?;
            create_vec_table(conn, 4)?;
            let a = insert_repo(conn, 1, "acme/r1", "connected");
            let b = insert_repo(conn, 2, "acme/r2", "connected");
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
    let db = temp_db(&tmp);
    let rows = db
        .write(|conn| {
            create_vec_table(conn, 4)?;
            let repo = insert_repo(conn, 1, "acme/r1", "connected");
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
    let db = temp_db(&tmp);
    let v1 = format!("{}\n{}", function("alpha", "same"), function("beta", "old"));
    let v2 = format!("{}\n{}", function("alpha", "same"), function("beta", "new"));

    let rows = db
        .write(move |conn| {
            let repo = insert_repo(conn, 1, "acme/r1", "connected");
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

/// `detail` column of `EXPLAIN QUERY PLAN`.
fn query_plan(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([1], |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// The `vec0` plan of a statement: idxStr starts with '1' (full scan), '2' (point) or '3' (KNN).
fn vec0_plan(conn: &Connection, sql: &str) -> char {
    let plan = query_plan(conn, sql);
    let line = plan
        .iter()
        .find(|line| line.contains("vec_chunks VIRTUAL TABLE INDEX"))
        .unwrap_or_else(|| panic!("no vec0 step in {plan:?}"));
    let idx_str = line.split_once(':').map(|(_, rest)| rest).unwrap_or("");
    idx_str.chars().next().unwrap_or(' ')
}

#[tokio::test]
async fn vector_reads_and_deletes_by_chunk_id_are_point_lookups() {
    let tmp = tempfile::tempdir().unwrap();
    let db = temp_db(&tmp);
    let plans = db
        .write(|conn| {
            create_vec_table(conn, 4)?;
            Ok::<_, rusqlite::Error>((
                vec0_plan(
                    conn,
                    "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM chunks WHERE file_id = ?1)",
                ),
                vec0_plan(conn, SELECT_VECTOR_SQL),
                vec0_plan(conn, DELETE_VECTOR_SQL),
            ))
        })
        .await
        .unwrap();
    // An `IN (…)` list over chunk_id makes vec0 scan every vector of every repository.
    assert_eq!(plans.0, '1', "the former statement was a full scan");
    assert_eq!((plans.1, plans.2), ('2', '2'));
}

#[tokio::test]
async fn reapply_and_delete_leave_no_orphan_vectors() {
    let tmp = tempfile::tempdir().unwrap();
    let db = temp_db(&tmp);
    let v1 = format!("{}\n{}", function("alpha", "same"), function("beta", "old"));
    let v2 = format!("{}\n{}", function("alpha", "same"), function("beta", "new"));
    let other = function("gamma", "kept");

    let counts = db
        .write(move |conn| {
            let repo = insert_repo(conn, 1, "acme/r1", "connected");
            apply_files(
                conn,
                repo,
                &[
                    prepare_file("src/lib.rs", "v1", v1.as_bytes()),
                    prepare_file("src/other.rs", "o", other.as_bytes()),
                ],
            )?;
            create_vec_table(conn, 4)?;
            let chunk_ids: Vec<i64> = conn
                .prepare("SELECT id FROM chunks ORDER BY id")?
                .query_map([], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            assert_eq!(chunk_ids.len(), 3);
            for (i, id) in chunk_ids.iter().enumerate() {
                let mut vector = [0.0f32; 4];
                vector[i] = 1.0;
                insert_vector(conn, *id, repo, &vector);
            }
            conn.execute("UPDATE chunks SET embed_state = 1", [])?;

            let orphans = |conn: &Connection| -> rusqlite::Result<(i64, i64)> {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM vec_chunks", [], |row| row.get(0))?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM vec_chunks WHERE chunk_id NOT IN (SELECT id FROM chunks)",
                        [],
                        |row| row.get(0),
                    )?,
                ))
            };
            apply_files(
                conn,
                repo,
                &[prepare_file("src/lib.rs", "v2", v2.as_bytes())],
            )?;
            let after_reapply = orphans(conn)?;
            delete_paths(conn, repo, &["src/lib.rs".to_string()])?;
            let after_delete = orphans(conn)?;
            while !purge_batch(conn, repo)? {}
            let after_purge = orphans(conn)?;
            Ok::<_, rusqlite::Error>((after_reapply, after_delete, after_purge))
        })
        .await
        .unwrap();
    // Re-apply keeps the unchanged `alpha` vector and `src/other.rs`; `beta` lost its vector.
    assert_eq!(counts.0, (2, 0));
    assert_eq!(counts.1, (1, 0));
    assert_eq!(counts.2, (0, 0));
}
