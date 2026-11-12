// Shared fake transport exercises the production service and transactional stores.
async fn p6c2_release_case(url: &str, status: &str) {
    let (service, fake, task) =
        setup_store_with_timeout(status, false, false, url, Duration::from_secs(20)).await;
    let id = approved(&service, 500).await;
    let other = approved(&service, 100).await;
    service.payout(id, 101).await.unwrap();
    let before = service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap();
    service
        .claim(Command::Observe {
            id,
            observation: Observation {
                state: before.state,
                paypal_batch_id: before.paypal_batch_id.clone(),
                item_id: Some("ITEM1".into()),
                binding: None,
                source: "sandbox-item-query:test-mismatch".into(),
                status: "Mismatch".into(),
            },
        })
        .await
        .unwrap();
    service
        .storage
        .execute(Command::ResetDemo { now: 0 })
        .await
        .unwrap();
    service
        .storage
        .execute(Command::ResetDemo { now: 72000 })
        .await
        .unwrap();
    let crate::domain::Output::Claims(rows) = service
        .storage
        .execute(Command::Claims(EVENT_ID))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.claims.len(), 1);
    assert_eq!(rows.claims[0].claim.id, id);
    assert!(service
        .claim(Command::Get(other))
        .await
        .unwrap()
        .payout
        .is_none());
    let release = |key: &str| Command::Disposition {
        id,
        actor: "finance".into(),
        now: 72001,
        request: crate::domain::DispositionRequest {
            disposition_id: key.into(),
            reason: "verified release".into(),
            reserved_adjustment_cents: 0,
            paid_adjustment_cents: 0,
        },
    };
    service.claim(release("release-1")).await.unwrap();
    service.claim(release("release-1")).await.unwrap();
    assert!(service.claim(release("release-2")).await.is_err());
    let result = service.payout(id, 72002).await.unwrap();
    let expected = if status == "SUCCESS" {
        ClaimState::Paid
    } else {
        ClaimState::PayoutFailed
    };
    assert_eq!(result.claim.state, expected);
    assert_eq!(result.payout.unwrap().reconciliation.len(), 1);
    service.payout(id, 72003).await.unwrap();
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    let crate::domain::Output::Budgets(b) = service
        .storage
        .execute(Command::Budgets(EVENT_ID))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
    assert_eq!(
        b.budget_lines[0].paid_cents.get(),
        if status == "SUCCESS" { 500 } else { 0 }
    );
    let pool = sqlx::AnyPool::connect(url).await.unwrap();
    let audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM provider_evidence WHERE id LIKE 'disposition:%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audits, 1);
    task.abort();
}
async fn p6c2_refill_case(url: &str) {
    let (service, _, task) =
        setup_store_with_timeout("FAILED", false, false, url, Duration::from_secs(20)).await;
    service
        .storage
        .execute(Command::ResetDemo { now: 0 })
        .await
        .unwrap();
    let id = approved(&service, 1000).await;
    service.payout(id, 101).await.unwrap();
    service
        .storage
        .execute(Command::ResetDemo { now: 72000 })
        .await
        .unwrap();
    service.payout(id, 72001).await.unwrap();
    let reopened: Arc<dyn Storage> = if url.starts_with("sqlite:") {
        Arc::new(SqliteStore::connect(url).await.unwrap())
    } else {
        Arc::new(crate::storage::PostgresStore::connect(url).await.unwrap())
    };
    let (a, b) = tokio::join!(
        service.storage.execute(Command::ResetDemo { now: 158400 }),
        reopened.execute(Command::ResetDemo { now: 158400 })
    );
    a.unwrap();
    b.unwrap();
    let crate::domain::Output::Budgets(b) =
        reopened.execute(Command::Budgets(EVENT_ID)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(
        b.budget_lines[0].approved_cents.get()
            - b.budget_lines[0].reserved_cents.get()
            - b.budget_lines[0].paid_cents.get(),
        10000
    );
    let pool = sqlx::AnyPool::connect(url).await.unwrap();
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT payload FROM provider_evidence WHERE id LIKE 'budget-refill:%'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| {
        let v: Value = serde_json::from_str(row).unwrap();
        v[1]["approved_before"] == 11000 && v[1]["approved_after"] == 10000
    }));
    task.abort();
}
#[tokio::test]
async fn p6c2_sqlite_release_success_and_failure() {
    for status in ["SUCCESS", "FAILED"] {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("release.db").display()
        );
        p6c2_release_case(&url, status).await;
    }
}
#[tokio::test]
async fn p6c2_sqlite_refill_over_cap_restart_and_instances() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("refill.db").display()
    );
    p6c2_refill_case(&url).await;
}
async fn p6c2_postgres_case(release: bool) {
    use sqlx::Connection;
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return;
    };
    let mut admin = sqlx::PgConnection::connect(&url)
        .await
        .unwrap_or_else(|_| panic!("Postgres connection failed"));
    for status in if release {
        vec!["SUCCESS", "FAILED"]
    } else {
        vec!["FAILED"]
    } {
        let schema = format!("p6c2_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let separator = if url.contains('?') { '&' } else { '?' };
        let scoped = format!("{url}{separator}options=-csearch_path%3D{schema}");
        if release {
            p6c2_release_case(&scoped, status).await;
        } else {
            p6c2_refill_case(&scoped).await;
        }
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&mut admin)
            .await
            .unwrap();
    }
}
#[tokio::test]
async fn p6c2_postgres_release_success_and_failure() {
    p6c2_postgres_case(true).await;
}
#[tokio::test]
async fn p6c2_postgres_refill_over_cap_restart_and_instances() {
    p6c2_postgres_case(false).await;
}
#[tokio::test]
async fn p6c2_query_mismatch_and_adjustments_freeze_only_target_without_accounting() {
    for status in ["SUCCESS", "REFUNDED", "REVERSED", "NEW_STATUS"] {
        for field in if status == "SUCCESS" {
            vec!["receiver", "amount"]
        } else {
            vec![""]
        } {
            let (service, fake, task) = setup(status, false, false).await;
            let id = approved(&service, 500).await;
            let other = approved(&service, 100).await;
            service.payout(id, 101).await.unwrap();
            let crate::domain::Output::Budgets(before) = service
                .storage
                .execute(Command::Budgets(EVENT_ID))
                .await
                .unwrap()
            else {
                panic!()
            };
            if !field.is_empty() {
                fake.body.lock().unwrap()["items"][0][field] = json!("mismatch");
            }
            let result = service.payout(id, 102).await;
            assert_eq!(result.is_err(), !field.is_empty());
            let held = service.claim(Command::Get(id)).await.unwrap();
            assert_eq!(held.claim.state, ClaimState::PayoutPending);
            assert_eq!(held.payout.unwrap().reconciliation.len(), 1);
            let calls = fake.calls.load(Ordering::SeqCst);
            service.payout(id, 103).await.unwrap();
            assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
            let crate::domain::Output::Budgets(after) = service
                .storage
                .execute(Command::Budgets(EVENT_ID))
                .await
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(
                serde_json::to_value(before).unwrap(),
                serde_json::to_value(after).unwrap()
            );
            // Another claim still passes the payout preparation gate.
            let crate::domain::Output::Prepared { action, .. } = service
                .storage
                .execute(Command::PreparePayout {
                    id: other,
                    payout_id: PayoutId::new(),
                    now: 103,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(action, crate::domain::PayoutAction::SendNew);
            task.abort();
        }
    }
}

#[tokio::test]
async fn p6c2_disposition_settles_exact_claim_and_releases_new_holds() {
    let (service, _, task) = setup("SUCCESS", false, false).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let hold = || Command::Observe {
        id,
        observation: Observation {
            state: PayoutState::PayoutPending,
            paypal_batch_id: Some("BATCH1".into()),
            item_id: Some("ITEM1".into()),
            binding: None,
            source: "sandbox-item-query:test-mismatch".into(),
            status: "Mismatch".into(),
        },
    };
    let dispose = |key: &str, reserved, paid| Command::Disposition {
        id,
        actor: "finance".into(),
        now: 102,
        request: crate::domain::DispositionRequest {
            disposition_id: key.into(),
            reason: "reviewed".into(),
            reserved_adjustment_cents: reserved,
            paid_adjustment_cents: paid,
        },
    };
    service.claim(hold()).await.unwrap();
    assert!(service.claim(dispose("bad", -1000, 1000)).await.is_err());
    service.claim(dispose("release", 0, 0)).await.unwrap();
    service.claim(hold()).await.unwrap();
    let held = service.claim(Command::Get(id)).await.unwrap();
    assert_eq!(held.payout.unwrap().reconciliation.len(), 2);
    let settled = service.claim(dispose("settle", -500, 500)).await.unwrap();
    assert_eq!(settled.claim.state, ClaimState::Paid);
    service.claim(dispose("settle", -500, 500)).await.unwrap();
    let mut changed = dispose("settle", -500, 500);
    if let Command::Disposition { request, .. } = &mut changed {
        request.reason = "changed".into();
    }
    assert!(service.claim(changed).await.is_err());
    service.payout(id, 103).await.unwrap();
    let crate::domain::Output::Budgets(b) = service
        .storage
        .execute(Command::Budgets(EVENT_ID))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
    assert_eq!(b.budget_lines[0].paid_cents.get(), 500);
    task.abort();
}

#[tokio::test]
async fn p6c2_query_dependent_webhooks_persist_mismatch_holds() {
    for recover in [false, true] {
        let status = if recover { "SUCCESS" } else { "FAILED" };
        let (service, fake, task) = setup(status, recover, false).await;
        let id = approved(&service, 500).await;
        let initial = service.payout(id, 101).await.unwrap().claim.state;
        let raw = event(&fake, "QUERY-MISMATCH", status);
        fake.body.lock().unwrap()["items"][0]["receiver"] = json!("wrong@example.invalid");
        inbox(&service, crate::webhook::Verification::Verified)
            .ingest(&raw, &paypal_headers())
            .await
            .unwrap();
        let held = service.claim(Command::Get(id)).await.unwrap();
        assert_eq!(held.claim.state, initial);
        assert!(!held.payout.unwrap().reconciliation.is_empty());
        let crate::domain::Output::Budgets(b) = service
            .storage
            .execute(Command::Budgets(EVENT_ID))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(b.budget_lines[0].reserved_cents.get(), 500);
        assert_eq!(b.budget_lines[0].paid_cents.get(), 0);
        assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
        task.abort();
    }
}

#[tokio::test]
async fn p6c2_query_hold_and_evidence_commit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("atomic.db").display()
    );
    let (service, fake, task) = setup_store("SUCCESS", false, false, &url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    fake.body.lock().unwrap()["items"][0]["receiver"] = json!("wrong@example.invalid");
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_query_audit BEFORE INSERT ON provider_evidence BEGIN SELECT RAISE(ABORT, 'injected failure'); END")
        .execute(&pool).await.unwrap();
    assert_eq!(
        service.payout(id, 102).await.unwrap_err().code,
        ErrorCode::StorageUnavailable
    );
    assert_eq!(snapshot(&service, id).await, before);
    sqlx::query("DROP TRIGGER fail_query_audit")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        service.payout(id, 103).await.unwrap_err().code,
        ErrorCode::WebhookMismatch
    );
    assert_eq!(
        service
            .claim(Command::Get(id))
            .await
            .unwrap()
            .payout
            .unwrap()
            .reconciliation
            .len(),
        1
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}

// Exercise the real sandbox parser over loopback HTTP through each service caller.
async fn merge_query_case(url: &str, status: &str, field: &str, path: &str) {
    if field.starts_with("absent:") {
        return merge_fault_case(url, path, field).await;
    }
    let (service, fake, task) = setup_store_with_timeout(
        status,
        path == "recovery",
        false,
        url,
        Duration::from_secs(20),
    )
    .await;
    let id = approved(&service, 500).await;
    let other = approved(&service, 100).await;
    service.payout(id, 101).await.unwrap();
    let before = accounting_snapshot(&service, id).await;
    let raw = event(&fake, "MERGE-QUERY", "FAILED");
    if field.starts_with("wrong:") {
        provider_response(&fake, field);
    } else if field == "sender_batch_id" {
        fake.body.lock().unwrap()["sender_batch_header"][field] = json!("wrong");
    } else if !field.is_empty() {
        fake.body.lock().unwrap()["items"][0][field] = json!("wrong");
    }
    match path {
        "payout" => {
            let _ = service.payout(id, 102).await;
        }
        "reserved" => {
            service.query_reserved(id, 1000, 5).await.unwrap();
        }
        "webhook" | "recovery" => {
            assert_eq!(
                inbox(&service, crate::webhook::Verification::Verified)
                    .ingest(&raw, &paypal_headers())
                    .await
                    .unwrap(),
                "mismatch"
            );
        }
        _ => unreachable!(),
    }
    assert_eq!(accounting_snapshot(&service, id).await, before);
    let held = service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap();
    assert_eq!(held.reconciliation.len(), 1);
    let calls = fake.calls.load(Ordering::SeqCst);
    service.payout(id, 103).await.unwrap();
    assert_eq!(fake.calls.load(Ordering::SeqCst), calls);
    let crate::domain::Output::Prepared { action, .. } = service
        .storage
        .execute(Command::PreparePayout {
            id: other,
            payout_id: PayoutId::new(),
            now: 103,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(action, crate::domain::PayoutAction::SendNew);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
// Deterministic fake provider responses. "absent:*" removes or empties data (retryable, no hold);
// "wrong:*" supplies a present, different fact (hold).
fn provider_response(fake: &Fake, kind: &str) {
    let mut batch = batch_json(fake);
    let mut item = item_json(fake);
    let (in_batch, in_item) = (&mut batch, &mut item);
    match kind {
        "absent:items_missing" => {
            in_batch.as_object_mut().unwrap().remove("items");
        }
        "absent:items_empty" => in_batch["items"] = json!([]),
        "absent:items_empty_total_zero" => {
            in_batch["items"] = json!([]);
            in_batch["total_items"] = json!(0);
        }
        "absent:batch_empty_object" => *in_batch = json!({}),
        "absent:batch_id" => {
            in_batch["batch_header"]
                .as_object_mut()
                .unwrap()
                .remove("payout_batch_id");
        }
        "absent:sender_batch_id" => {
            in_batch["batch_header"]["sender_batch_header"]
                .as_object_mut()
                .unwrap()
                .remove("sender_batch_id");
        }
        "absent:item_id" => {
            in_batch["items"][0]
                .as_object_mut()
                .unwrap()
                .remove("payout_item_id");
        }
        "absent:item_empty_object" => *in_item = json!({}),
        "absent:item_payout_item" => {
            in_item.as_object_mut().unwrap().remove("payout_item");
        }
        "absent:item_payout_item_id" => {
            in_item.as_object_mut().unwrap().remove("payout_item_id");
        }
        "absent:receiver" | "absent:recipient_type" | "absent:sender_item_id" | "absent:amount" => {
            let field = kind.strip_prefix("absent:").unwrap();
            in_item["payout_item"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        "absent:amount_value" | "absent:currency" => {
            let field = if kind == "absent:currency" {
                "currency"
            } else {
                "value"
            };
            in_item["payout_item"]["amount"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        "absent:status" => {
            in_item.as_object_mut().unwrap().remove("transaction_status");
        }
        "wrong:currency" => in_item["payout_item"]["amount"]["currency"] = json!("EUR"),
        "wrong:amount_value" => in_item["payout_item"]["amount"]["value"] = json!("0.01"),
        "wrong:empty_receiver" => in_item["payout_item"]["receiver"] = json!(""),
        "wrong:total_items" => in_batch["total_items"] = json!(2),
        "wrong:two_items" => {
            let first = in_batch["items"][0].clone();
            in_batch["items"].as_array_mut().unwrap().push(first);
        }
        "wrong:item_id" => in_batch["items"][0]["payout_item_id"] = json!("bad id!"),
        "wrong:batch_id" => in_batch["batch_header"]["payout_batch_id"] = json!("OTHER"),
        _ => unreachable!("{kind}"),
    }
    *fake.batch_override.lock().unwrap() = Some(batch);
    *fake.item_override.lock().unwrap() = Some(item);
}
fn provider_response_clear(fake: &Fake) {
    *fake.batch_override.lock().unwrap() = None;
    *fake.item_override.lock().unwrap() = None;
}
const MERGE_FACTS: [(&str, &str); 32] = [
    ("SUCCESS", "receiver"),
    ("SUCCESS", "amount"),
    ("SUCCESS", "sender_item_id"),
    ("SUCCESS", "recipient_type"),
    ("SUCCESS", "sender_batch_id"),
    ("SUCCESS", "wrong:currency"),
    ("SUCCESS", "wrong:amount_value"),
    ("SUCCESS", "wrong:empty_receiver"),
    ("SUCCESS", "wrong:total_items"),
    ("SUCCESS", "wrong:two_items"),
    ("SUCCESS", "wrong:item_id"),
    ("SUCCESS", "wrong:batch_id"),
    ("REFUNDED", ""),
    ("REVERSED", ""),
    ("UNRECOGNIZED", ""),
    ("SUCCESS", "absent:items_missing"),
    ("SUCCESS", "absent:items_empty"),
    ("SUCCESS", "absent:items_empty_total_zero"),
    ("SUCCESS", "absent:batch_empty_object"),
    ("SUCCESS", "absent:batch_id"),
    ("SUCCESS", "absent:sender_batch_id"),
    ("SUCCESS", "absent:item_id"),
    ("SUCCESS", "absent:item_empty_object"),
    ("SUCCESS", "absent:item_payout_item"),
    ("SUCCESS", "absent:item_payout_item_id"),
    ("SUCCESS", "absent:receiver"),
    ("SUCCESS", "absent:recipient_type"),
    ("SUCCESS", "absent:sender_item_id"),
    ("SUCCESS", "absent:amount"),
    ("SUCCESS", "absent:amount_value"),
    ("SUCCESS", "absent:currency"),
    ("SUCCESS", "absent:status"),
];
#[tokio::test]
async fn merge_sqlite_query_seams() {
    for (status, field) in MERGE_FACTS {
        for path in ["payout", "reserved", "webhook", "recovery"] {
            merge_query_case("sqlite::memory:", status, field, path).await;
        }
    }
}

async fn merge_transport_case(url: &str, path: &str) {
    merge_fault_case(url, path, "unavailable").await
}
// A fault leaves the healthy claim unfrozen and retryable; a later valid success settles once.
async fn merge_fault_case(url: &str, path: &str, fault: &str) {
    let (service, fake, task) = setup_store_with_timeout(
        "SUCCESS",
        path == "recovery",
        false,
        url,
        Duration::from_secs(20),
    )
    .await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    let raw = event(
        &fake,
        "TRANSPORT",
        if path == "recovery" {
            "SUCCESS"
        } else {
            "FAILED"
        },
    );
    if fault == "unavailable" {
        fake.query_unavailable.store(true, Ordering::SeqCst);
    } else {
        provider_response(&fake, fault);
    }
    let calls = fake.calls.load(Ordering::SeqCst);
    let result = match path {
        "payout" => service.payout(id, 102).await.map(|_| ()),
        "reserved" => service.query_reserved(id, 1000, 5).await.map(|_| ()),
        _ => inbox(&service, crate::webhook::Verification::Verified)
            .ingest(&raw, &paypal_headers())
            .await
            .map(|_| ()),
    };
    assert_eq!(result.unwrap_err().code, ErrorCode::AdapterUnavailable, "{fault}");
    assert!(fake.calls.load(Ordering::SeqCst) > calls, "{fault} reached provider");
    assert_eq!(snapshot(&service, id).await, before, "{fault}");
    assert!(service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap()
        .reconciliation
        .is_empty());
    fake.query_unavailable.store(false, Ordering::SeqCst);
    provider_response_clear(&fake);
    // A subsequent query succeeds; no transport error installed a hold.
    if path == "recovery" {
        service.storage.make_inbox_due().await.unwrap();
        inbox(&service, crate::webhook::Verification::Verified)
            .retry()
            .await
            .unwrap();
    } else {
        service.query_reserved(id, 1000, 5).await.unwrap();
    }
    assert_eq!(
        service.claim(Command::Get(id)).await.unwrap().claim.state,
        ClaimState::Paid
    );
    assert!(service
        .claim(Command::Get(id))
        .await
        .unwrap()
        .payout
        .unwrap()
        .reconciliation
        .is_empty());
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    task.abort();
}
#[tokio::test]
async fn merge_sqlite_transport_stays_retryable() {
    for path in ["payout", "reserved", "webhook", "recovery"] {
        merge_transport_case("sqlite::memory:", path).await;
    }
}
