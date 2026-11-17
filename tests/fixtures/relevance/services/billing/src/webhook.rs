//! Incoming webhooks of the payment provider.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

/// Webhooks older than this are rejected to stop replays.
const SIGNATURE_TOLERANCE_SECS: i64 = 300;

#[derive(Debug, Deserialize)]
pub struct ProviderEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub created: i64,
    pub data: serde_json::Value,
}

/// Checks the `Provider-Signature` header (`t=<timestamp>,v1=<hex hmac>`) against the shared
/// webhook secret with a constant-time comparison, and rejects stale timestamps.
pub fn verify_webhook_signature(header: &str, payload: &[u8], secret: &[u8], now: i64) -> bool {
    let mut timestamp = None;
    let mut signature = None;
    for part in header.split(',') {
        match part.split_once('=') {
            Some(("t", value)) => timestamp = value.parse::<i64>().ok(),
            Some(("v1", value)) => signature = hex::decode(value).ok(),
            _ => {}
        }
    }
    let (Some(timestamp), Some(signature)) = (timestamp, signature) else {
        return false;
    };
    if (now - timestamp).abs() > SIGNATURE_TOLERANCE_SECS {
        return false;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    mac.verify_slice(&signature).is_ok()
}

/// Parses a verified webhook body. Only payment events are handled; others are ignored.
pub fn parse_provider_event(payload: &[u8]) -> Result<Option<ProviderEvent>, serde_json::Error> {
    let event: ProviderEvent = serde_json::from_slice(payload)?;
    let handled = matches!(
        event.kind.as_str(),
        "charge.succeeded" | "charge.failed" | "charge.refunded" | "charge.dispute.created"
    );
    Ok(handled.then_some(event))
}
