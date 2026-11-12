//! P6F static repair candidate: behaviour tests for the MAJ-1 correlated lane.
//!
//! STATUS: all four scenarios passed in the R6 gate; revalidated by PP-HARDEN evidence.
//!
//! What this file does and does not show
//! * It exercises the real router (per-peer, outer ceiling and the 120 uncorrelated window), the real
//!   inbox backlog shed and the real verification budget, with a local fake verifier and a disposable
//!   SQLite file per scenario. No provider, no Postgres.
//! * A correlated delivery is one that names a payout row we hold. That is NOT authentication: the
//!   forged-correlated scenarios below show a known payout UUID still spends the lane. MAJ-1 is
//!   narrowed, not closed; `uncorrelated_event_is_still_refused` keeps the residual visible.
//! * The seeded payout is a raw row, not a decodable ledger payout, so reconcile after verification
//!   is not covered here. The assertions stop at "stored, correlated, sent to the verifier".
//! * Rules kept from p6e_intake.rs: the budget is real (never written here, enforced by a source
//!   check at the bottom) and budget-exact scenarios rerun on a clean minute or PANIC as inconclusive.
//!   The known-gap test in p6e_intake.rs is untouched and must keep passing.
use async_trait::async_trait;
use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use receipt_roots_server::{
    adapters::{MockPayPal, MockPayoutFixture},
    storage::SqliteStore,
    webhook::{router_with_proxy, Delivery, Inbox, Verification, WebhookVerifier},
};
use sqlx::AnyPool;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tower::ServiceExt;

// Mirrors proposed constants in server/src (engineering proposals, not owner-approved policy).
const UNCORRELATED_PER_MIN: usize = 120;
const PEER_PER_MIN: usize = 20;
const CORRELATED_PER_PAYOUT: i64 = 8;
const CORRELATED_BUDGET_PER_MIN: i64 = 20;
const FLOOD: usize = 200;

const START_WITHIN_SECONDS: i64 = 20;
const MAX_MINUTE_RERUNS: usize = 3;
const SUPPORTED: &str = "PAYMENT.PAYOUTS-ITEM.SUCCEEDED";
const PAYOUT: &str = "6f1c1f0e-0d6e-4b34-9a52-6d7f0f6a2c11";
const OTHER_PAYOUT: &str = "7a2d2a1f-1e7f-4c45-8b63-7e8a1a7b3d22";
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

/// Local stand-in for PayPal's verify endpoint: `genuine: true` verifies, anything else is rejected.
struct FakeVerifier(Arc<AtomicUsize>);
#[async_trait]
impl WebhookVerifier for FakeVerifier {
    async fn verify(&self, d: &Delivery) -> Verification {
        self.0.fetch_add(1, Ordering::SeqCst);
        let body: serde_json::Value = serde_json::from_slice(&d.body).unwrap_or_default();
        if body["genuine"] == true {
            Verification::Verified
        } else {
            Verification::Rejected
        }
    }
}

fn event(id: &str, payout: &str, genuine: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "event_type": SUPPORTED, "genuine": genuine,
        "resource": {"payout_item": {"sender_item_id": payout}}
    }))
    .unwrap()
}
fn forged_uncorrelated(n: usize) -> Vec<u8> {
    event(&format!("FORGED-{n}"), "nobody", false)
}

struct Db {
    inbox: Inbox,
    app: Router,
    pool: AnyPool,
    verifier_calls: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}
