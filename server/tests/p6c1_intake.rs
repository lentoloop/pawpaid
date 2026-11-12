use async_trait::async_trait;
use axum::http::HeaderMap;
use receipt_roots_server::{
    adapters::{MockPayPal, MockPayoutFixture},
    domain::{Command, Ledger, Output},
    reconcile,
    storage::{PostgresStore, SqliteStore, Storage},
    webhook::{Delivery, Inbox, Verification, WebhookVerifier},
};
use sqlx::{AnyPool, Connection};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const HEADERS: [&str; 5] = [
    "paypal-auth-algo",
    "paypal-cert-url",
    "paypal-transmission-id",
    "paypal-transmission-sig",
    "paypal-transmission-time",
];
fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in HEADERS {
        headers.insert(name, "fixture".parse().unwrap());
    }
    headers
}
struct Verifier(Verification, Arc<AtomicUsize>);
#[async_trait]
impl WebhookVerifier for Verifier {
    async fn verify(&self, _: &Delivery) -> Verification {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0
    }
}
struct LedgerCommands(Arc<dyn Storage>, Arc<AtomicUsize>);
#[async_trait]
impl Storage for LedgerCommands {
    async fn execute(&self, command: Command) -> Result<Output, contracts::ApiError> {
        if matches!(
            command,
            Command::Reconcile { .. } | Command::WebhookCandidate(_)
        ) {
            self.1.fetch_add(1, Ordering::SeqCst);
        }
        self.0.execute(command).await
    }
}
struct Db {
    store: Arc<dyn Storage>,
    pool: AnyPool,
    _dir: Option<tempfile::TempDir>,
    pg: Option<(sqlx::PgConnection, String)>,
}
impl Db {
    async fn new(pg: bool) -> Option<Self> {
        if pg {
            let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
                return None;
            };
            let mut admin = sqlx::PgConnection::connect(&url)
                .await
                .unwrap_or_else(|_| panic!("Postgres test connection failed"));
            let schema = format!("p6c1_{}", uuid::Uuid::new_v4().simple());
            sqlx::query(&format!("CREATE SCHEMA {schema}"))
                .execute(&mut admin)
                .await
                .unwrap();
            let separator = if url.contains('?') { '&' } else { '?' };
            let isolated = format!("{url}{separator}options=-csearch_path%3D{schema}");
            let store = Arc::new(PostgresStore::connect(&isolated).await.unwrap());
            let pool = AnyPool::connect(&isolated)
                .await
                .unwrap_or_else(|_| panic!("isolated Postgres connection failed"));
            Some(Self {
                store,
                pool,
                _dir: None,
                pg: Some((admin, schema)),
            })
        } else {
            let dir = tempfile::tempdir().unwrap();
            let url = format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("intake.db").display()
            );
            let store = Arc::new(SqliteStore::connect(&url).await.unwrap());
            let pool = AnyPool::connect(&url).await.unwrap();
            Some(Self {
                store,
                pool,
                _dir: Some(dir),
                pg: None,
            })
        }
    }
    fn inbox(&self, result: Verification) -> (Inbox, Arc<AtomicUsize>, Arc<MockPayPal>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let paypal = Arc::new(MockPayPal::new(MockPayoutFixture::Success));
        (
            Inbox {
                storage: self.store.clone(),
                verifier: Arc::new(Verifier(result, calls.clone())),
                paypal: paypal.clone(),
            },
            calls,
            paypal,
        )
    }
    async fn count(&self, table: &str) -> i64 {
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    async fn row(&self, id: &str) -> Delivery {
        let payload: String = sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id=$1")
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .unwrap();
        serde_json::from_str(&payload).unwrap()
    }
    async fn insert(&self, d: &Delivery) {
        sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES($1,$2)")
            .bind(&d.id)
            .bind(serde_json::to_string(d).unwrap())
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(&d.id).bind(&d.status).bind(d.next_attempt).bind(d.received_at).bind(d.attempts as i64).bind("").execute(&self.pool).await.unwrap();
    }
    async fn finish(self) {
        self.pool.close().await;
        drop(self.store);
        if let Some((mut admin, schema)) = self.pg {
            sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
                .execute(&mut admin)
                .await
                .unwrap();
        }
    }
}
async fn junk(db: &Db) {
    let (inbox, calls, _) = db.inbox(Verification::Verified);
    for _ in 0..1100 {
        assert!(inbox.ingest(b"{}", &HeaderMap::new()).await.is_err());
    }
    for name in HEADERS {
        let mut missing = headers();
        missing.remove(name);
        assert!(inbox.ingest(b"{}", &missing).await.is_err());
        let mut duplicate = headers();
        duplicate.append(name, "duplicate".parse().unwrap());
        assert!(inbox.ingest(b"{}", &duplicate).await.is_err());
        let mut empty = headers();
        empty.insert(name, " ".parse().unwrap());
        assert!(inbox.ingest(b"{}", &empty).await.is_err());
    }
    for body in [b"{".as_slice(), b"[]", &vec![b' '; 131_073]] {
        assert!(inbox.ingest(body, &headers()).await.is_err());
    }
    assert_eq!(db.count("webhook_inbox").await, 0);
    assert_eq!(db.count("inbox_budget").await, 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
async fn at_cap(db: &Db) {
    let (inbox, _, _) = db.inbox(Verification::Rejected);
    let mut target = Delivery::new(b"{}", &headers());
    let mut tx = db.pool.begin().await.unwrap();
    for n in 0..1000 {
        target.id = format!("cap-{n}");
        sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES($1,$2)")
            .bind(&target.id)
            .bind(serde_json::to_string(&target).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) VALUES($1,'pending',0,$2,0,'')").bind(&target.id).bind(target.received_at).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert!(inbox.ingest(b"{}", &headers()).await.is_err());
    assert!(inbox.process(&target).await.is_err());
    let row = db.row(&target.id).await;
    assert_eq!(row.status, "rejected");
    assert!(row.body.is_empty() && row.headers.is_empty());
    assert_eq!(row.hash, target.hash);
    assert_eq!(db.count("webhook_inbox").await, 1000);
}
async fn exhaustion(db: &Db) {
    let mut d = Delivery::new(b"{\"private\":true}", &headers());
    d.attempts = 8;
    db.insert(&d).await;
    db.store
        .execute(Command::InboxFailure(d.id.clone()))
        .await
        .unwrap();
    let row = db.row(&d.id).await;
    assert_eq!(row.status, "exhausted");
    assert!(row.body.is_empty() && row.headers.is_empty());
    assert_eq!(row.hash, d.hash);
    let mut crashed = d.clone();
    crashed.id = "crashed".into();
    db.insert(&crashed).await;
    db.store.execute(Command::InboxPrune).await.unwrap();
    let row = db.row(&crashed.id).await;
    assert_eq!(row.status, "exhausted");
    assert!(row.body.is_empty());
    sqlx::query("UPDATE inbox_schedule SET received=0")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store.execute(Command::InboxPrune).await.unwrap();
    assert_eq!(db.count("webhook_inbox").await, 0);
    assert_eq!(db.count("inbox_schedule").await, 0);
}
async fn retained_cap(db: &Db) {
    let mut d = Delivery::new(b"{}", &headers());
    d.status = "exhausted".into();
    d.body.clear();
    d.headers.clear();
    let mut tx = db.pool.begin().await.unwrap();
    for n in 0..1000 {
        d.id = format!("exhausted-{n}");
        sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES($1,$2)")
            .bind(&d.id)
            .bind(serde_json::to_string(&d).unwrap())
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) VALUES($1,'exhausted',0,$2,8,'')").bind(&d.id).bind(d.received_at).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    // Retained exhaustion metadata is history, not active work: it must not close intake.
    db.store
        .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
        .await
        .unwrap();
    assert_eq!(db.count("webhook_inbox").await, 1001);
    db.store.execute(Command::InboxPrune).await.unwrap();
    assert_eq!(db.count("webhook_inbox").await, 1001);
    d.id = "legacy-overflow".into();
    db.insert(&d).await;
    db.store.execute(Command::InboxPrune).await.unwrap();
    assert_eq!(db.count("webhook_inbox").await, 1002);
    sqlx::query("UPDATE inbox_schedule SET received=0")
        .execute(&db.pool)
        .await
        .unwrap();
    db.store.execute(Command::InboxPrune).await.unwrap();
    assert_eq!(db.count("webhook_inbox").await, 1);
}
async fn event_gate(db: &Db) {
    let (mut inbox, _, paypal) = db.inbox(Verification::Verified);
    let ledger_commands = Arc::new(AtomicUsize::new(0));
    inbox.storage = Arc::new(LedgerCommands(db.store.clone(), ledger_commands.clone()));
    // Invalid ledger JSON makes any ledger load/save fail. Inbox-only paths must still succeed.
    sqlx::query("UPDATE budget_lines SET payload='invalid'")
        .execute(&db.pool)
        .await
        .unwrap();
    for kind in [
        "UNRELATED.SUCCESS",
        "PAYMENT.PAYOUTSBATCH.SUCCESS",
        "",
        "PAYMENT.CAPTURE.COMPLETED",
    ] {
        let body = serde_json::to_vec(&serde_json::json!({"id":kind,"event_type":kind,"resource":{"payout_item":{"sender_item_id":"candidate"}}})).unwrap();
        assert_eq!(inbox.ingest(&body, &headers()).await.unwrap(), "ignored");
        let d = Delivery::new(&body, &headers());
        db.store.execute(Command::Inbox(d.clone())).await.unwrap();
        let Output::Reconciled(status) = db
            .store
            .execute(Command::Reconcile {
                delivery_id: d.id.clone(),
                verified: true,
                confirmation: None,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(status, "ignored");
        assert_eq!(db.row(&d.id).await.status, "ignored");
        // Domain defense in depth, independent of the SQL dispatch gate.
        let mut ledger = Ledger::default();
        ledger.inbox.insert(d.id.clone(), d.clone());
        reconcile::apply(&mut ledger, &d.id, true, None).unwrap();
        assert!(ledger.sandbox_events.is_empty() && ledger.evidence.is_empty());
        assert_eq!(ledger.inbox[&d.id].status, "ignored");
    }
    assert_eq!(db.count("sandbox_events").await, 0);
    assert_eq!(db.count("provider_evidence").await, 0);
    assert_eq!(paypal.query_calls.load(Ordering::SeqCst), 0);
    assert_eq!(ledger_commands.load(Ordering::SeqCst), 0);
}
macro_rules! backend_tests {
    ($($name:ident, $pg:expr, $case:ident;)*) => {$ (
        #[tokio::test]
        async fn $name() {
            let Some(db) = Db::new($pg).await else { return; };
            $case(&db).await;
            db.finish().await;
        }
    )*};
}
backend_tests! {
    sqlite_junk_never_persists, false, junk;
    postgres_junk_never_persists, true, junk;
    sqlite_at_cap_rejection_redacts, false, at_cap;
    postgres_at_cap_rejection_redacts, true, at_cap;
    sqlite_exhaustion_redacts_prunes_and_recovers_crash, false, exhaustion;
    postgres_exhaustion_redacts_prunes_and_recovers_crash, true, exhaustion;
    sqlite_exhausted_history_does_not_close_intake, false, retained_cap;
    postgres_exhausted_history_does_not_close_intake, true, retained_cap;
    sqlite_event_gate_intake_storage_and_domain, false, event_gate;
    postgres_event_gate_intake_storage_and_domain, true, event_gate;
}

struct QueryMismatch;
#[async_trait]
impl receipt_roots_server::adapters::PayPalPayouts for QueryMismatch {
    async fn send(
        &self,
        _: &contracts::Payout,
        _: &contracts::Approval,
    ) -> Result<
        receipt_roots_server::domain::Observation,
        receipt_roots_server::adapters::PayoutError,
    > {
        panic!("webhooks never send")
    }
    async fn query(
        &self,
        p: &contracts::Payout,
        _: &contracts::Approval,
    ) -> Result<
        receipt_roots_server::domain::Observation,
        receipt_roots_server::adapters::PayoutError,
    > {
        Ok(receipt_roots_server::domain::Observation {
            state: p.state,
            paypal_batch_id: p.paypal_batch_id.clone(),
            item_id: Some("ITEM1".into()),
            binding: None,
            source: "sandbox-item-query:immutable-fact-mismatch".into(),
            status: "Mismatch".into(),
        })
    }
    fn verify_webhook(&self, _: &[u8], _: &HeaderMap) -> Result<(), contracts::ApiError> {
        panic!("dedicated verifier required")
    }
}
async fn reserved(db: &Db, known_batch: bool, label: &str) -> contracts::ClaimResponse {
    use contracts::*;
    use receipt_roots_server::domain::{BUDGET_ID, CLAIMANT_ID, EVENT_ID, PAYEE_ID};
    let id = ClaimId::new();
    for command in [
        Command::Create {
            id,
            request: CreateClaimRequest {
                event_id: EVENT_ID,
                claimant_id: CLAIMANT_ID,
                budget_line_id: BUDGET_ID,
                receipt_text: format!("synthetic {label}"),
            },
        },
        Command::Submit {
            id,
            request: SubmitClaimRequest {
                fields: ReceiptFields {
                    merchant: Some(label.into()),
                    date: Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
                    total_cents: Some(Cents::try_from(500).unwrap()),
                    currency: Some(Currency::USD),
                    uncertain: vec![],
                },
                items: vec![ReceiptItem {
                    quantity: 1,
                    unit_cents: Cents::try_from(500).unwrap(),
                }],
            },
        },
        Command::Approve {
            id,
            approval_id: ApprovalId::new(),
            payee_id: PAYEE_ID,
            now: 100,
        },
        Command::PreparePayout {
            id,
            payout_id: PayoutId::new(),
            now: 101,
        },
    ] {
        db.store.execute(command).await.unwrap();
    }
    let Output::Claim(response) = db.store.execute(Command::Get(id)).await.unwrap() else {
        panic!()
    };
    if known_batch {
        db.store
            .execute(Command::Observe {
                id,
                observation: receipt_roots_server::domain::Observation {
                    state: PayoutState::PayoutPending,
                    paypal_batch_id: Some("BATCH".into()),
                    item_id: None,
                    binding: response.payout.as_ref().unwrap().binding.clone(),
                    source: "sandbox-create".into(),
                    status: "Pending".into(),
                },
            })
            .await
            .unwrap();
    }
    let Output::Claim(response) = db.store.execute(Command::Get(id)).await.unwrap() else {
        panic!()
    };
    response
}
async fn query_mismatch(db: &Db) {
    use receipt_roots_server::domain::EVENT_ID;
    db.store
        .execute(Command::ConfigureSandbox(true))
        .await
        .unwrap();
    let untouched = reserved(db, true, "other claim").await;
    let (mut inbox, _, _) = db.inbox(Verification::Verified);
    inbox.paypal = Arc::new(QueryMismatch);
    for known in [false, true] {
        let response = reserved(db, known, if known { "known failure" } else { "recovery" }).await;
        let p = response.payout.as_ref().unwrap();
        let Output::Budgets(before) = db.store.execute(Command::Budgets(EVENT_ID)).await.unwrap()
        else {
            panic!()
        };
        let body = serde_json::to_vec(&serde_json::json!({
            "id": format!("mismatch-{known}"), "event_type":"PAYMENT.PAYOUTS-ITEM.FAILED",
            "resource": { "payout_batch_id":"BATCH", "payout_item_id":"ITEM", "transaction_status":"FAILED",
                "payout_item": { "sender_item_id":p.binding.as_ref().unwrap().sender_item_id } }
        })).unwrap();
        assert_eq!(inbox.ingest(&body, &headers()).await.unwrap(), "mismatch");
        let Output::Claim(after) = db
            .store
            .execute(Command::Get(response.claim.id))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(after.claim.state, response.claim.state);
        let payout = after.payout.unwrap();
        assert_eq!(payout.paypal_batch_id, p.paypal_batch_id);
        assert_eq!(payout.reconciliation.len(), 1);
        assert_eq!(payout.reconciliation[0].status, "Mismatch");
        assert_eq!(
            payout.reconciliation[0].paypal_batch_id.as_deref(),
            Some("BATCH")
        );
        let Output::Budgets(after) = db.store.execute(Command::Budgets(EVENT_ID)).await.unwrap()
        else {
            panic!()
        };
        assert_eq!(
            serde_json::to_value(before).unwrap(),
            serde_json::to_value(after).unwrap()
        );
        let Output::Prepared { action, .. } = db
            .store
            .execute(Command::PreparePayout {
                id: response.claim.id,
                payout_id: contracts::PayoutId::new(),
                now: 102,
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(action, receipt_roots_server::domain::PayoutAction::Complete);
    }
    let Output::Claim(other) = db
        .store
        .execute(Command::Get(untouched.claim.id))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(other.payout.unwrap().reconciliation.is_empty());
    let statuses: Vec<String> = sqlx::query_scalar("SELECT status FROM inbox_schedule")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert!(statuses.iter().all(|s| s == "mismatch"));
    let query_evidence: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_evidence WHERE id LIKE 'webhook-query:%'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(query_evidence, 2);
}
backend_tests! {
    sqlite_query_mismatch_holds_only_claim_atomically, false, query_mismatch;
    postgres_query_mismatch_holds_only_claim_atomically, true, query_mismatch;
}

async fn mismatch_rollback(db: &Db) {
    db.store
        .execute(Command::ConfigureSandbox(true))
        .await
        .unwrap();
    let response = reserved(db, true, "rollback").await;
    let p = response.payout.as_ref().unwrap();
    let (mut inbox, _, _) = db.inbox(Verification::Verified);
    inbox.paypal = Arc::new(QueryMismatch);
    let body = serde_json::to_vec(&serde_json::json!({
        "id":"ROLLBACK", "event_type":"PAYMENT.PAYOUTS-ITEM.FAILED",
        "resource":{"payout_batch_id":"BATCH", "payout_item_id":"ITEM", "transaction_status":"FAILED",
            "payout_item":{"sender_item_id":p.binding.as_ref().unwrap().sender_item_id}}
    })).unwrap();
    let failure = if db.pg.is_some() {
        "CREATE FUNCTION fail_p6c1() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'synthetic commit failure'; END $$; CREATE TRIGGER fail_p6c1 BEFORE INSERT ON payouts FOR EACH ROW EXECUTE FUNCTION fail_p6c1();"
    } else {
        "CREATE TRIGGER fail_p6c1 BEFORE INSERT ON payouts BEGIN SELECT RAISE(ABORT, 'synthetic commit failure'); END;"
    };
    sqlx::raw_sql(failure).execute(&db.pool).await.unwrap();
    assert!(inbox.ingest(&body, &headers()).await.is_err());
    let Output::Claim(after) = db
        .store
        .execute(Command::Get(response.claim.id))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(response).unwrap()
    );
    assert_eq!(db.count("sandbox_events").await, 0);
    let status: String = sqlx::query_scalar("SELECT status FROM inbox_schedule")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    let query_evidence: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_evidence WHERE id LIKE 'webhook-query:%'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(query_evidence, 0);
    let cleanup = if db.pg.is_some() {
        "DROP TRIGGER fail_p6c1 ON payouts; DROP FUNCTION fail_p6c1();"
    } else {
        "DROP TRIGGER fail_p6c1;"
    };
    sqlx::raw_sql(cleanup).execute(&db.pool).await.unwrap();
    assert_eq!(inbox.ingest(&body, &headers()).await.unwrap(), "mismatch");
    assert_eq!(db.count("sandbox_events").await, 1);
}
backend_tests! {
    sqlite_query_mismatch_transaction_rolls_back, false, mismatch_rollback;
    postgres_query_mismatch_transaction_rolls_back, true, mismatch_rollback;
}

async fn lane_claim(db: &Db) -> contracts::ClaimId {
    use contracts::*;
    use receipt_roots_server::domain::{BUDGET_ID, CLAIMANT_ID, EVENT_ID, PAYEE_ID};
    let id = ClaimId::new();
    for command in [
        Command::Create {
            id,
            request: CreateClaimRequest {
                event_id: EVENT_ID,
                claimant_id: CLAIMANT_ID,
                budget_line_id: BUDGET_ID,
                receipt_text: format!("synthetic lane {id}"),
            },
        },
        Command::Submit {
            id,
            request: SubmitClaimRequest {
                fields: ReceiptFields {
                    merchant: Some(format!("lane {id}")),
                    date: Some(chrono::NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()),
                    total_cents: Some(Cents::try_from(500).unwrap()),
                    currency: Some(Currency::USD),
                    uncertain: vec![],
                },
                items: vec![ReceiptItem {
                    quantity: 1,
                    unit_cents: Cents::try_from(500).unwrap(),
                }],
            },
        },
        Command::Approve {
            id,
            approval_id: ApprovalId::new(),
            payee_id: PAYEE_ID,
            now: 100,
        },
        Command::PreparePayout {
            id,
            payout_id: PayoutId::new(),
            now: 101,
        },
    ] {
        db.store.execute(command).await.unwrap();
    }
    let Output::Claim(r) = db.store.execute(Command::Get(id)).await.unwrap() else {
        panic!()
    };
    db.store
        .execute(Command::Observe {
            id,
            observation: receipt_roots_server::domain::Observation {
                state: PayoutState::PayoutPending,
                paypal_batch_id: Some("BATCH".into()),
                item_id: None,
                binding: r.payout.as_ref().unwrap().binding.clone(),
                source: "sandbox-create".into(),
                status: "Pending".into(),
            },
        })
        .await
        .unwrap();
    id
}

// Run exact-budget scenarios on a fresh database after any minute rollover. Assertions
// from a stable window propagate unchanged; exhausted retries fail as inconclusive.
macro_rules! lane_backend_tests {
    ($($name:ident, $pg:expr, $case:ident;)*) => {$ (
        #[tokio::test]
        async fn $name() {
            for attempt in 0..3 {
                let Some(db) = Db::new($pg).await else { panic!("PG fixture required"); };
                let db = Arc::new(db);
                let second = chrono::Utc::now().timestamp().rem_euclid(60);
                if second > 5 {
                    tokio::time::sleep(std::time::Duration::from_secs((61 - second) as u64)).await;
                }
                let minute = chrono::Utc::now().timestamp() / 60;
                let run_db = db.clone();
                let result = tokio::spawn(async move { $case(&run_db).await }).await;
                let stable = chrono::Utc::now().timestamp() / 60 == minute;
                Arc::try_unwrap(db).ok().expect("scenario released fixture").finish().await;
                if stable {
                    if let Err(error) = result { std::panic::resume_unwind(error.into_panic()); }
                    return;
                }
                eprintln!("minute rollover: discarded lane attempt {}", attempt + 1);
            }
            panic!("inconclusive: no stable minute in three fresh-fixture attempts");
        }
    )*};
}

async fn lane_backlog(db: &Db) -> String {
    let claim = lane_claim(db).await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(claim.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    for n in 0..100 {
        let mut d = Delivery::new(b"{}", &headers());
        d.id = format!("lane-junk-{n}");
        db.store.execute(Command::Inbox(d)).await.unwrap();
    }
    let refused = db
        .store
        .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
        .await
        .unwrap_err();
    assert!(refused.message.contains("intake backlog full"));
    // Spend the entire ordinary (50) budget by reserving actual pending attempts.
    for n in 0..50 {
        let Output::Inbox(rows) = db
            .store
            .execute(Command::InboxAttempt(format!("lane-junk-{n}")))
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(rows.len(), 1);
    }
    assert!(db
        .store
        .execute(Command::InboxAttempt("lane-junk-50".into()))
        .await
        .is_err());
    payout
}
fn lane_body(id: &str, payout: &str) -> Vec<u8> {
    serde_json::to_vec(
        &serde_json::json!({"id":id,"event_type":"PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
        "resource":{"payout_item":{"sender_item_id":payout}}}),
    )
    .unwrap()
}
async fn lane_post(app: &axum::Router, body: Vec<u8>, network: u16) -> axum::http::StatusCode {
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri("/webhooks/paypal/sandbox");
    for name in HEADERS {
        request = request.header(name, "fixture");
    }
    let mut request = request.body(axum::body::Body::from(body)).unwrap();
    // Distinct IPv6 /64 networks; each network still shares the 20/min cap.
    let peer = std::net::SocketAddr::new(format!("2001:db8:{network:x}::1").parse().unwrap(), 1234);
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    app.clone().oneshot(request).await.unwrap().status()
}
async fn correlated_genuine_survives_outer_saturation(db: &Db) {
    let claim = lane_claim(db).await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(claim.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    struct FloodVerifier(Arc<AtomicUsize>);
    #[async_trait]
    impl WebhookVerifier for FloodVerifier {
        async fn verify(&self, d: &Delivery) -> Verification {
            self.0.fetch_add(1, Ordering::SeqCst);
            let body: serde_json::Value = serde_json::from_slice(&d.body).unwrap();
            if body["genuine"] == true {
                Verification::Verified
            } else {
                Verification::Unavailable
            }
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let inbox = Inbox {
        storage: db.store.clone(),
        verifier: Arc::new(FloodVerifier(calls.clone())),
        paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
    };
    let app = receipt_roots_server::webhook::router(inbox);
    // All four limits are spent by these real HTTP requests, with no budget-table writes.
    for n in 0..600u16 {
        let status = lane_post(
            &app,
            lane_body(
                &format!("FORGED-{n}"),
                "00000000-0000-4000-8000-000000000000",
            ),
            n / 20,
        )
        .await;
        if n < 120 {
            assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        } else {
            assert_eq!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
        }
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        50,
        "ordinary budget exhausted"
    );
    assert_eq!(
        db.count("webhook_inbox").await,
        100,
        "backlog remains capped"
    );
    let used: i64 = sqlx::query_scalar("SELECT used FROM inbox_budget WHERE id=1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(used, 50);
    let mut value: serde_json::Value =
        serde_json::from_slice(&lane_body("GENUINE-RESERVE", &payout)).unwrap();
    value["genuine"] = true.into();
    let body = serde_json::to_vec(&value).unwrap();
    let status = lane_post(&app, body.clone(), 100).await;
    assert_ne!(status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(calls.load(Ordering::SeqCst), 51, "genuine reaches verifier");
    let payloads: Vec<String> = sqlx::query_scalar("SELECT payload FROM webhook_inbox")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    let d = payloads
        .iter()
        .map(|p| serde_json::from_str::<Delivery>(p).unwrap())
        .find(|d| d.body == body)
        .expect("genuine persisted");
    assert!(d.verified && d.correlated);
}
async fn uncorrelated_event_is_still_refused(db: &Db) {
    lane_backlog(db).await;
    let (inbox, calls, _) = db.inbox(Verification::Verified);
    let app = receipt_roots_server::webhook::router(inbox);
    for n in 0..120u16 {
        assert_eq!(
            lane_post(
                &app,
                lane_body(
                    &format!("UNKNOWN-{n}"),
                    "00000000-0000-4000-8000-000000000000"
                ),
                n / 20
            )
            .await,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
    }
    assert_eq!(
        lane_post(
            &app,
            lane_body("UNKNOWN-GENUINE", "00000000-0000-4000-8000-000000000000"),
            100
        )
        .await,
        axum::http::StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(db.count("webhook_inbox").await, 100);
}
async fn forged_correlated_events_are_capped_per_payout(db: &Db) {
    let first = lane_claim(db).await;
    let second = lane_claim(db).await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(first.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let other: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(second.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let (inbox, calls, _) = db.inbox(Verification::Rejected);
    let app = receipt_roots_server::webhook::router(inbox);
    for n in 0..40u16 {
        assert_ne!(
            lane_post(
                &app,
                lane_body(&format!("FORGED-CORRELATED-{n}"), &payout),
                100 + n / 20
            )
            .await,
            axum::http::StatusCode::NO_CONTENT
        );
    }
    let pending:i64=sqlx::query_scalar("SELECT COUNT(*) FROM inbox_schedule s JOIN webhook_inbox w ON w.id=s.id WHERE s.status='pending' AND w.payload LIKE '%\"correlated\":true%'").fetch_one(&db.pool).await.unwrap();
    assert!(pending <= 8, "per-payout pending cap");
    let used: i64 = sqlx::query_scalar("SELECT used FROM inbox_budget WHERE id=2")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        used, 20,
        "correlated sub-budget is spent but never exceeded"
    );
    let before = calls.load(Ordering::SeqCst);
    let body = lane_body("GENUINE-OTHER-PAYOUT", &other);
    lane_post(&app, body.clone(), 103).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "spent sub-budget delays verification"
    );
    let rows: Vec<String> = sqlx::query_scalar("SELECT payload FROM webhook_inbox")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    let row = rows
        .iter()
        .map(|p| serde_json::from_str::<Delivery>(p).unwrap())
        .find(|d| d.body == body)
        .expect("other payout still stored");
    assert!(row.correlated);
}
lane_backend_tests! {
    sqlite_correlated_genuine_survives_outer_saturation, false, correlated_genuine_survives_outer_saturation;
    postgres_correlated_genuine_survives_outer_saturation, true, correlated_genuine_survives_outer_saturation;
    sqlite_uncorrelated_event_is_still_refused, false, uncorrelated_event_is_still_refused;
    postgres_uncorrelated_event_is_still_refused, true, uncorrelated_event_is_still_refused;
    sqlite_forged_correlated_events_are_capped_per_payout, false, forged_correlated_events_are_capped_per_payout;
    postgres_forged_correlated_events_are_capped_per_payout, true, forged_correlated_events_are_capped_per_payout;
}

// Seed directly in storage to avoid 90,000 router calls and verification-budget resets.
async fn cleared_history_byte_cap(db: &Db) {
    db.store
        .execute(Command::ConfigureSandbox(true))
        .await
        .unwrap();
    let claim = lane_claim(db).await;
    let payout: String = sqlx::query_scalar("SELECT id FROM payouts WHERE claim_id=$1")
        .bind(claim.to_string())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let mut history = Delivery::new(b"", &HeaderMap::new());
    history.id = "history".into();
    history.status = "rejected".into();
    history.received_at = chrono::Utc::now().timestamp() - 3600;
    let payload = serde_json::to_string(&history).unwrap();
    sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<90000) INSERT INTO webhook_inbox(id,payload) SELECT 'history-' || CAST(i AS TEXT),$1 FROM n")
        .bind(payload).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) SELECT id,'rejected',0,$1,1,'' FROM webhook_inbox")
        .bind(history.received_at).execute(&db.pool).await.unwrap();
    for n in 0..32 {
        let mut d = Delivery::new(&vec![b'a'; 128 * 1024], &headers());
        d.id = format!("full-backlog-{n}");
        db.insert(&d).await;
        let mut value = serde_json::to_value(&d).unwrap();
        value["pad"] = "".into();
        let base = serde_json::to_string(&value).unwrap().len();
        value["pad"] = "x".repeat(512 * 1024 - base).into();
        let payload = serde_json::to_string(&value).unwrap();
        assert_eq!(payload.len(), 512 * 1024);
        sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id=$2")
            .bind(payload)
            .bind(d.id)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    let backlog: i64 = sqlx::query_scalar("SELECT SUM(LENGTH(w.payload)) FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE s.status='pending'")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(backlog, 16 * 1024 * 1024);
    let total: i64 = sqlx::query_scalar("SELECT SUM(LENGTH(payload)) FROM webhook_inbox")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(total > 32 * 1024 * 1024);
    assert_eq!(db.count("webhook_inbox").await, 90032);
    // Direct bulk seeding bypasses normal traffic; give the cleanup planner current sizes.
    sqlx::query("ANALYZE webhook_inbox")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("ANALYZE inbox_schedule")
        .execute(&db.pool)
        .await
        .unwrap();
    let (inbox, calls, _) = db.inbox(Verification::Verified);
    let app = receipt_roots_server::webhook::router(inbox);
    let Output::Claim(response) = db.store.execute(Command::Get(claim)).await.unwrap() else {
        panic!()
    };
    let p = response.payout.as_ref().unwrap();
    assert_eq!(p.id.to_string(), payout);
    // Supply all immutable payout facts required by reconciliation, not just correlation.
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "GENUINE-AFTER-CLEARED-HISTORY",
        "event_type": "PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
        "resource": {
            "payout_batch_id": p.paypal_batch_id,
            "payout_item_id": "ITEM",
            "transaction_status": "SUCCESS",
            "payout_item": receipt_roots_server::sandbox::request_body(p)["items"][0]
        }
    }))
    .unwrap();
    let status = lane_post(&app, body.clone(), 100).await;
    assert!(status.is_success(), "{status}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let payloads: Vec<String> = sqlx::query_scalar("SELECT w.payload FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE s.status NOT IN ('rejected','pending')")
        .fetch_all(&db.pool).await.unwrap();
    let stored = payloads
        .iter()
        .map(|p| serde_json::from_str::<Delivery>(p).unwrap())
        .find(|d| d.body == body)
        .expect("genuine stored");
    assert!(stored.verified && stored.correlated);
    assert_eq!(db.count("webhook_inbox").await, 90033);
}
backend_tests! {
    sqlite_cleared_history_byte_cap, false, cleared_history_byte_cap;
    postgres_cleared_history_byte_cap, true, cleared_history_byte_cap;
}

// Release-mode measurement uses the real Command::Inbox transaction, including the ledger lock.
#[tokio::test]
async fn postgres_inbox_write_latency_at_attack_volume() {
    let db = Db::new(true).await.expect("dedicated PG fixture required");
    let mut cleared = Delivery::new(b"", &HeaderMap::new());
    cleared.status = "rejected".into();
    cleared.received_at = chrono::Utc::now().timestamp() - 3600;
    let payload = serde_json::to_string(&cleared).unwrap();
    sqlx::query("INSERT INTO webhook_inbox(id,payload) SELECT 'latency-' || CAST(i AS TEXT),$1 FROM generate_series(1,86000) i")
        .bind(payload).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) SELECT id,'rejected',0,$1,1,'' FROM webhook_inbox")
        .bind(cleared.received_at).execute(&db.pool).await.unwrap();
    for n in 0..32 {
        let mut d = Delivery::new(&vec![b'a'; 128 * 1024], &headers());
        d.id = format!("latency-backlog-{n}");
        db.insert(&d).await;
        let mut value = serde_json::to_value(&d).unwrap();
        value["pad"] = "".into();
        let base = serde_json::to_string(&value).unwrap().len();
        value["pad"] = "x".repeat(512 * 1024 - base).into();
        sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id=$2")
            .bind(serde_json::to_string(&value).unwrap())
            .bind(d.id)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    // Match a gradually populated production table; report this planner condition explicitly.
    sqlx::query("ANALYZE webhook_inbox")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("ANALYZE inbox_schedule")
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(db.count("webhook_inbox").await, 86032);
    let backlog: i64 = sqlx::query_scalar("SELECT SUM(LENGTH(w.payload)) FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE s.status='pending'").fetch_one(&db.pool).await.unwrap();
    assert_eq!(backlog, 16 * 1024 * 1024);
    for n in 0..6 {
        let mut d = Delivery::new(b"", &HeaderMap::new());
        d.id = format!("latency-write-{n}");
        d.status = "rejected".into();
        let started = std::time::Instant::now();
        db.store.execute(Command::Inbox(d)).await.unwrap();
        eprintln!(
            "INBOX_WRITE_SAMPLE warmup={} elapsed_us={}",
            n == 0,
            started.elapsed().as_micros()
        );
    }
    db.finish().await;
}

async fn counted_history_refuses(db: &Db, malformed: bool) {
    let mut d = Delivery::new(b"uncleared synthetic terminal content", &headers());
    d.status = "rejected".into();
    d.received_at = chrono::Utc::now().timestamp() - 3600;
    d.id = "counted-history".into();
    db.insert(&d).await;
    let payload = if malformed {
        "!".repeat(33 * 1024 * 1024)
    } else {
        let mut value = serde_json::to_value(&d).unwrap();
        value["pad"] = "x".repeat(33 * 1024 * 1024).into();
        serde_json::to_string(&value).unwrap()
    };
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id=$2")
        .bind(&payload)
        .bind(&d.id)
        .execute(&db.pool)
        .await
        .unwrap();
    let (inbox, calls, paypal) = db.inbox(Verification::Verified);
    let app = receipt_roots_server::webhook::router(inbox);
    assert_eq!(
        lane_post(&app, b"{}".to_vec(), 200).await,
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(paypal.send_calls.load(Ordering::SeqCst), 0);
    assert_eq!(db.count("webhook_inbox").await, 1);
    let retained: String = sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id=$1")
        .bind(&d.id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(retained, payload);
}
async fn malformed_history_refuses(db: &Db) {
    counted_history_refuses(db, true).await;
}
async fn uncleared_history_refuses(db: &Db) {
    counted_history_refuses(db, false).await;
}
backend_tests! {
    sqlite_malformed_history_still_counts, false, malformed_history_refuses;
    postgres_malformed_history_still_counts, true, malformed_history_refuses;
    sqlite_uncleared_terminal_history_still_counts, false, uncleared_history_refuses;
    postgres_uncleared_terminal_history_still_counts, true, uncleared_history_refuses;
}

async fn escaped_cleared_history_is_excluded(db: &Db) {
    let mut d = Delivery::new(b"", &HeaderMap::new());
    d.status = "rejected".into();
    d.id = "escaped-history".into();
    db.insert(&d).await;
    let mut value = serde_json::to_value(&d).unwrap();
    value["pad"] = "x".repeat(33 * 1024 * 1024).into();
    let payload = serde_json::to_string(&value)
        .unwrap()
        .replace("rejected", r"rej\u0065cted");
    assert_eq!(
        serde_json::from_str::<Delivery>(&payload).unwrap().status,
        "rejected"
    );
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id=$2")
        .bind(payload)
        .bind(&d.id)
        .execute(&db.pool)
        .await
        .unwrap();
    let (inbox, calls, _) = db.inbox(Verification::Rejected);
    assert_eq!(
        lane_post(
            &receipt_roots_server::webhook::router(inbox),
            b"{}".to_vec(),
            201
        )
        .await,
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "cleared history did not consume byte cap"
    );
    assert_eq!(db.count("webhook_inbox").await, 2);
}
backend_tests! {
    sqlite_escaped_cleared_history_is_excluded, false, escaped_cleared_history_is_excluded;
    postgres_escaped_cleared_history_is_excluded, true, escaped_cleared_history_is_excluded;
}

async fn malformed_canonical_history_still_counts(db: &Db) {
    let mut d = Delivery::new(b"", &HeaderMap::new());
    d.status = "rejected".into();
    let canonical = serde_json::to_string(&d).unwrap();
    let payload = canonical.replace("\"attempts\":0", "\"attempts\":00");
    assert!(serde_json::from_str::<Delivery>(&payload).is_err());
    assert!(payload.len() * 140000 > 32 * 1024 * 1024);
    sqlx::query("WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<140000) INSERT INTO webhook_inbox(id,payload) SELECT 'invalid-envelope-' || CAST(i AS TEXT),$1 FROM n")
        .bind(&payload).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) SELECT id,'rejected',0,$1,0,'' FROM webhook_inbox")
        .bind(d.received_at).execute(&db.pool).await.unwrap();
    let (inbox, calls, _) = db.inbox(Verification::Verified);
    let app = receipt_roots_server::webhook::router(inbox);
    for payload in [
        payload,
        canonical.replace("\"attempts\":0", "\"attempts\":4294967296"),
    ] {
        assert!(serde_json::from_str::<Delivery>(&payload).is_err());
        sqlx::query("UPDATE webhook_inbox SET payload=$1")
            .bind(payload)
            .execute(&db.pool)
            .await
            .unwrap();
        assert_eq!(
            lane_post(&app, b"{}".to_vec(), 202).await,
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(db.count("webhook_inbox").await, 140000);
    }
}
backend_tests! {
    sqlite_malformed_canonical_history_still_counts, false, malformed_canonical_history_still_counts;
    postgres_malformed_canonical_history_still_counts, true, malformed_canonical_history_still_counts;
}
