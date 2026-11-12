use async_trait::async_trait;
use axum::http::HeaderMap;
use contracts::ErrorCode;
use receipt_roots_server::{
    adapters::{MockPayPal, MockPayoutFixture},
    domain::{Command, Output},
    storage::{PostgresStore, SqliteStore, Storage},
    webhook::{Delivery, Inbox, Verification, WebhookVerifier},
};
use sqlx::{AnyPool, Connection};
use std::sync::Arc;

const HEADERS: [&str; 5] = [
    "paypal-auth-algo",
    "paypal-cert-url",
    "paypal-transmission-id",
    "paypal-transmission-sig",
    "paypal-transmission-time",
];
const DAY: i64 = 86_400;
const SUPPORTED: &str = "PAYMENT.PAYOUTS-ITEM.SUCCEEDED";
fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in HEADERS {
        headers.insert(name, "fixture".parse().unwrap());
    }
    headers
}
struct Verifier(Verification);
#[async_trait]
impl WebhookVerifier for Verifier {
    async fn verify(&self, _: &Delivery) -> Verification {
        self.0
    }
}
fn event(id: &str, kind: &str, note: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "event_type": kind, "note": note,
        "resource": {"payout_item": {"sender_item_id": "nobody"}}
    }))
    .unwrap()
}

