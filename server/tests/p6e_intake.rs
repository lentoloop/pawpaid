//! P6e MAJ-1 candidate-repair behaviour tests.
//!
//! Everything goes through public APIs (`Inbox::ingest`, `Inbox::retry`, `Storage::execute`) on a
//! disposable SQLite file per scenario. There is no live provider: the verifier is a local fake that
//! answers like PayPal does (forged signature => Rejected, genuine => Verified) and counts calls.
//!
//! Rules this file keeps:
//! * The per-minute verification budget is REAL. No test writes to the budget table; a source check
//!   at the bottom fails the file if that ever changes.
//! * Time is the wall clock. Budget-exact scenarios are guarded against a minute rollover: they start
//!   early in a minute, re-read the minute afterwards and, on rollover, discard the measurement and
//!   rerun on a fresh database (bounded). A scenario that never gets a clean minute PANICS as
//!   inconclusive. It is never reported as a pass. Fixed-clock exactness belongs to the unit tests in
//!   `storage.rs`.
//! * Baseline regressions (no shed, no reserved budget) passed in the R6 gate.
//!   Historical pre-repair behavior is retained in the scenario comments.
//! * MAJ-1 is NOT closed end-to-end by the repair. `known_gap_*` pins that: a genuine event arriving
//!   while a flood has saturated intake is still refused.
use async_trait::async_trait;
use axum::http::HeaderMap;
use contracts::ErrorCode;
use receipt_roots_server::{
    adapters::{MockPayPal, MockPayoutFixture},
    domain::{Command, Output},
    storage::SqliteStore,
    webhook::{Delivery, Inbox, Verification, WebhookVerifier},
};
use sqlx::AnyPool;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

// Mirrors the proposed constants in storage.rs (plan P6E section 4.2, open decision D-E). If the source
// owner changes them, change them here too; the invariants below are written in terms of them.
const VERIFY_BUDGET_PER_MIN: i64 = 60;
const VERIFIED_RESERVED_PER_MIN: i64 = 10;
const UNVERIFIED_BUDGET_PER_MIN: i64 = VERIFY_BUDGET_PER_MIN - VERIFIED_RESERVED_PER_MIN;
const UNVERIFIED_BACKLOG_ROWS: i64 = 100;
const FLOOD: usize = 200;
const _: () = assert!(FLOOD as i64 > UNVERIFIED_BUDGET_PER_MIN + UNVERIFIED_BACKLOG_ROWS);

// A scenario needs a clean window. Start only in the first seconds of a minute, rerun on rollover.
const START_WITHIN_SECONDS: i64 = 20;
const MAX_MINUTE_RERUNS: usize = 3;

const HEADERS: [&str; 5] = [
    "paypal-auth-algo",
    "paypal-cert-url",
    "paypal-transmission-id",
    "paypal-transmission-sig",
    "paypal-transmission-time",
];
const SUPPORTED: &str = "PAYMENT.PAYOUTS-ITEM.SUCCEEDED";

fn headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    for name in HEADERS {
        headers.insert(name, "fixture".parse().unwrap());
    }
    headers
}

/// Local stand-in for PayPal's verify endpoint: a body flagged `genuine` has a valid signature,
/// everything else fails verification. Never leaves the process.
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

fn forged(n: usize) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": format!("FORGED-{n}"), "event_type": SUPPORTED, "forged": true,
        "resource": {"payout_item": {"sender_item_id": "nobody"}}
    }))
    .unwrap()
}
fn genuine(id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": id, "event_type": SUPPORTED, "genuine": true,
        "resource": {"payout_item": {"sender_item_id": "nobody"}}
    }))
    .unwrap()
}

struct Db {
    inbox: Inbox,
    pool: AnyPool,
    verifier_calls: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}
