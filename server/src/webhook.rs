//! Durable sandbox inbox and asynchronous verification. No raw evidence or secrets are logged.
use crate::{
    domain::{Command, Output},
    http::HttpError,
    storage::Storage,
    Service,
};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use contracts::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delivery {
    pub id: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, Vec<u8>)>,
    pub hash: String,
    pub received_at: i64,
    pub status: String,
    pub claim_id: Option<ClaimId>,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub next_attempt: i64,
    /// Set only by storage, from a local payout lookup at first write. Never from the request.
    /// A correlation is NOT authentication: a known payout UUID can be forged.
    #[serde(default)]
    pub correlated: bool,
}
impl Delivery {
    pub fn new(body: &[u8], headers: &HeaderMap) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            body: body.into(),
            headers: headers
                .iter()
                .filter(|(k, _)| {
                    matches!(
                        k.as_str(),
                        "paypal-auth-algo"
                            | "paypal-cert-url"
                            | "paypal-transmission-id"
                            | "paypal-transmission-sig"
                            | "paypal-transmission-time"
                    )
                })
                .map(|(k, v)| (k.to_string(), v.as_bytes().to_vec()))
                .collect(),
            hash: format!("{:x}", Sha256::digest(body)),
            received_at: chrono::Utc::now().timestamp(),
            status: "pending".into(),
            claim_id: None,
            verified: false,
            attempts: 0,
            next_attempt: 0,
            correlated: false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verification {
    Verified,
    Rejected,
    Unavailable,
}
#[async_trait]
pub trait WebhookVerifier: Send + Sync {
    async fn verify(&self, delivery: &Delivery) -> Verification;
}
pub struct SandboxVerifier {
    client: reqwest::Client,
    client_id: String,
    secret: String,
    webhook_id: String,
    origin: String,
}
impl SandboxVerifier {
    pub fn new(client_id: String, secret: String, webhook_id: String) -> Result<Self, ApiError> {
        if webhook_id.trim().is_empty() {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "PAYPAL_WEBHOOK_ID is required",
            ));
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|_| unavailable())?,
            client_id,
            secret,
            webhook_id,
            origin: crate::sandbox::SANDBOX_ORIGIN.into(),
        })
    }
    async fn remote(&self, d: &Delivery) -> Option<Verification> {
        let header = |name: &str| -> Option<String> {
            let values: Vec<_> = d.headers.iter().filter(|(k, _)| k == name).collect();
            if values.len() != 1 {
                return None;
            }
            String::from_utf8(values[0].1.clone())
                .ok()
                .filter(|s| !s.is_empty())
        };
        let mut request =
            std::collections::BTreeMap::<String, Box<serde_json::value::RawValue>>::new();
        request.insert(
            "webhook_id".into(),
            serde_json::value::to_raw_value(&self.webhook_id).ok()?,
        );
        for (field, name) in [
            ("auth_algo", "paypal-auth-algo"),
            ("cert_url", "paypal-cert-url"),
            ("transmission_id", "paypal-transmission-id"),
            ("transmission_sig", "paypal-transmission-sig"),
            ("transmission_time", "paypal-transmission-time"),
        ] {
            let Some(value) = header(name) else {
                return Some(Verification::Rejected);
            };
            request.insert(field.into(), serde_json::value::to_raw_value(&value).ok()?);
        }
        let event = match serde_json::from_slice::<Box<serde_json::value::RawValue>>(&d.body) {
            Ok(v) => v,
            Err(_) => return Some(Verification::Rejected),
        };
        request.insert("webhook_event".into(), event);
        let token = self
            .client
            .post(format!("{}/v1/oauth2/token", self.origin))
            .basic_auth(&self.client_id, Some(&self.secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .ok()?;
        if token.status() != StatusCode::OK {
            return None;
        }
        let token: serde_json::Value = token.json().await.ok()?;
        let token = token["access_token"].as_str().filter(|s| !s.is_empty())?;
        let response = self
            .client
            .post(format!(
                "{}/v1/notifications/verify-webhook-signature",
                self.origin
            ))
            .bearer_auth(token)
            .json(&request)
            .send()
            .await
            .ok()?;
        if response.status().is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS
        {
            return None;
        }
        if response.status() != StatusCode::OK {
            return Some(Verification::Rejected);
        }
        let result: serde_json::Value = response.json().await.ok()?;
        match result["verification_status"].as_str() {
            Some("SUCCESS") => Some(Verification::Verified),
            Some("FAILURE") => Some(Verification::Rejected),
            _ => None,
        }
    }
}
#[async_trait]
impl WebhookVerifier for SandboxVerifier {
    async fn verify(&self, delivery: &Delivery) -> Verification {
        self.remote(delivery)
            .await
            .unwrap_or(Verification::Unavailable)
    }
}
fn unavailable() -> ApiError {
    ApiError::new(
        ErrorCode::AdapterUnavailable,
        "sandbox verification unavailable; inbox retained",
    )
}
#[derive(Clone)]
pub struct Inbox {
    pub storage: Arc<dyn Storage>,
    pub verifier: Arc<dyn WebhookVerifier>,
    pub paypal: Arc<dyn crate::adapters::PayPalPayouts>,
}
const REQUIRED_HEADERS: [&str; 5] = [
    "paypal-auth-algo",
    "paypal-cert-url",
    "paypal-transmission-id",
    "paypal-transmission-sig",
    "paypal-transmission-time",
];
fn validate_intake(body: &[u8], headers: &HeaderMap) -> Result<(), ApiError> {
    if body.len() > 131_072 {
        return Err(ApiError::new(
            ErrorCode::InvalidInput,
            "webhook body too large",
        ));
    }
    for name in REQUIRED_HEADERS {
        let mut values = headers.get_all(name).iter();
        if values
            .next()
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.trim().is_empty())
            || values.next().is_some()
        {
            return Err(ApiError::new(
                ErrorCode::WebhookUnverified,
                "required PayPal header invalid",
            ));
        }
    }
    // RawValue accepts the original numeric representation without rounding or reserialization.
    let raw = serde_json::from_slice::<Box<serde_json::value::RawValue>>(body)
        .map_err(|_| ApiError::new(ErrorCode::InvalidInput, "invalid webhook JSON"))?;
    if !raw.get().trim_start().starts_with('{') {
        return Err(ApiError::new(
            ErrorCode::InvalidInput,
            "webhook JSON object required",
        ));
    }
    Ok(())
}
impl Inbox {
    pub async fn ingest(&self, body: &[u8], headers: &HeaderMap) -> Result<String, ApiError> {
        validate_intake(body, headers)?;
        let d = Delivery::new(body, headers);
        self.storage.execute(Command::Inbox(d.clone())).await?;
        self.process(&d).await
    }
    pub async fn process(&self, d: &Delivery) -> Result<String, ApiError> {
        let result = self.process_attempt(d).await;
        if result.is_err() {
            self.storage
                .execute(Command::InboxFailure(d.id.clone()))
                .await?;
        }
        result
    }
    async fn process_attempt(&self, d: &Delivery) -> Result<String, ApiError> {
        let Output::Inbox(mut rows) = self
            .storage
            .execute(Command::InboxAttempt(d.id.clone()))
            .await?
        else {
            unreachable!()
        };
        let Some(current) = rows.pop() else {
            return Err(unavailable());
        };
        let d = &current;
        let verified = match if d.verified {
            Verification::Verified
        } else {
            self.verifier.verify(d).await
        } {
            Verification::Verified => true,
            Verification::Rejected => false,
            Verification::Unavailable => return Err(unavailable()),
        };
        if verified {
            self.storage
                .execute(Command::InboxVerified(d.id.clone()))
                .await?;
        }
        if !verified {
            // Rejected input may update only its bounded inbox row, never the ledger.
            let mut rejected = d.clone();
            rejected.status = "rejected".into();
            rejected.body.clear();
            rejected.headers.clear();
            self.storage.execute(Command::Inbox(rejected)).await?;
            return Err(ApiError::new(
                ErrorCode::WebhookUnverified,
                "signature rejected",
            ));
        }
        if !crate::reconcile::supported_event(&d.body) {
            let mut ignored = d.clone();
            ignored.verified = true;
            ignored.status = "ignored".into();
            self.storage.execute(Command::Inbox(ignored)).await?;
            return Ok("ignored".into());
        }
        let mut confirmation = None;
        if verified {
            if let Ok(event) = serde_json::from_slice::<serde_json::Value>(&d.body) {
                if event["event_type"]
                    .as_str()
                    .is_some_and(|t| t.starts_with("PAYMENT.PAYOUTS-ITEM."))
                {
                    if let Some(sender) =
                        event["resource"]["payout_item"]["sender_item_id"].as_str()
                    {
                        if let Output::Candidate(Some(candidate)) = self
                            .storage
                            .execute(Command::WebhookCandidate(sender.into()))
                            .await?
                        {
                            let mut payout = candidate.payout.as_ref().unwrap().clone();
                            let recover = payout.paypal_batch_id.is_none();
                            let failure = matches!(
                                event["resource"]["transaction_status"].as_str(),
                                Some("FAILED" | "BLOCKED" | "DENIED" | "RETURNED" | "CANCELED")
                            );
                            if recover {
                                payout.paypal_batch_id = event["resource"]["payout_batch_id"]
                                    .as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned);
                            }
                            if payout.paypal_batch_id.is_some()
                                && (recover
                                    || (failure
                                        && !matches!(
                                            payout.state,
                                            PayoutState::Paid | PayoutState::PayoutFailed
                                        )))
                            {
                                confirmation = Some(
                                    match self
                                        .paypal
                                        .query(&payout, candidate.approval.as_ref().unwrap())
                                        .await
                                    {
                                        Ok(observation) => observation,
                                        Err(_) => return Err(unavailable()),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }
        let Output::Reconciled(status) = self
            .storage
            .execute(Command::Reconcile {
                delivery_id: d.id.clone(),
                verified,
                confirmation,
            })
            .await?
        else {
            unreachable!()
        };
        Ok(status)
    }
    /// Lane for the global window: true when a well-formed delivery names one of our payouts.
    /// Best effort and read-only; any failure means the stricter uncorrelated lane.
    async fn names_known_payout(&self, body: &[u8], headers: &HeaderMap) -> bool {
        if validate_intake(body, headers).is_err() {
            return false;
        }
        match crate::storage::correlated_payout(body) {
            Some(payout) => self.storage.payout_known(payout).await.unwrap_or(false),
            None => false,
        }
    }
    pub async fn retry(&self) -> Result<(), ApiError> {
        self.storage.execute(Command::InboxPrune).await?;
        // Level-triggered: repeats (throttled in storage) while verified exhausted rows exist.
        if self
            .storage
            .report_verified_exhausted(chrono::Utc::now().timestamp())
            .await
            .is_err()
        {
            self.storage.verified_exhausted_gauge_failed();
        }
        let Output::Inbox(rows) = self.storage.execute(Command::InboxPending).await? else {
            unreachable!()
        };
        let mut failed = false;
        for row in rows {
            if self.process(&row).await.is_err() {
                failed = true;
            }
        }
        if failed {
            eprintln!("sandbox inbox retry failed; reconciliation required");
            Err(unavailable())
        } else {
            Ok(())
        }
    }
}
pub fn router(inbox: Inbox) -> Router {
    router_with_proxy(inbox, false)
}
pub fn router_with_proxy(inbox: Inbox, trust_proxy: bool) -> Router {
    router_with_limit(
        inbox,
        Arc::new(crate::policy::IntakeLimit::new_laned(trust_proxy)),
    )
}
fn router_with_limit(inbox: Inbox, limit: Arc<crate::policy::IntakeLimit>) -> Router {
    Router::new()
        .route("/webhooks/paypal/sandbox", post(ingest))
        .layer(axum::Extension(limit.clone()))
        .layer(DefaultBodyLimit::max(131_072))
        .layer(axum::middleware::from_fn_with_state(
            limit,
            crate::policy::limit_webhook,
        ))
        .with_state(inbox)
}
async fn ingest(
    State(inbox): State<Inbox>,
    axum::Extension(limit): axum::Extension<Arc<crate::policy::IntakeLimit>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Parsing and the lookup are bounded per peer. Charge shared capacity after classification.
    let correlated = limit.is_laned() && inbox.names_known_payout(&body, &headers).await;
    if !limit.admit_outer(correlated)
        || (limit.is_laned() && !correlated && !limit.admit_uncorrelated())
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "webhook request limit reached",
        )
            .into_response();
    }
    ingest_checked(&inbox, &body, &headers)
        .await
        .into_response()
}
async fn ingest_checked(
    inbox: &Inbox,
    body: &[u8],
    headers: &HeaderMap,
) -> Result<StatusCode, HttpError> {
    let status = inbox.ingest(body, headers).await?;
    match status.as_str() {
        "conflict" => Err(ApiError::new(ErrorCode::EventConflict, "event ID conflict").into()),
        "mismatch" | "unmatched" | "rejected" => {
            Err(ApiError::new(ErrorCode::WebhookMismatch, "event needs reconciliation").into())
        }
        _ => Ok(StatusCode::NO_CONTENT),
    }
}
impl Service {
    /// Read-only provider operation: never calls prepare/send, even for a claim without an intent.
    pub async fn query_reserved(
        &self,
        id: ClaimId,
        now: i64,
        age_minutes: i64,
    ) -> Result<ClaimResponse, ApiError> {
        let response = self.claim(Command::Get(id)).await?;
        if let Output::QueryDue(true) = self
            .storage
            .execute(Command::QueryDue {
                id,
                now,
                age: age_minutes.max(1).saturating_mul(60),
            })
            .await?
        {
            let payout = response
                .payout
                .as_ref()
                .ok_or_else(crate::domain::not_found)?;
            let approval = response
                .approval
                .as_ref()
                .ok_or_else(crate::domain::not_found)?;
            let observation = self
                .paypal
                .query(payout, approval)
                .await
                .map_err(|_| unavailable())?;
            return self.persist_observation(id, observation).await;
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json};
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    struct Fake {
        code: StatusCode,
        result: Value,
        calls: Arc<AtomicUsize>,
    }
    async fn oauth() -> Json<Value> {
        Json(json!({"access_token":"fake"}))
    }
    async fn verify(State(fake): State<Fake>, raw: Bytes) -> (StatusCode, Json<Value>) {
        let body: Value = serde_json::from_slice(&raw).unwrap();
        assert!(std::str::from_utf8(&raw)
            .unwrap()
            .contains(r#"{ "id": "EV1", "n": 1.2300, "large": 18446744073709551616 }"#));
        fake.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(body["webhook_id"], "fake-webhook-id");
        assert_eq!(body["webhook_event"]["id"], "EV1");
        for key in [
            "auth_algo",
            "cert_url",
            "transmission_id",
            "transmission_sig",
            "transmission_time",
        ] {
            assert!(body[key].is_string());
        }
        (fake.code, Json(fake.result))
    }
    #[tokio::test]
    async fn fake_http_verifier_requires_exact_success_and_all_original_headers() {
        for (code, result, expected) in [
            (
                StatusCode::OK,
                json!({"verification_status":"SUCCESS"}),
                Verification::Verified,
            ),
            (
                StatusCode::OK,
                json!({"verification_status":"FAILURE"}),
                Verification::Rejected,
            ),
            (StatusCode::OK, json!({}), Verification::Unavailable),
            (
                StatusCode::CREATED,
                json!({"verification_status":"SUCCESS"}),
                Verification::Rejected,
            ),
            (StatusCode::BAD_REQUEST, json!({}), Verification::Rejected),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({}),
                Verification::Rejected,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({}),
                Verification::Unavailable,
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                json!({}),
                Verification::Unavailable,
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let app = Router::new()
                .route("/v1/oauth2/token", post(oauth))
                .route("/v1/notifications/verify-webhook-signature", post(verify))
                .with_state(Fake {
                    code,
                    result,
                    calls: calls.clone(),
                });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut verifier = SandboxVerifier::new(
                "fake-client".into(),
                "fake-secret".into(),
                "fake-webhook-id".into(),
            )
            .unwrap();
            verifier.origin = origin;
            let mut headers = HeaderMap::new();
            for key in [
                "paypal-auth-algo",
                "paypal-cert-url",
                "paypal-transmission-id",
                "paypal-transmission-sig",
                "paypal-transmission-time",
            ] {
                headers.insert(key, "fixture".parse().unwrap());
            }
            let raw = br#"{ "id": "EV1", "n": 1.2300, "large": 18446744073709551616 }"#;
            assert_eq!(
                verifier.verify(&Delivery::new(raw, &headers)).await,
                expected
            );
            headers.remove("paypal-transmission-sig");
            assert_eq!(
                verifier.verify(&Delivery::new(raw, &headers)).await,
                Verification::Rejected
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            headers.insert("paypal-transmission-sig", "fixture".parse().unwrap());
            headers.append("paypal-transmission-id", "duplicate".parse().unwrap());
            assert_eq!(
                verifier.verify(&Delivery::new(raw, &headers)).await,
                Verification::Rejected
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            task.abort();
        }
    }
}

#[cfg(test)]
mod intake_tests {
    use super::*;
    use crate::{
        adapters::{MockPayPal, MockPayoutFixture},
        storage::SqliteStore,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        for name in REQUIRED_HEADERS {
            headers.insert(name, "fixture".parse().unwrap());
        }
        headers
    }
    struct Verified(Arc<AtomicUsize>);
    #[async_trait]
    impl WebhookVerifier for Verified {
        async fn verify(&self, _: &Delivery) -> Verification {
            self.0.fetch_add(1, Ordering::SeqCst);
            Verification::Verified
        }
    }
    #[tokio::test]
    async fn verified_batch_is_ignored_and_unmatched_retry_reuses_verification() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inbox = Inbox {
            storage: Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
            verifier: Arc::new(Verified(calls.clone())),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
        };
        assert_eq!(
            inbox
                .ingest(
                    br#"{"id":"batch","event_type":"PAYMENT.PAYOUTSBATCH.SUCCESS"}"#,
                    &headers()
                )
                .await
                .unwrap(),
            "ignored"
        );
        let d = Delivery::new(
            br#"{"id":"unmatched","event_type":"PAYMENT.PAYOUTS-ITEM.SUCCEEDED"}"#,
            &HeaderMap::new(),
        );
        inbox
            .storage
            .execute(Command::Inbox(d.clone()))
            .await
            .unwrap();
        assert_eq!(inbox.process(&d).await.unwrap(), "unmatched");
        // Immediate competing/retry attempts cannot call the verifier again.
        assert!(inbox.process(&d).await.is_err());
        inbox.storage.make_inbox_due().await.unwrap();
        assert_eq!(inbox.process(&d).await.unwrap(), "unmatched");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let Output::Inbox(rows) = inbox.storage.execute(Command::InboxPending).await.unwrap()
        else {
            panic!()
        };
        assert!(rows.is_empty());
    }
    #[tokio::test]
    async fn throttle_rejects_before_persist_and_trusts_only_configured_loopback() {
        use axum::{body::Body, extract::ConnectInfo, http::Request};
        use tower::ServiceExt;
        for (trusted, peer, expected_calls) in [
            (false, "127.0.0.1:1234", 20),
            (true, "127.0.0.1:1234", 21),
            (true, "192.0.2.1:1234", 20),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let inbox = Inbox {
                storage: Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
                verifier: Arc::new(Verified(calls.clone())),
                paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
            };
            // Keep all 21 requests in one minute even under a slow full gate.
            let app = router_with_limit(
                inbox,
                Arc::new(crate::policy::IntakeLimit::at_time(trusted, 120)),
            );
            for n in 0..21 {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri("/webhooks/paypal/sandbox")
                            .header("x-real-ip", format!("192.0.2.{n}"))
                            .header("paypal-auth-algo", "fixture")
                            .header("paypal-cert-url", "fixture")
                            .header("paypal-transmission-id", "fixture")
                            .header("paypal-transmission-sig", "fixture")
                            .header("paypal-transmission-time", "fixture")
                            .extension(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()))
                            .body(Body::from(format!(
                                r#"{{"id":"{n}","event_type":"PAYMENT.PAYOUTSBATCH.SUCCESS"}}"#
                            )))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    if n == 20 && expected_calls == 20 {
                        StatusCode::TOO_MANY_REQUESTS
                    } else {
                        StatusCode::NO_CONTENT
                    }
                );
            }
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        }
    }
}
