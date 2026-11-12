#[tokio::test]
async fn missing_batch_recovery_requires_verification_and_details_and_never_resends() {
    use crate::webhook::Verification::*;
    let (service, fake, task) = setup("SUCCESS", true, false).await;
    let id = approved(&service, 500).await;
    assert_eq!(
        service.payout(id, 101).await.unwrap().claim.state,
        ClaimState::PayoutUnknown
    );
    let raw = event(&fake, "RECOVER", "SUCCESS");
    let calls = fake.calls.load(Ordering::SeqCst);
    assert!(inbox(&service, Rejected)
        .ingest(&raw, &paypal_headers())
        .await
        .is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    // Even verified non-item notifications cannot initiate batch-ID recovery.
    for event_type in ["PAYMENT.PAYOUTSBATCH.SUCCESS", "UNRELATED.SUCCESS"] {
        let mut non_item: Value = serde_json::from_slice(&raw).unwrap();
        non_item["id"] = event_type.into();
        non_item["event_type"] = event_type.into();
        let status = inbox(&service, Verified)
            .ingest(&serde_json::to_vec(&non_item).unwrap(), &paypal_headers())
            .await
            .unwrap();
        assert_eq!(status, "ignored");
        assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
        assert!(service
            .claim(Command::Get(id))
            .await
            .unwrap()
            .payout
            .unwrap()
            .paypal_batch_id
            .is_none());
    }
    let mut bad: Value = serde_json::from_slice(&raw).unwrap();
    bad["id"] = "BAD-BATCH".into();
    bad["resource"]["payout_batch_id"] = "UNRELATED".into();
    let good = inbox(&service, Verified);
    assert!(good
        .ingest(&serde_json::to_vec(&bad).unwrap(), &paypal_headers())
        .await
        .is_err());
    let unknown = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(unknown.claim.state, ClaimState::PayoutUnknown);
    assert!(unknown.payout.unwrap().paypal_batch_id.is_none());
    assert_eq!(
        good.ingest(&raw, &paypal_headers()).await.unwrap(),
        "applied"
    );
    let paid = service.payout(id, 102).await.unwrap();
    assert_eq!(paid.claim.state, ClaimState::Paid);
    assert_eq!(
        paid.payout.unwrap().paypal_batch_id.as_deref(),
        Some("BATCH1")
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn recovery_commit_failure_keeps_dedupe_and_ids_retryable_after_reopen() {
    use crate::webhook::Verification::Verified;
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("recovery.db").display()
    );
    let (service, fake, task) = setup_store("SUCCESS", true, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE fail_parent(id TEXT PRIMARY KEY); CREATE TABLE fail_commit(id TEXT REFERENCES fail_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_recovery AFTER INSERT ON sandbox_events BEGIN INSERT INTO fail_commit VALUES ('absent'); END;").execute(&pool).await.unwrap();
    let before = snapshot(&service, id).await;
    let raw = event(&fake, "RECOVERY-COMMIT", "SUCCESS");
    assert!(inbox(&service, Verified)
        .ingest(&raw, &paypal_headers())
        .await
        .is_err());
    assert_eq!(snapshot(&service, id).await, before);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sandbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_recovery")
        .execute(&pool)
        .await
        .unwrap();
    let reopened = crate::webhook::Inbox {
        storage: Arc::new(SqliteStore::connect(&url).await.unwrap()),
        verifier: Arc::new(Verifier(Verified)),
        paypal: service.paypal.clone(),
    };
    reopened.storage.make_inbox_due().await.unwrap();
    reopened.retry().await.unwrap();
    let crate::domain::Output::Claim(result) =
        reopened.storage.execute(Command::Get(id)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(result.claim.state, ClaimState::Paid);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}

struct FailObserve {
    inner: Arc<dyn Storage>,
    failures: AtomicUsize,
}
#[async_trait]
impl Storage for FailObserve {
    async fn make_inbox_due(&self) -> Result<(), ApiError> {
        self.inner.make_inbox_due().await
    }

    async fn execute(&self, command: Command) -> Result<crate::domain::Output, ApiError> {
        if matches!(command, Command::Observe { .. })
            && self
                .failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
        {
            return Err(ApiError::new(
                ErrorCode::StorageUnavailable,
                "injected commit failure",
            ));
        }
        self.inner.execute(command).await
    }
}
#[tokio::test]
async fn observe_persistence_retry_does_not_repeat_send_or_query() {
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    let storage = Arc::new(FailObserve {
        inner: service.storage.clone(),
        failures: AtomicUsize::new(2),
    });
    let retrying = Service {
        storage: storage.clone(),
        paypal: service.paypal.clone(),
        extractor: Arc::new(MockLlm),
        finance_token: "test-finance".into(),
    };
    assert_eq!(
        retrying.payout(id, 101).await.unwrap().claim.state,
        ClaimState::PayoutPending
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    storage.failures.store(2, Ordering::SeqCst);
    assert_eq!(
        retrying.payout(id, 102).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 5);
    task.abort();
}

#[tokio::test]
async fn queried_adjustments_and_unknown_statuses_are_reconciliation_holds() {
    for (raw, normalized) in [
        ("REFUNDED", "Refunded"),
        ("REVERSED", "Reversed"),
        ("NEW_STATUS", "Unknown"),
    ] {
        let (service, fake, task) = setup(raw, false, false).await;
        let id = approved(&service, 500).await;
        service.payout(id, 101).await.unwrap();
        let result = service.payout(id, 102).await.unwrap();
        let p = result.payout.unwrap();
        assert_eq!(p.state, PayoutState::PayoutPending);
        assert_eq!(p.reconciliation.len(), 1);
        assert_eq!(p.reconciliation[0].status, normalized);
        let before = fake.calls.load(Ordering::SeqCst);
        service.payout(id, 103).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), before);
        assert_eq!(
            snapshot(&service, id).await["budget"]["budget_lines"][0]["reserved_cents"],
            500
        );
        task.abort();
    }
}

#[tokio::test]
async fn held_canceled_and_denied_event_names_match_provider_statuses() {
    for (raw, suffix, state) in [
        ("ONHOLD", "HELD", ClaimState::PayoutPending),
        ("CANCELED", "CANCELED", ClaimState::PayoutFailed),
        ("DENIED", "DENIED", ClaimState::PayoutFailed),
    ] {
        let (service, fake, task) = setup(raw, false, false).await;
        let id = approved(&service, 500).await;
        service.payout(id, 101).await.unwrap();
        let mut value: Value = serde_json::from_slice(&event(&fake, "EVENT-NAME", raw)).unwrap();
        value["event_type"] = format!("PAYMENT.PAYOUTS-ITEM.{suffix}").into();
        assert_eq!(
            inbox(&service, crate::webhook::Verification::Verified)
                .ingest(&serde_json::to_vec(&value).unwrap(), &paypal_headers())
                .await
                .unwrap(),
            "applied"
        );
        assert_eq!(
            service.claim(Command::Get(id)).await.unwrap().claim.state,
            state
        );
        task.abort();
    }
}

#[tokio::test]
async fn approval_receiver_snapshot_survives_configuration_change_before_send() {
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    service
        .storage
        .execute(Command::ConfigureRecipient(
            "replacement@example.invalid".into(),
        ))
        .await
        .unwrap();
    let result = service.payout(id, 101).await.unwrap();
    assert_eq!(result.approval.unwrap().receiver, "fixed@example.invalid");
    assert_eq!(
        fake.body.lock().unwrap()["items"][0]["receiver"],
        "fixed@example.invalid"
    );
    task.abort();
}

#[tokio::test]
async fn oauth_failure_before_payout_request_is_not_sent() {
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    let app = Router::new().route(
        "/v1/oauth2/token",
        post(|| async { StatusCode::UNAUTHORIZED }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let oauth_task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let failing = Service {
        storage: service.storage.clone(),
        paypal: Arc::new(
            SandboxPayPal::transport("fake".into(), "fake".into(), origin, Duration::from_secs(1))
                .unwrap(),
        ),
        extractor: Arc::new(MockLlm),
        finance_token: "test-finance".into(),
    };
    assert!(failing.payout(id, 101).await.is_err());
    let result = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(result.claim.state, ClaimState::PayoutPending);
    assert_eq!(result.payout.unwrap().reconciliation[0].status, "NotSent");
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    oauth_task.abort();
    task.abort();
}

#[tokio::test]
async fn submit_above_ten_dollars_has_no_reservation_or_provider_calls() {
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = ClaimId::new();
    service
        .claim(Command::Create {
            id,
            request: CreateClaimRequest {
                event_id: EVENT_ID,
                budget_line_id: BUDGET_ID,
                claimant_id: CLAIMANT_ID,
                receipt_text: "over cap".into(),
            },
        })
        .await
        .unwrap();
    let before = snapshot(&service, id).await;
    let error = service
        .claim(Command::Submit {
            id,
            request: SubmitClaimRequest {
                fields: ReceiptFields {
                    merchant: Some("Shop".into()),
                    date: Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()),
                    total_cents: Some(Cents::try_from(1001).unwrap()),
                    currency: Some(Currency::USD),
                    uncertain: vec![],
                },
                items: vec![ReceiptItem {
                    quantity: 1,
                    unit_cents: Cents::try_from(1001).unwrap(),
                }],
            },
        })
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidInput);
    assert_eq!(snapshot(&service, id).await, before);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn finance_disposition_is_authorized_append_only_idempotent_and_claim_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("disposition.db").display()
    );
    let (service, fake, task) = setup_store("REFUNDED", false, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    service.payout(id, 102).await.unwrap();
    let original = service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap();
    // An unrelated claim in the same budget is still approvable and payable.
    let other = approved(&service, 100).await;
    service.payout(other, 103).await.unwrap();
    let request = json!({"disposition_id":"finance-1", "reason":"verified adjustment",
        "reserved_adjustment_cents":-500,"paid_adjustment_cents":500});
    let app = crate::http::router(service.clone());
    let send = |token: String, body: Value| {
        Request::builder()
            .method("POST")
            .uri(format!("/api/claims/{id}/disposition"))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .unwrap()
    };
    let before = snapshot(&service, id).await;
    for token in [
        "".to_string(),
        crate::http::claimant_token("test-finance", CLAIMANT_ID),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(send(token, request.clone()))
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut invalid = request.clone();
    invalid["reserved_adjustment_cents"] = (-5000).into();
    assert_eq!(
        app.clone()
            .oneshot(send("test-finance".into(), invalid))
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(snapshot(&service, id).await, before);
    assert_eq!(
        app.clone()
            .oneshot(send("test-finance".into(), request.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let after = snapshot(&service, id).await;
    assert_eq!(after["budget"]["budget_lines"][0]["reserved_cents"], 100);
    assert_eq!(after["budget"]["budget_lines"][0]["paid_cents"], 500);
    assert_eq!(
        service
            .claim(Command::Get(id))
            .await
            .unwrap()
            .payout
            .unwrap()
            .reconciliation,
        original.reconciliation
    );
    assert_eq!(
        app.clone()
            .oneshot(send("test-finance".into(), request.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(snapshot(&service, id).await, after);
    let mut changed = request;
    changed["reason"] = "changed reason".into();
    assert_eq!(
        app.oneshot(send("test-finance".into(), changed))
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let audit: String = sqlx::query_scalar("SELECT payload FROM provider_evidence WHERE id = ?")
        .bind(format!("disposition:{id}:finance-1"))
        .fetch_one(&pool)
        .await
        .unwrap();
    let row: (String, Value) = serde_json::from_str(&audit).unwrap();
    assert_eq!(row.1["facts"]["actor"], "demo-finance");
    assert_eq!(row.1["facts"]["request"]["reason"], "verified adjustment");
    let reopened = SqliteStore::connect(&url).await.unwrap();
    let crate::domain::Output::Claim(result) = reopened.execute(Command::Get(id)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        result.payout.unwrap().reconciliation,
        original.reconciliation
    );
    // A new verified contradictory fact creates a new hold; the old disposition remains intact.
    let p = service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap();
    let observation = Observation {
        state: p.state,
        status: "Reversed".into(),
        binding: p.binding,
        paypal_batch_id: p.paypal_batch_id,
        item_id: p.item_id,
        source: "sandbox-item-query:REVERSED".into(),
    };
    service
        .claim(Command::Observe { id, observation })
        .await
        .unwrap();
    let crate::domain::Output::Prepared { action, .. } = service
        .storage
        .execute(Command::PreparePayout {
            id,
            payout_id: PayoutId::new(),
            now: 104,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(action, crate::domain::PayoutAction::Complete);
    let audit_after: String =
        sqlx::query_scalar("SELECT payload FROM provider_evidence WHERE id = ?")
            .bind(format!("disposition:{id}:finance-1"))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit, audit_after);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 2);
    task.abort();
}

#[tokio::test]
async fn daily_refill_at_0400_is_durable_audited_and_idempotent_across_pools() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("refill.db").display()
    );
    let (service, fake, task) = setup_store("SUCCESS", false, false, &url).await;
    service
        .storage
        .execute(Command::ResetDemo { now: 0 })
        .await
        .unwrap();
    let paid = approved(&service, 500).await;
    service.payout(paid, 101).await.unwrap();
    service.payout(paid, 102).await.unwrap();
    let pending = approved(&service, 600).await;
    service.payout(pending, 103).await.unwrap();
    let unsent = approved(&service, 700).await;
    service
        .storage
        .execute(Command::ResetDemo { now: 20 * 3600 - 1 })
        .await
        .unwrap();
    assert_eq!(
        snapshot(&service, paid).await["budget"]["budget_lines"][0]["approved_cents"],
        10000
    );
    let second = SqliteStore::connect(&url).await.unwrap();
    let (a, b) = tokio::join!(
        service
            .storage
            .execute(Command::ResetDemo { now: 20 * 3600 }),
        second.execute(Command::ResetDemo { now: 20 * 3600 })
    );
    a.unwrap();
    b.unwrap();
    assert!(service.payout(unsent, 20 * 3600).await.is_err());
    let totals = snapshot(&service, paid).await["budget"]["budget_lines"][0].clone();
    assert_eq!(totals["approved_cents"], 11100);
    assert_eq!(totals["reserved_cents"], 600);
    assert_eq!(totals["paid_cents"], 500);
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_evidence WHERE id LIKE 'budget-refill:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    let reopened = SqliteStore::connect(&url).await.unwrap();
    reopened
        .execute(Command::ResetDemo { now: 20 * 3600 })
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_evidence WHERE id LIKE 'budget-refill:%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 2);
    task.abort();
}

struct NoQuotaAi(AtomicUsize);
#[async_trait]
impl crate::adapters::ReceiptExtractor for NoQuotaAi {
    fn ai_enabled(&self) -> bool {
        true
    }
    async fn extract(&self, _: &str) -> ReceiptFields {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("quota exhausted: provider must not be called")
    }
}
#[tokio::test]
async fn extraction_quota_fallback_preserves_all_existing_fields() {
    let (service, _, task) = setup("SUCCESS", false, false).await;
    let id = ClaimId::new();
    service
        .claim(Command::Create {
            id,
            request: CreateClaimRequest {
                event_id: EVENT_ID,
                claimant_id: CLAIMANT_ID,
                budget_line_id: BUDGET_ID,
                receipt_text: "edited receipt".into(),
            },
        })
        .await
        .unwrap();
    let fields = ReceiptFields {
        merchant: Some("Edited merchant".into()),
        date: Some(chrono::NaiveDate::from_ymd_opt(2026, 11, 5).unwrap()),
        total_cents: Some(Cents::try_from(123).unwrap()),
        currency: Some(Currency::USD),
        uncertain: vec!["date".into()],
    };
    service
        .claim(Command::Extract {
            id,
            fields: fields.clone(),
        })
        .await
        .unwrap();
    for _ in 0..10 {
        service
            .storage
            .execute(Command::ExtractionQuota {
                account: CLAIMANT_ID.to_string(),
                now: chrono::Utc::now().timestamp(),
            })
            .await
            .unwrap();
    }
    let extractor = Arc::new(NoQuotaAi(AtomicUsize::new(0)));
    let exhausted = Service {
        storage: service.storage.clone(),
        paypal: service.paypal.clone(),
        extractor: extractor.clone(),
        finance_token: "test-finance".into(),
    };
    assert_eq!(exhausted.extract(id).await.unwrap().claim.fields, fields);
    assert_eq!(extractor.0.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn verified_mismatch_record_and_inbox_dedupe_commit_together() {
    use crate::webhook::Verification::Verified;
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("mismatch.db").display()
    );
    let (service, fake, task) = setup_store("PROCESSING", false, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::raw_sql("PRAGMA foreign_keys=ON; CREATE TABLE fail_parent(id TEXT PRIMARY KEY); CREATE TABLE fail_commit(id TEXT REFERENCES fail_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER fail_mismatch AFTER INSERT ON sandbox_events BEGIN INSERT INTO fail_commit VALUES ('absent'); END;").execute(&pool).await.unwrap();
    let mut raw: Value =
        serde_json::from_slice(&event(&fake, "MISMATCH-COMMIT", "SUCCESS")).unwrap();
    raw["resource"]["payout_item"]["receiver"] = "wrong@example.invalid".into();
    let raw = serde_json::to_vec(&raw).unwrap();
    let good = inbox(&service, Verified);
    assert!(good.ingest(&raw, &paypal_headers()).await.is_err());
    assert_eq!(snapshot(&service, id).await, before);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sandbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_mismatch")
        .execute(&pool)
        .await
        .unwrap();
    service.storage.make_inbox_due().await.unwrap();
    good.retry().await.unwrap();
    let result = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(result.payout.unwrap().reconciliation.len(), 1);
    assert_eq!(accounting_snapshot(&service, id).await, before);
    let status: String =
        sqlx::query_scalar("SELECT json_extract(payload, '$.status') FROM webhook_inbox")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "mismatch");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sandbox_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let reopened = SqliteStore::connect(&url).await.unwrap();
    let crate::domain::Output::Claim(result) = reopened.execute(Command::Get(id)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(result.payout.unwrap().reconciliation.len(), 1);
    task.abort();
}

#[tokio::test]
async fn exhausted_observe_retries_recover_from_verified_event_without_resend() {
    let (service, fake, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    let failing = Service {
        storage: Arc::new(FailObserve {
            inner: service.storage.clone(),
            failures: AtomicUsize::new(3),
        }),
        paypal: service.paypal.clone(),
        extractor: Arc::new(MockLlm),
        finance_token: "test-finance".into(),
    };
    assert_eq!(
        failing.payout(id, 101).await.unwrap_err().code,
        ErrorCode::StorageUnavailable
    );
    assert!(service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap()
        .paypal_batch_id
        .is_none());
    assert!(service.payout(id, 102).await.is_err());
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    let raw = event(&fake, "AFTER-OBSERVE-FAILURE", "SUCCESS");
    let bad = inbox(&service, crate::webhook::Verification::Unavailable);
    assert!(bad.ingest(&raw, &paypal_headers()).await.is_err());
    service.storage.make_inbox_due().await.unwrap();
    assert!(bad.retry().await.is_err());
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
    inbox(&service, crate::webhook::Verification::Verified)
        .retry()
        .await
        .unwrap();
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