impl Db {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}?mode=rwc", dir.path().join("p6e.db").display());
        let store = Arc::new(SqliteStore::connect(&url).await.unwrap());
        let pool = AnyPool::connect(&url).await.unwrap();
        let verifier_calls = Arc::new(AtomicUsize::new(0));
        let inbox = Inbox {
            storage: store,
            verifier: Arc::new(FakeVerifier(verifier_calls.clone())),
            paypal: Arc::new(MockPayPal::new(MockPayoutFixture::Success)),
        };
        Self {
            inbox,
            pool,
            verifier_calls,
            _dir: dir,
        }
    }
    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }
    async fn rows(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM webhook_inbox").await
    }
    async fn pending(&self) -> i64 {
        self.count("SELECT COUNT(*) FROM inbox_schedule WHERE status='pending'")
            .await
    }
    /// Budget used in the current minute window (read only; the table is never written here).
    async fn budget_used(&self) -> i64 {
        sqlx::query_scalar("SELECT used FROM inbox_budget WHERE id=1")
            .fetch_optional(&self.pool)
            .await
            .unwrap()
            .unwrap_or(0)
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
    /// An already verified provider delivery that still needs matching (for example after a
    /// failed or deferred reconcile). It is written through the public storage command.
    async fn seed_verified_work(&self, id: &str) {
        let mut d = Delivery::new(&genuine("VERIFIED-WORK"), &headers());
        d.id = id.into();
        d.verified = true;
        d.status = "unmatched".into();
        self.inbox.storage.execute(Command::Inbox(d)).await.unwrap();
    }
    async fn attempt(&self, id: &str) -> Result<Vec<Delivery>, contracts::ApiError> {
        match self
            .inbox
            .storage
            .execute(Command::InboxAttempt(id.into()))
            .await?
        {
            Output::Inbox(rows) => Ok(rows),
            _ => panic!("unexpected output"),
        }
    }
}

fn minute() -> i64 {
    chrono::Utc::now().timestamp().div_euclid(60)
}
/// Waits (blocking, bounded to under a minute) until the wall clock is early in a minute.
fn wait_for_clean_window() {
    let into = chrono::Utc::now().timestamp().rem_euclid(60);
    if into > START_WITHIN_SECONDS {
        std::thread::sleep(std::time::Duration::from_secs((61 - into) as u64));
    }
}
/// Runs `scenario` until it reports a measurement taken inside one minute. The closure builds its
/// own fresh database and returns `None` when it saw the minute change. The budget is never reset.
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

struct Flooded {
    verifier_calls: i64,
    used: i64,
    pending: i64,
    stored: i64,
    shed: i64,
}
/// Sends `FLOOD` forged deliveries through `Inbox::ingest` with the real budget.
async fn flood(db: &Db) -> Flooded {
    let before = db.rows().await;
    for n in 0..FLOOD {
        let outcome = db.inbox.ingest(&forged(n), &headers()).await;
        let e = outcome.expect_err("a forged delivery is never accepted");
        assert!(
            matches!(
                e.code,
                ErrorCode::WebhookUnverified | ErrorCode::AdapterUnavailable
            ),
            "unexpected refusal {:?}",
            e.code
        );
    }
    let stored = db.rows().await - before;
    Flooded {
        verifier_calls: db.verifier_calls.load(Ordering::SeqCst) as i64,
        used: db.budget_used().await,
        pending: db.pending().await,
        stored,
        shed: FLOOD as i64 - stored,
    }
}

/// Regression for R1+R2 (baseline: 60 verifier calls, 140 pending, nothing shed).
/// A forged flood inside one real minute must be bounded in verification spend and in backlog,
/// and the excess must be refused before it is written.
#[tokio::test]
async fn real_budget_forged_flood_is_bounded() {
    let m = in_one_minute("flood", || async {
        let db = Db::new().await;
        let start = minute();
        let m = flood(&db).await;
        (minute() == start).then_some(m)
    })
    .await;
    assert_eq!(
        m.used, m.verifier_calls,
        "budget used equals verifier spend"
    );
    assert!(
        m.verifier_calls <= UNVERIFIED_BUDGET_PER_MIN,
        "unverified traffic spent {} of the verification budget; at most {} allowed (rest reserved)",
        m.verifier_calls,
        UNVERIFIED_BUDGET_PER_MIN
    );
    assert!(
        m.pending <= UNVERIFIED_BACKLOG_ROWS,
        "unverified backlog {} exceeds {}",
        m.pending,
        UNVERIFIED_BACKLOG_ROWS
    );
    assert!(
        m.shed >= FLOOD as i64 - UNVERIFIED_BUDGET_PER_MIN - UNVERIFIED_BACKLOG_ROWS,
        "excess forged requests must be refused before being written: shed {} stored {}",
        m.shed,
        m.stored
    );
}

