fn paypal_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in [
        "paypal-auth-algo",
        "paypal-cert-url",
        "paypal-transmission-id",
        "paypal-transmission-sig",
        "paypal-transmission-time",
    ] {
        headers.insert(name, "fixture".parse().unwrap());
    }
    headers
}
struct Verifier(crate::webhook::Verification);
#[async_trait]
impl crate::webhook::WebhookVerifier for Verifier {
    async fn verify(&self, _: &crate::webhook::Delivery) -> crate::webhook::Verification {
        self.0
    }
}
fn inbox(service: &Service, verdict: crate::webhook::Verification) -> crate::webhook::Inbox {
    crate::webhook::Inbox {
        storage: service.storage.clone(),
        verifier: Arc::new(Verifier(verdict)),
        paypal: service.paypal.clone(),
    }
}
fn event(fake: &Fake, id: &str, status: &str) -> Vec<u8> {
    let suffix = if status == "SUCCESS" {
        "SUCCEEDED"
    } else {
        status
    };
    serde_json::to_vec(&json!({"id":id,"event_type":format!("PAYMENT.PAYOUTS-ITEM.{suffix}"),
            "resource":{"payout_batch_id":"BATCH1","payout_item_id":"ITEM1","transaction_status":status,
            "payout_item":fake.body.lock().unwrap()["items"][0]}})).unwrap()
}
async fn snapshot(service: &Service, id: ClaimId) -> Value {
    let claim = service.claim(Command::Get(id)).await.unwrap();
    let crate::domain::Output::Budgets(budget) = service
        .storage
        .execute(Command::Budgets(EVENT_ID))
        .await
        .unwrap()
    else {
        panic!()
    };
    json!({"claim":claim,"budget":budget})
}
async fn accounting_snapshot(service: &Service, id: ClaimId) -> Value {
    let mut value = snapshot(service, id).await;
    value["claim"]["payout"]["reconciliation"] = json!([]);
    value
}
#[tokio::test]
async fn webhook_mismatches_record_evidence_without_accounting_changes() {
    use crate::webhook::Verification::*;
    let (service, fake, task) = setup("PROCESSING", false, false).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    service.payout(id, 102).await.unwrap(); // establish immutable provider item ID
    let before = accounting_snapshot(&service, id).await;
    let raw = event(&fake, "EV1", "SUCCESS");
    assert!(inbox(&service, Rejected)
        .ingest(&raw, &paypal_headers())
        .await
        .is_err());
    assert_eq!(accounting_snapshot(&service, id).await, before);
    let good = inbox(&service, Verified);
    for field in [
        "receiver",
        "sender_item_id",
        "recipient_type",
        "amount",
        "payout_batch_id",
        "payout_item_id",
    ] {
        let mut v: Value = serde_json::from_slice(&raw).unwrap();
        v["id"] = format!("BAD-{field}").into();
        if field.starts_with("payout_") {
            v["resource"][field] = "WRONG".into();
        } else {
            v["resource"]["payout_item"][field] = "WRONG".into();
        }
        assert!(matches!(
            good.ingest(&serde_json::to_vec(&v).unwrap(), &paypal_headers())
                .await
                .unwrap()
                .as_str(),
            "mismatch" | "unmatched"
        ));
        assert_eq!(accounting_snapshot(&service, id).await, before);
    }
    assert_eq!(
        good.ingest(&raw, &paypal_headers()).await.unwrap(),
        "applied"
    );
    let paid = accounting_snapshot(&service, id).await;
    assert_eq!(
        good.ingest(&raw, &paypal_headers()).await.unwrap(),
        "replay"
    );
    assert_eq!(accounting_snapshot(&service, id).await, paid);
    assert_eq!(
        good.ingest(&event(&fake, "EV1", "FAILED"), &paypal_headers())
            .await
            .unwrap(),
        "conflict"
    );
    assert_eq!(accounting_snapshot(&service, id).await, paid);
    let response = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(response.claim.state, ClaimState::PayoutPending);
    assert_eq!(response.payout.unwrap().reconciliation.len(), 6);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn webhook_orders_unclaimed_failed_returned_and_query_race() {
    use crate::webhook::Verification::Verified;
    for sequence in [
        vec!["SUCCESS", "PROCESSING"],
        vec!["PROCESSING", "SUCCESS"],
        vec!["UNCLAIMED"],
        vec!["FAILED"],
        vec!["RETURNED"],
        vec!["SUCCESS", "RETURNED"],
        vec!["FAILED", "SUCCESS"],
    ] {
        let query_status = if sequence[0] == "FAILED" || sequence[0] == "RETURNED" {
            sequence[0]
        } else {
            "SUCCESS"
        };
        let (service, fake, task) = setup(query_status, false, false).await;
        let id = approved(&service, 500).await;
        service.payout(id, 101).await.unwrap();
        let good = inbox(&service, Verified);
        for (n, status) in sequence.iter().enumerate() {
            assert_eq!(
                good.ingest(&event(&fake, &format!("EV{n}"), status), &paypal_headers())
                    .await
                    .unwrap(),
                "applied"
            );
        }
        let result = service.claim(Command::Get(id)).await.unwrap();
        let crate::domain::Output::Budgets(b) = service
            .storage
            .execute(Command::Budgets(EVENT_ID))
            .await
            .unwrap()
        else {
            panic!()
        };
        if sequence == ["UNCLAIMED"] {
            assert_eq!(result.claim.state, ClaimState::PayoutPending);
            assert_eq!(b.budget_lines[0].reserved_cents.get(), 500);
            assert_eq!(b.budget_lines[0].paid_cents.get(), 0);
        } else if sequence[0] == "FAILED" || sequence[0] == "RETURNED" {
            assert_eq!(result.claim.state, ClaimState::PayoutFailed);
            assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
            assert_eq!(b.budget_lines[0].paid_cents.get(), 0);
        } else {
            assert_eq!(result.claim.state, ClaimState::Paid);
            assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
            assert_eq!(b.budget_lines[0].paid_cents.get(), 500);
        }
        if sequence == ["SUCCESS", "RETURNED"] || sequence == ["FAILED", "SUCCESS"] {
            assert_eq!(result.payout.unwrap().reconciliation.len(), 1);
        }
        task.abort();
    }
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let good = inbox(&service, Verified);
    let raw = event(&fake, "RACE", "SUCCESS");
    let headers = paypal_headers();
    let (query, webhook) = tokio::join!(
        service.query_reserved(id, 1000, 5),
        good.ingest(&raw, &headers)
    );
    query.unwrap();
    webhook.unwrap();
    let crate::domain::Output::Budgets(b) = service
        .storage
        .execute(Command::Budgets(EVENT_ID))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.budget_lines[0].paid_cents.get(), 500);
    assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn inbox_restart_retry_and_replay_are_durable() {
    use crate::webhook::Verification::*;
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("inbox.db").display()
    );
    let (service, fake, task) = setup_store("SUCCESS", false, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    let raw = event(&fake, "DURABLE", "SUCCESS");
    let mut headers = paypal_headers();
    headers.insert("paypal-transmission-id", "first".parse().unwrap());

    headers.insert(
        "x-original",
        axum::http::HeaderValue::from_static("fixture-original"),
    );
    assert!(inbox(&service, Unavailable)
        .ingest(&raw, &headers)
        .await
        .is_err());
    assert_eq!(snapshot(&service, id).await, before);
    drop(service);
    task.abort();
    let storage = Arc::new(SqliteStore::connect(&url).await.unwrap());
    storage.make_inbox_due().await.unwrap();
    let crate::domain::Output::Inbox(rows) = storage.execute(Command::InboxPending).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, raw);
    assert_eq!(
        rows[0].headers,
        crate::webhook::Delivery::new(&raw, &headers).headers
    );
    let good = crate::webhook::Inbox {
        storage: storage.clone(),
        verifier: Arc::new(Verifier(Verified)),
        paypal: Arc::new(crate::adapters::MockPayPal::new(
            crate::adapters::MockPayoutFixture::Success,
        )),
    };
    good.storage.make_inbox_due().await.unwrap();
    good.retry().await.unwrap();
    let crate::domain::Output::Claim(c) = storage.execute(Command::Get(id)).await.unwrap() else {
        panic!()
    };
    assert_eq!(c.claim.state, ClaimState::Paid);
    drop(good);
    drop(storage);
    let good = crate::webhook::Inbox {
        storage: Arc::new(SqliteStore::connect(&url).await.unwrap()),
        verifier: Arc::new(Verifier(Verified)),
        paypal: Arc::new(crate::adapters::MockPayPal::new(
            crate::adapters::MockPayoutFixture::Success,
        )),
    };
    assert_eq!(
        good.ingest(&raw, &paypal_headers()).await.unwrap(),
        "replay"
    );
}
#[tokio::test]
async fn webhook_route_limits_body_and_commit_failure_is_retryable_without_ledger_changes() {
    use crate::webhook::Verification::Verified;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}?mode=rwc", dir.path().join("fail.db").display());
    let (service, fake, task) = setup_store("SUCCESS", false, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let good = inbox(&service, Verified);
    let app = crate::webhook::router(good.clone());
    let before = snapshot(&service, id).await;
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    // Deferred FK violation occurs on COMMIT, after dedupe and ledger writes succeeded.
    sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE fail_parent(id TEXT PRIMARY KEY); CREATE TABLE fail_commit(id TEXT REFERENCES fail_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_reconcile AFTER INSERT ON sandbox_events BEGIN INSERT INTO fail_commit VALUES ('absent'); END;").execute(&pool).await.unwrap();
    let raw = event(&fake, "COMMIT", "SUCCESS");
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/webhooks/paypal/sandbox")
            .header("paypal-auth-algo", "fixture")
            .header("paypal-cert-url", "fixture")
            .header("paypal-transmission-id", "fixture")
            .header("paypal-transmission-sig", "fixture")
            .header("paypal-transmission-time", "fixture")
            .body(axum::body::Body::from(raw.clone()))
            .unwrap()
    };
    let response = app.clone().oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(snapshot(&service, id).await, before);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sandbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    service.storage.make_inbox_due().await.unwrap();
    let crate::domain::Output::Inbox(rows) = service
        .storage
        .execute(Command::InboxPending)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    sqlx::query("DROP TRIGGER fail_reconcile")
        .execute(&pool)
        .await
        .unwrap();
    good.storage.make_inbox_due().await.unwrap();
    good.retry().await.unwrap();
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::Paid
    );
    let paid = snapshot(&service, id).await;
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(snapshot(&service, id).await, paid);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/paypal/sandbox")
                .body(axum::body::Body::from(vec![0; 131_073]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(snapshot(&service, id).await, paid);
    sqlx::query("CREATE TRIGGER fail_inbox BEFORE INSERT ON webhook_inbox BEGIN SELECT RAISE(ABORT, 'injected'); END").execute(&pool).await.unwrap();
    assert_eq!(
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(snapshot(&service, id).await, paid);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn query_get_waits_for_age_and_never_sends_without_intent() {
    let (service, fake, task) = setup("FAILED", false, false).await;
    let id = approved(&service, 500).await;
    service.query_reserved(id, 1000, 5).await.unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    service.payout(id, 101).await.unwrap();
    let calls = fake.calls.load(Ordering::SeqCst);
    service.query_reserved(id, 200, 5).await.unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    assert_eq!(
        service
            .query_reserved(id, 1000, 5)
            .await
            .unwrap()
            .claim
            .state,
        ClaimState::PayoutFailed
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn failure_needs_query_proof_and_unavailable_query_retains_inbox() {
    use crate::webhook::Verification::Verified;
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = accounting_snapshot(&service, id).await;
    let good = inbox(&service, Verified);
    assert_eq!(
        good.ingest(&event(&fake, "FALSE-FAILURE", "FAILED"), &paypal_headers())
            .await
            .unwrap(),
        "mismatch"
    );
    assert_eq!(accounting_snapshot(&service, id).await, before);
    task.abort();
    assert!(good
        .ingest(
            &event(&fake, "UNAVAILABLE-FAILURE", "FAILED"),
            &paypal_headers()
        )
        .await
        .is_err());
    assert_eq!(accounting_snapshot(&service, id).await, before);
    service.storage.make_inbox_due().await.unwrap();
    let crate::domain::Output::Inbox(rows) = service
        .storage
        .execute(Command::InboxPending)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn webhook_before_create_ack_recovers_ids_through_details() {
    use crate::webhook::Verification::Verified;
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    let crate::domain::Output::Prepared { response, .. } = service
        .storage
        .execute(Command::PreparePayout {
            id,
            payout_id: PayoutId::new(),
            now: 101,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let payout = response.payout.unwrap();
    *fake.body.lock().unwrap() = request_body(&payout);
    let good = inbox(&service, Verified);
    assert_eq!(
        good.ingest(&event(&fake, "EARLY", "SUCCESS"), &paypal_headers())
            .await
            .unwrap(),
        "applied"
    );
    service
        .claim(Command::Observe {
            id,
            observation: Observation {
                state: PayoutState::PayoutPending,
                paypal_batch_id: Some("BATCH1".into()),
                item_id: None,
                binding: payout.binding.clone(),
                source: "sandbox-create".into(),
                status: "Pending".into(),
            },
        })
        .await
        .unwrap();
    good.storage.make_inbox_due().await.unwrap();
    good.retry().await.unwrap();
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn malformed_batch_unknown_and_wrong_event_type_never_change_the_ledger() {
    use crate::webhook::Verification::Verified;
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = accounting_snapshot(&service, id).await;
    let good = inbox(&service, Verified);
    let mut wrong: Value = serde_json::from_slice(&event(&fake, "WRONG-TYPE", "SUCCESS")).unwrap();
    wrong["event_type"] = "PAYMENT.PAYOUTSBATCH.SUCCESS".into();
    assert!(good
        .ingest(b"invalid json", &paypal_headers())
        .await
        .is_err());
    assert_eq!(accounting_snapshot(&service, id).await, before);
    let unknown = event(&fake, "UNKNOWN", "FUTURE-STATUS");
    assert_eq!(
        good.ingest(&unknown, &paypal_headers()).await.unwrap(),
        "mismatch"
    );
    assert_eq!(accounting_snapshot(&service, id).await, before);
    for raw in [b"{}".to_vec(), serde_json::to_vec(&wrong).unwrap(), serde_json::to_vec(&json!({"id":"BATCH-EVENT", "event_type":"PAYMENT.PAYOUTSBATCH.SUCCESS", "resource":{"payout_batch_id":"BATCH1"}})).unwrap()] {
        assert_eq!(good.ingest(&raw, &paypal_headers()).await.unwrap(), "ignored");
        assert_eq!(accounting_snapshot(&service, id).await, before);
    }
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
