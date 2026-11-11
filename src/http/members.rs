use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use rusqlite::{params, ErrorCode, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::http::session::Owner;
use crate::http::{Body, PathId};
use crate::model::Role;
use crate::state::{now, AppState};

const MEMBER_COLUMNS: &str = "id, login, name, avatar_url, role, github_id IS NULL, last_login_at";

/// `/api/members[/{id}]`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/members", get(list).post(add))
        .route("/api/members/{id}", delete(remove))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Member {
    id: i64,
    login: String,
    name: Option<String>,
    avatar_url: Option<String>,
    role: Role,
    pending: bool,
    last_login_at: Option<i64>,
}

fn member_from_row(row: &Row<'_>) -> rusqlite::Result<Member> {
    Ok(Member {
        id: row.get(0)?,
        login: row.get(1)?,
        name: row.get(2)?,
        avatar_url: row.get(3)?,
        role: row.get(4)?,
        pending: row.get(5)?,
        last_login_at: row.get(6)?,
    })
}

/// GitHub user and organization names: `^[A-Za-z0-9-]{1,39}$`.
pub(crate) fn valid_github_name(name: &str) -> bool {
    (1..=39).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

async fn list(State(state): State<AppState>, _owner: Owner) -> ApiResult<Json<Vec<Member>>> {
    let members = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MEMBER_COLUMNS} FROM users ORDER BY role = 'owner' DESC, login"
            ))?;
            let rows = stmt.query_map([], member_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(Json(members))
}

#[derive(Deserialize)]
struct AddMember {
    login: String,
}

async fn add(
    State(state): State<AppState>,
    _owner: Owner,
    Body(req): Body<AddMember>,
) -> ApiResult<Json<Member>> {
    let login = req.login.trim().trim_start_matches('@').to_string();
    if !valid_github_name(&login) {
        return Err(ApiError::unprocessable(
            "invalid_login",
            "Enter a GitHub username (letters, digits and hyphens).",
        ));
    }
    let ts = now();
    let member = state
        .db
        .write(move |conn| -> ApiResult<Member> {
            let result = conn.execute(
                "INSERT INTO users (login, role, created_at) VALUES (?1, ?2, ?3)",
                params![login, Role::Member, ts],
            );
            match result {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == ErrorCode::ConstraintViolation =>
                {
                    return Err(ApiError::conflict(
                        "member_exists",
                        "This GitHub user is already a member.",
                    ))
                }
                Err(err) => return Err(err.into()),
            }
            Ok(conn.query_row(
                &format!("SELECT {MEMBER_COLUMNS} FROM users WHERE id = ?1"),
                [conn.last_insert_rowid()],
                member_from_row,
            )?)
        })
        .await?;
    Ok(Json(member))
}

async fn remove(
    State(state): State<AppState>,
    _owner: Owner,
    PathId(id): PathId,
) -> ApiResult<StatusCode> {
    let role: Option<Role> = state
        .db
        .write(move |conn| {
            let role: Option<Role> = conn
                .query_row("SELECT role FROM users WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            if role == Some(Role::Member) {
                conn.execute("DELETE FROM users WHERE id = ?1", [id])?;
            }
            Ok::<_, rusqlite::Error>(role)
        })
        .await?;
    match role {
        Some(Role::Member) => Ok(StatusCode::NO_CONTENT),
        Some(Role::Owner) => Err(ApiError::conflict(
            "cannot_remove_owner",
            "The owner cannot be removed.",
        )),
        None => Err(ApiError::not_found(
            "member_not_found",
            "This member does not exist.",
        )),
    }
}
