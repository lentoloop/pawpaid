use super::*;
use crate::storage::PostgresStore;
use sqlx::{Connection, Row};

struct Database {
    url: String,
    schema: String,
    admin: sqlx::PgConnection,
}
impl Database {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("SKIP Postgres integration: TEST_DATABASE_URL is unset");
            return None;
        };
        let mut admin = sqlx::PgConnection::connect(&url)
            .await
            .unwrap_or_else(|_| panic!("TEST_DATABASE_URL connection failed"));
        let version: String = sqlx::query_scalar("SHOW server_version")
            .fetch_one(&mut admin)
            .await
            .unwrap();
        assert!(version.starts_with("17."), "Postgres 17 required");
        let schema = format!("p5_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut admin)
            .await
            .unwrap();
        let separator = if url.contains('?') { '&' } else { '?' };
        let url = format!("{url}{separator}options=-csearch_path%3D{schema}");
        Some(Self { url, schema, admin })
    }
    async fn finish(mut self) {
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }
    async fn pool(&self) -> sqlx::AnyPool {
        sqlx::any::install_default_drivers();
        sqlx::AnyPool::connect(&self.url)
            .await
            .unwrap_or_else(|_| panic!("isolated Postgres connection failed"))
    }
}
async fn parallel(
    commands: Vec<Command>,
    stores: &[Arc<dyn Storage>],
) -> Vec<Result<crate::domain::Output, ApiError>> {
    let mut tasks = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(commands.len()));
    for (n, command) in commands.into_iter().enumerate() {
        let store = stores[n % stores.len()].clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            store.execute(command).await
        });
    }
    let mut results = Vec::new();
    while let Some(result) = tasks.join_next().await {
        results.push(result.unwrap());
    }
    results
}
async fn stores(db: &Database) -> Vec<Arc<dyn Storage>> {
    let mut result: Vec<Arc<dyn Storage>> = Vec::new();
    for _ in 0..3 {
        result.push(Arc::new(PostgresStore::connect(&db.url).await.unwrap()));
    }
    result
}
#[tokio::test]
async fn postgres_migrations_empty_rerun_and_sqlite_shape() {
    let Some(db) = Database::new().await else {
        return;
    };
    let pool = db.pool().await;
    let pg = [
        include_str!("../../migrations/postgres/0001_ledger.sql"),
        include_str!("../../migrations/postgres/0002_public_policy.sql"),
        include_str!("../../migrations/postgres/0003_webhook_inbox.sql"),
        include_str!("../../migrations/postgres/0004_inbox_limits.sql"),
        include_str!("../../migrations/postgres/0005_inbox_archive.sql"),
    ];
    let sqlite = [
        include_str!("../../migrations/sqlite/0001_ledger.sql"),
        include_str!("../../migrations/sqlite/0002_public_policy.sql"),
        include_str!("../../migrations/sqlite/0003_webhook_inbox.sql"),
        include_str!("../../migrations/sqlite/0004_inbox_limits.sql"),
        include_str!("../../migrations/sqlite/0005_inbox_archive.sql"),
    ];
    // Exact DDL parity covers keys, references, checks, uniqueness and nullability.
    assert_eq!(pg, sqlite);
    let sqlite_pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
    for migration in pg {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
        sqlx::raw_sql(migration)
            .execute(&sqlite_pool)
            .await
            .unwrap();
    }
    let shape = "SELECT table_name::text, column_name::text, data_type::text, is_nullable::text FROM information_schema.columns WHERE table_schema = current_schema() ORDER BY table_name, ordinal_position";
    let before: Vec<(String, String, String, String)> =
        sqlx::query_as(shape).fetch_all(&pool).await.unwrap();
    assert_eq!(
        before
            .iter()
            .map(|r| &r.0)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        17
    );
    sqlx::query("INSERT INTO policy_counters(id,payload) VALUES ('migration-sentinel', '[\"migration-sentinel\",7]')").execute(&pool).await.unwrap();
    for migration in pg {
        sqlx::raw_sql(migration).execute(&pool).await.unwrap();
    }
    let after: Vec<(String, String, String, String)> =
        sqlx::query_as(shape).fetch_all(&pool).await.unwrap();
    assert_eq!(before, after);
    for table in before
        .iter()
        .map(|r| &r.0)
        .collect::<std::collections::BTreeSet<_>>()
    {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&sqlite_pool)
            .await
            .unwrap();
        let columns: Vec<String> = rows.iter().map(|r| r.get("name")).collect();
        assert_eq!(
            columns,
            before
                .iter()
                .filter(|r| &r.0 == table)
                .map(|r| r.1.clone())
                .collect::<Vec<_>>()
        );
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            count,
            i64::from(table == "ledger_lock" || table == "policy_counters")
        );
    }
    let sentinel: String =
        sqlx::query_scalar("SELECT payload FROM policy_counters WHERE id='migration-sentinel'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sentinel, "[\"migration-sentinel\",7]");
    pool.close().await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_concurrent_payout_and_ai_caps() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, _, task) = setup_store("PROCESSING", false, false, &db.url).await;
    let stores = stores(&db).await;
    let id = approved(&service, 1).await;
    let results = parallel(
        (0..24)
            .map(|_| Command::PreparePayout {
                id,
                payout_id: PayoutId::new(),
                now: 101,
            })
            .collect(),
        &stores,
    )
    .await;
    assert!(results.iter().all(Result::is_ok));
    assert_eq!(
        results
            .iter()
            .filter(|r| matches!(
                r,
                Ok(crate::domain::Output::Prepared {
                    action: crate::domain::PayoutAction::SendNew,
                    ..
                })
            ))
            .count(),
        1
    );
    let mut commands = Vec::new();
    for _ in 0..29 {
        let id = approved(&service, 1).await;
        commands.push(Command::PreparePayout {
            id,
            payout_id: PayoutId::new(),
            now: 101,
        });
    }
    let results = parallel(commands, &stores).await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 19);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == ErrorCode::InvalidInput),
        "unexpected rejection codes: {:?}",
        results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| &e.code))
            .collect::<Vec<_>>()
    );
    let results = parallel(
        (0..32)
            .map(|_| Command::ExtractionQuota {
                account: "one".into(),
                now: 101,
            })
            .collect(),
        &stores,
    )
    .await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 10);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == ErrorCode::InvalidInput),
        "unexpected rejection codes: {:?}",
        results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| &e.code))
            .collect::<Vec<_>>()
    );
    let results = parallel(
        (0..80)
            .map(|n| Command::ExtractionQuota {
                account: format!("account-{n}"),
                now: 101,
            })
            .collect(),
        &stores,
    )
    .await;
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 40);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == ErrorCode::InvalidInput),
        "unexpected rejection codes: {:?}",
        results
            .iter()
            .filter_map(|r| r.as_ref().err().map(|e| &e.code))
            .collect::<Vec<_>>()
    );
    let pool = db.pool().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM payouts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 20);
    task.abort();
    drop(service);
    drop(stores);
    let reopened = PostgresStore::connect(&db.url).await.unwrap();
    for account in ["one", "fresh-account"] {
        let error = reopened
            .execute(Command::ExtractionQuota {
                account: account.into(),
                now: 102,
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidInput);
    }
    reopened
        .execute(Command::ExtractionQuota {
            account: "one".into(),
            now: 86400,
        })
        .await
        .unwrap();
    drop(reopened);
    pool.close().await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_parallel_webhooks_query_race_and_restart() {
    let Some(db) = Database::new().await else {
        return;
    };
    // This tests ledger concurrency, not HTTP deadlines. The create handler also
    // checks Postgres; the short timeout fixture can lose its acknowledgement.
    let (service, fake, task) =
        setup_store_with_timeout("SUCCESS", false, false, &db.url, Duration::from_secs(20)).await;
    let id = approved(&service, 500).await;
    let created = service.payout(id, 101).await.unwrap();
    assert_eq!(created.claim.state, ClaimState::PayoutPending);
    assert_eq!(
        created.payout.unwrap().paypal_batch_id.as_deref(),
        Some("BATCH1")
    );
    let before = snapshot(&service, id).await;
    let raw = event(&fake, "DURABLE", "SUCCESS");
    assert!(inbox(&service, crate::webhook::Verification::Unavailable)
        .ingest(&raw, &paypal_headers())
        .await
        .is_err());
    let paypal = service.paypal.clone();
    drop(service); // all service-owned pool handles are dropped
    let storage = Arc::new(PostgresStore::connect(&db.url).await.unwrap());
    let service = Arc::new(Service {
        storage: storage.clone(),
        paypal,
        extractor: Arc::new(MockLlm),
        finance_token: "test-finance".into(),
    });
    assert_eq!(snapshot(&service, id).await, before);
    storage.make_inbox_due().await.unwrap();
    let crate::domain::Output::Inbox(rows) = storage.execute(Command::InboxPending).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].body, raw);
    let mut tasks = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(17));
    for _ in 0..16 {
        let good = crate::webhook::Inbox {
            storage: Arc::new(PostgresStore::connect_worker(&db.url).await.unwrap()),
            verifier: Arc::new(Verifier(crate::webhook::Verification::Verified)),
            paypal: service.paypal.clone(),
        };
        let raw = raw.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            good.ingest(&raw, &paypal_headers()).await.unwrap()
        });
    }
    let query = service.clone();
    tasks.spawn(async move {
        barrier.wait().await;
        query.query_reserved(id, 1000, 5).await.unwrap();
        "query".into()
    });
    let mut applied = 0;
    while let Some(result) = tasks.join_next().await {
        if result.unwrap() == "applied" {
            applied += 1;
        }
    }
    assert_eq!(applied, 1);
    let good = inbox(&service, crate::webhook::Verification::Verified);
    good.storage.make_inbox_due().await.unwrap();
    good.retry().await.unwrap();
    let paid = snapshot(&service, id).await;
    assert_eq!(paid["claim"]["claim"]["state"], "Paid");
    let crate::domain::Output::Budgets(b) =
        storage.execute(Command::Budgets(EVENT_ID)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(b.budget_lines[0].reserved_cents.get(), 0);
    assert_eq!(b.budget_lines[0].paid_cents.get(), 500);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    drop(good);
    drop(storage);
    drop(service);
    let reopened = PostgresStore::connect(&db.url).await.unwrap();
    let crate::domain::Output::Claim(c) = reopened.execute(Command::Get(id)).await.unwrap() else {
        panic!()
    };
    assert_eq!(c.claim.state, ClaimState::Paid);
    let pool = db.pool().await;
    for (table, expected) in [
        ("sandbox_events", 1_i64),
        ("webhook_inbox", 17),
        ("immutable_requests", 1),
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, expected);
    }
    task.abort();
    drop(reopened);
    pool.close().await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_commit_failure_returns_503_and_preserves_retry_inbox() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, fake, task) = setup_store("SUCCESS", false, false, &db.url).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    let pool = db.pool().await;
    sqlx::raw_sql("CREATE TABLE fail_parent(id TEXT PRIMARY KEY); CREATE TABLE fail_commit(id TEXT REFERENCES fail_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE FUNCTION fail_reconcile() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO fail_commit VALUES ('absent'); RETURN NEW; END $$; CREATE TRIGGER fail_reconcile AFTER INSERT ON sandbox_events FOR EACH ROW EXECUTE FUNCTION fail_reconcile();").execute(&pool).await.unwrap();
    let good = inbox(&service, crate::webhook::Verification::Verified);
    let app = crate::webhook::router(good.clone());
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
    assert_eq!(
        app.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(snapshot(&service, id).await, before);
    for table in ["sandbox_events", "fail_commit"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
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
    assert_eq!(rows[0].body, raw);
    sqlx::query("DROP TRIGGER fail_reconcile ON sandbox_events")
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
        app.oneshot(request()).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(snapshot(&service, id).await, paid);
    task.abort();
    drop(good);
    drop(service);
    pool.close().await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_reads_do_not_write_ledger_lock() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, _, task) = setup_store("SUCCESS", false, false, &db.url).await;
    let id = approved(&service, 500).await;
    let pool = db.pool().await;
    let before: i64 = sqlx::query_scalar("SELECT version FROM ledger_lock WHERE id=1")
        .fetch_one(&pool)
        .await
        .unwrap();
    for command in [
        Command::Get(id),
        Command::Receipt(id),
        Command::ExtractionInput(id),
        Command::Budgets(EVENT_ID),
        Command::Payees,
        Command::Claims(EVENT_ID),
        Command::Review(id),
        Command::InboxPending,
        Command::WebhookCandidate(id.to_string()),
        Command::QueryDue {
            id,
            now: 1000,
            age: 5,
        },
    ] {
        if let Err(error) = service.storage.execute(command).await {
            assert_ne!(error.code, ErrorCode::StorageUnavailable);
        }
        let after: i64 = sqlx::query_scalar("SELECT version FROM ledger_lock WHERE id=1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(after, before);
    }
    task.abort();
    drop(service);
    pool.close().await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_parallel_service_payout_sends_once() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, fake, task) = setup_store("PROCESSING", false, false, &db.url).await;
    let id = approved(&service, 500).await;
    let mut tasks = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(24));
    for _ in 0..24 {
        let service = service.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            service.payout(id, 101).await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => successes += 1,
            Err(error) => assert_ne!(error.code, ErrorCode::StorageUnavailable),
        }
    }
    assert!(successes >= 1);
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
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
    let pool = db.pool().await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM payouts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    task.abort();
    drop(service);
    pool.close().await;
    db.finish().await;
}

