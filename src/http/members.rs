use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get};
use axum::{Json, Router};
use rusqlite::{params, ErrorCode, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::github::app::GitHub;
use crate::github::oauth::GhUser;
use crate::http::session::{user_token, Owner};
use crate::http::{Body, PathId};
use crate::model::Role;
use crate::state::{lock, now, AppState};

const MEMBER_COLUMNS: &str =
    "id, login, name, avatar_url, role, last_login_at IS NULL, last_login_at";

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
    /// Invited, never signed in.
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
                "SELECT {MEMBER_COLUMNS} FROM users ORDER BY role = ?1 DESC, login"
            ))?;
            let rows = stmt.query_map([Role::Owner], member_from_row)?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(Json(members))
}

#[derive(Deserialize)]
struct AddMember {
    login: String,
}

/// Invites a GitHub account. The login is resolved to the account id now, so whoever holds the login
/// later (GitHub releases renamed logins) cannot take over the invitation.
async fn add(
    State(state): State<AppState>,
    Owner(owner): Owner,
    Body(req): Body<AddMember>,
) -> ApiResult<Json<Member>> {
    let login = req.login.trim().trim_start_matches('@').to_string();
    if !valid_github_name(&login) {
        return Err(invalid_login());
    }
    let token = user_token(&state, &owner).await?;
    let user = github_user(&state, &token, &login).await?;
    let ts = now();
    let member = state
        .db
        .write(move |conn| -> ApiResult<Member> {
            let result = conn.execute(
                "INSERT INTO users (github_id, login, name, avatar_url, role, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    user.id,
                    user.login,
                    user.name,
                    user.avatar_url,
                    Role::Member,
                    ts
                ],
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

/// `GET {api}/users/{login}` with the owner's token; an unknown login is 422 `invalid_login`.
async fn github_user(state: &AppState, token: &str, login: &str) -> ApiResult<GhUser> {
    let url = format!("{}/users/{login}", state.github.api_base());
    let response = state
        .github
        .send(|| state.http.get(&url).bearer_auth(token))
        .await
        .map_err(|err| match err.github {
            Some(GitHubFailure::Status(404)) => {
                ApiError::unprocessable("invalid_login", format!("GitHub has no user \"{login}\"."))
            }
            _ => err,
        })?;
    GitHub::json(response, &url).await
}

fn invalid_login() -> ApiError {
    ApiError::unprocessable(
        "invalid_login",
        "Enter a GitHub username (letters, digits and hyphens).",
    )
}

/// Removes a member together with the API keys they created, so the keys stop working at once.
async fn remove(
    State(state): State<AppState>,
    _owner: Owner,
    PathId(id): PathId,
) -> ApiResult<StatusCode> {
    let (role, revoked_keys) = state
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let role: Option<Role> = tx
                .query_row("SELECT role FROM users WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            let mut revoked_keys = Vec::new();
            if role == Some(Role::Member) {
                let mut stmt =
                    tx.prepare("DELETE FROM api_keys WHERE created_by = ?1 RETURNING id")?;
                revoked_keys = stmt
                    .query_map([id], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(stmt);
                tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
            }
            tx.commit()?;
            Ok::<_, rusqlite::Error>((role, revoked_keys))
        })
        .await?;
    match role {
        Some(Role::Member) => {
            let mut touched = lock(&state.key_touch);
            for key_id in revoked_keys {
                touched.remove(&key_id);
            }
            Ok(StatusCode::NO_CONTENT)
        }
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
