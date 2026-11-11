use serde::{Deserialize, Serialize};

use crate::error::ApiResult;
use crate::http::session::AuthUser;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GhToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
}

pub async fn user_token(_state: &AppState, _user: &AuthUser) -> ApiResult<String> {
    todo!("github-auth")
}
