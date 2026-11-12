pub mod adapters;
pub mod config;
pub mod domain;
pub mod extract;
pub mod http;
pub mod policy;
pub mod reconcile;
pub mod sandbox;
pub mod security;
pub mod storage;
pub mod webhook;

use adapters::{PayPalPayouts, PayoutError, ReceiptExtractor};
use contracts::*;
use domain::{Command, Observation, Output, PayoutAction};
use std::sync::Arc;
use storage::Storage;

fn manual_fallback(mut fields: ReceiptFields) -> ReceiptFields {
    for (name, missing) in [
        ("merchant", fields.merchant.is_none()),
        ("date", fields.date.is_none()),
        ("total_cents", fields.total_cents.is_none()),
        ("currency", fields.currency.is_none()),
    ] {
        if missing && !fields.uncertain.iter().any(|value| value == name) {
            fields.uncertain.push(name.into());
        }
    }
    fields
}

pub struct Service {
    pub storage: Arc<dyn Storage>,
    pub paypal: Arc<dyn PayPalPayouts>,
    pub extractor: Arc<dyn ReceiptExtractor>,
    pub finance_token: String,
}
impl Service {
    pub async fn claim(&self, command: Command) -> Result<ClaimResponse, ApiError> {
        match self.storage.execute(command).await? {
            Output::Claim(response) => Ok(response),
            _ => Err(ApiError::new(
                ErrorCode::InvalidInput,
                "expected claim command",
            )),
        }
    }
    pub async fn extract(&self, id: ClaimId) -> Result<ClaimResponse, ApiError> {
        let text = match self.storage.execute(Command::ExtractionInput(id)).await? {
            Output::Receipt(text) => text,
            _ => unreachable!(),
        };
        if text.chars().count() > 4000 {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "maximum input is 4000 characters",
            ));
        }
        let existing = self.claim(Command::Get(id)).await?;
        let account = existing.claim.claimant_id.to_string();
        if !self.extractor.ai_enabled() {
            return self
                .claim(Command::Extract {
                    id,
                    fields: self.extractor.extract(&text).await,
                })
                .await;
        }
        let quota = self
            .storage
            .execute(Command::ExtractionQuota {
                account,
                now: chrono::Utc::now().timestamp(),
            })
            .await;
        if let Err(error) = quota {
            if error.code != ErrorCode::InvalidInput {
                return Err(error);
            }
            return self
                .claim(Command::Extract {
                    id,
                    fields: manual_fallback(existing.claim.fields.clone()),
                })
                .await;
        }
        let fields = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.extractor.extract(&text),
        )
        .await
        .unwrap_or_else(|_| manual_fallback(existing.claim.fields));
        self.claim(Command::Extract { id, fields }).await
    }
    pub async fn payout(&self, id: ClaimId, now: i64) -> Result<ClaimResponse, ApiError> {
        let (response, action) = match self
            .storage
            .execute(Command::PreparePayout {
                payout_id: PayoutId::new(),
                id,
                now,
            })
            .await?
        {
            Output::Prepared { response, action } => (response, action),
            _ => unreachable!(),
        };
        if action == PayoutAction::Complete {
            return Ok(response);
        }
        let payout = response.payout.as_ref().unwrap();
        let approval = response.approval.as_ref().unwrap();
        let observation = match action {
            PayoutAction::SendNew => match self.paypal.send(payout, approval).await {
                Ok(result) => result,
                Err(PayoutError::Ambiguous(_)) => Observation {
                    state: PayoutState::PayoutUnknown,
                    paypal_batch_id: None,
                    item_id: None,
                    binding: None,
                    source: "ambiguous-create".into(),
                    status: "Ambiguous".into(),
                },
                Err(PayoutError::NotSent(error)) => {
                    self.claim(Command::NotSent(id)).await?;
                    return Err(error);
                }
                Err(PayoutError::QueryFailed { error, .. }) => return Err(error),
            },
            PayoutAction::QueryExisting => {
                let mut attempt = 0;
                loop {
                    match self.paypal.query(payout, approval).await {
                        Ok(result) => break result,
                        Err(PayoutError::QueryFailed {
                            retryable: true, ..
                        }) if attempt < 2 => {
                            tokio::time::sleep(std::time::Duration::from_millis(20 << attempt))
                                .await;
                            attempt += 1;
                        }
                        Err(
                            PayoutError::NotSent(error)
                            | PayoutError::Ambiguous(error)
                            | PayoutError::QueryFailed { error, .. },
                        ) => {
                            eprintln!("payout query retry failed; reservation retained");
                            return Err(error);
                        }
                    }
                }
            }
            PayoutAction::Complete => unreachable!(),
        };
        let mismatch = observation.status == "Mismatch";
        let response = self.persist_observation(id, observation).await?;
        if mismatch {
            return Err(ApiError::new(
                ErrorCode::WebhookMismatch,
                "provider facts require reconciliation",
            ));
        }
        Ok(response)
    }
    /// Retry only the durable observation; never repeat the provider operation.
    pub(crate) async fn persist_observation(
        &self,
        id: ClaimId,
        observation: Observation,
    ) -> Result<ClaimResponse, ApiError> {
        for attempt in 0..3 {
            match self
                .claim(Command::Observe {
                    id,
                    observation: observation.clone(),
                })
                .await
            {
                Err(error) if error.code == ErrorCode::StorageUnavailable && attempt < 2 => {
                    tokio::time::sleep(std::time::Duration::from_millis(20 << attempt)).await;
                }
                result => return result,
            }
        }
        unreachable!()
    }
    pub async fn webhook(
        &self,
        body: &[u8],
        headers: &axum::http::HeaderMap,
    ) -> Result<WebhookResponse, ApiError> {
        self.paypal.verify_webhook(body, headers)?;
        let request = serde_json::from_slice(body)
            .map_err(|_| ApiError::new(ErrorCode::InvalidInput, "invalid webhook JSON"))?;
        match self.storage.execute(Command::Webhook(request)).await? {
            Output::Webhook(response) => Ok(response),
            _ => unreachable!(),
        }
    }
}