/// Regression for R2 (baseline: the flood takes all 60, so verified work is refused).
/// Verified deliveries never call the verifier, so a flood must not starve them of budget.
#[tokio::test]
async fn verified_work_still_gets_budget_during_flood() {
    let (used_after, attempt, row) = in_one_minute("verified-work", || async {
        let db = Db::new().await;
        let start = minute();
        db.seed_verified_work("VERIFIED-WORK").await;
        flood(&db).await;
        let attempt = db.attempt("VERIFIED-WORK").await;
        let used_after = db.budget_used().await;
        let row = db.row("VERIFIED-WORK").await;
        (minute() == start).then_some((used_after, attempt, row))
    })
    .await;
    let rows = attempt.unwrap_or_else(|e| {
        panic!(
            "verified work refused while flooded: {:?} {}",
            e.code, e.message
        )
    });
    assert_eq!(rows.len(), 1, "verified delivery was handed out for work");
    assert_eq!(rows[0].attempts, 1);
    assert!(used_after <= VERIFY_BUDGET_PER_MIN);
    let row = row.unwrap();
    assert!(row.verified && !row.body.is_empty() && !row.headers.is_empty());
    assert_eq!(row.body, genuine("VERIFIED-WORK"), "evidence untouched");
}

/// KNOWN GAP, NOT FIXED END-TO-END (MAJ-1 residual A1/A2/A3, owner has not accepted it).
/// While a flood has saturated verification and the unverified backlog, a genuine, correctly signed
/// delivery is still not verified in that window: it is either shed (not stored) or stored
/// unverified. Passing here documents the limit, it does not mean the exposure is closed. If this
/// test starts failing because genuine events now get through, the architecture changed (for example
/// local signature verification); update the docs in the same change.
#[tokio::test]
async fn known_gap_genuine_event_is_refused_while_flood_saturated() {
    let (result, genuine_calls, stored, used) = in_one_minute("known-gap", || async {
        let db = Db::new().await;
        let start = minute();
        flood(&db).await;
        let calls_before = db.verifier_calls.load(Ordering::SeqCst);
        let rows_before = db.rows().await;
        let result = db
            .inbox
            .ingest(&genuine("GENUINE-IN-FLOOD"), &headers())
            .await;
        let calls = db.verifier_calls.load(Ordering::SeqCst) - calls_before;
        let stored = db.rows().await - rows_before;
        let used = db.budget_used().await;
        (minute() == start).then_some((result, calls, stored, used))
    })
    .await;
    let e = result.expect_err("NOT FIXED: genuine delivery cannot complete during saturation");
    assert_eq!(e.code, ErrorCode::AdapterUnavailable);
    assert_eq!(
        genuine_calls, 0,
        "the genuine event never reached the verifier"
    );
    assert!(
        used >= UNVERIFIED_BUDGET_PER_MIN,
        "budget really was exhausted"
    );
    eprintln!(
        "known gap: genuine delivery refused during flood ({}); stored={stored}. NOT FIXED end-to-end",
        if stored == 0 { "shed before write" } else { "stored unverified" }
    );
}

/// Regression for R1 on the second half (baseline never sheds).
/// Resolving one backlog row frees exactly one slot: the next delivery is written (still unverified
/// because the budget is spent, attempts untouched, evidence complete), the one after is shed.
#[tokio::test]
async fn backlog_slot_reopens_exactly_once_without_budget_reset() {
    let m = in_one_minute("slot", || async {
        let db = Db::new().await;
        let start = minute();
        flood(&db).await;
        let victim: String =
            sqlx::query_scalar("SELECT id FROM inbox_schedule WHERE status='pending' LIMIT 1")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        db.inbox
            .storage
            .execute(Command::Reconcile {
                delivery_id: victim,
                verified: false,
                confirmation: None,
            })
            .await
            .unwrap();
        let rows0 = db.rows().await;
        let a = db.inbox.ingest(&genuine("SLOT-A"), &headers()).await;
        let rows1 = db.rows().await;
        let b = db.inbox.ingest(&genuine("SLOT-B"), &headers()).await;
        let rows2 = db.rows().await;
        let stored_a = db.row_by_body(&genuine("SLOT-A")).await;
        (minute() == start).then_some((a, b, rows1 - rows0, rows2 - rows1, stored_a))
    })
    .await;
    let (a, b, a_rows, b_rows, stored_a) = m;
    assert_eq!(a_rows, 1, "freed slot admits the next delivery");
    assert_eq!(a.unwrap_err().code, ErrorCode::AdapterUnavailable);
    let stored_a = stored_a.expect("slot A evidence stored");
    assert_eq!(stored_a.status, "pending");
    assert_eq!(
        stored_a.attempts, 0,
        "a starved row does not burn an attempt"
    );
    assert!(!stored_a.verified);
    assert_eq!(stored_a.body, genuine("SLOT-A"));
    assert_eq!(b.unwrap_err().code, ErrorCode::AdapterUnavailable);
    assert_eq!(
        b_rows, 0,
        "only one slot was freed; the next is refused before write"
    );
}

