//! HTTP client for an OpenAI-compatible `POST {baseUrl}/embeddings` endpoint.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use axum::http::StatusCode;
use reqwest::header::RETRY_AFTER;
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;

const MAX_ATTEMPTS: u32 = 5;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Back-off without `Retry-After` is `2^attempt` seconds, the exponent capped here.
const MAX_BACKOFF_EXPONENT: u32 = 6;
/// Random extra delay added to every retry, in milliseconds (inclusive upper bound).
const JITTER_MS: u64 = 250;
/// Self-hosted models on a CPU can take minutes for one large batch.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest provider error message passed on to the user.
const MAX_ERROR_MESSAGE_CHARS: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// The provider refused a single input (400/413).
    #[error("{0}")]
    Rejected(String),
    /// Network error, unexpected status or malformed response after all retries.
    #[error("{0}")]
    Provider(String),
}

pub struct Embedder {
    http: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    request_dimensions: Option<u32>,
}

/// Outcome of one batch request after retries.
enum Failure {
    /// 400 or 413: the batch (or input) was refused; halving may help.
    Refused(String),
    Provider(String),
}

/// One slot per input: its vector, or the provider's reason for refusing that single input.
/// The outer `Err` is a provider failure that affects the whole call.
pub(crate) type PartialResult = Result<Vec<Result<Vec<f32>, String>>, String>;

#[derive(Deserialize)]
struct EmbeddingsResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    embedding: Vec<f32>,
    index: Option<usize>,
}

impl Embedder {
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        request_dimensions: Option<u32>,
    ) -> Self {
        Self {
            http,
            url: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            request_dimensions,
        }
    }

    /// Embeds `inputs` in order and returns L2-normalised vectors.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let results = self
            .embed_partial(inputs)
            .await
            .map_err(EmbedError::Provider)?;
        results
            .into_iter()
            .map(|r| r.map_err(EmbedError::Rejected))
            .collect()
    }

    /// Like `embed`, but a refused single input yields `Err(message)` in its slot instead of failing
    /// the whole call. Refused batches are split in halves and retried.
    pub(crate) fn embed_partial<'a>(
        &'a self,
        inputs: &'a [String],
    ) -> Pin<Box<dyn Future<Output = PartialResult> + Send + 'a>> {
        Box::pin(async move {
            if inputs.is_empty() {
                return Ok(Vec::new());
            }
            match self.request(inputs).await {
                Ok(vectors) => Ok(vectors.into_iter().map(Ok).collect()),
                Err(Failure::Refused(_)) if inputs.len() > 1 => {
                    let (left, right) = inputs.split_at(inputs.len() / 2);
                    let mut out = self.embed_partial(left).await?;
                    out.extend(self.embed_partial(right).await?);
                    Ok(out)
                }
                Err(Failure::Refused(message)) => Ok(vec![Err(message)]),
                Err(Failure::Provider(message)) => Err(message),
            }
        })
    }

    /// One batch with retries on network errors, 429 and 5xx.
    async fn request(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, Failure> {
        let mut body = serde_json::json!({ "model": self.model, "input": inputs });
        if let Some(dimensions) = self.request_dimensions {
            body["dimensions"] = dimensions.into();
        }
        let mut last_error = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            let mut builder = self
                .http
                .post(&self.url)
                .timeout(REQUEST_TIMEOUT)
                .json(&body);
            if let Some(key) = &self.api_key {
                builder = builder.bearer_auth(key);
            }
            let retry_after = match builder.send().await {
                Err(err) => {
                    last_error = format!("could not reach {}: {err}", self.url);
                    None
                }
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return parse_response(response, inputs.len()).await;
                    }
                    let retry_after = response
                        .headers()
                        .get(RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.trim().parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let message = error_message(status, &response.text().await.unwrap_or_default());
                    if status == StatusCode::BAD_REQUEST || status == StatusCode::PAYLOAD_TOO_LARGE
                    {
                        return Err(Failure::Refused(message));
                    }
                    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
                        return Err(Failure::Provider(message));
                    }
                    last_error = message;
                    retry_after
                }
            };
            if attempt + 1 < MAX_ATTEMPTS {
                tokio::time::sleep(retry_delay(attempt, retry_after)).await;
            }
        }
        Err(Failure::Provider(last_error))
    }
}

async fn parse_response(
    response: reqwest::Response,
    expected: usize,
) -> Result<Vec<Vec<f32>>, Failure> {
    let parsed: EmbeddingsResponse = response
        .json()
        .await
        .map_err(|err| Failure::Provider(format!("unreadable response: {err}")))?;
    if parsed.data.len() != expected {
        return Err(Failure::Provider(format!(
            "expected {expected} embeddings, received {}",
            parsed.data.len()
        )));
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; expected];
    for (position, item) in parsed.data.into_iter().enumerate() {
        let index = item.index.unwrap_or(position);
        match slots.get_mut(index) {
            Some(slot @ None) => *slot = Some(normalize(item.embedding)),
            _ => {
                return Err(Failure::Provider(format!(
                    "invalid embedding index {index} in response"
                )))
            }
        }
    }
    slots
        .into_iter()
        .map(|slot| {
            slot.filter(|v| !v.is_empty())
                .ok_or_else(|| Failure::Provider("response contains an empty embedding".into()))
        })
        .collect()
}

fn normalize(mut vector: Vec<f32>) -> Vec<f32> {
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 && norm.is_finite() {
        for x in &mut vector {
            *x /= norm;
        }
    }
    vector
}

fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let base = retry_after
        .unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(MAX_BACKOFF_EXPONENT)));
    let mut jitter = [0u8; 2];
    let jitter_ms = match SystemRandom::new().fill(&mut jitter) {
        Ok(()) => u64::from(u16::from_le_bytes(jitter)) % (JITTER_MS + 1),
        Err(_) => 0,
    };
    base.min(MAX_RETRY_DELAY) + Duration::from_millis(jitter_ms)
}

/// The status plus the JSON `error.message` field. Nothing else of the body is passed on: the base
/// URL can point anywhere, and the body must not turn the connection test into a way to read it.
fn error_message(status: StatusCode, body: &str) -> String {
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")?.as_str().map(|m| {
                m.trim()
                    .chars()
                    .take(MAX_ERROR_MESSAGE_CHARS)
                    .collect::<String>()
            })
        })
        .filter(|m| !m.is_empty());
    match message {
        Some(message) => format!("HTTP {}: {message}", status.as_u16()),
        None => format!("HTTP {}", status.as_u16()),
    }
}
