use crate::domain::Observation;
use async_trait::async_trait;
use axum::http::HeaderMap;
use contracts::*;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

type HmacSha256 = Hmac<Sha256>;
pub const MOCK_WEBHOOK_ID: &str = "synthetic-webhook-s1";
pub const MOCK_WEBHOOK_KEY: &str = "synthetic-only-not-a-real-secret";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockPayoutFixture {
    Success,
    Failed,
    Pending,
    Timeout,
}
impl MockPayoutFixture {
    pub fn parse(value: &str) -> Result<Self, ApiError> {
        match value {
            "success" => Ok(Self::Success),
            "failed" => Ok(Self::Failed),
            "pending" => Ok(Self::Pending),
            "timeout" => Ok(Self::Timeout),
            _ => Err(ApiError::new(
                ErrorCode::InvalidInput,
                "unknown mock payout fixture",
            )),
        }
    }
}
/// Operation-specific failures prevent query outages from changing payout finality.
/// Verified query facts requiring a hold are successful `Observation`s with source
/// `sandbox-item-query:*` and status Mismatch, Refunded, Reversed, or Unknown.
/// `QueryFailed` is reserved for unavailable/unreadable results, never fact mismatches.
#[derive(Debug)]
pub enum PayoutError {
    NotSent(ApiError),
    Ambiguous(ApiError),
    QueryFailed { error: ApiError, retryable: bool },
}
#[async_trait]
pub trait PayPalPayouts: Send + Sync {
    fn sandbox(&self) -> bool {
        false
    }
    async fn send(&self, payout: &Payout, approval: &Approval) -> Result<Observation, PayoutError>;
    async fn query(&self, payout: &Payout, approval: &Approval)
        -> Result<Observation, PayoutError>;
    fn verify_webhook(&self, raw_body: &[u8], headers: &HeaderMap) -> Result<(), ApiError>;
}
#[derive(Clone, Default)]
pub struct MockProvider(Arc<Mutex<BTreeMap<String, Observation>>>);
pub struct MockPayPal {
    provider: MockProvider,
    pub fixture: MockPayoutFixture,
    pub send_calls: AtomicUsize,
    pub query_calls: AtomicUsize,
}
impl MockPayPal {
    pub fn new(fixture: MockPayoutFixture) -> Self {
        Self::with_provider(fixture, MockProvider::default())
    }
    pub fn provider(&self) -> MockProvider {
        self.provider.clone()
    }
    pub fn with_provider(fixture: MockPayoutFixture, provider: MockProvider) -> Self {
        Self {
            provider,
            fixture,
            send_calls: AtomicUsize::new(0),
            query_calls: AtomicUsize::new(0),
        }
    }
    pub fn observation(payout: &Payout, state: PayoutState) -> Observation {
        Observation {
            state,
            paypal_batch_id: Some(format!("mock-batch-{}", payout.id)),
            item_id: Some(format!("mock-item-{}", payout.id)),
            binding: payout.binding.clone(),
            source: "mock-item-query".into(),
            status: match state {
                PayoutState::Paid => "Succeeded",
                PayoutState::PayoutFailed => "Failed",
                PayoutState::PayoutPending => "Pending",
                PayoutState::PayoutUnknown => "Unknown",
            }
            .into(),
        }
    }
}
#[async_trait]
impl PayPalPayouts for MockPayPal {
    async fn send(&self, payout: &Payout, approval: &Approval) -> Result<Observation, PayoutError> {
        if payout.sender_batch_id != approval.sender_batch_id() || payout.approval_id != approval.id
        {
            return Err(PayoutError::NotSent(ApiError::new(
                ErrorCode::ApprovalInvalid,
                "request ID binding invalid",
            )));
        }
        self.send_calls.fetch_add(1, Ordering::SeqCst);
        let state = match self.fixture {
            MockPayoutFixture::Pending => PayoutState::PayoutPending,
            MockPayoutFixture::Failed => PayoutState::PayoutFailed,
            _ => PayoutState::Paid,
        };
        self.provider
            .0
            .lock()
            .unwrap()
            .entry(payout.sender_batch_id.clone())
            .or_insert_with(|| Self::observation(payout, state));
        match self.fixture {
            MockPayoutFixture::Timeout => Err(PayoutError::Ambiguous(ApiError::new(
                ErrorCode::AdapterUnavailable,
                "synthetic timeout after provider acceptance",
            ))),
            MockPayoutFixture::Failed => Ok(Self::observation(payout, PayoutState::PayoutFailed)),
            _ => Ok(Self::observation(payout, PayoutState::PayoutPending)),
        }
    }
    async fn query(&self, payout: &Payout, _: &Approval) -> Result<Observation, PayoutError> {
        self.query_calls.fetch_add(1, Ordering::SeqCst);
        self.provider
            .0
            .lock()
            .unwrap()
            .get(&payout.sender_batch_id)
            .cloned()
            .ok_or_else(|| PayoutError::QueryFailed {
                error: ApiError::new(
                    ErrorCode::AdapterUnavailable,
                    "synthetic provider batch not found; do not resend",
                ),
                retryable: false,
            })
    }