/// Regression for R1+R2 (baseline: retry spends 60, not 50).
/// After a flood the backlog drains at the real, next-minute budget (no reset), never faster than
/// the unverified budget allows, and intake is open again afterwards.
#[tokio::test]
async fn backlog_drains_across_a_real_minute_and_reopens_intake() {
    let m = in_one_minute("drain", || async {
        let db = Db::new().await;
        let start = minute();
        flood(&db).await;
        let pending_before = db.pending().await;
        if minute() != start {
            return None;
        }
        // Cross into the next real minute; the budget rolls over by its own ON CONFLICT rule.
        let into = chrono::Utc::now().timestamp().rem_euclid(60);
        std::thread::sleep(std::time::Duration::from_secs((61 - into) as u64));
        let next = minute();
        let _ = db.inbox.retry().await;
        let drained_used = db.budget_used().await;
        let pending_after = db.pending().await;
        if minute() != next {
            return None;
        }
        let rows0 = db.rows().await;
        let _ = db.inbox.ingest(&genuine("AFTER-DRAIN"), &headers()).await;
        let admitted = db.rows().await - rows0;
        (minute() == next).then_some((pending_before, pending_after, drained_used, admitted))
    })
    .await;
    let (before, after, used, admitted) = m;
    assert!(after < before, "backlog shrank: {before} -> {after}");
    assert!(
        used <= UNVERIFIED_BUDGET_PER_MIN,
        "one minute of draining spent {used}; unverified ceiling is {UNVERIFIED_BUDGET_PER_MIN}"
    );
    assert!(before - after <= UNVERIFIED_BUDGET_PER_MIN);
    assert_eq!(admitted, 1, "intake reopened once the backlog drained");
}

/// Passes on the baseline: verified exhausted evidence is kept and findable with the exact read-only
/// query documented in DEPLOY.md; unverified exhausted rows are not offered as evidence.
#[tokio::test]
async fn runbook_archive_query_finds_verified_exhausted_only() {
    let db = Db::new().await;
    let mut verified = Delivery::new(&genuine("EXH-VERIFIED"), &headers());
    verified.id = "exh-verified".into();
    verified.verified = true;
    verified.status = "exhausted".into();
    verified.attempts = 8;
    let mut unverified = Delivery::new(&forged(1), &headers());
    unverified.id = "exh-unverified".into();
    unverified.status = "exhausted".into();
    unverified.attempts = 8;
    for d in [verified.clone(), unverified] {
        db.inbox.storage.execute(Command::Inbox(d)).await.unwrap();
    }
    db.inbox.storage.execute(Command::InboxPrune).await.unwrap();
    let found: Vec<(String, Option<String>, i64)> = sqlx::query_as(
        "SELECT id, claim_id, archived_at FROM webhook_inbox_archive WHERE status='exhausted'",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        found.len(),
        1,
        "exactly the verified exhausted delivery: {found:?}"
    );
    assert_eq!(found[0].0, "exh-verified");
    let kept = db.archived_body("exh-verified").await;
    assert_eq!(kept, verified.body, "verified body retained losslessly");
}

/// The suite must never reset the budget; this fails the file if a reset statement is added.
#[test]
fn this_file_never_writes_the_budget_table() {
    let source = include_str!("p6e_intake.rs");
    let needles = [
        concat!("UPDATE ", "inbox_budget"),
        concat!("DELETE FROM ", "inbox_budget"),
        concat!("INSERT INTO ", "inbox_budget"),
    ];
    for needle in needles {
        assert!(!source.contains(needle), "budget write found: {needle}");
    }
}

impl Db {
    async fn row_by_body(&self, body: &[u8]) -> Option<Delivery> {
        let hash = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(body))
        };
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE payload LIKE $1")
                .bind(format!("%\"hash\":\"{hash}\"%"))
                .fetch_optional(&self.pool)
                .await
                .unwrap();
        payload.map(|p| serde_json::from_str(&p).unwrap())
    }
    async fn archived_body(&self, id: &str) -> Vec<u8> {
        let payload: String =
            sqlx::query_scalar("SELECT payload FROM webhook_inbox_archive WHERE id=$1")
                .bind(id)
                .fetch_one(&self.pool)
                .await
                .unwrap();
        serde_json::from_str::<Delivery>(&payload).unwrap().body
    }
}
