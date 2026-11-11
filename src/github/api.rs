use std::path::Path;

use serde::Deserialize;

use crate::error::ApiResult;
use crate::github::app::GitHub;

#[derive(Debug, Clone, Deserialize)]
pub struct RawTreeEntry {
    pub path: String,
    pub mode: String,
    #[serde(rename = "type")]
    pub r#type: String,
    pub sha: String,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhAccount {
    pub login: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub id: u64,
    pub account: GhAccount,
    pub html_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GhRepo {
    pub id: u64,
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
}

pub async fn branch_head(
    _gh: &GitHub,
    _token: &str,
    _full_name: &str,
    _branch: &str,
) -> ApiResult<String> {
    todo!("repos-jobs")
}

pub async fn branches(_gh: &GitHub, _token: &str, _full_name: &str) -> ApiResult<Vec<String>> {
    todo!("repos-jobs")
}

pub async fn tree(
    _gh: &GitHub,
    _token: &str,
    _full_name: &str,
    _sha: &str,
) -> ApiResult<(Vec<RawTreeEntry>, bool)> {
    todo!("repos-jobs")
}

pub async fn blob(
    _gh: &GitHub,
    _token: &str,
    _full_name: &str,
    _blob_sha: &str,
) -> ApiResult<Vec<u8>> {
    todo!("repos-jobs")
}

pub async fn tarball(
    _gh: &GitHub,
    _token: &str,
    _full_name: &str,
    _sha: &str,
    _dest: &Path,
) -> ApiResult<()> {
    todo!("repos-jobs")
}

pub async fn user_installations(_gh: &GitHub, _user_token: &str) -> ApiResult<Vec<Installation>> {
    todo!("repos-jobs")
}

pub async fn user_installation_repos(
    _gh: &GitHub,
    _user_token: &str,
    _installation_id: u64,
) -> ApiResult<(Vec<GhRepo>, bool)> {
    todo!("repos-jobs")
}

pub async fn user_repo(_gh: &GitHub, _user_token: &str, _full_name: &str) -> ApiResult<GhRepo> {
    todo!("repos-jobs")
}
