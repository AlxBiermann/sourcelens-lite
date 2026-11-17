use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::github::app::GitHub;
use crate::github::oauth::GhUser;
use crate::http::session::{user_token, Owner, PROVIDER_SQL};
use crate::http::{Body, PathId};
use crate::model::{Provider, Role};
use crate::state::{lock, now, AppState};

/// `SELECT` of the columns [`member_from_row`] reads, from `users`, followed by `rest`.
fn select_members(rest: &str) -> String {
    format!(
        "SELECT id, login, name, avatar_url, role, last_login_at IS NULL, last_login_at,
                {PROVIDER_SQL}, email, status = 'pending'
         FROM users {rest}"
    )
}

/// `/api/members[/{id}[/approve]]`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/members", get(list).post(add))
        .route("/api/members/{id}", patch(update_role).delete(remove))
        .route("/api/members/{id}/approve", post(approve))
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
    provider: Provider,
    email: Option<String>,
    /// Signed in with SSO and waits for an admin's approval.
    awaiting_approval: bool,
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
        provider: row.get(7)?,
        email: row.get(8)?,
        awaiting_approval: row.get(9)?,
    })
}

fn load_member(conn: &Connection, id: i64) -> rusqlite::Result<Option<Member>> {
    conn.query_row(&select_members("WHERE id = ?1"), [id], member_from_row)
        .optional()
}

/// GitHub user and organization names: `^[A-Za-z0-9-]{1,39}$`.
pub(crate) fn valid_github_name(name: &str) -> bool {
    (1..=39).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Accounts awaiting approval first, then admins, then by login.
async fn list(State(state): State<AppState>, _owner: Owner) -> ApiResult<Json<Vec<Member>>> {
    let members = state
        .db
        .read(|conn| {
            let mut stmt = conn.prepare(&select_members(
                "ORDER BY status = 'pending' DESC, role = ?1 DESC, login",
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
    let token = match owner.provider {
        Provider::Github => Some(user_token(&state, &owner).await?),
        Provider::Sso => None,
    };
    let user = github_user(&state, token.as_deref(), &login).await?;
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
                &select_members("WHERE id = ?1"),
                [conn.last_insert_rowid()],
                member_from_row,
            )?)
        })
        .await?;
    Ok(Json(member))
}

/// `GET {api}/users/{login}`, with the admin's token when they sign in with GitHub (SSO admins have
/// none; the endpoint is public). An unknown login is 422 `invalid_login`.
async fn github_user(state: &AppState, token: Option<&str>, login: &str) -> ApiResult<GhUser> {
    let url = format!("{}/users/{login}", state.github.api_base());
    let response = state
        .github
        .send(|| {
            let request = state.http.get(&url);
            match token {
                Some(token) => request.bearer_auth(token),
                None => request,
            }
        })
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

/// Lets an account that signed in with SSO in; approving an active account changes nothing.
async fn approve(
    State(state): State<AppState>,
    _owner: Owner,
    PathId(id): PathId,
) -> ApiResult<Json<Member>> {
    let member = state
        .db
        .write(move |conn| -> ApiResult<Member> {
            conn.execute("UPDATE users SET status = 'active' WHERE id = ?1", [id])?;
            load_member(conn, id)?.ok_or_else(member_not_found)
        })
        .await?;
    Ok(Json(member))
}

#[derive(Deserialize)]
struct RoleChange {
    role: String,
}

/// Makes an approved SSO account an admin or a member again. GitHub accounts keep their role, so
/// the admin who set up the instance cannot be demoted; nobody changes their own role. Demoting
/// revokes the API keys the account created, since members who sign in with SSO cannot have any.
async fn update_role(
    State(state): State<AppState>,
    Owner(owner): Owner,
    PathId(id): PathId,
    Body(req): Body<RoleChange>,
) -> ApiResult<Json<Member>> {
    let role = Role::parse(&req.role).ok_or_else(|| {
        ApiError::unprocessable("invalid_role", "Use the role \"owner\" or \"member\".")
    })?;
    let own_id = owner.id;
    let (member, revoked_keys) = state
        .db
        .write(move |conn| -> ApiResult<(Member, Vec<i64>)> {
            let tx = conn.transaction()?;
            let changeable: Option<bool> = tx
                .query_row(
                    "SELECT sso_sub IS NOT NULL AND status = 'active' FROM users WHERE id = ?1",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            match changeable {
                None => return Err(member_not_found()),
                Some(_) if id == own_id => {
                    return Err(ApiError::conflict(
                        "cannot_change_own_role",
                        "You cannot change your own role.",
                    ))
                }
                Some(false) => {
                    return Err(ApiError::conflict(
                        "role_not_changeable",
                        "Only approved SSO members can change roles.",
                    ))
                }
                Some(true) => {}
            }
            tx.execute(
                "UPDATE users SET role = ?1 WHERE id = ?2",
                params![role, id],
            )?;
            let revoked_keys = if role == Role::Member {
                delete_keys(&tx, id)?
            } else {
                Vec::new()
            };
            let member = load_member(&tx, id)?.ok_or_else(member_not_found)?;
            tx.commit()?;
            Ok((member, revoked_keys))
        })
        .await?;
    forget_key_touches(&state, revoked_keys);
    Ok(Json(member))
}

/// Deletes the API keys `user_id` created and returns their ids.
pub(crate) fn delete_keys(conn: &Connection, user_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("DELETE FROM api_keys WHERE created_by = ?1 RETURNING id")?;
    let ids = stmt
        .query_map([user_id], |row| row.get::<_, i64>(0))?
        .collect();
    ids
}

/// Drops pending `last_used_at` updates of deleted keys.
pub(crate) fn forget_key_touches(state: &AppState, key_ids: Vec<i64>) {
    let mut touched = lock(&state.key_touch);
    for key_id in key_ids {
        touched.remove(&key_id);
    }
}

/// Removes a member together with the API keys they created, so the keys stop working at once.
/// For an account awaiting approval this is the rejection.
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
                revoked_keys = delete_keys(&tx, id)?;
                tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
            }
            tx.commit()?;
            Ok::<_, rusqlite::Error>((role, revoked_keys))
        })
        .await?;
    match role {
        Some(Role::Member) => {
            forget_key_touches(&state, revoked_keys);
            Ok(StatusCode::NO_CONTENT)
        }
        Some(Role::Owner) => Err(ApiError::conflict(
            "cannot_remove_owner",
            "Admins cannot be removed. Make them a member first.",
        )),
        None => Err(member_not_found()),
    }
}

fn member_not_found() -> ApiError {
    ApiError::not_found("member_not_found", "This member does not exist.")
}