impl Db {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("p6f.db").display());
        let store = Arc::new(SqliteStore::connect(&url).await.unwrap());
        let pool = AnyPool::connect(&url).await.unwrap();
        let verifier_calls = Arc::new(AtomicUsize::new(0));
        let inbox = Inbox {
            storage: store,
            verifier: Arc::new(FakeVerifier(verifier_calls.clone())),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
        };
        // trust_proxy so each request can present its own network through x-real-ip.
        let app = router_with_proxy(inbox.clone(), true);
        Self {
            inbox,
            app,
            pool,
            verifier_calls,
            _dir: dir,
        }
    }
    /// Raw payout row so the local lookup finds it. Foreign keys are switched off on this one
    /// connection; the row is not a decodable ledger payout (see the module note).
    async fn seed_payout(&self, payout: &str, claim: &str) {
        let mut conn = self.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payouts(id,claim_id,approval_id,sender_batch_id,payload) VALUES($1,$2,$3,$4,'{}')")
            .bind(payout)
            .bind(claim)
            .bind(format!("ap-{payout}"))
            .bind(format!("sb-{payout}"))
            .execute(&mut *conn)
            .await
            .unwrap();
    }
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
    async fn rows(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM webhook_inbox").await
    }
    async fn lane_used(&self, id: i64) -> i64 {
        sqlx::query_scalar("SELECT used FROM inbox_budget WHERE id=$1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .unwrap()
            .unwrap_or(0)
    }
    fn calls(&self) -> usize {
        self.verifier_calls.load(Ordering::SeqCst)
    }
    async fn by_body(&self, body: &[u8]) -> Option<Delivery> {
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(body));
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE payload LIKE $1")
                .bind(format!("%\"hash\":\"{hash}\"%"))
                .fetch_optional(&self.pool)
                .await
                .unwrap();
        payload.map(|p| serde_json::from_str(&p).unwrap())
    }
    /// One POST through the real router from a distinct network.
    async fn post(&self, network: usize, body: Vec<u8>) -> StatusCode {
        let mut request = Request::builder()
            .method("POST")
            .uri("/webhooks/paypal/sandbox")
            .header(
                "x-real-ip",
                format!("198.51.{}.{}", network / 250, network % 250 + 1),
            );
        for name in HEADERS {
            request = request.header(name, "fixture");
        }
        let mut request = request.body(Body::from(body)).unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:1234".parse::<std::net::SocketAddr>().unwrap(),
        ));
        self.app.clone().oneshot(request).await.unwrap().status()
    }
    /// Saturates verification and the unverified backlog through `Inbox::ingest` (the p6e flood),
    /// then spends the 120 uncorrelated window through the router from six networks.
    async fn saturate_everything(&self) {
        for n in 0..FLOOD {
            let _ = self.inbox.ingest(&forged_uncorrelated(n), &headers()).await;
        }
        for k in 0..(UNCORRELATED_PER_MIN / PEER_PER_MIN) {
            for j in 0..PEER_PER_MIN {
                let status = self
                    .post(k, forged_uncorrelated(1000 + k * PEER_PER_MIN + j))
                    .await;
                assert_ne!(
                    status,
                    StatusCode::NO_CONTENT,
                    "a forged event is never accepted"
                );
            }
        }
    }
}

fn minute() -> i64 {
    chrono::Utc::now().timestamp().div_euclid(60)
}
fn wait_for_clean_window() {
    let into = chrono::Utc::now().timestamp().rem_euclid(60);
    if into > START_WITHIN_SECONDS {
        std::thread::sleep(std::time::Duration::from_secs((61 - into) as u64));
    }
}
/// Runs `scenario` until it reports a measurement taken inside one minute, else panics.
async fn in_one_minute<T, F, Fut>(name: &str, mut scenario: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for run in 1..=MAX_MINUTE_RERUNS {
        wait_for_clean_window();
        if let Some(measured) = scenario().await {
            return measured;
        }
        eprintln!("{name}: minute rolled over during run {run}; discarding and rerunning");
    }
    panic!("{name}: INCONCLUSIVE, no clean minute in {MAX_MINUTE_RERUNS} runs (not a pass)");
}

