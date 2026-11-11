use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::error::{ApiError, ApiResult, GitHubFailure};
use crate::github::app::GitHub;

const RAW_ACCEPT: &str = "application/vnd.github.raw+json";
const TARBALL_TIMEOUT: Duration = Duration::from_secs(600);
/// Tarballs hold a private repository's whole source: owner read/write only.
#[cfg(unix)]
const TARBALL_FILE_MODE: u32 = 0o600;
const BYTES_PER_GIB: f64 = (1u64 << 30) as f64;
const PER_PAGE: usize = 100;
const MAX_REPO_PAGES: u32 = 10;
const MAX_INSTALLATION_PAGES: u32 = 10;

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

/// Commit SHA at the head of `branch`; a missing branch is 422 `branch_not_found`.
pub async fn branch_head(
    gh: &GitHub,
    token: &str,
    full_name: &str,
    branch: &str,
) -> ApiResult<String> {
    #[derive(Deserialize)]
    struct Commit {
        sha: String,
    }
    #[derive(Deserialize)]
    struct Branch {
        commit: Commit,
    }
    let url = format!(
        "{}/repos/{full_name}/branches/{}",
        gh.api_base(),
        percent_encode(branch)
    );
    get_json::<Branch>(gh, token, &url)
        .await
        .map(|branch| branch.commit.sha)
        .map_err(|err| {
            map_not_found(err, || {
                ApiError::unprocessable(
                    "branch_not_found",
                    format!("Branch \"{branch}\" was not found in {full_name}."),
                )
            })
        })
}

/// Branch names of the first page (100 branches).
pub async fn branches(gh: &GitHub, token: &str, full_name: &str) -> ApiResult<Vec<String>> {
    #[derive(Deserialize)]
    struct Branch {
        name: String,
    }
    let url = format!(
        "{}/repos/{full_name}/branches?per_page={PER_PAGE}",
        gh.api_base()
    );
    let branches: Vec<Branch> = get_json(gh, token, &url).await?;
    Ok(branches.into_iter().map(|b| b.name).collect())
}

/// Recursive tree of a commit and whether GitHub truncated it.
pub async fn tree(
    gh: &GitHub,
    token: &str,
    full_name: &str,
    sha: &str,
) -> ApiResult<(Vec<RawTreeEntry>, bool)> {
    #[derive(Deserialize)]
    struct Tree {
        tree: Vec<RawTreeEntry>,
        #[serde(default)]
        truncated: bool,
    }
    let url = format!(
        "{}/repos/{full_name}/git/trees/{}?recursive=1",
        gh.api_base(),
        percent_encode(sha)
    );
    let tree: Tree = get_json(gh, token, &url).await?;
    Ok((tree.tree, tree.truncated))
}

/// Raw bytes of a blob.
pub async fn blob(gh: &GitHub, token: &str, full_name: &str, blob_sha: &str) -> ApiResult<Vec<u8>> {
    let url = format!(
        "{}/repos/{full_name}/git/blobs/{}",
        gh.api_base(),
        percent_encode(blob_sha)
    );
    let response = gh
        .send(|| {
            gh.http()
                .get(&url)
                .bearer_auth(token)
                .header(reqwest::header::ACCEPT, RAW_ACCEPT)
        })
        .await?;
    let bytes = response.bytes().await.map_err(read_failed)?;
    Ok(bytes.to_vec())
}

/// Streams the gzipped tarball of a commit to `dest` (created with mode 0600). An archive larger
/// than `max_bytes` fails with 422 `archive_too_large`; the caller removes the partial file.
pub async fn tarball(
    gh: &GitHub,
    token: &str,
    full_name: &str,
    sha: &str,
    dest: &Path,
    max_bytes: u64,
) -> ApiResult<()> {
    let url = format!(
        "{}/repos/{full_name}/tarball/{}",
        gh.api_base(),
        percent_encode(sha)
    );
    let mut response = gh
        .send(|| {
            gh.http()
                .get(&url)
                .bearer_auth(token)
                .timeout(TARBALL_TIMEOUT)
        })
        .await?;
    if response.content_length().is_some_and(|len| len > max_bytes) {
        return Err(archive_too_large(max_bytes));
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(TARBALL_FILE_MODE);
    let mut file = options.open(dest).await.map_err(ApiError::internal)?;
    let mut written: u64 = 0;
    while let Some(chunk) = response.chunk().await.map_err(read_failed)? {
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err(archive_too_large(max_bytes));
        }
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
    }
    file.flush().await.map_err(ApiError::internal)?;
    Ok(())
}

