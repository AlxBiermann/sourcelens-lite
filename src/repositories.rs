//! The `repositories` table: every query and state change of connected repositories.
//!
//! States: `connected` (indexed and searchable), `disconnected` (GitHub access is gone; the index is
//! kept but hidden from search, MCP and key scopes), `deleting` (the index is being purged).

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};

use crate::db::sql::{column_u64, placeholders};
use crate::model::{JobStatus, RepoState, EMBED_PENDING};

/// Subquery of the ids of connected repositories, for `… IN (CONNECTED_IDS)` inside other tables' queries.
pub const CONNECTED_IDS: &str = "SELECT id FROM repositories WHERE state = 'connected'";
/// Subquery of the ids of repositories that are not being deleted.
pub const LISTED_IDS: &str = "SELECT id FROM repositories WHERE state != 'deleting'";

const COLUMNS: &str = "id, github_id, installation_id, full_name, private, default_branch, branch,
    state, state_reason, indexed_commit, indexed_at, chunker_version, file_count, chunk_count";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    pub id: i64,
    pub github_id: i64,
    pub installation_id: u64,
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
    /// `None` = follow `default_branch`.
    pub branch: Option<String>,
    pub state: RepoState,
    pub state_reason: Option<String>,
    pub indexed_commit: Option<String>,
    pub indexed_at: Option<i64>,
    pub chunker_version: i64,
    pub file_count: i64,
    pub chunk_count: i64,
}

impl RepoRow {
    /// The branch that is indexed: the chosen one, else the default branch.
    pub fn effective_branch(&self) -> &str {
        self.branch.as_deref().unwrap_or(&self.default_branch)
    }
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<RepoRow> {
    Ok(RepoRow {
        id: row.get(0)?,
        github_id: row.get(1)?,
        installation_id: column_u64(row, 2)?,
        full_name: row.get(3)?,
        private: row.get(4)?,
        default_branch: row.get(5)?,
        branch: row.get(6)?,
        state: row.get(7)?,
        state_reason: row.get(8)?,
        indexed_commit: row.get(9)?,
        indexed_at: row.get(10)?,
        chunker_version: row.get(11)?,
        file_count: row.get(12)?,
        chunk_count: row.get(13)?,
    })
}

/// The repository with this id, in any state.
pub fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<RepoRow>> {
    conn.prepare_cached(&format!("SELECT {COLUMNS} FROM repositories WHERE id = ?1"))?
        .query_row([id], from_row)
        .optional()
}

/// The repository with this GitHub id, in any state.
pub fn by_github_id(conn: &Connection, github_id: i64) -> rusqlite::Result<Option<RepoRow>> {
    conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM repositories WHERE github_id = ?1"
    ))?
    .query_row([github_id], from_row)
    .optional()
}

/// Repositories that are not being deleted, ordered by name; `id` narrows to one.
pub fn listed(conn: &Connection, id: Option<i64>) -> rusqlite::Result<Vec<RepoRow>> {
    conn.prepare_cached(&format!(
        "SELECT {COLUMNS} FROM repositories
         WHERE state != 'deleting' AND (?1 IS NULL OR id = ?1)
         ORDER BY full_name, id"
    ))?
    .query_map([id], from_row)?
    .collect()
}

/// GitHub ids of the repositories that are not being deleted.
pub fn listed_github_ids(conn: &Connection) -> rusqlite::Result<HashSet<i64>> {
    conn.prepare_cached("SELECT github_id FROM repositories WHERE state != 'deleting'")?
        .query_map([], |row| row.get(0))?
        .collect()
}

/// Ids of the repositories in `state`, ascending.
pub fn ids_in_state(conn: &Connection, state: RepoState) -> rusqlite::Result<Vec<i64>> {
    conn.prepare_cached("SELECT id FROM repositories WHERE state = ?1 ORDER BY id")?
        .query_map([state], |row| row.get(0))?
        .collect()
}

/// Ids of the connected repositories, ascending: the scope of searches and all-repository keys.
pub fn connected_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    ids_in_state(conn, RepoState::Connected)
}

/// The connected repositories among `ids`, ordered by name.
pub fn connected_in(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<RepoRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {COLUMNS} FROM repositories WHERE state = 'connected' AND id IN ({})
         ORDER BY full_name, id",
        placeholders(ids.len())
    );
    conn.prepare(&sql)?
        .query_map(params_from_iter(ids), from_row)?
        .collect()
}

/// Connected repositories that have chunks waiting for an embedding.
pub fn connected_with_pending_embeddings(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    conn.prepare_cached(
        "SELECT r.id FROM repositories r
         WHERE r.state = 'connected'
           AND EXISTS (SELECT 1 FROM chunks c WHERE c.repo_id = r.id AND c.embed_state = ?1)
         ORDER BY r.id",
    )?
    .query_map([EMBED_PENDING], |row| row.get(0))?
    .collect()
}

pub fn is_connected(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let state: Option<RepoState> = conn
        .prepare_cached("SELECT state FROM repositories WHERE id = ?1")?
        .query_row([id], |row| row.get(0))
        .optional()?;
    Ok(state == Some(RepoState::Connected))
}

/// `id → full_name` for the given ids (any state).
pub fn full_names(conn: &Connection, ids: &[i64]) -> rusqlite::Result<HashMap<i64, String>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT id, full_name FROM repositories WHERE id IN ({})",
        placeholders(ids.len())
    );
    conn.prepare(&sql)?
        .query_map(params_from_iter(ids), |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect()
}