/// Inversion of the p6e `known_gap_*` test for deliveries that name a payout we hold. With the 429
/// window, the backlog and the budget all saturated, a correlated event is still stored and reaches
/// the verifier. It does not show the event is genuine-by-construction: see the forged test below.
#[tokio::test]
async fn correlated_genuine_is_stored_and_verified_during_saturation() {
    let (status, calls, row) = in_one_minute("correlated-genuine", || async {
        let db = Db::new().await;
        db.seed_payout(PAYOUT, "claim-a").await;
        let start = minute();
        db.saturate_everything().await;
        let calls_before = db.calls();
        let body = event("GENUINE-CORRELATED", PAYOUT, true);
        let status = db.post(50, body.clone()).await;
        let calls = db.calls() - calls_before;
        let row = db.by_body(&body).await;
        (minute() == start).then_some((status, calls, row))
    })
    .await;
    assert_ne!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "correlated delivery must not be refused by the global window"
    );
    assert_eq!(calls, 1, "the correlated event reached the verifier");
    let row = row.expect("stored, not shed");
    assert!(row.correlated, "classified from the local payout table");
    assert!(row.verified, "verified by the fake verifier");
}

/// The residual, kept visible. Same saturation, but the event names no payout we hold: it is still
/// refused at the global window and never stored or verified. MAJ-1 is NOT closed.
#[tokio::test]
async fn uncorrelated_event_is_still_refused() {
    let (status, calls, stored) = in_one_minute("uncorrelated-refused", || async {
        let db = Db::new().await;
        db.seed_payout(PAYOUT, "claim-a").await;
        let start = minute();
        db.saturate_everything().await;
        let (calls_before, rows_before) = (db.calls(), db.rows().await);
        let status = db
            .post(
                60,
                event("GENUINE-UNCORRELATED", "not-a-payout-we-hold", true),
            )
            .await;
        let result = (
            status,
            db.calls() - calls_before,
            db.rows().await - rows_before,
        );
        (minute() == start).then_some(result)
    })
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(calls, 0, "never reached the verifier");
    assert_eq!(stored, 0, "refused before storage");
}

/// A known payout UUID can be forged into the lane. The lane is bounded: pending correlated rows per
/// payout, and the share of the verification budget they may use. It is not authenticated, and the
/// shared sub-budget means this also delays correlated verification for other payouts this minute
/// (the row is still stored and retried).
#[tokio::test]
async fn forged_correlated_events_are_capped_per_payout() {
    let m = in_one_minute("forged-correlated", || async {
        let db = Db::new().await;
        db.seed_payout(PAYOUT, "claim-a").await;
        db.seed_payout(OTHER_PAYOUT, "claim-b").await;
        let start = minute();
        for n in 0..40 {
            let body = event(&format!("FORGED-CORR-{n}"), PAYOUT, false);
            let _ = db.inbox.ingest(&body, &headers()).await;
        }
        let pending_correlated = db
            .count("SELECT COUNT(*) FROM inbox_schedule s JOIN webhook_inbox w ON w.id = s.id WHERE s.status='pending' AND w.payload LIKE '%\"correlated\":true%'")
            .await;
        let sub_budget = db.lane_used(2).await;
        let other_body = event("GENUINE-OTHER", OTHER_PAYOUT, true);
        let _ = db.inbox.ingest(&other_body, &headers()).await;
        let other = db.by_body(&other_body).await;
        (minute() == start).then_some((pending_correlated, sub_budget, other))
    })
    .await;
    let (pending_correlated, sub_budget, other) = m;
    assert!(
        pending_correlated <= CORRELATED_PER_PAYOUT,
        "pending correlated rows for one payout: {pending_correlated}"
    );
    assert!(
        sub_budget <= CORRELATED_BUDGET_PER_MIN,
        "correlated sub-budget used: {sub_budget}"
    );
    let other = other.expect("a genuine event for another payout is still stored");
    assert!(other.correlated);
    eprintln!(
        "forged correlated: sub-budget used {sub_budget}/{CORRELATED_BUDGET_PER_MIN}; other payout attempts={}, verified={} (delay, not loss)",
        other.attempts, other.verified
    );
}

/// Source check: this file never writes the budget table.
#[test]
fn this_file_never_writes_the_budget_table() {
    let source = include_str!("p6f_repair.rs");
    for needle in [
        concat!("UPDATE ", "inbox_budget"),
        concat!("DELETE FROM ", "inbox_budget"),
        concat!("INSERT INTO ", "inbox_budget"),
    ] {
        assert!(!source.contains(needle), "budget write found: {needle}");
    }
}
