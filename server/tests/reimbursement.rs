use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use contracts::*;
use http_body_util::BodyExt;
use receipt_roots_server::{
    adapters::{mock_signature, MockLlm, MockPayPal, MockPayoutFixture, MOCK_WEBHOOK_ID},
    config::Config,
    domain::{
        self, Command, Ledger, Output, PayoutAction, BUDGET_ID, CLAIMANT_ID, EVENT_ID, PAYEE_ID,
    },
    http::router,
    storage::{SqliteStore, Storage},
    Service,
};
use std::{
    collections::BTreeMap,
    sync::{atomic::Ordering, Arc},
};
use tower::ServiceExt;

fn cents(value: i64) -> Cents {
    Cents::try_from(value).unwrap()
}
fn request(amount: i64, merchant: &str) -> SubmitClaimRequest {
    SubmitClaimRequest {
        fields: ReceiptFields {
            merchant: Some(merchant.into()),
            date: Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
            total_cents: Some(cents(amount)),
            currency: Some(Currency::USD),
            uncertain: vec![],
        },
        items: vec![ReceiptItem {
            quantity: 1,
            unit_cents: cents(amount),
        }],
    }
}
fn create(text: &str) -> Command {
    Command::Create {
        id: ClaimId::new(),
        request: CreateClaimRequest {
            event_id: EVENT_ID,
            budget_line_id: BUDGET_ID,
            claimant_id: CLAIMANT_ID,
            receipt_text: text.into(),
        },
    }
}
async fn setup(fixture: MockPayoutFixture) -> (Service, Arc<MockPayPal>) {
    service(
        Arc::new(SqliteStore::connect("sqlite::memory:").await.unwrap()),
        fixture,
    )
}
fn service(store: Arc<dyn Storage>, fixture: MockPayoutFixture) -> (Service, Arc<MockPayPal>) {
    let paypal = Arc::new(MockPayPal::new(fixture));
    (
        Service {
            storage: store,
            paypal: paypal.clone(),
            extractor: Arc::new(MockLlm),
            finance_token: "synthetic-test-finance".into(),
        },
        paypal,
    )
}
async fn submitted(service: &Service, text: &str, amount: i64, merchant: &str) -> ClaimId {
    let id = service.claim(create(text)).await.unwrap().claim.id;
    service
        .claim(Command::Submit {
            id,
            request: request(amount, merchant),
        })
        .await
        .unwrap();
    id
}
async fn approved(service: &Service) -> ClaimId {
    let id = submitted(service, "mock:clean", 750, "Synthetic shop").await;
    service
        .claim(Command::Approve {
            approval_id: ApprovalId::new(),
            id,
            payee_id: PAYEE_ID,
            now: 100,
        })
        .await
        .unwrap();
    id
}
fn envelope(response: &ClaimResponse, event: &str, kind: PayoutEventKind) -> PayPalWebhookRequest {
    let payout = response.payout.as_ref().unwrap();
    PayPalWebhookRequest {
        paypal_event_id: event.into(),
        payout_id: payout.id,
        sender_batch_id: payout.sender_batch_id.clone(),
        paypal_batch_id: format!("mock-batch-{}", payout.id),
        item_id: format!("mock-item-{}", payout.id),
        amount_cents: response.claim.amount_cents,
        currency: Currency::USD,
        payee_id: response.approval.as_ref().unwrap().payee_id,
        kind,
    }
}
fn signed(body: &[u8]) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-mock-signature", mock_signature(body).parse().unwrap());
    headers.insert("x-mock-webhook-id", MOCK_WEBHOOK_ID.parse().unwrap());
    headers
}
async fn webhook(
    service: &Service,
    event: &PayPalWebhookRequest,
) -> Result<WebhookResponse, ApiError> {
    let body = serde_json::to_vec(event).unwrap();
    service.webhook(&body, &signed(&body)).await
}
async fn budget(service: &Service) -> BudgetLine {
    match service
        .storage
        .execute(Command::Budgets(EVENT_ID))
        .await
        .unwrap()
    {
        Output::Budgets(response) => response.budget_lines[0].clone(),
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn happy_path_replay_and_late_event_never_double_pay() {
    let (service, paypal) = setup(MockPayoutFixture::Success).await;
    let draft = service.claim(create("mock:clean")).await.unwrap();
    let extracted = service.extract(draft.claim.id).await.unwrap();
    assert_eq!(extracted.claim.fields.total_cents, Some(cents(750)));
    assert_eq!(extracted.claim.state, ClaimState::Draft);
    let id = extracted.claim.id;
    service
        .claim(Command::Submit {
            id,
            request: request(750, "Synthetic shop"),
        })
        .await
        .unwrap();
    service
        .claim(Command::Approve {
            approval_id: ApprovalId::new(),
            id,
            payee_id: PAYEE_ID,
            now: 100,
        })
        .await
        .unwrap();
    let pending = service.payout(id, 101).await.unwrap();
    assert_eq!(pending.claim.state, ClaimState::PayoutPending);
    let event = envelope(&pending, "synthetic-event-1", PayoutEventKind::Succeeded);
    assert!(!webhook(&service, &event).await.unwrap().duplicate);
    assert!(webhook(&service, &event).await.unwrap().duplicate);
    let mut late = event.clone();
    late.paypal_event_id = "synthetic-late".into();
    late.kind = PayoutEventKind::Failed;
    let conflict = webhook(&service, &late).await.unwrap();
    assert_eq!(conflict.payout.state, PayoutState::Paid);
    assert_eq!(conflict.payout.reconciliation.len(), 1);
    assert_eq!(conflict.payout.reconciliation[0].status, "Failed");
    assert_eq!(
        service.payout(id, 10000).await.unwrap().claim.state,
        ClaimState::Paid
    );
    let budget = budget(&service).await;
    assert_eq!(budget.paid_cents.get(), 750);
    assert_eq!(budget.reserved_cents.get(), 0);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn timeout_retry_queries_fixed_batch_without_resend() {
    let (service, paypal) = setup(MockPayoutFixture::Timeout).await;
    let id = approved(&service).await;
    let unknown = service.payout(id, 101).await.unwrap();
    assert_eq!(unknown.claim.state, ClaimState::PayoutUnknown);
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
    let paid = service.payout(id, 102).await.unwrap();
    assert_eq!(paid.claim.state, ClaimState::Paid);
    assert_eq!(
        unknown.payout.unwrap().sender_batch_id,
        paid.payout.unwrap().sender_batch_id
    );
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn fake_signature_and_changed_body_do_not_poison_dedupe() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let event = envelope(&pending, "synthetic-event", PayoutEventKind::Succeeded);
    let body = serde_json::to_vec(&event).unwrap();
    assert_eq!(
        service
            .webhook(&body, &HeaderMap::new())
            .await
            .unwrap_err()
            .code,
        ErrorCode::WebhookUnverified
    );
    let mut headers = signed(&body);
    headers.insert(
        "x-mock-signature",
        "0000000000000000000000000000000000000000000000000000000000000000"
            .parse()
            .unwrap(),
    );
    assert_eq!(
        service.webhook(&body, &headers).await.unwrap_err().code,
        ErrorCode::WebhookUnverified
    );
    let mut tampered = body.clone();
    tampered.push(b' ');
    assert_eq!(
        service
            .webhook(&tampered, &signed(&body))
            .await
            .unwrap_err()
            .code,
        ErrorCode::WebhookUnverified
    );
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    assert!(!webhook(&service, &event).await.unwrap().duplicate);
}
#[tokio::test]
async fn signed_mismatched_amount_payee_and_identity_are_rejected() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let event = envelope(&pending, "synthetic-event", PayoutEventKind::Succeeded);
    for field in ["amount", "payee", "sender", "batch", "item"] {
        let mut bad = event.clone();
        match field {
            "amount" => bad.amount_cents = cents(1),
            "payee" => bad.payee_id = PayeeId::new(),
            "sender" => bad.sender_batch_id = "wrong".into(),
            "batch" => bad.paypal_batch_id = "wrong".into(),
            _ => bad.item_id = "wrong".into(),
        }
        assert_eq!(
            webhook(&service, &bad).await.unwrap_err().code,
            ErrorCode::WebhookMismatch
        );
    }
    assert!(!webhook(&service, &event).await.unwrap().duplicate);
}
#[tokio::test]
async fn event_id_with_changed_payload_conflicts() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let mut event = envelope(&pending, "synthetic-replay", PayoutEventKind::Succeeded);
    webhook(&service, &event).await.unwrap();
    event.kind = PayoutEventKind::Failed;
    assert_eq!(
        webhook(&service, &event).await.unwrap_err().code,
        ErrorCode::EventConflict
    );
}
#[tokio::test]
async fn parallel_submits_across_independent_pools_reserve_only_one() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("race.db").display());
    let (left, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let (right, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    for n in 0..9 {
        submitted(
            &left,
            &format!("capacity-{n}"),
            1000,
            &format!("Capacity {n}"),
        )
        .await;
    }
    let a = left.claim(create("synthetic-a")).await.unwrap().claim.id;
    let b = right.claim(create("synthetic-b")).await.unwrap().claim.id;
    let (a_result, b_result) = tokio::join!(
        left.claim(Command::Submit {
            id: a,
            request: request(600, "Synthetic a")
        }),
        right.claim(Command::Submit {
            id: b,
            request: request(600, "Synthetic b")
        })
    );
    assert_eq!(
        usize::from(a_result.is_ok()) + usize::from(b_result.is_ok()),
        1
    );
    assert_eq!(
        a_result.err().or_else(|| b_result.err()).unwrap().code,
        ErrorCode::OverBudget
    );
    assert_eq!(budget(&left).await.reserved_cents.get(), 9600);
    let states = [
        left.claim(Command::Get(a)).await.unwrap().claim.state,
        right.claim(Command::Get(b)).await.unwrap().claim.state,
    ];
    assert!(states.contains(&ClaimState::OverBudget));
}
#[tokio::test]
async fn concurrent_duplicate_submits_accept_only_one() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let a = service
        .claim(create("same receipt"))
        .await
        .unwrap()
        .claim
        .id;
    let b = service
        .claim(create("same receipt"))
        .await
        .unwrap()
        .claim
        .id;
    let (a, b) = tokio::join!(
        service.claim(Command::Submit {
            id: a,
            request: request(100, "One")
        }),
        service.claim(Command::Submit {
            id: b,
            request: request(200, "Two")
        })
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    assert_eq!(
        a.err().or_else(|| b.err()).unwrap().code,
        ErrorCode::DuplicateReceipt
    );
}
#[tokio::test]
async fn hash_and_normalized_tuple_duplicates_are_blocked() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    submitted(&service, "receipt1", 500, "  SYNTHETIC Shop ").await;
    let same_hash = service.claim(create("receipt1")).await.unwrap().claim.id;
    assert_eq!(
        service
            .claim(Command::Submit {
                id: same_hash,
                request: request(600, "Other")
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::DuplicateReceipt
    );
    let same_tuple = service.claim(create("receipt2")).await.unwrap().claim.id;
    assert_eq!(
        service
            .claim(Command::Submit {
                id: same_tuple,
                request: request(500, "synthetic shop")
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::DuplicateReceipt
    );
    assert_eq!(budget(&service).await.reserved_cents.get(), 500);
}
#[tokio::test]
async fn submit_retry_is_idempotent_and_submitted_facts_are_immutable() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = submitted(&service, "receipt", 250, "Synthetic").await;
    let before = service.claim(Command::Get(id)).await.unwrap();
    let retry = service
        .claim(Command::Submit {
            id,
            request: request(250, "Synthetic"),
        })
        .await
        .unwrap();
    assert_eq!(retry.claim.revision, before.claim.revision);
    assert_eq!(budget(&service).await.reserved_cents.get(), 250);
    assert_eq!(
        service.extract(id).await.unwrap_err().code,
        ErrorCode::InvalidTransition
    );
    assert_eq!(
        service
            .claim(Command::Submit {
                id,
                request: request(251, "Synthetic")
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidTransition
    );
}
#[tokio::test]
async fn expired_approval_and_wrong_payee_cannot_pay() {
    let (service, paypal) = setup(MockPayoutFixture::Success).await;
    let id = submitted(&service, "receipt", 100, "Synthetic").await;
    assert_eq!(
        service
            .claim(Command::Approve {
                approval_id: ApprovalId::new(),
                id,
                payee_id: PayeeId::new(),
                now: 100
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::PayeeNotAllowed
    );
    service
        .claim(Command::Approve {
            approval_id: ApprovalId::new(),
            id,
            payee_id: PAYEE_ID,
            now: 100,
        })
        .await
        .unwrap();
    assert_eq!(
        service.payout(id, 1000).await.unwrap_err().code,
        ErrorCode::ApprovalInvalid
    );
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn uncertain_and_malformed_model_output_need_human_facts() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    for fixture in [
        "mock:uncertain",
        "mock:malformed",
        "{\"total_cents\":-1}",
        "{\"payee_id\":\"forged\"}",
    ] {
        let id = service.claim(create(fixture)).await.unwrap().claim.id;
        let extracted = service.extract(id).await.unwrap();
        assert_eq!(extracted.claim.state, ClaimState::NeedsInfo);
        assert!(extracted.approval.is_none());
        assert_eq!(extracted.claim.amount_cents, Cents::ZERO);
        service
            .claim(Command::Submit {
                id,
                request: request(10, fixture),
            })
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn failed_payout_releases_once_and_never_resends() {
    let (service, paypal) = setup(MockPayoutFixture::Failed).await;
    let id = approved(&service).await;
    let failed = service.payout(id, 101).await.unwrap();
    assert_eq!(failed.claim.state, ClaimState::PayoutFailed);
    assert_eq!(budget(&service).await.reserved_cents, Cents::ZERO);
    assert_eq!(budget(&service).await.paid_cents, Cents::ZERO);
    service.payout(id, 102).await.unwrap();
    let event = envelope(&failed, "synthetic-failure", PayoutEventKind::Failed);
    webhook(&service, &event).await.unwrap();
    webhook(&service, &event).await.unwrap();
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget(&service).await.reserved_cents, Cents::ZERO);
}
#[tokio::test]
async fn pending_fixture_retains_reservation() {
    let (service, paypal) = setup(MockPayoutFixture::Pending).await;
    let id = approved(&service).await;
    service.payout(id, 101).await.unwrap();
    assert_eq!(
        service.payout(id, 102).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn restart_after_persist_before_send_queries_without_sending() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("restart.db").display()
    );
    let (original, original_paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let id = approved(&original).await;
    let prepared = original
        .storage
        .execute(Command::PreparePayout {
            payout_id: PayoutId::new(),
            id,
            now: 101,
        })
        .await
        .unwrap();
    let batch = match prepared {
        Output::Prepared {
            response,
            action: PayoutAction::SendNew,
        } => response.payout.unwrap().sender_batch_id,
        _ => panic!("send grant required"),
    };
    assert_eq!(original_paypal.send_calls.load(Ordering::SeqCst), 0);
    drop(original);
    drop(original_paypal);
    let (restarted, paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    assert_eq!(
        restarted.payout(id, 20000).await.unwrap_err().code,
        ErrorCode::AdapterUnavailable
    );
    let unknown = restarted.claim(Command::Get(id)).await.unwrap();
    assert_eq!(unknown.claim.state, ClaimState::PayoutPending);
    assert_eq!(unknown.payout.unwrap().sender_batch_id, batch);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget(&restarted).await.paid_cents.get(), 0);
    assert_eq!(budget(&restarted).await.reserved_cents.get(), 750);
}
#[tokio::test]
async fn restart_after_timeout_recovers_persisted_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("unknown.db").display()
    );
    let (first, first_paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Timeout,
    );
    let id = approved(&first).await;
    assert_eq!(
        first.payout(id, 101).await.unwrap().claim.state,
        ClaimState::PayoutUnknown
    );
    let provider = first_paypal.provider();
    drop(first);
    drop(first_paypal);
    let (mut second, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Timeout,
    );
    let paypal = Arc::new(MockPayPal::with_provider(
        MockPayoutFixture::Timeout,
        provider,
    ));
    second.paypal = paypal.clone();
    assert_eq!(
        second.payout(id, 50000).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn concurrent_payout_requests_grant_only_one_send() {
    let (service, paypal) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let (left, right) = tokio::join!(service.payout(id, 101), service.payout(id, 101));
    left.unwrap();
    right.unwrap();
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    service.payout(id, 102).await.unwrap();
    assert_eq!(budget(&service).await.paid_cents.get(), 750);
}
#[tokio::test]
async fn http_auth_and_input_errors_use_frozen_codes() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let app = router(Arc::new(service));
    for (uri, body, expected, code) in [
        (
            format!("/api/claims/{id}/approve"),
            "{}",
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
        ),
        (
            "/api/claims".into(),
            "{",
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
        ),
        (
            "/api/claims".into(),
            "{\"extra\":true}",
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(
                        "authorization",
                        if expected == StatusCode::FORBIDDEN {
                            "Bearer wrong"
                        } else {
                            "Bearer synthetic-test-finance"
                        },
                    )
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let json: serde_json::Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(json["code"], code);
    }
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/claims/not-a-uuid")
                .header("authorization", "Bearer synthetic-test-finance")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
#[test]
fn mode_switches_reject_every_unimplemented_mode_before_database() {
    assert_eq!(
        Config::from_values(BTreeMap::new()).unwrap_err().code,
        ErrorCode::ModeNotImplemented
    );
    // Once sandbox is selected, absent account configuration still fails as invalid input.
    assert_eq!(
        Config::from_values(BTreeMap::from([("PAYPAL_MODE".into(), "sandbox".into())]))
            .unwrap_err()
            .code,
        ErrorCode::InvalidInput
    );
    let valid = BTreeMap::from([
        ("PAYPAL_MODE".into(), "sandbox".into()),
        ("PAYPAL_CLIENT_ID".into(), "synthetic".into()),
        ("PAYPAL_CLIENT_SECRET".into(), "synthetic".into()),
        (
            "PAYPAL_SANDBOX_RECIPIENT".into(),
            "synthetic@example.invalid".into(),
        ),
        ("PAYPAL_WEBHOOK_ID".into(), "synthetic".into()),
        (
            "JUDGE_CLAIMANT_PASSWORD".into(),
            "synthetic-claimant-password-24".into(),
        ),
        (
            "JUDGE_FINANCE_PASSWORD".into(),
            "synthetic-finance-password-24".into(),
        ),
    ]);
    assert!(Config::from_values(valid.clone()).is_ok());
    for (name, value) in [
        ("PAYPAL_MODE", "production"),
        ("PAYPAL_MODE", "live"),
        ("PAYPAL_MODE", "mock"),
        ("PAYPAL_MODE", ""),
        ("LLM_MODE", "live"),
        ("LLM_MODE", "bogus"),
    ] {
        let mut values = valid.clone();
        values.insert(name.into(), value.into());
        assert_eq!(
            Config::from_values(values).unwrap_err().code,
            ErrorCode::ModeNotImplemented
        );
    }
}

#[test]
fn pure_reducer_rejects_changed_approved_facts() {
    for changed in ["revision", "amount", "hash", "payee"] {
        let mut ledger = Ledger::default();
        ledger.seed();
        let id = match domain::apply(&mut ledger, create("synthetic")).unwrap() {
            Output::Claim(c) => c.claim.id,
            _ => unreachable!(),
        };
        domain::apply(
            &mut ledger,
            Command::Submit {
                id,
                request: request(100, "Synthetic"),
            },
        )
        .unwrap();
        domain::apply(
            &mut ledger,
            Command::Approve {
                approval_id: ApprovalId::new(),
                id,
                payee_id: PAYEE_ID,
                now: 100,
            },
        )
        .unwrap();
        match changed {
            "revision" => ledger.claims.get_mut(&id).unwrap().revision += 1,
            "amount" => ledger.claims.get_mut(&id).unwrap().amount_cents = cents(101),
            "hash" => {
                ledger.claims.get_mut(&id).unwrap().receipt_sha256 =
                    ReceiptHash::try_from("f".repeat(64)).unwrap()
            }
            _ => ledger.approvals.get_mut(&id).unwrap().payee_id = PayeeId::new(),
        }
        assert_eq!(
            domain::apply(
                &mut ledger,
                Command::PreparePayout {
                    payout_id: PayoutId::new(),
                    id,
                    now: 101
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::ApprovalInvalid
        );
        assert!(ledger.payouts.is_empty());
    }
}

#[tokio::test]
async fn malformed_paths_and_oversized_bodies_return_api_errors() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let app = router(Arc::new(service));
    for uri in ["/api/claims/%FF", "/api/events/%FF/budget-lines"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .header("authorization", "Bearer synthetic-test-finance")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: ApiError =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body.code, ErrorCode::InvalidInput);
    }
    let response = app
        .oneshot(
            Request::builder()
                .header("authorization", "Bearer synthetic-test-finance")
                .method("POST")
                .uri("/api/claims")
                .header("content-type", "application/json")
                .body(Body::from(" ".repeat(131_073)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body: ApiError =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body.code, ErrorCode::InvalidInput);
}

#[tokio::test]
async fn restart_after_provider_acceptance_before_local_ack_queries_paid() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("accepted.db").display()
    );
    let (original, original_paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let id = approved(&original).await;
    let prepared = original
        .storage
        .execute(Command::PreparePayout {
            payout_id: PayoutId::new(),
            id,
            now: 101,
        })
        .await
        .unwrap();
    let response = match prepared {
        Output::Prepared {
            response,
            action: PayoutAction::SendNew,
        } => response,
        _ => unreachable!(),
    };
    let payout = response.payout.as_ref().unwrap();
    original
        .paypal
        .send(payout, response.approval.as_ref().unwrap())
        .await
        .unwrap();
    assert_eq!(
        original.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    let provider = original_paypal.provider();
    drop(original);
    drop(original_paypal);
    let (mut restarted, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let paypal = Arc::new(MockPayPal::with_provider(
        MockPayoutFixture::Success,
        provider,
    ));
    restarted.paypal = paypal.clone();
    assert_eq!(
        restarted.payout(id, 2000).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget(&restarted).await.paid_cents.get(), 750);
}

#[tokio::test]
async fn verified_event_dedupe_and_paid_accounting_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("event.db").display()
    );
    let (first, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let id = approved(&first).await;
    let pending = first.payout(id, 101).await.unwrap();
    let event = envelope(
        &pending,
        "synthetic-durable-event",
        PayoutEventKind::Succeeded,
    );
    webhook(&first, &event).await.unwrap();
    drop(first);
    let (second, paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    assert!(webhook(&second, &event).await.unwrap().duplicate);
    assert_eq!(
        second.payout(id, 5000).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(budget(&second).await.paid_cents.get(), 750);
    assert_eq!(budget(&second).await.reserved_cents.get(), 0);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
}

async fn api_get(
    app: axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header("authorization", token);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn s2_finance_reads_require_auth_and_validate_paths() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let app = router(Arc::new(service));
    for uri in [
        format!("/api/events/{EVENT_ID}/claims"),
        format!("/api/claims/{}/review", ClaimId::new()),
    ] {
        for token in [None, Some("Bearer wrong"), Some("synthetic-test-finance")] {
            let (status, body) = api_get(app.clone(), &uri, token).await;
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert_eq!(body["code"], "FORBIDDEN");
        }
    }
    for uri in [
        "/api/events/bad/claims",
        "/api/claims/bad/review",
        "/api/claims/%FF/review",
    ] {
        let (status, body) = api_get(app.clone(), uri, Some("Bearer synthetic-test-finance")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "INVALID_INPUT");
    }
    for uri in [
        format!("/api/events/{}/claims", EventId::new()),
        format!("/api/claims/{}/review", ClaimId::new()),
    ] {
        let (status, body) =
            api_get(app.clone(), &uri, Some("Bearer synthetic-test-finance")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], "NOT_FOUND");
    }
}

#[tokio::test]
async fn s2_queue_and_review_survive_reload_without_changing_money() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("review.db").display()
    );
    let (service, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let (status, body) = api_get(
        router(Arc::new(service)),
        &format!("/api/events/{EVENT_ID}/claims"),
        Some("Bearer synthetic-test-finance"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["claims"], serde_json::json!([]));
    let (service, _) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let draft = service
        .claim(create("synthetic review source"))
        .await
        .unwrap()
        .claim
        .id;
    let app = router(Arc::new(service));
    let (status, body) = api_get(
        app,
        &format!("/api/claims/{draft}/review"),
        Some("Bearer synthetic-test-finance"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["submission"].is_null());
    let (service, _) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let id = approved(&service).await;
    let before = budget(&service).await;
    drop(service);
    let (service, provider) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let service = Arc::new(service);
    let app = router(service.clone());
    for _ in 0..2 {
        let (status, body) = api_get(
            app.clone(),
            &format!("/api/events/{EVENT_ID}/claims"),
            Some("Bearer synthetic-test-finance"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let claims: ClaimsResponse = serde_json::from_value(body).unwrap();
        assert_eq!(claims.claims.len(), 2);
        assert!(claims
            .claims
            .windows(2)
            .all(|pair| pair[0].claim.id < pair[1].claim.id));
        let (status, body) = api_get(
            app.clone(),
            &format!("/api/claims/{id}/review"),
            Some("Bearer synthetic-test-finance"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let review: ClaimReviewResponse = serde_json::from_value(body).unwrap();
        assert_eq!(review.receipt_text, "mock:clean");
        assert_eq!(review.claim.claim.state, ClaimState::Approved);
        assert!(review.claim.payout.is_none());
        assert_eq!(
            serde_json::to_value(review.submission.unwrap()).unwrap(),
            serde_json::to_value(request(750, "Synthetic shop")).unwrap()
        );
    }
    let after = budget(&service).await;
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
    assert_eq!(provider.send_calls.load(Ordering::SeqCst), 0);
    assert_eq!(provider.query_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn s2_public_config_contains_only_supported_modes_and_demo_ids() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let app = router(Arc::new(service));
    let (status, body) = api_get(app.clone(), "/api/config", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!({"paypal_mode":"mock", "llm_mode":"manual", "event_id":EVENT_ID, "claimant_id":CLAIMANT_ID})
    );
    let config: PublicConfigResponse = serde_json::from_value(body).unwrap();
    assert_eq!(config.paypal_mode, RuntimeMode::Mock);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

struct UnavailableStorage;
#[async_trait::async_trait]
impl Storage for UnavailableStorage {
    async fn execute(&self, _: Command) -> Result<Output, ApiError> {
        Err(ApiError::new(
            ErrorCode::StorageUnavailable,
            "database operation failed",
        ))
    }
}

#[tokio::test]
async fn s2_finance_reads_report_storage_outages() {
    let (service, _) = service(Arc::new(UnavailableStorage), MockPayoutFixture::Success);
    let app = router(Arc::new(service));
    for uri in [
        format!("/api/events/{EVENT_ID}/claims"),
        format!("/api/claims/{}/review", ClaimId::new()),
    ] {
        let (status, body) =
            api_get(app.clone(), &uri, Some("Bearer synthetic-test-finance")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "STORAGE_UNAVAILABLE");
        let (status, _) = api_get(app.clone(), &uri, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    assert_eq!(api_get(app, "/api/config", None).await.0, StatusCode::OK);
}

#[tokio::test]
async fn s2_invalid_amounts_do_not_reserve_or_change_draft() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = service
        .claim(create("amount validation"))
        .await
        .unwrap()
        .claim
        .id;
    let initial = service.claim(Command::Get(id)).await.unwrap();
    let mut invalid = Vec::new();
    invalid.push(request(0, "Zero"));
    let mut empty = request(10, "Empty");
    empty.items.clear();
    invalid.push(empty);
    let mut zero_quantity = request(10, "Quantity");
    zero_quantity.items[0].quantity = 0;
    invalid.push(zero_quantity);
    let mut multiply = request(10, "Multiply overflow");
    multiply.items[0].quantity = 2;
    multiply.items[0].unit_cents = cents(i64::MAX);
    invalid.push(multiply);
    let mut sum = request(10, "Sum overflow");
    sum.items = vec![
        ReceiptItem {
            quantity: 1,
            unit_cents: cents(i64::MAX),
        },
        ReceiptItem {
            quantity: 1,
            unit_cents: cents(1),
        },
    ];
    invalid.push(sum);
    for (index, request) in invalid.into_iter().enumerate() {
        let expected = if index == 0 {
            ErrorCode::NeedsInfo
        } else {
            ErrorCode::InvalidInput
        };
        assert_eq!(
            service
                .claim(Command::Submit { id, request })
                .await
                .unwrap_err()
                .code,
            expected
        );
        assert_eq!(
            serde_json::to_value(service.claim(Command::Get(id)).await.unwrap().claim).unwrap(),
            serde_json::to_value(&initial.claim).unwrap()
        );
        assert_eq!(budget(&service).await.reserved_cents, Cents::ZERO);
    }
}

async fn api_post(
    app: axum::Router,
    uri: &str,
    body: serde_json::Value,
    headers: HeaderMap,
) -> (StatusCode, serde_json::Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    request.headers_mut().unwrap().extend(headers);
    let response = app
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn all_s1_routes_work_in_process_with_errors_and_money_replay() {
    let (service, provider) = setup(MockPayoutFixture::Success).await;
    let app = router(Arc::new(service));
    for uri in [
        format!("/api/events/{EVENT_ID}/budget-lines"),
        "/api/payees".into(),
    ] {
        assert_eq!(
            api_get(app.clone(), &uri, Some("Bearer synthetic-test-finance"))
                .await
                .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        api_get(
            app.clone(),
            &format!("/api/events/{}/budget-lines", EventId::new()),
            Some("Bearer synthetic-test-finance")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        api_get(
            app.clone(),
            &format!("/api/claims/{}", ClaimId::new()),
            Some("Bearer synthetic-test-finance")
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for action in ["extract", "submit", "payout"] {
        let (_, body) = api_post(
            app.clone(),
            &format!("/api/claims/{}/{action}", ClaimId::new()),
            serde_json::json!({}),
            finance_headers(),
        )
        .await;
        assert!(body.get("code").is_some());
    }
    let command = create("mock:clean");
    let Command::Create { request, .. } = command else {
        unreachable!()
    };
    let (status, body) = api_post(
        app.clone(),
        "/api/claims",
        serde_json::to_value(request).unwrap(),
        finance_headers(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id: ClaimId = serde_json::from_value(body["claim"]["id"].clone()).unwrap();
    let base = format!("/api/claims/{id}");
    assert_eq!(
        api_post(
            app.clone(),
            &format!("{base}/extract"),
            serde_json::json!({}),
            finance_headers()
        )
        .await
        .0,
        StatusCode::OK
    );
    let mut negative = serde_json::to_value(self::request(750, "Synthetic shop")).unwrap();
    negative["items"][0]["unit_cents"] = serde_json::json!(-1);
    let (status, body) = api_post(
        app.clone(),
        &format!("{base}/submit"),
        negative,
        finance_headers(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "INVALID_INPUT");
    let submit = serde_json::to_value(self::request(750, "Synthetic shop")).unwrap();
    for _ in 0..2 {
        assert_eq!(
            api_post(
                app.clone(),
                &format!("{base}/submit"),
                submit.clone(),
                finance_headers()
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let mut finance = finance_headers();
    finance.insert(
        "authorization",
        "Bearer synthetic-test-finance".parse().unwrap(),
    );
    assert_eq!(
        api_post(
            app.clone(),
            &format!("{base}/approve"),
            serde_json::json!({"payee_id":PAYEE_ID}),
            finance
        )
        .await
        .0,
        StatusCode::OK
    );
    let (status, body) = api_post(
        app.clone(),
        &format!("{base}/payout"),
        serde_json::json!({}),
        finance_headers(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let response: ClaimResponse = serde_json::from_value(body).unwrap();
    let event = serde_json::to_value(envelope(
        &response,
        "s2-http-event",
        PayoutEventKind::Succeeded,
    ))
    .unwrap();
    assert_eq!(
        api_post(
            app.clone(),
            "/api/webhooks/paypal",
            event.clone(),
            finance_headers()
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for duplicate in [false, true] {
        let (status, body) = api_post(app.clone(), "/api/webhooks/paypal", event.clone(), {
            let mut headers = signed(event.to_string().as_bytes());
            headers.extend(finance_headers());
            headers
        })
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["duplicate"], duplicate);
    }
    assert_eq!(
        api_post(
            app.clone(),
            &format!("{base}/payout"),
            serde_json::json!({}),
            finance_headers()
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        api_get(app.clone(), &base, Some("Bearer synthetic-test-finance"))
            .await
            .1["claim"]["state"],
        "Paid"
    );
    let budget = api_get(
        app,
        &format!("/api/events/{EVENT_ID}/budget-lines"),
        Some("Bearer synthetic-test-finance"),
    )
    .await
    .1;
    assert_eq!(budget["budget_lines"][0]["paid_cents"], 750);
    assert_eq!(budget["budget_lines"][0]["reserved_cents"], 0);
    assert_eq!(provider.send_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn s2_queues_exclude_other_events() {
    let mut ledger = Ledger::default();
    ledger.seed();
    let other_event = EventId::new();
    let other_budget = BudgetLineId::new();
    let mut line = ledger.budgets[&BUDGET_ID].clone();
    line.id = other_budget;
    line.event_id = other_event;
    ledger.budgets.insert(other_budget, line);
    let mut ids = Vec::new();
    for (event, budget) in [(EVENT_ID, BUDGET_ID), (other_event, other_budget)] {
        let id = ClaimId::new();
        domain::apply(
            &mut ledger,
            Command::Create {
                id,
                request: CreateClaimRequest {
                    event_id: event,
                    budget_line_id: budget,
                    claimant_id: CLAIMANT_ID,
                    receipt_text: "synthetic queue receipt".into(),
                },
            },
        )
        .unwrap();
        ids.push((event, id));
    }
    for (event, id) in ids {
        let Output::Claims(queue) = domain::apply(&mut ledger, Command::Claims(event)).unwrap()
        else {
            panic!("expected queue");
        };
        assert_eq!(
            queue
                .claims
                .iter()
                .map(|row| row.claim.id)
                .collect::<Vec<_>>(),
            vec![id]
        );
    }
}

#[tokio::test]
async fn receipt_text_limit_counts_unicode_characters() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    service.claim(create(&"é".repeat(4000))).await.unwrap();
    for text in ["x".repeat(4001), "é".repeat(4001), " ".into()] {
        let error = service.claim(create(&text)).await.unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidInput);
    }
}

#[tokio::test]
async fn s2_payout_reviews_survive_reload_without_provider_calls() {
    for fixture in [MockPayoutFixture::Pending, MockPayoutFixture::Success] {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("payout-review.db").display()
        );
        let (writer, _) = service(Arc::new(SqliteStore::connect(&url).await.unwrap()), fixture);
        let id = approved(&writer).await;
        writer.payout(id, 101).await.unwrap();
        let expected = writer.payout(id, 102).await.unwrap();
        assert_eq!(
            expected.claim.state,
            if matches!(fixture, MockPayoutFixture::Pending) {
                ClaimState::PayoutPending
            } else {
                ClaimState::Paid
            }
        );
        let before = budget(&writer).await;
        drop(writer);
        let (reader, provider) =
            service(Arc::new(SqliteStore::connect(&url).await.unwrap()), fixture);
        let reader = Arc::new(reader);
        for _ in 0..2 {
            let (status, body) = api_get(
                router(reader.clone()),
                &format!("/api/claims/{id}/review"),
                Some("Bearer synthetic-test-finance"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            let review: ClaimReviewResponse = serde_json::from_value(body).unwrap();
            assert_eq!(
                serde_json::to_value(review.claim).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
            assert!(expected.payout.is_some());
            assert_eq!(review.receipt_text, "mock:clean");
            assert!(review.submission.is_some());
        }
        let after = budget(&reader).await;
        assert_eq!(before.reserved_cents, after.reserved_cents);
        assert_eq!(before.paid_cents, after.paid_cents);
        assert_eq!(provider.send_calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.query_calls.load(Ordering::SeqCst), 0);
    }
}

struct WrongOutputStorage;
#[async_trait::async_trait]
impl Storage for WrongOutputStorage {
    async fn execute(&self, _: Command) -> Result<Output, ApiError> {
        Ok(Output::Receipt(String::new()))
    }
}

#[tokio::test]
async fn s2_finance_reads_reject_unexpected_storage_output() {
    let (service, _) = service(Arc::new(WrongOutputStorage), MockPayoutFixture::Success);
    let app = router(Arc::new(service));
    for uri in [
        format!("/api/events/{EVENT_ID}/claims"),
        format!("/api/claims/{}/review", ClaimId::new()),
    ] {
        let (status, body) =
            api_get(app.clone(), &uri, Some("Bearer synthetic-test-finance")).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "STORAGE_UNAVAILABLE");
    }
}

#[tokio::test]
async fn recovered_pending_ids_survive_reopen_and_bind_webhooks() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}/recovery.db?mode=rwc", dir.path().display());
    let (service, paypal) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Timeout,
    );
    let id = approved(&service).await;
    let unknown = service.payout(id, 101).await.unwrap();
    let event = envelope(&unknown, "recovered-pending", PayoutEventKind::Pending);
    let result = webhook(&service, &event).await.unwrap();
    assert_eq!(
        result.payout.paypal_batch_id.as_deref(),
        Some(event.paypal_batch_id.as_str())
    );
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    drop(service);
    let (service, _) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Pending,
    );
    let stored = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(
        stored.payout.unwrap().item_id.as_deref(),
        Some(event.item_id.as_str())
    );
    let mut wrong = event;
    wrong.paypal_event_id = "wrong-recovered-id".into();
    wrong.item_id = "unrelated-item".into();
    assert_eq!(
        webhook(&service, &wrong).await.unwrap_err().code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
}

#[tokio::test]
async fn terminal_conflict_is_durable_and_freezes_only_its_claim() {
    let (service, paypal) = setup(MockPayoutFixture::Failed).await;
    let id = approved(&service).await;
    let failed = service.payout(id, 101).await.unwrap();
    let result = webhook(
        &service,
        &envelope(&failed, "contradictory-success", PayoutEventKind::Succeeded),
    )
    .await
    .unwrap();
    assert_eq!(result.payout.state, PayoutState::PayoutFailed);
    assert_eq!(result.payout.reconciliation.len(), 1);
    let other = submitted(&service, "another receipt", 100, "Other shop").await;
    assert_eq!(
        service
            .claim(Command::Get(other))
            .await
            .unwrap()
            .claim
            .state,
        ClaimState::Submitted
    );
    service.payout(id, 102).await.unwrap();
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
}

struct FlakyQuery {
    inner: MockPayPal,
    attempts: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl receipt_roots_server::adapters::PayPalPayouts for FlakyQuery {
    async fn send(
        &self,
        p: &Payout,
        a: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.inner.send(p, a).await
    }
    async fn query(
        &self,
        p: &Payout,
        a: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(receipt_roots_server::adapters::PayoutError::QueryFailed {
                error: ApiError::new(ErrorCode::AdapterUnavailable, "transient query"),
                retryable: true,
            });
        }
        self.inner.query(p, a).await
    }
    fn verify_webhook(&self, b: &[u8], h: &HeaderMap) -> Result<(), ApiError> {
        self.inner.verify_webhook(b, h)
    }
}
#[tokio::test]
async fn transient_query_retries_without_unknown_or_resend() {
    let (mut service, _) = setup(MockPayoutFixture::Pending).await;
    let adapter = Arc::new(FlakyQuery {
        inner: MockPayPal::new(MockPayoutFixture::Pending),
        attempts: 0.into(),
    });
    service.paypal = adapter.clone();
    let id = approved(&service).await;
    service.payout(id, 101).await.unwrap();
    assert_eq!(
        service.payout(id, 102).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    assert_eq!(adapter.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(adapter.inner.send_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unauthenticated_claim_and_payout_routes_are_forbidden() {
    let (service, paypal) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let app = router(Arc::new(service));
    for (method, uri) in [
        ("GET", format!("/api/claims/{id}")),
        ("POST", format!("/api/claims/{id}/payout")),
        ("GET", "/api/payees".into()),
        ("GET", format!("/api/events/{EVENT_ID}/budget-lines")),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn mock_verifier_refuses_non_loopback_even_with_custom_token() {
    for bind in ["0.0.0.0:3000", "[::]:3000", "192.0.2.1:3000"] {
        assert!(Config::from_values(BTreeMap::from([
            ("BIND_ADDR".into(), bind.into()),
            ("FINANCE_TOKEN".into(), "custom-token".into())
        ]))
        .is_err());
    }
}

fn finance_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        "authorization",
        "Bearer synthetic-test-finance".parse().unwrap(),
    );
    headers
}

#[tokio::test]
async fn reconciliation_allows_other_previously_approved_payouts() {
    let (service, paypal) = setup(MockPayoutFixture::Failed).await;
    let second = submitted(&service, "second receipt", 100, "Second shop").await;
    service
        .claim(Command::Approve {
            approval_id: ApprovalId::new(),
            id: second,
            payee_id: PAYEE_ID,
            now: 100,
        })
        .await
        .unwrap();
    let first = approved(&service).await;
    let failed = service.payout(first, 101).await.unwrap();
    webhook(
        &service,
        &envelope(
            &failed,
            "freeze-existing-approval",
            PayoutEventKind::Succeeded,
        ),
    )
    .await
    .unwrap();
    assert!(service.payout(second, 102).await.is_ok());
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn observation_status_cannot_claim_paid_without_item_success() {
    let (service, _) = setup(MockPayoutFixture::Pending).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let mut observation =
        MockPayPal::observation(pending.payout.as_ref().unwrap(), PayoutState::Paid);
    observation.status = "Pending".into();
    assert_eq!(
        service
            .claim(Command::Observe { id, observation })
            .await
            .unwrap_err()
            .code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
}

#[tokio::test]
async fn claimant_capability_scopes_reads_writes_and_hides_provider_ids() {
    use receipt_roots_server::http::claimant_token;
    let (service, paypal) = setup(MockPayoutFixture::Pending).await;
    let id = approved(&service).await;
    service.payout(id, 101).await.unwrap();
    let app = router(Arc::new(service));
    let owner = format!(
        "Bearer {}",
        claimant_token("synthetic-test-finance", CLAIMANT_ID)
    );
    let stranger = format!(
        "Bearer {}",
        claimant_token("synthetic-test-finance", ClaimantId::new())
    );
    let (status, body) = api_get(app.clone(), &format!("/api/claims/{id}"), Some(&owner)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["payout"].is_null());
    assert!(body["approval"].is_null());
    assert_eq!(
        api_get(app.clone(), &format!("/api/claims/{id}"), Some(&stranger))
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut headers = HeaderMap::new();
    headers.insert("authorization", owner.parse().unwrap());
    for action in ["approve", "payout"] {
        assert_eq!(
            api_post(
                app.clone(),
                &format!("/api/claims/{id}/{action}"),
                serde_json::json!({}),
                headers.clone()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let mut wrong_headers = HeaderMap::new();
    wrong_headers.insert("authorization", stranger.parse().unwrap());
    for action in ["extract", "submit"] {
        assert_eq!(
            api_post(
                app.clone(),
                &format!("/api/claims/{id}/{action}"),
                serde_json::json!({}),
                wrong_headers.clone()
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
    }
    let Command::Create { request, .. } = create("impersonation") else {
        unreachable!()
    };
    assert_eq!(
        api_post(
            app.clone(),
            "/api/claims",
            serde_json::to_value(request).unwrap(),
            wrong_headers
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (_, body) = api_get(app, "/api/payees", Some(&stranger)).await;
    assert!(body["payees"].as_array().unwrap().is_empty());
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provider_observations_bind_all_facts_before_accounting_even_after_terminal() {
    let (service, _) = setup(MockPayoutFixture::Success).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let original = MockPayPal::observation(pending.payout.as_ref().unwrap(), PayoutState::Paid);
    for field in [
        "amount_cents",
        "receiver",
        "recipient_type",
        "sender_batch_id",
        "sender_item_id",
        "payee_id",
        "missing-binding",
        "missing-batch",
        "missing-item",
    ] {
        let mut observation = original.clone();
        let binding = observation.binding.as_mut().unwrap();
        match field {
            "amount_cents" => binding.amount_cents = cents(1),
            "receiver" => binding.receiver = "wrong@example.invalid".into(),
            "recipient_type" => binding.recipient_type = "PHONE".into(),
            "sender_batch_id" => binding.sender_batch_id = "wrong".into(),
            "sender_item_id" => binding.sender_item_id = "wrong".into(),
            "payee_id" => binding.payee_id = PayeeId::new(),
            "missing-binding" => observation.binding = None,
            "missing-batch" => observation.paypal_batch_id = None,
            "missing-item" => observation.item_id = None,
            _ => unreachable!(),
        }
        assert_eq!(
            service
                .claim(Command::Observe { id, observation })
                .await
                .unwrap_err()
                .code,
            ErrorCode::WebhookMismatch,
            "{field}"
        );
        assert_eq!(budget(&service).await.reserved_cents.get(), 750);
    }
    service
        .claim(Command::Observe {
            id,
            observation: original.clone(),
        })
        .await
        .unwrap();
    let mut wrong = original;
    wrong.item_id = Some("other-terminal-item".into());
    assert_eq!(
        service
            .claim(Command::Observe {
                id,
                observation: wrong
            })
            .await
            .unwrap_err()
            .code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(budget(&service).await.paid_cents.get(), 750);
}

#[tokio::test]
async fn adjustments_and_terminal_conflicts_survive_reopen_without_accounting_changes() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}/adjustments.db?mode=rwc", dir.path().display());
    let (service, _) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let paid = webhook(
        &service,
        &envelope(
            &pending,
            "success-before-adjustment",
            PayoutEventKind::Succeeded,
        ),
    )
    .await
    .unwrap();
    assert_eq!(paid.payout.state, PayoutState::Paid);
    for (event_id, kind) in [
        ("refund", PayoutEventKind::Refunded),
        ("reversal", PayoutEventKind::Reversed),
        ("unknown-status", PayoutEventKind::Unknown),
        ("late-failure", PayoutEventKind::Failed),
    ] {
        let event = envelope(&pending, event_id, kind);
        let response = webhook(&service, &event).await.unwrap();
        assert_eq!(response.payout.state, PayoutState::Paid);
        assert!(webhook(&service, &event).await.unwrap().duplicate);
    }
    drop(service);
    let (service, paypal) = self::service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Success,
    );
    let response = service.payout(id, 20000).await.unwrap();
    assert_eq!(response.payout.unwrap().reconciliation.len(), 4);
    assert_eq!(budget(&service).await.paid_cents.get(), 750);
    assert_eq!(budget(&service).await.reserved_cents.get(), 0);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 0);
}

struct DelayedSend {
    inner: MockPayPal,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl receipt_roots_server::adapters::PayPalPayouts for DelayedSend {
    async fn send(
        &self,
        p: &Payout,
        a: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.send(p, a).await
    }
    async fn query(
        &self,
        p: &Payout,
        a: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.inner.query(p, a).await
    }
    fn verify_webhook(&self, b: &[u8], h: &HeaderMap) -> Result<(), ApiError> {
        self.inner.verify_webhook(b, h)
    }
}
#[tokio::test]
async fn late_pending_create_merges_ids_after_concurrent_unknown() {
    let (mut service, _) = setup(MockPayoutFixture::Pending).await;
    let adapter = Arc::new(DelayedSend {
        inner: MockPayPal::new(MockPayoutFixture::Pending),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    service.paypal = adapter.clone();
    let id = approved(&service).await;
    let concurrent = async {
        adapter.entered.notified().await;
        service
            .claim(Command::Observe {
                id,
                observation: domain::Observation {
                    state: PayoutState::PayoutUnknown,
                    paypal_batch_id: None,
                    item_id: None,
                    binding: None,
                    source: "ambiguous-create".into(),
                    status: "Ambiguous".into(),
                },
            })
            .await
            .unwrap();
        adapter.release.notify_one();
    };
    let (result, ()) = tokio::join!(service.payout(id, 101), concurrent);
    let response = result.unwrap();
    let payout = response.payout.as_ref().unwrap();
    assert_eq!(payout.state, PayoutState::PayoutUnknown);
    assert!(payout.paypal_batch_id.is_some());
    assert!(payout.item_id.is_some());
    let mut event = envelope(&response, "wrong-after-race", PayoutEventKind::Succeeded);
    event.paypal_batch_id = "wrong-provider-batch".into();
    assert_eq!(
        webhook(&service, &event).await.unwrap_err().code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(adapter.inner.send_calls.load(Ordering::SeqCst), 1);
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
}

struct QueryOutage {
    inner: MockPayPal,
    calls: std::sync::atomic::AtomicUsize,
    retryable: bool,
}
#[async_trait::async_trait]
impl receipt_roots_server::adapters::PayPalPayouts for QueryOutage {
    async fn send(
        &self,
        p: &Payout,
        a: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.inner.send(p, a).await
    }
    async fn query(
        &self,
        _: &Payout,
        _: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(receipt_roots_server::adapters::PayoutError::QueryFailed {
            error: ApiError::new(ErrorCode::AdapterUnavailable, "query unavailable"),
            retryable: self.retryable,
        })
    }
    fn verify_webhook(&self, b: &[u8], h: &HeaderMap) -> Result<(), ApiError> {
        self.inner.verify_webhook(b, h)
    }
}
#[tokio::test]
async fn query_backoff_is_bounded_and_nonretryable_errors_do_not_retry() {
    for retryable in [true, false] {
        let (mut service, _) = setup(MockPayoutFixture::Pending).await;
        let adapter = Arc::new(QueryOutage {
            inner: MockPayPal::new(MockPayoutFixture::Pending),
            calls: 0.into(),
            retryable,
        });
        service.paypal = adapter.clone();
        let id = approved(&service).await;
        service.payout(id, 101).await.unwrap();
        assert_eq!(
            service.payout(id, 102).await.unwrap_err().code,
            ErrorCode::AdapterUnavailable
        );
        assert_eq!(
            adapter.calls.load(Ordering::SeqCst),
            if retryable { 3 } else { 1 }
        );
        assert_eq!(
            service.claim(Command::Get(id)).await.unwrap().claim.state,
            ClaimState::PayoutPending
        );
        assert_eq!(budget(&service).await.reserved_cents.get(), 750);
        assert_eq!(adapter.inner.send_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn unknown_observation_cannot_bypass_provider_fact_binding() {
    let (service, _) = setup(MockPayoutFixture::Pending).await;
    let id = approved(&service).await;
    let pending = service.payout(id, 101).await.unwrap();
    let mut observation =
        MockPayPal::observation(pending.payout.as_ref().unwrap(), PayoutState::PayoutUnknown);
    observation.status = "Ambiguous".into();
    observation.binding.as_mut().unwrap().receiver = "wrong@example.invalid".into();
    assert_eq!(
        service
            .claim(Command::Observe { id, observation })
            .await
            .unwrap_err()
            .code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    assert_eq!(budget(&service).await.reserved_cents.get(), 750);
}

async fn encoded_claim_scope(method: &str) {
    use receipt_roots_server::http::claimant_token;
    let (service, paypal) = setup(MockPayoutFixture::Pending).await;
    let id = ClaimId(uuid::Uuid::parse_str("6abcdef0-1234-4567-89ab-cdef01234567").unwrap());
    let Command::Create {
        request: create_request,
        ..
    } = create("mock:clean")
    else {
        unreachable!()
    };
    let before = service
        .claim(Command::Create {
            id,
            request: create_request,
        })
        .await
        .unwrap();
    let service = Arc::new(service);
    let app = router(service.clone());
    let owner = format!(
        "Bearer {}",
        claimant_token(&service.finance_token, CLAIMANT_ID)
    );
    let stranger = format!(
        "Bearer {}",
        claimant_token(&service.finance_token, ClaimantId::new())
    );
    let normal = id.to_string();
    let variants = [
        normal.clone(),
        format!("%36{}", &normal[1..]),
        normal.replacen('-', "%2d", 1).replacen('-', "%2D", 1),
        format!("%2F{normal}"),
        format!("{normal}%2f"),
    ];
    for encoded in variants {
        let path = format!(
            "/api/claims/{encoded}{}",
            if method == "get" {
                String::new()
            } else {
                format!("/{method}")
            }
        );
        let status = if method == "get" {
            api_get(app.clone(), &path, Some(&stranger)).await.0
        } else {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", stranger.parse().unwrap());
            api_post(
                app.clone(),
                &path,
                if method == "extract" {
                    serde_json::json!({})
                } else {
                    serde_json::to_value(request(750, "Synthetic shop")).unwrap()
                },
                headers,
            )
            .await
            .0
        };
        assert!(
            matches!(
                status,
                StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND
            ),
            "{method} {encoded}: {status}"
        );
        assert_eq!(
            serde_json::to_value(service.claim(Command::Get(id)).await.unwrap()).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(budget(&service).await.reserved_cents.get(), 0);
    }
    let path = format!(
        "/api/claims/{id}{}",
        if method == "get" {
            String::new()
        } else {
            format!("/{method}")
        }
    );
    let status = if method == "get" {
        api_get(app, &path, Some(&owner)).await.0
    } else {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", owner.parse().unwrap());
        api_post(
            app,
            &path,
            if method == "extract" {
                serde_json::json!({})
            } else {
                serde_json::to_value(request(750, "Synthetic shop")).unwrap()
            },
            headers,
        )
        .await
        .0
    };
    assert_eq!(status, StatusCode::OK);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn encoded_claim_get_denies_nonowners() {
    encoded_claim_scope("get").await;
}
#[tokio::test]
async fn encoded_claim_extract_denies_nonowners() {
    encoded_claim_scope("extract").await;
}
#[tokio::test]
async fn encoded_claim_submit_denies_nonowners() {
    encoded_claim_scope("submit").await;
}

struct NeverSent {
    sends: std::sync::atomic::AtomicUsize,
    queries: std::sync::atomic::AtomicUsize,
}
#[async_trait::async_trait]
impl receipt_roots_server::adapters::PayPalPayouts for NeverSent {
    async fn send(
        &self,
        _: &Payout,
        _: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Err(receipt_roots_server::adapters::PayoutError::NotSent(
            ApiError::new(ErrorCode::AdapterUnavailable, "synthetic not sent"),
        ))
    }
    async fn query(
        &self,
        _: &Payout,
        _: &Approval,
    ) -> Result<domain::Observation, receipt_roots_server::adapters::PayoutError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Err(receipt_roots_server::adapters::PayoutError::QueryFailed {
            error: ApiError::new(ErrorCode::AdapterUnavailable, "not found"),
            retryable: false,
        })
    }
    fn verify_webhook(&self, _: &[u8], _: &HeaderMap) -> Result<(), ApiError> {
        unreachable!()
    }
}

#[tokio::test]
async fn not_sent_persists_finance_reconciliation_without_releasing_or_retrying() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("not-sent.db").display()
    );
    let (mut service, _) = service(
        Arc::new(SqliteStore::connect(&url).await.unwrap()),
        MockPayoutFixture::Pending,
    );
    let adapter = Arc::new(NeverSent {
        sends: 0.into(),
        queries: 0.into(),
    });
    service.paypal = adapter.clone();
    let id = approved(&service).await;
    assert_eq!(
        service.payout(id, 101).await.unwrap_err().code,
        ErrorCode::AdapterUnavailable
    );
    drop(service);
    let service = Service {
        storage: Arc::new(SqliteStore::connect(&url).await.unwrap()),
        paypal: adapter.clone(),
        extractor: Arc::new(MockLlm),
        finance_token: "synthetic-test-finance".into(),
    };
    let response = service.claim(Command::Get(id)).await.unwrap();
    let payout = response.payout.as_ref().unwrap();
    assert_eq!(payout.reconciliation.len(), 1);
    assert_eq!(payout.reconciliation[0].status, "NotSent");
    assert_eq!(payout.reconciliation[0].source, "create-not-sent");
    assert_eq!(payout.state, PayoutState::PayoutPending);
    assert_eq!(response.claim.state, ClaimState::PayoutPending);
    assert!(payout.paypal_batch_id.is_none());
    assert!(payout.item_id.is_none());
    for now in [102, 103] {
        let replay = service.payout(id, now).await.unwrap();
        assert_eq!(
            serde_json::to_value(replay).unwrap(),
            serde_json::to_value(&response).unwrap()
        );
    }
    let app = router(Arc::new(service));
    let (status, review) = api_get(
        app.clone(),
        &format!("/api/claims/{id}/review"),
        Some("Bearer synthetic-test-finance"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        review["claim"]["payout"]["reconciliation"][0]["status"],
        "NotSent"
    );
    let (_, budgets) = api_get(
        app,
        &format!("/api/events/{EVENT_ID}/budget-lines"),
        Some("Bearer synthetic-test-finance"),
    )
    .await;
    assert_eq!(budgets["budget_lines"][0]["reserved_cents"], 750);
    assert_eq!(budgets["budget_lines"][0]["paid_cents"], 0);
    assert_eq!(adapter.sends.load(Ordering::SeqCst), 1);
    assert_eq!(adapter.queries.load(Ordering::SeqCst), 0);
}