/// Whether `id` exists and is not being deleted.
pub fn is_listed(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    conn.prepare_cached("SELECT 1 FROM repositories WHERE id = ?1 AND state != 'deleting'")?
        .exists([id])
}

/// A repository to connect, as GitHub describes it.
#[derive(Debug, Clone)]
pub struct NewRepository {
    pub github_id: i64,
    pub installation_id: u64,
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
    pub added_by: Option<i64>,
    pub created_at: i64,
}

/// Inserts a `connected` repository and returns its id.
pub fn insert(conn: &Connection, repo: &NewRepository) -> rusqlite::Result<i64> {
    conn.query_row(
        "INSERT INTO repositories
           (github_id, installation_id, full_name, private, default_branch, added_by, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         RETURNING id",
        params![
            repo.github_id,
            installation_param(repo.installation_id),
            repo.full_name,
            repo.private,
            repo.default_branch,
            repo.added_by,
            repo.created_at
        ],
        |row| row.get(0),
    )
}

/// Sets the tracked branch (`None` = follow the default branch).
pub fn set_branch(conn: &Connection, id: i64, branch: Option<&str>) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE repositories SET branch = ?2 WHERE id = ?1",
        params![id, branch],
    )?;
    Ok(())
}

pub fn set_installation(conn: &Connection, id: i64, installation_id: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE repositories SET installation_id = ?2 WHERE id = ?1",
        params![id, installation_param(installation_id)],
    )?;
    Ok(())
}

/// Copies the given GitHub facts; `None` keeps the stored value.
pub fn update_from_github(
    conn: &Connection,
    id: i64,
    full_name: Option<&str>,
    private: Option<bool>,
    default_branch: Option<&str>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE repositories SET full_name = COALESCE(?2, full_name), private = COALESCE(?3, private),
           default_branch = COALESCE(?4, default_branch)
         WHERE id = ?1",
        params![id, full_name, private, default_branch],
    )?;
    Ok(())
}

/// Records a finished index of `commit` and recounts files and chunks.
pub fn record_index(
    conn: &Connection,
    id: i64,
    commit: &str,
    indexed_at: i64,
    chunker_version: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE repositories SET indexed_commit = ?2, indexed_at = ?3, chunker_version = ?4,
           file_count = (SELECT count(*) FROM files WHERE repo_id = ?1 AND skip_reason IS NULL),
           chunk_count = (SELECT count(*) FROM chunks WHERE repo_id = ?1)
         WHERE id = ?1",
        params![id, commit, indexed_at, chunker_version],
    )?;
    Ok(())
}

/// Moves a repository to `deleting`; `false` when it does not exist or is already being deleted.
pub fn mark_deleting(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE repositories SET state = ?2 WHERE id = ?1 AND state != 'deleting'",
        params![id, RepoState::Deleting],
    )?;
    Ok(changed == 1)
}

/// Deletes the repository row; its index rows must be gone already (see `store::purge_batch`).
pub fn delete(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM repositories WHERE id = ?1", [id])?;
    Ok(())
}

/// Which repositories a state change applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Id(i64),
    GithubId(i64),
    /// Every repository connected through this GitHub App installation.
    Installation(u64),
}

impl Target {
    fn condition(self) -> (&'static str, Value) {
        match self {
            Self::Id(id) => ("id = ?", Value::from(id)),
            Self::GithubId(id) => ("github_id = ?", Value::from(id)),
            Self::Installation(id) => ("installation_id = ?", installation_param(id).into()),
        }
    }
}

/// Marks the target repositories (unless they are being deleted) as `disconnected` with `reason`
/// and deletes their queued jobs, which no worker would claim. Returns the number of repositories
/// changed.
pub fn disconnect(conn: &mut Connection, target: Target, reason: &str) -> rusqlite::Result<usize> {
    let (condition, value) = target.condition();
    let savepoint = conn.savepoint()?;
    let ids = savepoint
        .prepare(&format!(
            "UPDATE repositories SET state = ?, state_reason = ?
             WHERE {condition} AND state != 'deleting'
             RETURNING id"
        ))?
        .query_map(params![RepoState::Disconnected, reason, value], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    {
        let mut delete_queued =
            savepoint.prepare_cached("DELETE FROM jobs WHERE repo_id = ?1 AND status = ?2")?;
        for id in &ids {
            delete_queued.execute(params![id, JobStatus::Queued])?;
        }
    }
    savepoint.commit()?;
    Ok(ids.len())
}

/// Marks the `disconnected` target repositories as `connected` through `installation_id` and returns their ids.
pub fn reconnect(
    conn: &Connection,
    target: Target,
    installation_id: u64,
) -> rusqlite::Result<Vec<i64>> {
    let (condition, value) = target.condition();
    conn.prepare(&format!(
        "UPDATE repositories SET state = ?, state_reason = NULL, installation_id = ?
         WHERE {condition} AND state = 'disconnected' RETURNING id"
    ))?
    .query_map(
        params![
            RepoState::Connected,
            installation_param(installation_id),
            value
        ],
        |row| row.get(0),
    )?
    .collect()
}

/// GitHub ids fit in SQLite's signed INTEGER; larger values are stored saturated.
fn installation_param(installation_id: u64) -> i64 {
    i64::try_from(installation_id).unwrap_or(i64::MAX)
}
