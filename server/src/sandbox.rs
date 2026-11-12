//! Sandbox-only REST transport. No configurable production origin or redirects.
use crate::{
    adapters::{PayPalPayouts, PayoutError},
    domain::Observation,
};
use async_trait::async_trait;
use axum::http::HeaderMap;
use contracts::*;
use reqwest::{Client, Method};
use serde_json::{json, Value};
use std::time::Duration;

pub const SANDBOX_ORIGIN: &str = "https://api-m.sandbox.paypal.com";
pub struct SandboxPayPal {
    client: Client,
    origin: String,
    client_id: String,
    secret: String,
}
fn unavailable() -> ApiError {
    ApiError::new(
        ErrorCode::AdapterUnavailable,
        "sandbox result unavailable; reservation retained",
    )
}
fn query_error() -> PayoutError {
    PayoutError::QueryFailed {
        error: unavailable(),
        retryable: true,
    }
}
fn query_mismatch(p: &Payout, batch: &str, item: Option<&str>) -> Observation {
    Observation {
        state: p.state,
        paypal_batch_id: Some(batch.into()),
        item_id: item.map(str::to_owned),
        binding: None,
        source: "sandbox-item-query:immutable-fact-mismatch".into(),
        status: "Mismatch".into(),
    }
}
/// Absent provider data stays retryable; only present, different facts justify a finance hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fact {
    Absent,
    Same,
    Differs,
}
impl Fact {
    fn of(actual: &Value, expected: &Value) -> Self {
        if actual.is_null() {
            Self::Absent
        } else if actual == expected {
            Self::Same
        } else {
            Self::Differs
        }
    }
    /// Optional count: omitted is fine, zero means the provider has listed nothing yet.
    fn count(actual: &Value) -> Self {
        match actual.as_u64() {
            _ if actual.is_null() => Self::Same,
            Some(0) => Self::Absent,
            Some(1) => Self::Same,
            _ => Self::Differs,
        }
    }
    fn resolve<const N: usize>(facts: [Self; N]) -> Self {
        if facts.contains(&Self::Differs) {
            Self::Differs
        } else if facts.contains(&Self::Absent) {
            Self::Absent
        } else {
            Self::Same
        }
    }
}
pub fn request_body(p: &Payout) -> Value {
    let b = p.binding.as_ref().expect("immutable binding");
    let cents = b.amount_cents.get();
    json!({"sender_batch_header": {"sender_batch_id": p.sender_batch_id}, "items": [{
        "recipient_type": b.recipient_type, "receiver": b.receiver,
        "sender_item_id": b.sender_item_id,
        "amount": {"currency": "USD", "value": format!("{}.{:02}", cents / 100, cents % 100)}
    }]})
}
impl SandboxPayPal {
    pub fn new(client_id: String, secret: String) -> Result<Self, ApiError> {
        Self::transport(
            client_id,
            secret,
            SANDBOX_ORIGIN.into(),
            Duration::from_secs(20),
        )
    }
    fn transport(
        client_id: String,
        secret: String,
        origin: String,
        timeout: Duration,
    ) -> Result<Self, ApiError> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .build()
            .map_err(|_| unavailable())?;
        Ok(Self {
            client,
            origin,
            client_id,
            secret,
        })
    }
    async fn token(&self) -> Result<String, PayoutError> {
        let response = self
            .client
            .post(format!("{}/v1/oauth2/token", self.origin))
            .basic_auth(&self.client_id, Some(&self.secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await
            .map_err(|_| query_error())?;
        if !response.status().is_success() {
            return Err(query_error());
        }
        let value: Value = response.json().await.map_err(|_| query_error())?;
        value["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(query_error)
    }
    async fn get(&self, path: &str, token: &str) -> Result<Value, PayoutError> {
        let response = self
            .client
            .request(Method::GET, format!("{}{path}", self.origin))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| query_error())?;
        if !response.status().is_success() {
            return Err(query_error());
        }
        response.json().await.map_err(|_| query_error())
    }
    fn safe_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }
    async fn details(
        &self,
        p: &Payout,
        batch: &str,
        token: &str,
    ) -> Result<Observation, PayoutError> {
        if !Self::safe_id(batch) {
            return Err(query_error());
        }
        let value = self
            .get(&format!("/v1/payments/payouts/{batch}"), token)
            .await?;
        let header = &value["batch_header"];
        let listed = value["items"].as_array();
        let items_fact = match listed {
            None if value["items"].is_null() => Fact::Absent,
            Some(items) if items.is_empty() => Fact::Absent,
            Some(items) if items.len() == 1 => Fact::Same,
            _ => Fact::Differs,
        };
        let id_value = listed
            .filter(|items| items.len() == 1)
            .map(|items| &items[0]["payout_item_id"]);
        let id_fact = match id_value {
            None => Fact::Same,
            Some(Value::Null) => Fact::Absent,
            Some(Value::String(s)) if Self::safe_id(s) => Fact::Same,
            Some(_) => Fact::Differs,
        };
        let batch_fact = Fact::resolve([
            Fact::of(&header["payout_batch_id"], &json!(batch)),
            Fact::of(
                &header["sender_batch_header"]["sender_batch_id"],
                &json!(p.sender_batch_id),
            ),
            Fact::count(&value["total_items"]),
            Fact::count(&value["total_pages"]),
            items_fact,
            id_fact,
        ]);
        match batch_fact {
            Fact::Differs => return Ok(query_mismatch(p, batch, None)),
            Fact::Absent => return Err(query_error()),
            Fact::Same => {}
        }
        let (Some(items), Some(id)) = (listed, id_value.and_then(Value::as_str)) else {
            return Err(query_error());
        };
        let b = p.binding.as_ref().ok_or_else(query_error)?;
        let item = self
            .get(&format!("/v1/payments/payouts-item/{id}"), token)
            .await?;
        let expected = request_body(p);
        let facts = &item["payout_item"];
        let amount = &facts["amount"];
        let expected_amount = &expected["items"][0]["amount"];
        let item_fact = Fact::resolve([
            match p.item_id.as_deref() {
                Some(known) if known != id => Fact::Differs,
                _ => Fact::Same,
            },
            // The list copy is advisory; the item query below is authoritative.
            match &items[0]["payout_item"]["sender_item_id"] {
                Value::Null => Fact::Same,
                v => Fact::of(v, &json!(b.sender_item_id)),
            },
            Fact::of(&item["payout_item_id"], &json!(id)),
            Fact::of(&item["payout_batch_id"], &json!(batch)),
            Fact::of(&facts["sender_item_id"], &json!(b.sender_item_id)),
            Fact::of(&facts["receiver"], &json!(b.receiver)),
            Fact::of(&facts["recipient_type"], &json!(b.recipient_type)),
            if amount.is_object() || amount.is_null() {
                Fact::resolve([
                    Fact::of(&amount["value"], &expected_amount["value"]),
                    Fact::of(&amount["currency"], &expected_amount["currency"]),
                ])
            } else {
                Fact::Differs
            },
        ]);
        match item_fact {
            Fact::Differs => return Ok(query_mismatch(p, batch, Some(id))),
            Fact::Absent => return Err(query_error()),
            Fact::Same => {}
        }
        let Some(raw) = item["transaction_status"].as_str() else {
            return Err(query_error());
        };
        let (state, status) = match raw {
            "SUCCESS" => (PayoutState::Paid, "Succeeded"),
            "FAILED" | "BLOCKED" | "DENIED" | "RETURNED" | "CANCELED" => {
                (PayoutState::PayoutFailed, "Failed")
            }
            "PENDING" | "PROCESSING" | "UNCLAIMED" | "ONHOLD" => {
                (PayoutState::PayoutPending, "Pending")
            }
            "REFUNDED" => (p.state, "Refunded"),
            "REVERSED" => (p.state, "Reversed"),
            _ => (p.state, "Unknown"),
        };
        Ok(Observation {
            state,
            paypal_batch_id: Some(batch.into()),
            item_id: Some(id.into()),
            binding: p.binding.clone(),
            source: format!("sandbox-item-query:{raw}"),
            status: status.into(),
        })
    }
}
#[async_trait]
impl PayPalPayouts for SandboxPayPal {
    fn sandbox(&self) -> bool {
        true
    }
    async fn send(&self, p: &Payout, approval: &Approval) -> Result<Observation, PayoutError> {
        let b = p.binding.as_ref().ok_or_else(query_error)?;
        if p.approval_id != approval.id
            || p.sender_batch_id != approval.sender_batch_id()
            || b.amount_cents != approval.amount_cents
            || b.receiver != approval.receiver
            || b.amount_cents.get() > 1000
        {
            return Err(PayoutError::NotSent(ApiError::new(
                ErrorCode::ApprovalInvalid,
                "invalid sandbox intent",
            )));
        }
        let token = self
            .token()
            .await
            .map_err(|_| PayoutError::NotSent(unavailable()))?;
        let response = self
            .client
            .post(format!("{}/v1/payments/payouts", self.origin))
            .bearer_auth(token)
            .header("PayPal-Request-Id", &p.sender_batch_id)
            .json(&request_body(p))
            .send()
            .await
            .map_err(|_| PayoutError::Ambiguous(unavailable()))?;
        if !response.status().is_success() {
            return Err(PayoutError::Ambiguous(unavailable()));
        }
        let value: Value = response
            .json()
            .await
            .map_err(|_| PayoutError::Ambiguous(unavailable()))?;
        let batch = value["batch_header"]["payout_batch_id"]
            .as_str()
            .filter(|s| Self::safe_id(s))
            .ok_or_else(|| PayoutError::Ambiguous(unavailable()))?;
        // Persist accepted IDs before any further network operation; a later call only queries.
        Ok(Observation {
            state: PayoutState::PayoutPending,
            paypal_batch_id: Some(batch.into()),
            item_id: None,
            binding: p.binding.clone(),
            source: "sandbox-create".into(),
            status: "Pending".into(),
        })
    }
    async fn query(&self, p: &Payout, _: &Approval) -> Result<Observation, PayoutError> {
        let batch = p.paypal_batch_id.as_deref().ok_or_else(query_error)?;
        let token = self.token().await?;
        self.details(p, batch, &token).await
    }
    fn verify_webhook(&self, _: &[u8], _: &HeaderMap) -> Result<(), ApiError> {
        Err(ApiError::new(
            ErrorCode::WebhookUnverified,
            "sandbox webhook ingestion disabled; use authenticated query",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adapters::MockLlm,
        domain::{Command, BUDGET_ID, CLAIMANT_ID, EVENT_ID, PAYEE_ID},
        storage::{SqliteStore, Storage},
        Service,
    };
    use axum::{
        body::Bytes,
        extract::State,
        http::{Request, StatusCode},
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use tower::ServiceExt;
    #[derive(Default)]
    struct Fake {
        sends: AtomicUsize,
        calls: AtomicUsize,
        body: Mutex<Value>,
        status: String,
        failure: bool,
        timeout: bool,
        query_unavailable: std::sync::atomic::AtomicBool,
        database_url: Option<String>,
        batch_override: Mutex<Option<Value>>,
        item_override: Mutex<Option<Value>>,
    }
    fn batch_json(fake: &Fake) -> Value {
        let body = fake.body.lock().unwrap();
        json!({"batch_header":{"payout_batch_id":"BATCH1", "sender_batch_header":body["sender_batch_header"]},
            "items":[{"payout_item_id":"ITEM1", "payout_item":body["items"][0]}]})
    }
    fn item_json(fake: &Fake) -> Value {
        json!({"payout_batch_id":"BATCH1", "payout_item_id":"ITEM1", "transaction_status":fake.status,
            "payout_item":fake.body.lock().unwrap()["items"][0]})
    }
    async fn oauth(State(fake): State<Arc<Fake>>) -> Json<Value> {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({"access_token":"fake-token"}))
    }
    async fn create(State(fake): State<Arc<Fake>>, bytes: Bytes) -> axum::response::Response {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        fake.sends.fetch_add(1, Ordering::SeqCst);
        *fake.body.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
        if let Some(url) = &fake.database_url {
            sqlx::any::install_default_drivers();
            let pool = sqlx::AnyPool::connect(url).await.unwrap();
            let payload: String = sqlx::query_scalar("SELECT payload FROM immutable_requests")
                .fetch_one(&pool)
                .await
                .unwrap();
            let stored: crate::domain::ImmutableRequest = serde_json::from_str(&payload).unwrap();
            assert_eq!(stored.body, *fake.body.lock().unwrap());
        }
        if fake.timeout {
            tokio::time::sleep(Duration::from_millis(600)).await;
        }
        if fake.failure {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        (
            StatusCode::CREATED,
            Json(json!({"batch_header":{"payout_batch_id":"BATCH1"}})),
        )
            .into_response()
    }
    async fn batch(State(fake): State<Arc<Fake>>) -> axum::response::Response {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        if fake.query_unavailable.load(Ordering::SeqCst) {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        let replaced = fake.batch_override.lock().unwrap().clone();
        Json(replaced.unwrap_or_else(|| batch_json(&fake))).into_response()
    }
    async fn item(State(fake): State<Arc<Fake>>) -> Json<Value> {
        fake.calls.fetch_add(1, Ordering::SeqCst);
        let replaced = fake.item_override.lock().unwrap().clone();
        Json(replaced.unwrap_or_else(|| item_json(&fake)))
    }
    async fn setup(
        status: &str,
        failure: bool,
        timeout: bool,
    ) -> (Arc<Service>, Arc<Fake>, tokio::task::JoinHandle<()>) {
        setup_store(status, failure, timeout, "sqlite::memory:").await
    }
    async fn setup_store(
        status: &str,
        failure: bool,
        timeout: bool,
        url: &str,
    ) -> (Arc<Service>, Arc<Fake>, tokio::task::JoinHandle<()>) {
        setup_store_with_timeout(status, failure, timeout, url, Duration::from_millis(250)).await
    }
    async fn setup_store_with_timeout(
        status: &str,
        failure: bool,
        timeout: bool,
        url: &str,
        transport_timeout: Duration,
    ) -> (Arc<Service>, Arc<Fake>, tokio::task::JoinHandle<()>) {
        let fake = Arc::new(Fake {
            status: status.into(),
            database_url: if url.contains(":memory:") {
                None
            } else {
                Some(url.into())
            },
            failure,
            timeout,
            ..Fake::default()
        });
        let app = Router::new()
            .route("/v1/oauth2/token", post(oauth))
            .route("/v1/payments/payouts", post(create))
            .route("/v1/payments/payouts/BATCH1", get(batch))
            .route("/v1/payments/payouts-item/ITEM1", get(item))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let storage: Arc<dyn Storage> = if url.starts_with("sqlite:") {
            Arc::new(SqliteStore::connect(url).await.unwrap())
        } else {
            Arc::new(crate::storage::PostgresStore::connect(url).await.unwrap())
        };
        storage
            .execute(Command::ConfigureRecipient("fixed@example.invalid".into()))
            .await
            .unwrap();
        storage
            .execute(Command::ConfigureSandbox(true))
            .await
            .unwrap();
        let adapter = SandboxPayPal::transport(
            "fake-client".into(),
            "fake-secret".into(),
            origin,
            transport_timeout,
        )
        .unwrap();
        (
            Arc::new(Service {
                storage,
                paypal: Arc::new(adapter),
                extractor: Arc::new(MockLlm),
                finance_token: "test-finance".into(),
            }),
            fake,
            task,
        )
    }
    async fn approved(service: &Service, amount: i64) -> ClaimId {
        approved_at(service, amount, 100).await
    }
    async fn approved_at(service: &Service, amount: i64, now: i64) -> ClaimId {
        let id = ClaimId::new();
        service
            .claim(Command::Create {
                id,
                request: CreateClaimRequest {
                    event_id: EVENT_ID,
                    claimant_id: CLAIMANT_ID,
                    budget_line_id: BUDGET_ID,
                    receipt_text: Uuid::new_v4().to_string(),
                },
            })
            .await
            .unwrap();
        service
            .claim(Command::Submit {
                id,
                request: SubmitClaimRequest {
                    fields: ReceiptFields {
                        merchant: Some(id.to_string()),
                        date: Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()),
                        total_cents: Some(Cents::try_from(amount).unwrap()),
                        currency: Some(Currency::USD),
                        uncertain: vec![],
                    },
                    items: vec![ReceiptItem {
                        quantity: 1,
                        unit_cents: Cents::try_from(amount).unwrap(),
                    }],
                },
            })
            .await
            .unwrap();
        service
            .claim(Command::Approve {
                approval_id: ApprovalId::new(),
                id,
                payee_id: PAYEE_ID,
                now,
            })
            .await
            .unwrap();
        id
    }
    use uuid::Uuid;
    #[tokio::test]
    async fn success_requires_matching_item_and_duplicate_approval_sends_once() {
        let (service, fake, task) = setup("SUCCESS", false, false).await;
        let id = approved(&service, 1000).await;
        service
            .claim(Command::Approve {
                approval_id: ApprovalId::new(),
                id,
                payee_id: PAYEE_ID,
                now: 101,
            })
            .await
            .unwrap();
        assert_eq!(
            service.payout(id, 101).await.unwrap().claim.state,
            ClaimState::PayoutPending
        );
        assert_eq!(
            service.payout(id, 102).await.unwrap().claim.state,
            ClaimState::Paid
        );
        service.payout(id, 103).await.unwrap();
        assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.body.lock().unwrap()["items"][0]["receiver"],
            "fixed@example.invalid"
        );
        task.abort();
    }
    #[tokio::test]
    async fn unclaimed_and_unknown_remain_reserved() {
        for status in ["UNCLAIMED", "PENDING", "NEW_STATUS"] {
            let (service, fake, task) = setup(status, false, false).await;
            let id = approved(&service, 500).await;
            service.payout(id, 101).await.unwrap();
            let result = service.payout(id, 102).await.unwrap();
            assert_eq!(result.claim.state, ClaimState::PayoutPending);
            assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
            match service
                .storage
                .execute(Command::Budgets(EVENT_ID))
                .await
                .unwrap()
            {
                crate::domain::Output::Budgets(b) => {
                    assert_eq!(b.budget_lines[0].reserved_cents.get(), 500)
                }
                _ => panic!(),
            }
            task.abort();
        }
    }
    #[tokio::test]
    async fn server_error_and_timeout_never_resend() {
        for (failure, timeout) in [(true, false), (false, true)] {
            let (service, fake, task) = setup("SUCCESS", failure, timeout).await;
            let id = approved(&service, 500).await;
            assert_eq!(
                service.payout(id, 101).await.unwrap().claim.state,
                ClaimState::PayoutUnknown
            );
            assert!(service.payout(id, 102).await.is_err());
            assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
            assert_eq!(fake.calls.load(Ordering::SeqCst), 2); // Missing IDs: no outbound query, either.
            task.abort();
        }
    }
    #[tokio::test]
    async fn expired_approval_unauthorized_and_cross_claimant_have_zero_outbound_calls() {
        let (service, fake, task) = setup("SUCCESS", false, false).await;
        let id = approved(&service, 1000).await;
        // An expired approval still rejects before any outbound request.
        assert!(service.payout(id, 1001).await.is_err());
        let app = crate::http::router(service.clone());
        for (path, token) in [
            (format!("/api/claims/{id}/payout"), None),
            (
                format!("/api/claims/{id}/extract"),
                Some(crate::http::claimant_token(
                    "test-finance",
                    ClaimantId::new(),
                )),
            ),
        ] {
            let mut request = Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = app
                .clone()
                .oneshot(request.body(axum::body::Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
    #[tokio::test]
    async fn daily_cap_is_site_wide_and_counts_uncertain_attempts() {
        let (service, fake, task) = setup("SUCCESS", true, false).await;
        for _ in 0..20 {
            let id = approved(&service, 1).await;
            service.payout(id, 101).await.unwrap();
        }
        let id = approved(&service, 1).await;
        let before = fake.calls.load(Ordering::SeqCst);
        assert!(service.payout(id, 102).await.is_err());
        assert_eq!(fake.calls.load(Ordering::SeqCst), before);
        assert_eq!(fake.sends.load(Ordering::SeqCst), 20);
        task.abort();
    }
    #[tokio::test]
    async fn mismatched_success_never_marks_paid() {
        for field in ["receiver", "sender_item_id", "recipient_type", "amount"] {
            let (service, fake, task) = setup("SUCCESS", false, false).await;
            let id = approved(&service, 500).await;
            service.payout(id, 101).await.unwrap();
            fake.body.lock().unwrap()["items"][0][field] = json!("mismatch");
            assert!(service.payout(id, 102).await.is_err());
            assert_eq!(
                service.claim(Command::Get(id)).await.unwrap().claim.state,
                ClaimState::PayoutPending
            );
            assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
            task.abort();
        }
    }
    #[tokio::test]
    async fn immutable_request_and_reservation_survive_reopen_and_recipient_change() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("ledger.db").display()
        );
        let (service, fake, task) = setup_store("SUCCESS", false, false, &url).await;
        let id = approved(&service, 500).await;
        service.payout(id, 101).await.unwrap();
        service
            .storage
            .execute(Command::ConfigureRecipient(
                "changed@example.invalid".into(),
            ))
            .await
            .unwrap();
        let paypal = service.paypal.clone();
        drop(service);
        let reopened = Service {
            storage: Arc::new(SqliteStore::connect(&url).await.unwrap()),
            paypal,
            extractor: Arc::new(MockLlm),
            finance_token: "test-finance".into(),
        };
        assert_eq!(
            reopened.payout(id, 10000).await.unwrap().claim.state,
            ClaimState::Paid
        );
        assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        let payload: String = sqlx::query_scalar("SELECT payload FROM immutable_requests")
            .fetch_one(&pool)
            .await
            .unwrap();
        let stored: crate::domain::ImmutableRequest = serde_json::from_str(&payload).unwrap();
        assert_eq!(stored.body, *fake.body.lock().unwrap());
        assert_eq!(stored.environment, "sandbox");
        use sha2::Digest;
        assert_eq!(
            stored.sha256,
            format!(
                "{:x}",
                sha2::Sha256::digest(serde_json::to_vec(&stored.body).unwrap())
            )
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_evidence")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 2);
        task.abort();
    }
    #[tokio::test]
    async fn daily_reset_preserves_uncertain_payouts_and_audit() {
        let (service, fake, task) = setup("SUCCESS", true, false).await;
        service
            .storage
            .execute(Command::ResetDemo { now: 0 })
            .await
            .unwrap();
        let unsent = approved(&service, 500).await;
        let uncertain = approved(&service, 600).await;
        service.payout(uncertain, 101).await.unwrap();
        service
            .storage
            .execute(Command::ResetDemo { now: 20 * 3600 })
            .await
            .unwrap();
        assert!(service.payout(unsent, 20 * 3600).await.is_err());
        assert!(service.payout(uncertain, 20 * 3600).await.is_err());
        let crate::domain::Output::Budgets(budget) = service
            .storage
            .execute(Command::Budgets(EVENT_ID))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(budget.budget_lines[0].reserved_cents.get(), 600);
        let crate::domain::Output::Claims(claims) = service
            .storage
            .execute(Command::Claims(EVENT_ID))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(claims.claims.is_empty());
        assert_eq!(
            service
                .claim(Command::Get(uncertain))
                .await
                .unwrap()
                .claim
                .state,
            ClaimState::PayoutUnknown
        );
        let crate::domain::Output::Receipt(receipt) = service
            .storage
            .execute(Command::Receipt(uncertain))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(receipt.contains("removed"));
        assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
        task.abort();
    }
    async fn judge_login(app: &Router, username: &str, password: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/session")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"username":username,"password":password}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()["token"]
            .as_str()
            .unwrap()
            .into()
    }
    #[tokio::test]
    async fn runtime_judge_boundary_and_expired_approval_deny_before_any_provider_call() {
        let (service, fake, task) = setup("SUCCESS", false, false).await;
        let id = approved_at(&service, 1000, chrono::Utc::now().timestamp() - 1000).await;
        let app = crate::policy::router(
            service.clone(),
            "claimant-password".into(),
            "finance-password".into(),
        );
        let claimant = judge_login(&app, "demo-claimant", "claimant-password").await;
        let finance = judge_login(&app, "demo-finance", "finance-password").await;
        for (path, token, body, expected) in [
            (
                format!("/api/claims/{id}/payout"),
                "",
                json!({}),
                StatusCode::FORBIDDEN,
            ),
            (
                format!("/api/claims/{id}/payout"),
                "test-finance",
                json!({}),
                StatusCode::FORBIDDEN,
            ),
            (
                format!("/api/claims/{id}/payout"),
                claimant.as_str(),
                json!({}),
                StatusCode::FORBIDDEN,
            ),
            (
                format!("/api/claims/{id}/payout"),
                finance.as_str(),
                json!({}),
                StatusCode::CONFLICT,
            ),
            (
                format!("/api/claims/{id}/extract"),
                finance.as_str(),
                json!({}),
                StatusCode::FORBIDDEN,
            ),
            (
                "/api/claims".into(),
                claimant.as_str(),
                json!({"event_id":EVENT_ID,"budget_line_id":BUDGET_ID,"claimant_id":ClaimantId::new(),"receipt_text":"synthetic"}),
                StatusCode::FORBIDDEN,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(body.to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
        task.abort();
    }
    #[tokio::test]
    #[ignore = "manual sandbox smoke; requires PAYPAL_CLIENT_ID, PAYPAL_CLIENT_SECRET, PAYPAL_SANDBOX_RECIPIENT"]
    async fn sandbox_smoke() {
        // Explicitly opt-in only: this creates one USD 0.01 sandbox payout.
        let id = std::env::var("PAYPAL_CLIENT_ID").expect("sandbox client id required");
        let secret = std::env::var("PAYPAL_CLIENT_SECRET").expect("sandbox secret required");
        let recipient =
            std::env::var("PAYPAL_SANDBOX_RECIPIENT").expect("fixed sandbox recipient required");
        let (mut service, _, task) = setup("SUCCESS", false, false).await;
        let svc = Arc::get_mut(&mut service).unwrap();
        svc.paypal = Arc::new(SandboxPayPal::new(id, secret).unwrap());
        svc.storage
            .execute(Command::ConfigureRecipient(recipient))
            .await
            .unwrap();
        let claim = approved(svc, 1).await;
        let result = svc.payout(claim, 101).await.unwrap();
        assert_eq!(result.claim.state, ClaimState::PayoutPending);
        assert!(svc.payout(claim, 102).await.is_ok());
        task.abort();
    }
    include!("reconcile_tests.rs");
    include!("p6b_tests.rs");
    include!("p6c2_domain.rs");
    mod postgres_tests {
        include!("postgres_tests.rs");
    }
}