#[tokio::test]
async fn merge_postgres_query_seams() {
    for (status, field) in MERGE_FACTS {
        for path in ["payout", "reserved", "webhook", "recovery"] {
            let Some(db) = Database::new().await else {
                return;
            };
            merge_query_case(&db.url, status, field, path).await;
            db.finish().await;
        }
    }
}

#[tokio::test]
async fn merge_postgres_transport_stays_retryable() {
    for path in ["payout", "reserved", "webhook", "recovery"] {
        let Some(db) = Database::new().await else {
            return;
        };
        merge_transport_case(&db.url, path).await;
        db.finish().await;
    }
}
#[tokio::test]
async fn merge_postgres_query_hold_rolls_back_with_evidence() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, fake, task) =
        setup_store_with_timeout("SUCCESS", false, false, &db.url, Duration::from_secs(20)).await;
    let id = approved(&service, 500).await;
    service.payout(id, 101).await.unwrap();
    let before = snapshot(&service, id).await;
    fake.body.lock().unwrap()["items"][0]["receiver"] = json!("wrong");
    let pool = db.pool().await;
    sqlx::raw_sql("CREATE FUNCTION reject_query_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER reject_query_audit BEFORE INSERT OR UPDATE ON provider_evidence FOR EACH ROW EXECUTE FUNCTION reject_query_audit();").execute(&pool).await.unwrap();
    assert_eq!(
        service.query_reserved(id, 1000, 5).await.unwrap_err().code,
        ErrorCode::StorageUnavailable
    );
    assert_eq!(snapshot(&service, id).await, before);
    sqlx::query("DROP TRIGGER reject_query_audit ON provider_evidence")
        .execute(&pool)
        .await
        .unwrap();
    service.query_reserved(id, 1000, 5).await.unwrap();
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
    pool.close().await;
    task.abort();
    db.finish().await;
}