    fn verify_webhook(&self, raw_body: &[u8], headers: &HeaderMap) -> Result<(), ApiError> {
        let error = || {
            ApiError::new(
                ErrorCode::WebhookUnverified,
                "synthetic webhook signature invalid",
            )
        };
        let supplied = headers
            .get("x-mock-signature")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(error)?;
        if headers
            .get("x-mock-webhook-id")
            .and_then(|h| h.to_str().ok())
            != Some(MOCK_WEBHOOK_ID)
        {
            return Err(error());
        }
        let bytes = decode_hex(supplied).ok_or_else(error)?;
        let mut mac = HmacSha256::new_from_slice(MOCK_WEBHOOK_KEY.as_bytes()).unwrap();
        mac.update(MOCK_WEBHOOK_ID.as_bytes());
        mac.update(b"\n");
        mac.update(raw_body);
        mac.verify_slice(&bytes).map_err(|_| error())
    }
}
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() != 64 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}
pub fn mock_signature(raw_body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(MOCK_WEBHOOK_KEY.as_bytes()).unwrap();
    mac.update(MOCK_WEBHOOK_ID.as_bytes());
    mac.update(b"\n");
    mac.update(raw_body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[async_trait]
pub trait ReceiptExtractor: Send + Sync {
    fn ai_enabled(&self) -> bool {
        false
    }
    async fn extract(&self, receipt_text: &str) -> ReceiptFields;
}
pub struct MockLlm;
#[async_trait]
impl ReceiptExtractor for MockLlm {
    async fn extract(&self, receipt_text: &str) -> ReceiptFields {
        let output = match receipt_text {
            "mock:clean" => {
                r#"{"merchant":"Synthetic shop","date":"2026-10-01","total_cents":750,"currency":"USD","uncertain":[]}"#
            }
            "mock:uncertain" => {
                r#"{"merchant":null,"date":"2026-10-01","total_cents":null,"currency":"USD","uncertain":["merchant","total_cents"]}"#
            }
            "mock:malformed" => "this is not JSON",
            other => other,
        };
        serde_json::from_str(output).unwrap_or_else(|_| ReceiptFields {
            uncertain: vec!["malformed_model_output".into()],
            ..ReceiptFields::default()
        })
    }
}

/// No provider is configured: fields must be entered and confirmed manually.
pub struct ManualInput;
#[async_trait]
impl ReceiptExtractor for ManualInput {
    async fn extract(&self, _: &str) -> ReceiptFields {
        ReceiptFields {
            uncertain: vec![
                "merchant".into(),
                "date".into(),
                "total_cents".into(),
                "currency".into(),
            ],
            ..ReceiptFields::default()
        }
    }
}