struct Db {
    store: Arc<dyn Storage>,
    pool: AnyPool,
    _dir: Option<tempfile::TempDir>,
    pg: Option<(sqlx::PgConnection, String)>,
}
impl Db {
    async fn new(pg: bool) -> Self {
        if pg {
            let url = std::env::var("TEST_DATABASE_URL")
                .expect("TEST_DATABASE_URL is required: Postgres tests must not be skipped");
            let mut admin = sqlx::PgConnection::connect(&url)
                .await
                .unwrap_or_else(|_| panic!("Postgres test connection failed"));
            let schema = format!("p6d1_{}", uuid::Uuid::new_v4().simple());
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
            Self {
                store,
                pool,
                _dir: None,
                pg: Some((admin, schema)),
            }
        } else {
            let dir = tempfile::tempdir().unwrap();
            let url = format!(
                "sqlite://{}?mode=rwc",
                dir.path().join("retention.db").display()
            );
            let store = Arc::new(SqliteStore::connect(&url).await.unwrap());
            let pool = AnyPool::connect(&url).await.unwrap();
            Self {
                store,
                pool,
                _dir: Some(dir),
                pg: None,
            }
        }
    }
    fn inbox(&self, result: Verification) -> Inbox {
        Inbox {
            storage: self.store.clone(),
            verifier: Arc::new(Verifier(result)),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
        }
    }
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
    async fn status_count(&self, status: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM inbox_schedule WHERE status=$1")
            .bind(status)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    async fn row(&self, id: &str) -> Option<Delivery> {
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id=$1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .unwrap();
        payload.map(|p| serde_json::from_str(&p).unwrap())
    }
    async fn archived(&self, id: &str) -> Option<Delivery> {
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM webhook_inbox_archive WHERE id=$1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .unwrap();
        payload.map(|p| serde_json::from_str(&p).unwrap())
    }
    async fn insert_many(&self, template: &Delivery, prefix: &str, n: usize, claim: &str) {
        let mut d = template.clone();
        let mut tx = self.pool.begin().await.unwrap();
        for i in 0..n {
            d.id = format!("{prefix}-{i}");
            sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES($1,$2)")
                .bind(&d.id)
                .bind(serde_json::to_string(&d).unwrap())
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query("INSERT INTO inbox_schedule(id,status,due,received,attempts,claim_id) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(&d.id)
                .bind(&d.status)
                .bind(d.next_attempt)
                .bind(d.received_at)
                .bind(i64::from(d.attempts))
                .bind(claim)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
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
fn aged(status: &str, age: i64, verified: bool) -> Delivery {
    let mut d = Delivery::new(&event("AGED", SUPPORTED, "x"), &headers());
    d.status = status.into();
    d.verified = verified;
    d.received_at = chrono::Utc::now().timestamp() - age;
    d
}
async fn fresh_budget(db: &Db) {
    // The per-minute verification budget is a separate limit; history tests need many deliveries.
    sqlx::query("UPDATE inbox_budget SET used=0")
        .execute(&db.pool)
        .await
        .unwrap();
}
async fn prune(db: &Db) {
    db.store.execute(Command::InboxPrune).await.unwrap();
}

async fn admission_after_rejected_history(db: &Db) {
    let junk = db.inbox(Verification::Rejected);
    for _ in 0..1000 {
        fresh_budget(db).await;
        let e = junk.ingest(b"{}", &headers()).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::WebhookUnverified);
    }
    assert_eq!(db.status_count("rejected").await, 1000);
    let genuine = db.inbox(Verification::Verified);
    let body = event("GENUINE-AFTER-JUNK", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
    prune(db).await;
    let body = event("GENUINE-AFTER-PRUNE", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
    assert_eq!(db.status_count("unmatched").await, 2);
}

async fn admission_after_terminal_history(db: &Db) {
    let genuine = db.inbox(Verification::Verified);
    for n in 0..350 {
        let id = format!("T-{n}");
        let first = event(&id, SUPPORTED, "first");
        fresh_budget(db).await;
        assert_eq!(
            genuine.ingest(&first, &headers()).await.unwrap(),
            "unmatched"
        );
        fresh_budget(db).await;
        assert_eq!(genuine.ingest(&first, &headers()).await.unwrap(), "replay");
        fresh_budget(db).await;
        let other = event(&id, SUPPORTED, "different body");
        assert_eq!(
            genuine.ingest(&other, &headers()).await.unwrap(),
            "conflict"
        );
    }
    for n in 0..300 {
        fresh_budget(db).await;
        let body = event(&format!("IGN-{n}"), "PAYMENT.CAPTURE.COMPLETED", "x");
        assert_eq!(genuine.ingest(&body, &headers()).await.unwrap(), "ignored");
    }
    db.insert_many(&aged("applied", 10, true), "applied", 200, "")
        .await;
    db.insert_many(&aged("mismatch", 10, true), "mismatch", 200, "")
        .await;
    let terminal = db
        .count("SELECT COUNT(*) FROM inbox_schedule WHERE status NOT IN ('pending','unmatched')")
        .await;
    assert!(terminal >= 1000, "terminal history {terminal}");
    let body = event("GENUINE-AFTER-HISTORY", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
    prune(db).await;
    let body = event("GENUINE-AFTER-HISTORY-PRUNE", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
}

async fn active_saturation_stays_bounded(db: &Db) {
    // ACTIVE_INBOX_CAP is saturated with verified work awaiting a match. Unverified pending rows are
    // bounded earlier by the P6e backlog shed (p6e_intake.rs), so they cannot be the fixture that
    // reaches the active cap. Every assertion below is unchanged.
    let mut pending = Delivery::new(b"{}", &headers());
    pending.status = "pending".into();
    pending.verified = true;
    db.insert_many(&pending, "active", 1000, "").await;
    let genuine = db.inbox(Verification::Verified);
    db.insert_many(&aged("rejected", 1, false), "junk", 50, "")
        .await;
    let body = event("BLOCKED", SUPPORTED, "a");
    let e = genuine.ingest(&body, &headers()).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::AdapterUnavailable);
    assert_eq!(db.status_count("pending").await, 1000);
    // Resolving one active row frees exactly one slot. The row is verified, so it is resolved as
    // verified (unsupported body => ignored) rather than through the unverified redaction path.
    db.store
        .execute(Command::Reconcile {
            delivery_id: "active-0".into(),
            verified: true,
            confirmation: None,
        })
        .await
        .unwrap();
    let body = event("UNBLOCKED", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
    let body = event("BLOCKED-AGAIN", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap_err().code,
        ErrorCode::AdapterUnavailable
    );
}

async fn byte_cap_still_closes_intake(db: &Db) {
    let genuine = db.inbox(Verification::Verified);
    let mut big = Delivery::new(b"{}", &headers());
    big.status = "pending".into();
    db.insert_many(&big, "big", 1, "").await;
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id='big-0'")
        .bind("x".repeat(33 * 1024 * 1024))
        .execute(&db.pool)
        .await
        .unwrap();
    let body = event("BYTES", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap_err().code,
        ErrorCode::AdapterUnavailable
    );
    assert!(db.row("BYTES").await.is_none());
}

async fn bounded_terminal_history(db: &Db) {
    db.insert_many(&aged("rejected", 10, false), "junk", 1200, "")
        .await;
    db.insert_many(&aged("applied", 2 * DAY, true), "applied-old", 3, "claim-a")
        .await;
    db.insert_many(&aged("applied", 10, true), "applied-new", 3, "claim-a")
        .await;
    db.insert_many(
        &aged("mismatch", 2 * DAY, true),
        "mismatch-old",
        3,
        "claim-a",
    )
    .await;
    db.insert_many(&aged("ignored", 2 * DAY, true), "ignored-old", 3, "")
        .await;
    db.insert_many(
        &aged("applied", 400 * DAY, true),
        "applied-ancient",
        3,
        "claim-a",
    )
    .await;
    db.insert_many(&aged("ignored", 400 * DAY, true), "ignored-ancient", 3, "")
        .await;
    db.insert_many(&aged("pending", 400 * DAY, false), "stuck", 3, "")
        .await;
    prune(db).await;
    assert_eq!(db.status_count("rejected").await, 1200);
    let mut cleared = aged("rejected", DAY + 60, false);
    cleared.body.clear();
    cleared.headers.clear();
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id LIKE 'junk-%'")
        .bind(serde_json::to_string(&cleared).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE inbox_schedule SET received=$1 WHERE id LIKE 'junk-%'")
        .bind(cleared.received_at)
        .execute(&db.pool)
        .await
        .unwrap();
    prune(db).await;
    assert_eq!(db.status_count("rejected").await, 0);
    let original = aged("applied", 2 * DAY, true);
    for (id, status) in [
        ("applied-old-0", "applied"),
        ("mismatch-old-0", "mismatch"),
        ("ignored-old-0", "ignored"),
        ("applied-ancient-0", "applied"),
        ("ignored-ancient-2", "ignored"),
    ] {
        assert!(db.row(id).await.is_none(), "{id} left the live inbox");
        let r = db
            .archived(id)
            .await
            .unwrap_or_else(|| panic!("{id} archived"));
        assert_eq!(r.status, status);
        assert_eq!(r.body, original.body, "{id} body kept losslessly");
        assert_eq!(r.headers, original.headers, "{id} headers kept losslessly");
        assert_eq!(r.hash, original.hash);
        assert!(r.verified);
        let stored: (String, i64) =
            sqlx::query_as("SELECT status, received FROM webhook_inbox_archive WHERE id=$1")
                .bind(id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(stored.0, status);
        assert_eq!(stored.1, r.received_at, "archive keeps the received time");
    }
    assert_eq!(
        db.count("SELECT COUNT(*) FROM webhook_inbox_archive").await,
        15
    );
    let fresh = db.row("applied-new-0").await.unwrap();
    assert!(!fresh.body.is_empty(), "recent verified payload stays live");
    assert!(db.archived("applied-new-0").await.is_none());
    assert!(
        db.row("stuck-0").await.is_some(),
        "active rows are never pruned"
    );
    assert_eq!(
        db.count(
            "SELECT COUNT(*) FROM inbox_schedule WHERE id NOT IN (SELECT id FROM webhook_inbox)"
        )
        .await,
        0
    );
    let live = db.count("SELECT COUNT(*) FROM webhook_inbox").await;
    prune(db).await;
    assert_eq!(live, db.count("SELECT COUNT(*) FROM webhook_inbox").await);
    assert_eq!(
        db.count("SELECT COUNT(*) FROM webhook_inbox_archive").await,
        15
    );
    for _ in 0..97 {
        db.store
            .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
            .await
            .unwrap();
    }
    let refused = db
        .store
        .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
        .await
        .unwrap_err();
    assert!(refused.message.contains("intake backlog full"));
}

async fn archive_is_idempotent_after_partial_failure(db: &Db) {
    db.insert_many(&aged("applied", 2 * DAY, true), "half", 4, "claim-h")
        .await;
    let original = db.row("half-0").await.unwrap();
    // A crash after the archive copy but before the live delete leaves the row in both tables.
    sqlx::query("INSERT INTO webhook_inbox_archive(id,payload,status,received,claim_id,archived_at) SELECT w.id,w.payload,s.status,s.received,s.claim_id,1 FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE w.id IN ('half-0','half-1')")
        .execute(&db.pool)
        .await
        .unwrap();
    for _ in 0..3 {
        prune(db).await;
        assert_eq!(
            db.count("SELECT COUNT(*) FROM webhook_inbox_archive").await,
            4
        );
        assert_eq!(db.count("SELECT COUNT(*) FROM webhook_inbox").await, 0);
    }
    let archived = db.archived("half-0").await.unwrap();
    assert_eq!(
        serde_json::to_string(&archived).unwrap(),
        serde_json::to_string(&original).unwrap()
    );
    let kept: i64 =
        sqlx::query_scalar("SELECT archived_at FROM webhook_inbox_archive WHERE id='half-0'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(kept, 1, "an existing archive row is never rewritten");
}

async fn verified_exhausted_is_preserved(db: &Db) {
    let verified = aged("exhausted", 10, true);
    let junk = aged("exhausted", 10, false);
    db.insert_many(&verified, "keep", 1, "").await;
    db.insert_many(&junk, "junk", 1100, "").await;
    prune(db).await;
    assert!(db.row("keep-0").await.is_none());
    let r = db
        .archived("keep-0")
        .await
        .expect("verified exhausted archived");
    assert_eq!(r.body, verified.body, "verified evidence is not redacted");
    assert_eq!(db.status_count("exhausted").await, 1100);
    let mut cleared = aged("exhausted", DAY + 60, false);
    cleared.body.clear();
    cleared.headers.clear();
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id LIKE 'junk-%'")
        .bind(serde_json::to_string(&cleared).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE inbox_schedule SET received=$1 WHERE id LIKE 'junk-%'")
        .bind(cleared.received_at)
        .execute(&db.pool)
        .await
        .unwrap();
    prune(db).await;
    assert_eq!(db.status_count("exhausted").await, 0);

    db.insert_many(&aged("pending", 5, false), "p", 1, "").await;
    let mut row = db.row("p-0").await.unwrap();
    row.verified = true;
    row.attempts = 8;
    sqlx::query("UPDATE webhook_inbox SET payload=$1 WHERE id='p-0'")
        .bind(serde_json::to_string(&row).unwrap())
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE inbox_schedule SET attempts=8, due=0 WHERE id='p-0'")
        .execute(&db.pool)
        .await
        .unwrap();
    prune(db).await;
    let r = db
        .archived("p-0")
        .await
        .expect("exhausted after verification is archived");
    assert_eq!(r.status, "exhausted");
    assert_eq!(r.body, row.body);
    for _ in 0..100 {
        db.store
            .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
            .await
            .unwrap();
    }
    let refused = db
        .store
        .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
        .await
        .unwrap_err();
    assert!(refused.message.contains("intake backlog full"));
}

async fn byte_pressure_archives_instead_of_refusing(db: &Db) {
    let genuine = db.inbox(Verification::Verified);
    let mut big = aged("replay", 10, true);
    big.body = vec![b'a'; 20 * 1024];
    db.insert_many(&big, "bulk", 700, "").await;
    let live = db
        .count("SELECT COALESCE(SUM(LENGTH(payload)),0) FROM webhook_inbox")
        .await;
    assert!(
        live > 32 * 1024 * 1024,
        "fixture exceeds the byte cap: {live}"
    );
    let body = event("AFTER-BULK", SUPPORTED, "a");
    assert_eq!(
        genuine.ingest(&body, &headers()).await.unwrap(),
        "unmatched"
    );
    let live = db
        .count("SELECT COALESCE(SUM(LENGTH(payload)),0) FROM webhook_inbox")
        .await;
    assert!(
        live < 32 * 1024 * 1024,
        "live inbox back under the cap: {live}"
    );
    let total = db.count("SELECT COUNT(*) FROM webhook_inbox").await
        + db.count("SELECT COUNT(*) FROM webhook_inbox_archive").await;
    assert_eq!(total, 701, "no delivery was lost");
    let kept = db
        .archived("bulk-699")
        .await
        .or(db.row("bulk-699").await)
        .unwrap();
    assert_eq!(kept.body, big.body);
}

async fn postgres_marker(db: &Db) {
    if db.pg.is_some() {
        let version: String = sqlx::query_scalar("SELECT version()")
            .fetch_one(&db.pool)
            .await
            .unwrap();
        assert!(
            version.starts_with("PostgreSQL"),
            "not a real Postgres backend"
        );
    }
}

async fn dedupe_and_query_due_survive_pruning(db: &Db) {
    let genuine = db.inbox(Verification::Verified);
    let first = event("DEDUPE-1", SUPPORTED, "first");
    assert_eq!(
        genuine.ingest(&first, &headers()).await.unwrap(),
        "unmatched"
    );
    assert_eq!(genuine.ingest(&first, &headers()).await.unwrap(), "replay");
    sqlx::query("UPDATE inbox_schedule SET received=received-100000000 WHERE status='replay'")
        .execute(&db.pool)
        .await
        .unwrap();
    prune(db).await;
    assert_eq!(db.status_count("replay").await, 0);
    assert_eq!(genuine.ingest(&first, &headers()).await.unwrap(), "replay");
    let other = event("DEDUPE-1", SUPPORTED, "other");
    assert_eq!(
        genuine.ingest(&other, &headers()).await.unwrap(),
        "conflict"
    );

    let claim = query_due_claim(db).await;
    let due = |now: i64| {
        let store = db.store.clone();
        async move {
            let Output::QueryDue(v) = store
                .execute(Command::QueryDue {
                    id: claim,
                    now,
                    age: 100,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            v
        }
    };
    let now = chrono::Utc::now().timestamp();
    assert!(
        due(now).await,
        "no applied evidence: due from first attempt"
    );
    let mut applied = aged("applied", 10, true);
    applied.claim_id = Some(claim);
    let evidence_at = applied.received_at;
    db.insert_many(&applied, "evidence", 1, &claim.to_string())
        .await;
    assert!(
        !due(evidence_at + 50).await,
        "recent applied event defers the query"
    );
    prune(db).await;
    assert!(db.row("evidence-0").await.is_some());
    assert!(!due(evidence_at + 50).await);
    assert!(due(evidence_at + 100).await);
}
async fn query_due_reads_archived_evidence(db: &Db) {
    let claim = query_due_claim(db).await;
    let mut applied = aged("applied", 2 * DAY, true);
    applied.claim_id = Some(claim);
    let evidence_at = applied.received_at;
    db.insert_many(&applied, "old-evidence", 1, &claim.to_string())
        .await;
    let due = |now: i64| {
        let store = db.store.clone();
        async move {
            let Output::QueryDue(v) = store
                .execute(Command::QueryDue {
                    id: claim,
                    now,
                    age: 100,
                })
                .await
                .unwrap()
            else {
                panic!()
            };
            v
        }
    };
    assert!(!due(evidence_at + 50).await);
    prune(db).await;
    assert!(
        db.row("old-evidence-0").await.is_none(),
        "moved to the archive"
    );
    assert!(db.archived("old-evidence-0").await.is_some());
    assert!(
        !due(evidence_at + 50).await,
        "archived evidence still defers the query"
    );
    assert!(due(evidence_at + 100).await);
}
async fn query_due_claim(db: &Db) -> contracts::ClaimId {
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
                receipt_text: "synthetic due".into(),
            },
        },
        Command::Submit {
            id,
            request: SubmitClaimRequest {
                fields: ReceiptFields {
                    merchant: Some("due".into()),
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

macro_rules! backend_tests {
    ($($name:ident, $pg:expr, $case:ident;)*) => {$(
        #[tokio::test]
        async fn $name() {
            let db = Db::new($pg).await;
            $case(&db).await;
            db.finish().await;
        }
    )*};
}
backend_tests! {
    sqlite_admits_genuine_after_rejected_history, false, admission_after_rejected_history;
    postgres_admits_genuine_after_rejected_history, true, admission_after_rejected_history;
    sqlite_admits_genuine_after_terminal_history, false, admission_after_terminal_history;
    postgres_admits_genuine_after_terminal_history, true, admission_after_terminal_history;
    sqlite_active_saturation_stays_bounded, false, active_saturation_stays_bounded;
    postgres_active_saturation_stays_bounded, true, active_saturation_stays_bounded;
    sqlite_byte_cap_closes_intake, false, byte_cap_still_closes_intake;
    postgres_byte_cap_closes_intake, true, byte_cap_still_closes_intake;
    sqlite_terminal_history_is_archived_losslessly, false, bounded_terminal_history;
    postgres_terminal_history_is_archived_losslessly, true, bounded_terminal_history;
    sqlite_archive_is_idempotent, false, archive_is_idempotent_after_partial_failure;
    postgres_archive_is_idempotent, true, archive_is_idempotent_after_partial_failure;
    sqlite_verified_exhausted_is_preserved, false, verified_exhausted_is_preserved;
    postgres_verified_exhausted_is_preserved, true, verified_exhausted_is_preserved;
    sqlite_byte_pressure_archives, false, byte_pressure_archives_instead_of_refusing;
    postgres_byte_pressure_archives, true, byte_pressure_archives_instead_of_refusing;
    sqlite_query_due_reads_archive, false, query_due_reads_archived_evidence;
    postgres_query_due_reads_archive, true, query_due_reads_archived_evidence;
    postgres_backend_is_real_postgres, true, postgres_marker;
    sqlite_dedupe_and_query_due_survive_pruning, false, dedupe_and_query_due_survive_pruning;
    postgres_dedupe_and_query_due_survive_pruning, true, dedupe_and_query_due_survive_pruning;
}

async fn q43_retention_rule(db: &Db) {
    for (name, verified, status, cleared, age, deleted) in [
        ("all", false, "rejected", true, DAY + 60, true),
        ("exhausted", false, "exhausted", true, DAY + 60, true),
        ("verified", true, "rejected", true, DAY + 60, false),
        ("active", false, "pending", true, DAY + 60, false),
        ("content", false, "rejected", false, DAY + 60, false),
        ("young", false, "rejected", true, DAY - 60, false),
    ] {
        let mut d = aged(status, age, verified);
        if cleared {
            d.body.clear();
            d.headers.clear();
        }
        db.insert_many(&d, name, 1, "").await;
        prune(db).await;
        assert_eq!(
            db.row(&format!("{name}-0")).await.is_none(),
            deleted,
            "{name}"
        );
    }
    let mut d = aged("exhausted", 2 * DAY, true);
    d.body.clear();
    d.headers.clear();
    db.insert_many(&d, "verified-exhausted", 1, "").await;
    let original = db.row("verified-exhausted-0").await.unwrap();
    for _ in 0..3 {
        prune(db).await;
        let copy = db.archived("verified-exhausted-0").await.unwrap();
        assert_eq!(
            serde_json::to_string(&copy).unwrap(),
            serde_json::to_string(&original).unwrap()
        );
        assert!(db.row("verified-exhausted-0").await.is_none());
        assert!(
            db.row("verified-0").await.is_some(),
            "verified rejected history survives repeated pruning"
        );
    }
    let d = aged("rejected", 2 * DAY, false);
    db.insert_many(&d, "malformed", 1, "").await;
    sqlx::query("UPDATE webhook_inbox SET payload='not json' WHERE id='malformed-0'")
        .execute(&db.pool)
        .await
        .unwrap();
    prune(db).await;
    let payload: String =
        sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id='malformed-0'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(payload, "not json");
    // Even a pre-existing identical archive must not make a young unverified row deletable.
    let d = aged("rejected", 10, false);
    db.insert_many(&d, "unverified-copy", 1, "").await;
    sqlx::query("INSERT INTO webhook_inbox_archive(id,payload,status,received,claim_id,archived_at) SELECT w.id,w.payload,s.status,s.received,s.claim_id,1 FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE w.id='unverified-copy-0'")
        .execute(&db.pool).await.unwrap();
    // A conflicting archive must never authorize removal of a verified live event.
    let d = aged("exhausted", 2 * DAY, true);
    db.insert_many(&d, "different-copy", 1, "").await;
    sqlx::query("INSERT INTO webhook_inbox_archive(id,payload,status,received,claim_id,archived_at) SELECT w.id,'{}',s.status,s.received,s.claim_id,1 FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE w.id='different-copy-0'")
        .execute(&db.pool).await.unwrap();
    prune(db).await;
    assert!(db.row("unverified-copy-0").await.is_some());
    assert!(db.row("different-copy-0").await.is_some());
    let untouched: String =
        sqlx::query_scalar("SELECT payload FROM webhook_inbox_archive WHERE id='different-copy-0'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(untouched, "{}");
    // Retaining young failed history does not relax the existing backlog/admission bound.
    for n in 0..99 {
        let mut pending = Delivery::new(b"{}", &headers());
        pending.id = format!("backlog-{n}");
        db.store.execute(Command::Inbox(pending)).await.unwrap();
    }
    assert!(db
        .store
        .execute(Command::Inbox(Delivery::new(b"{}", &headers())))
        .await
        .is_err());
}
#[tokio::test]
async fn sqlite_q43_retention_rule() {
    let db = Db::new(false).await;
    q43_retention_rule(&db).await;
    db.finish().await;
}
#[tokio::test]
async fn postgres_q43_retention_rule() {
    let db = Db::new(true).await;
    q43_retention_rule(&db).await;
    db.finish().await;
}