#[tokio::test]
async fn merge_postgres_prepare_write_set_is_atomic() {
    let Some(db) = Database::new().await else {
        return;
    };
    let (service, _, task) = setup_store("PROCESSING", false, false, &db.url).await;
    let id = approved(&service, 500).await;
    let before = snapshot(&service, id).await;
    let pool = db.pool().await;
    sqlx::raw_sql("CREATE FUNCTION reject_prepare() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER reject_prepare BEFORE INSERT ON immutable_requests FOR EACH ROW EXECUTE FUNCTION reject_prepare();").execute(&pool).await.unwrap();
    let prepare = || Command::PreparePayout {
        id,
        payout_id: PayoutId::new(),
        now: 101,
    };
    assert_eq!(
        service.storage.execute(prepare()).await.err().unwrap().code,
        ErrorCode::StorageUnavailable
    );
    assert_eq!(snapshot(&service, id).await, before);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM policy_counters WHERE id LIKE 'payout:%'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    sqlx::raw_sql("DROP TRIGGER reject_prepare ON immutable_requests; CREATE TRIGGER reject_unrelated BEFORE INSERT OR UPDATE ON budget_lines FOR EACH ROW EXECUTE FUNCTION reject_prepare();").execute(&pool).await.unwrap();
    service.storage.execute(prepare()).await.unwrap();
    sqlx::query("DROP TRIGGER reject_unrelated ON budget_lines")
        .execute(&pool)
        .await
        .unwrap();
    let reopened = PostgresStore::connect(&db.url).await.unwrap();
    let crate::domain::Output::Claim(result) = reopened.execute(Command::Get(id)).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(result.claim.state, ClaimState::PayoutPending);
    assert_eq!(
        result.payout.unwrap().binding.unwrap().amount_cents.get(),
        500
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM immutable_requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM policy_counters WHERE id LIKE 'payout:%' AND payload LIKE '%,1]'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    task.abort();
    db.finish().await;
}

// MAJ-2 production-engine seams: real payout, committed attribution and observable gauge errors.
#[tokio::test]
async fn postgres_exhaustion_commits_claim_with_transition_and_archive() {
    let db = Database::new().await.unwrap();
    let (service, _, task) =
        setup_store_with_timeout("SUCCESS", false, false, &db.url, Duration::from_secs(20)).await;
    let claim = approved(&service, 500).await;
    service.payout(claim, 101).await.unwrap();
    let pool = db.pool().await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(claim.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    let store = PostgresStore::connect(&db.url).await.unwrap();
    let mut d = crate::webhook::Delivery::new(
        &serde_json::to_vec(&serde_json::json!({
            "id":"PG-EXHAUST", "event_type":"PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
            "resource":{"payout_item":{"sender_item_id":payout}}
        }))
        .unwrap(),
        &paypal_headers(),
    );
    d.verified = true;
    d.attempts = 8;
    d.received_at = 5;
    store.execute(Command::Inbox(d.clone())).await.unwrap();
    store
        .execute(Command::InboxFailure(d.id.clone()))
        .await
        .unwrap();
    let stored: (String, String) =
        sqlx::query_as("SELECT status,claim_id FROM inbox_schedule WHERE id=$1")
            .bind(&d.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored, ("exhausted".into(), claim.to_string()));
    store.execute(Command::InboxPrune).await.unwrap();
    let archived: (String, String, String) =
        sqlx::query_as("SELECT status,claim_id,payload FROM webhook_inbox_archive WHERE id=$1")
            .bind(&d.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (&*archived.0, &*archived.1),
        ("exhausted", &*claim.to_string())
    );
    let copy: crate::webhook::Delivery = serde_json::from_str(&archived.2).unwrap();
    assert_eq!(copy.body, d.body);
    assert_eq!(copy.claim_id, Some(claim));
    assert!(copy.verified);
    // Recovery also performs the attribution SELECT inside the committing transaction.
    d.id = "PG-RECOVERY".into();
    store.execute(Command::Inbox(d.clone())).await.unwrap();
    store.execute(Command::InboxPrune).await.unwrap();
    let recovered: String =
        sqlx::query_scalar("SELECT claim_id FROM webhook_inbox_archive WHERE id=$1")
            .bind(&d.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(recovered, claim.to_string());
    drop(store);
    drop(service);
    task.abort();
    pool.close().await;
    db.finish().await;
}

#[tokio::test]
async fn postgres_outstanding_line_counts_live_and_archived_and_is_throttled() {
    let db = Database::new().await.unwrap();
    let (service, _, task) =
        setup_store_with_timeout("SUCCESS", false, false, &db.url, Duration::from_secs(20)).await;
    let claim = approved(&service, 500).await;
    service.payout(claim, 101).await.unwrap();
    let pool = db.pool().await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(claim.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let store = PostgresStore::connect(&db.url)
        .await
        .unwrap()
        .with_signal(Arc::new(move |line| sink.lock().unwrap().push(line.into())));
    store.report_verified_exhausted(1000).await.unwrap();
    assert!(lines.lock().unwrap().is_empty());
    let mut d = crate::webhook::Delivery::new(
        &serde_json::to_vec(&serde_json::json!({
            "id":"PG-GAUGE", "event_type":"PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
            "resource":{"payout_item":{"sender_item_id":payout}}
        }))
        .unwrap(),
        &paypal_headers(),
    );
    d.verified = true;
    d.attempts = 8;
    d.received_at = 5;
    store.execute(Command::Inbox(d.clone())).await.unwrap();
    store
        .execute(Command::InboxFailure(d.id.clone()))
        .await
        .unwrap();
    store.execute(Command::InboxPrune).await.unwrap();
    d.id = "PG-LIVE".into();
    d.body = b"{}".to_vec();
    d.received_at = 6;
    store.execute(Command::Inbox(d.clone())).await.unwrap();
    store
        .execute(Command::InboxFailure(d.id.clone()))
        .await
        .unwrap();
    d.id = "PG-FORGED".into();
    d.verified = false;
    store.execute(Command::Inbox(d.clone())).await.unwrap();
    store.execute(Command::InboxFailure(d.id)).await.unwrap();
    for now in [1000, 1010, 1300] {
        store.report_verified_exhausted(now).await.unwrap();
    }
    let outstanding: Vec<String> = lines
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.starts_with("webhook_verified_exhausted_outstanding "))
        .cloned()
        .collect();
    assert_eq!(
        outstanding,
        vec![
            "webhook_verified_exhausted_outstanding count=2 unattributed=1 oldest_received_at=5";
            2
        ]
    );
    let archived: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox_archive")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(archived, 1);
    drop(store);
    drop(service);
    task.abort();
    pool.close().await;
    db.finish().await;
}

#[tokio::test]
async fn postgres_broken_gauge_emits_fixed_failure_line() {
    let db = Database::new().await.unwrap();
    let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = lines.clone();
    let store = PostgresStore::connect(&db.url)
        .await
        .unwrap()
        .with_signal(Arc::new(move |line| sink.lock().unwrap().push(line.into())));
    let pool = db.pool().await;
    // Break only this newly-created synthetic schema. Prune remains usable; the gauge's
    // archive MIN(received) expression now fails on PostgreSQL.
    sqlx::query(
        "ALTER TABLE webhook_inbox_archive ALTER COLUMN received TYPE TEXT USING received::text",
    )
    .execute(&pool)
    .await
    .unwrap();
    let inbox = crate::webhook::Inbox {
        storage: Arc::new(store),
        verifier: Arc::new(Verifier(crate::webhook::Verification::Unavailable)),
        paypal: Arc::new(crate::adapters::MockPayPal::new(
            crate::adapters::MockPayoutFixture::Success,
        )),
    };
    inbox.retry().await.unwrap();
    assert_eq!(
        *lines.lock().unwrap(),
        vec!["webhook_verified_exhausted_gauge_failed"]
    );
    drop(inbox);
    pool.close().await;
    db.finish().await;
}