fn archive_too_large(max_bytes: u64) -> ApiError {
    let gib = (max_bytes as f64 / BYTES_PER_GIB * 100.0).round() / 100.0;
    ApiError::unprocessable(
        "archive_too_large",
        format!("Repository archive is larger than {gib} GiB. Raise SOURCELENS_MAX_ARCHIVE_BYTES."),
    )
}

/// Installations of this app that the signed-in user can access.
pub async fn user_installations(gh: &GitHub, user_token: &str) -> ApiResult<Vec<Installation>> {
    #[derive(Deserialize)]
    struct Page {
        installations: Vec<Installation>,
    }
    let mut all = Vec::new();
    for page in 1..=MAX_INSTALLATION_PAGES {
        let url = format!(
            "{}/user/installations?per_page={PER_PAGE}&page={page}",
            gh.api_base()
        );
        let batch: Page = get_json(gh, user_token, &url).await?;
        let count = batch.installations.len();
        all.extend(batch.installations);
        if count < PER_PAGE {
            break;
        }
    }
    Ok(all)
}

/// Repositories of one installation visible to the user, at most 1,000; `true` when more exist.
pub async fn user_installation_repos(
    gh: &GitHub,
    user_token: &str,
    installation_id: u64,
) -> ApiResult<(Vec<GhRepo>, bool)> {
    #[derive(Deserialize)]
    struct Page {
        total_count: u64,
        repositories: Vec<GhRepo>,
    }
    let mut all = Vec::new();
    let mut total = 0;
    for page in 1..=MAX_REPO_PAGES {
        let url = format!(
            "{}/user/installations/{installation_id}/repositories?per_page={PER_PAGE}&page={page}",
            gh.api_base()
        );
        let batch: Page = get_json(gh, user_token, &url).await?;
        total = batch.total_count;
        let count = batch.repositories.len();
        all.extend(batch.repositories);
        if count < PER_PAGE {
            break;
        }
    }
    let truncated = total > all.len() as u64;
    Ok((all, truncated))
}

/// Repository as seen with the user's token; 404 → 404 `repository_not_accessible`.
pub async fn user_repo(gh: &GitHub, user_token: &str, full_name: &str) -> ApiResult<GhRepo> {
    let url = format!("{}/repos/{full_name}", gh.api_base());
    get_json(gh, user_token, &url).await.map_err(|err| {
        map_not_found(err, || {
            ApiError::not_found(
                "repository_not_accessible",
                format!("{full_name} is not accessible to you and the GitHub App."),
            )
        })
    })
}

/// Replaces a GitHub 404 with `not_found()`, keeping [`ApiError::github`]; other errors pass through.
fn map_not_found(err: ApiError, not_found: impl FnOnce() -> ApiError) -> ApiError {
    if err.github == Some(GitHubFailure::Status(404)) {
        not_found().with_github(err.github)
    } else {
        err
    }
}

async fn get_json<T: DeserializeOwned>(gh: &GitHub, token: &str, url: &str) -> ApiResult<T> {
    let response = gh.send(|| gh.http().get(url).bearer_auth(token)).await?;
    GitHub::json(response, url).await
}

fn read_failed(err: reqwest::Error) -> ApiError {
    ApiError::bad_gateway(
        "github_unreachable",
        format!("Reading the GitHub response failed: {err}"),
    )
}

/// Percent-encodes everything except RFC 3986 unreserved characters (so `/` in branch names becomes `%2F`).
pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn percent_encode_keeps_unreserved_and_escapes_the_rest() {
        assert_eq!(percent_encode("main"), "main");
        assert_eq!(percent_encode("feature/x y"), "feature%2Fx%20y");
        assert_eq!(percent_encode("ä"), "%C3%A4");
    }
}
