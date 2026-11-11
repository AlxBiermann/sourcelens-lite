use serde::{Deserialize, Serialize};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettings {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub request_dimensions: Option<u32>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub batch_size: u32,
    /// Detected from the provider; read-only.
    pub dimensions: Option<u32>,
    /// Read-only.
    pub api_key_set: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingSettingsInput {
    pub enabled: bool,
    pub base_url: String,
    pub model: String,
    pub request_dimensions: Option<u32>,
    pub query_prefix: String,
    pub document_prefix: String,
    pub batch_size: u32,
    pub api_key: Option<String>,
    pub clear_api_key: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Provider(String),
}

pub struct Embedder {
    _private: (),
}

impl Embedder {
    pub async fn embed(&self, _inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        todo!("embeddings")
    }
}

pub async fn load(_state: &AppState) -> Result<Option<(EmbeddingSettings, Embedder)>, ApiError> {
    todo!("embeddings")
}

pub async fn embed_pending(_state: &AppState, _repo_id: i64, _job_id: i64) -> ApiResult<u32> {
    todo!("embeddings")
}

pub async fn apply_settings(
    _state: &AppState,
    _new: EmbeddingSettingsInput,
) -> ApiResult<(EmbeddingSettings, u32)> {
    todo!("embeddings")
}
