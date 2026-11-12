use crate::domain::{self, Command, Ledger, Output, StoredEvent};
use async_trait::async_trait;
use contracts::*;
use serde::{de::DeserializeOwned, Serialize};
use sqlx::{any::AnyPoolOptions, Any, AnyPool, Row, Transaction};
use std::{
    io::Write,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
    time::Duration,
};

#[async_trait]
pub trait Storage: Send + Sync {
    #[cfg(test)]
    async fn make_inbox_due(&self) -> Result<(), ApiError>;
    async fn execute(&self, command: Command) -> Result<Output, ApiError>;
    /// Read-only: does a payout with this id exist locally. Defaults to false, which keeps a
    /// wrapper that forwards only `execute` in the stricter uncorrelated lane.
    async fn payout_known(&self, _payout: uuid::Uuid) -> Result<bool, ApiError> {
        Ok(false)
    }
    fn verified_exhausted_gauge_failed(&self) {
        (stderr_sink())("webhook_verified_exhausted_gauge_failed");
    }
    /// Level-triggered visibility for verified exhausted deliveries. Default: no-op.
    async fn report_verified_exhausted(&self, _now: i64) -> Result<(), ApiError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SqliteStore(SqlStore);
#[derive(Clone)]
pub struct PostgresStore(SqlStore);
#[derive(Clone)]
struct SqlStore {
    pool: AnyPool,
    sqlite: bool,
    signal: SignalSink,
    // Last time the level-triggered line was written; throttles it, never gates evidence.
    gauge_at: Arc<AtomicI64>,
}

impl SqliteStore {
    pub async fn connect(url: &str) -> Result<Self, ApiError> {
        Ok(Self(SqlStore::connect(url, true).await?))
    }
}
impl PostgresStore {
    #[cfg(test)]
    pub(crate) fn with_signal(mut self, signal: SignalSink) -> Self {
        self.0.signal = signal;
        self
    }

    #[cfg(test)]
    pub(crate) async fn connect_worker(url: &str) -> Result<Self, ApiError> {
        // One operation per independent worker; retain sixteen independent pools
        // without allocating spare connections while peers wait at the barrier.
        Ok(Self(
            SqlStore::connect_with_pool_limit(url, false, 1).await?,
        ))
    }
    pub async fn connect(url: &str) -> Result<Self, ApiError> {
        Ok(Self(SqlStore::connect(url, false).await?))
    }
}
#[async_trait]
impl Storage for SqliteStore {
    #[cfg(test)]
    async fn make_inbox_due(&self) -> Result<(), ApiError> {
        self.0.make_inbox_due().await
    }
    async fn execute(&self, command: Command) -> Result<Output, ApiError> {
        self.0.execute(command).await
    }
    async fn payout_known(&self, payout: uuid::Uuid) -> Result<bool, ApiError> {
        self.0.payout_known(payout).await
    }
    async fn report_verified_exhausted(&self, now: i64) -> Result<(), ApiError> {
        self.0.report_verified_exhausted(now).await
    }
    fn verified_exhausted_gauge_failed(&self) {
        (self.0.signal)("webhook_verified_exhausted_gauge_failed");
    }
}
#[async_trait]
impl Storage for PostgresStore {
    #[cfg(test)]
    async fn make_inbox_due(&self) -> Result<(), ApiError> {
        self.0.make_inbox_due().await
    }
    async fn execute(&self, command: Command) -> Result<Output, ApiError> {
        self.0.execute(command).await
    }
    async fn payout_known(&self, payout: uuid::Uuid) -> Result<bool, ApiError> {
        self.0.payout_known(payout).await
    }
    async fn report_verified_exhausted(&self, now: i64) -> Result<(), ApiError> {
        self.0.report_verified_exhausted(now).await
    }
    fn verified_exhausted_gauge_failed(&self) {
        (self.0.signal)("webhook_verified_exhausted_gauge_failed");
    }
}
fn storage_error(_: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorCode::StorageUnavailable, "database operation failed")
}
fn connect_error(error: sqlx::Error) -> ApiError {
    #[cfg(test)]
    {
        // Native diagnostics stay in synthetic unit tests; public errors remain redacted.
        if let sqlx::Error::Database(db) = &error {
            eprintln!("TEST connect SQLSTATE={:?}", db.code());
        } else {
            eprintln!(
                "TEST connect SQLx kind={}",
                match &error {
                    sqlx::Error::PoolTimedOut => "PoolTimedOut",
                    sqlx::Error::PoolClosed => "PoolClosed",
                    sqlx::Error::Io(_) => "Io",
                    _ => "other",
                }
            );
        }
    }
    storage_error(error)
}
impl SqlStore {
    async fn connect(url: &str, sqlite: bool) -> Result<Self, ApiError> {
        Self::connect_with_pool_limit(url, sqlite, 5).await
    }
    async fn connect_with_pool_limit(
        url: &str,
        sqlite: bool,
        max_connections: u32,
    ) -> Result<Self, ApiError> {
        if (sqlite && !url.starts_with("sqlite:"))
            || (!sqlite && !url.starts_with("postgres://") && !url.starts_with("postgresql://"))
        {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "database URL does not match backend",
            ));
        }
        sqlx::any::install_default_drivers();
        let pool = AnyPoolOptions::new()
            .max_connections(if url.contains(":memory:") {
                1
            } else {
                max_connections
            })
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    if sqlite {
                        sqlx::query("PRAGMA busy_timeout = 10000")
                            .execute(&mut *connection)
                            .await?;
                        sqlx::query("PRAGMA foreign_keys = ON")
                            .execute(&mut *connection)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(url)
            .await
            .map_err(connect_error)?;
        let migration = if sqlite {
            include_str!("../../migrations/sqlite/0001_ledger.sql")
        } else {
            include_str!("../../migrations/postgres/0001_ledger.sql")
        };
        sqlx::raw_sql(migration)
            .execute(&pool)
            .await
            .map_err(connect_error)?;
        let policy_migration = if sqlite {
            include_str!("../../migrations/sqlite/0002_public_policy.sql")
        } else {
            include_str!("../../migrations/postgres/0002_public_policy.sql")
        };
        sqlx::raw_sql(policy_migration)
            .execute(&pool)
            .await
            .map_err(connect_error)?;
        sqlx::raw_sql(if sqlite {
            include_str!("../../migrations/sqlite/0003_webhook_inbox.sql")
        } else {
            include_str!("../../migrations/postgres/0003_webhook_inbox.sql")
        })
        .execute(&pool)
        .await
        .map_err(connect_error)?;
        sqlx::raw_sql(if sqlite {
            include_str!("../../migrations/sqlite/0004_inbox_limits.sql")
        } else {
            include_str!("../../migrations/postgres/0004_inbox_limits.sql")
        })
        .execute(&pool)
        .await
        .map_err(connect_error)?;
        sqlx::raw_sql(if sqlite {
            include_str!("../../migrations/sqlite/0005_inbox_archive.sql")
        } else {
            include_str!("../../migrations/postgres/0005_inbox_archive.sql")
        })
        .execute(&pool)
        .await
        .map_err(connect_error)?;
        let store = Self {
            pool,
            sqlite,
            signal: stderr_sink(),
            gauge_at: Arc::new(AtomicI64::new(i64::MIN)),
        };
        let mut tx = store.begin_locked().await?;
        // Upgrade existing inboxes once, outside normal ledger load/save.
        let legacy = sqlx::query(
            "SELECT id,payload FROM webhook_inbox WHERE id NOT IN (SELECT id FROM inbox_schedule)",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(connect_error)?;
        for row in legacy {
            let d: crate::webhook::Delivery =
                serde_json::from_str(&row.try_get::<String, _>("payload").map_err(connect_error)?)
                    .map_err(storage_error)?;
            put_inbox(&mut tx, &d).await?;
        }
        let mut ledger = load(&mut tx).await?;
        ledger.seed();
        save(&mut tx, &ledger).await?;
        tx.commit().await.map_err(connect_error)?;
        Ok(store)
    }
    async fn begin_locked(&self) -> Result<Transaction<'_, Any>, ApiError> {
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        sqlx::query("UPDATE ledger_lock SET version = version + 1 WHERE id = 1")
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        Ok(tx)
    }
    async fn execute(&self, command: Command) -> Result<Output, ApiError> {
        let now = chrono::Utc::now().timestamp();
        if matches!(
            command,
            Command::Inbox(_)
                | Command::InboxPending
                | Command::InboxAttempt(_)
                | Command::InboxVerified(_)
                | Command::InboxFailure(_)
                | Command::InboxPrune
        ) {
            return self.inbox(command, now).await;
        }
        // Same-day maintenance is read-only; recheck under the writer lock on rollover.
        if let Command::ResetDemo { now } = &command {
            let payload: Option<String> =
                sqlx::query_scalar("SELECT payload FROM policy_counters WHERE id = 'reset-day'")
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(storage_error)?;
            if payload
                .and_then(|p| serde_json::from_str::<(String, i64)>(&p).ok())
                .is_some_and(|(_, day)| day >= (now + 14400).div_euclid(86400))
            {
                return Ok(Output::ClaimantConfigured);
            }
        }
        if matches!(
            command,
            Command::WebhookCandidate(_)
                | Command::InboxPending
                | Command::QueryDue { .. }
                | Command::Get(_)
                | Command::ExtractionInput(_)
                | Command::Receipt(_)
                | Command::Budgets(_)
                | Command::Payees
                | Command::Claims(_)
                | Command::Review(_)
        ) {
            let mut tx = self.pool.begin().await.map_err(storage_error)?;
            if !self.sqlite {
                sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                    .execute(&mut *tx)
                    .await
                    .map_err(storage_error)?;
            }
            let mut ledger = load(&mut tx).await?;
            if let Command::QueryDue { id, .. } = &command {
                let mut payloads = sqlx::query("SELECT payload FROM webhook_inbox JOIN inbox_schedule USING(id) WHERE status = 'applied' AND claim_id = $1").bind(id.to_string()).fetch_all(&mut *tx).await.map_err(storage_error)?;
                payloads.extend(sqlx::query("SELECT payload FROM webhook_inbox_archive WHERE status = 'applied' AND claim_id = $1").bind(id.to_string()).fetch_all(&mut *tx).await.map_err(storage_error)?);
                for r in payloads {
                    let d: crate::webhook::Delivery = serde_json::from_str(
                        &r.try_get::<String, _>("payload").map_err(storage_error)?,
                    )
                    .map_err(storage_error)?;
                    if d.claim_id == Some(*id) {
                        ledger.inbox.insert(d.id.clone(), d);
                    }
                }
            }
            let result = domain::apply(&mut ledger, command);
            tx.rollback().await.map_err(storage_error)?;
            return result;
        }
        let mut tx = self.begin_locked().await?;
        // Gate before loading/saving any ledger tables, including direct command callers.
        if let Command::Reconcile {
            delivery_id,
            verified,
            ..
        } = &command
        {
            let mut row = inbox_row(&mut tx, delivery_id).await?;
            if !verified || !crate::reconcile::supported_event(&row.body) {
                if matches!(row.status.as_str(), "pending" | "unmatched") {
                    row.status = if *verified { "ignored" } else { "rejected" }.into();
                    if !verified {
                        row.body.clear();
                        row.headers.clear();
                    }
                    put_inbox(&mut tx, &row).await?;
                }
                tx.commit().await.map_err(storage_error)?;
                return Ok(Output::Reconciled(row.status));
            }
        }
        let mut ledger = load(&mut tx).await?;
        let reconcile_id = if let Command::Reconcile { delivery_id, .. } = &command {
            let row = inbox_row(&mut tx, delivery_id).await?;
            ledger.inbox.insert(delivery_id.clone(), row);
            Some(delivery_id.clone())
        } else {
            None
        };
        // These commands have narrow write sets. Rewriting every accumulated claim
        // under the global lock makes concurrent preparations exceed pool deadlines.
        let prepare_id = if let Command::PreparePayout { id, .. } = &command {
            Some(*id)
        } else {
            None
        };
        let policy_before = (!self.sqlite
            && (prepare_id.is_some() || matches!(command, Command::ExtractionQuota { .. })))
        .then(|| ledger.policy.clone());
        let mut exhausted = Vec::new();
        let result = domain::apply(&mut ledger, command);
        if result.is_ok()
            || result.as_ref().is_err_and(|e| {
                matches!(e.code, ErrorCode::DuplicateReceipt | ErrorCode::OverBudget)
            })
        {
            if let Some(before) = policy_before {
                if let (
                    Some(id),
                    Ok(Output::Prepared {
                        action: domain::PayoutAction::SendNew,
                        ..
                    }),
                ) = (prepare_id, &result)
                {
                    // PreparePayout changes exactly these rows and the daily counter.
                    // Existing intents do not write financial rows or recreate requests.
                    let mut changed = Ledger::default();
                    changed.claims.insert(id, ledger.claims[&id].clone());
                    changed.payouts.insert(id, ledger.payouts[&id].clone());
                    changed.requests.insert(id, ledger.requests[&id].clone());
                    save(&mut tx, &changed).await?;
                }
                for (key, count) in &ledger.policy {
                    if before.get(key) != Some(count) {
                        upsert(&mut tx, "policy_counters", key.clone(), &(key, count), &[]).await?;
                    }
                }
            } else {
                save(&mut tx, &ledger).await?;
                if let Some(id) = &reconcile_id {
                    let mut row = ledger.inbox[id].clone();
                    if row.attempts >= 8 && matches!(row.status.as_str(), "pending" | "unmatched") {
                        row.status = "exhausted".into();
                        if row.verified {
                            attribute_claim(&mut tx, &mut row).await;
                            exhausted.push((row.clone(), VIA_RECONCILE));
                        }
                    }
                    put_inbox(&mut tx, &row).await?;
                }
            }
            tx.commit().await.map_err(storage_error)?;
            self.emit_verified_exhausted(exhausted).await;
        } else {
            tx.rollback().await.map_err(storage_error)?;
        }
        result
    }
}
// Inbox policy. Admission is bounded by active rows and live payload bytes. Verified provider
// deliveries are never deleted: terminal ones move, body and headers intact, into
// webhook_inbox_archive after a hot window, which keeps the live inbox bounded. Dedupe lives in
// sandbox_events, and QueryDue reads applied evidence from both tables.
const ACTIVE_INBOX_CAP: i64 = 1000;
const INBOX_BYTES_CAP: i64 = 32 * 1024 * 1024;
const VERIFIED_TERMINAL: &str = "('applied','mismatch','ignored','replay','conflict')";
const INBOX_HOT_WINDOW: i64 = 86_400;
const ARCHIVE_BATCH: i64 = 1000;
// Unverified pending intake is bounded separately so that unauthenticated traffic cannot consume
// the whole active/byte allowance or the remote verification budget. Rows that already exist and
// verified rows are never shed.
const UNVERIFIED_BACKLOG_ROWS: i64 = 100;
const UNVERIFIED_BACKLOG_BYTES: i64 = 16 * 1024 * 1024;
const VERIFY_BUDGET_PER_MIN: i64 = 60;
const VERIFIED_RESERVED_PER_MIN: i64 = 10;
const VERIFIED_LITERAL: &str = "%\"verified\":true%";
// Correlated lane (MAJ-1 partial mitigation). Engineering proposals, not owner-approved policy.
// A delivery naming a payout we hold skips the unverified backlog shed, within these bounds. When
// any bound is hit the row is demoted to the ordinary lane (and shed there), never refused here.
// Correlation is NOT authentication: whoever knows a payout UUID can forge into this lane.
const CORRELATED_PER_PAYOUT: i64 = 8;
const CORRELATED_TOTAL_ROWS: i64 = 64;
const CORRELATED_TOTAL_BYTES: i64 = 4 * 1024 * 1024;
// Share of the per-minute verification budget that correlated, still unverified rows may use.
const CORRELATED_BUDGET_PER_MIN: i64 = 20;
const BUDGET_GLOBAL: i64 = 1;
const BUDGET_CORRELATED: i64 = 2;
const CORRELATED_LITERAL: &str = "%\"correlated\":true%";
// Minimum seconds between level-triggered outstanding lines.
const GAUGE_INTERVAL_SECS: i64 = 300;

// The payout UUID a delivery names, only for a PAYOUTS-ITEM event whose sender_item_id is the
// canonical lowercase hyphenated form we generate. Pure and bounded; it proves nothing about the
// sender.
pub(crate) fn correlated_payout(body: &[u8]) -> Option<uuid::Uuid> {
    if body.len() > SIGNAL_BODY_MAX {
        return None;
    }
    let event: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !event["event_type"]
        .as_str()?
        .starts_with(SIGNAL_TYPE_PREFIX)
    {
        return None;
    }
    let sender = event["resource"]["payout_item"]["sender_item_id"].as_str()?;
    let id = uuid::Uuid::parse_str(sender).ok()?;
    (id.to_string() == sender).then_some(id)
}

async fn payout_exists(
    tx: &mut Transaction<'_, Any>,
    payout: uuid::Uuid,
) -> Result<bool, ApiError> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payouts WHERE id = $1")
        .bind(payout.to_string())
        .fetch_one(&mut **tx)
        .await
        .map_err(storage_error)?;
    Ok(n > 0)
}

// True while the correlated lane has room for one more unverified row naming `payout` and
// `new_bytes` more bytes. Counts only pending or unmatched, still unverified, correlated rows. The
// text match is a prefilter; the decoded fields decide, and a miss can only demote, never admit.
async fn correlated_has_room(
    tx: &mut Transaction<'_, Any>,
    payout: uuid::Uuid,
    new_bytes: i64,
) -> Result<bool, ApiError> {
    let rows = sqlx::query(&format!("SELECT w.payload FROM inbox_schedule s JOIN webhook_inbox w ON w.id = s.id WHERE s.status IN ('pending','unmatched') AND w.payload LIKE '{CORRELATED_LITERAL}' LIMIT {}", ACTIVE_INBOX_CAP + 1))
        .fetch_all(&mut **tx).await.map_err(storage_error)?;
    let (mut total, mut same, mut bytes) = (0i64, 0i64, 0i64);
    for row in rows {
        let payload: String = row.try_get("payload").map_err(storage_error)?;
        let Ok(d) = serde_json::from_str::<crate::webhook::Delivery>(&payload) else {
            continue;
        };
        if d.correlated && !d.verified {
            total += 1;
            bytes += payload.len() as i64;
            if correlated_payout(&d.body) == Some(payout) {
                same += 1;
            }
        }
    }
    Ok(same < CORRELATED_PER_PAYOUT
        && total < CORRELATED_TOTAL_ROWS
        && bytes + new_bytes <= CORRELATED_TOTAL_BYTES)
}

// Starts the window row for `id` when the minute changed and returns what it has used.
async fn budget_used(tx: &mut Transaction<'_, Any>, id: i64, minute: i64) -> Result<i64, ApiError> {
    sqlx::query("INSERT INTO inbox_budget(id,minute,used) VALUES($1,$2,0) ON CONFLICT(id) DO UPDATE SET minute=excluded.minute,used=0 WHERE inbox_budget.minute <> excluded.minute").bind(id).bind(minute).execute(&mut **tx).await.map_err(storage_error)?;
    sqlx::query_scalar("SELECT used FROM inbox_budget WHERE id=$1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage_error)
}

// Sets the claim from the payouts table inside the transition's own transaction, so the archive
// carries the attribution. Best effort: a failed lookup leaves claim_id empty and does not stop
// the transition on SQLite. On Postgres a failed statement aborts the transaction, so the
// following write fails and the whole transition rolls back; the crash-recovery pass retries it.
async fn attribute_claim(tx: &mut Transaction<'_, Any>, d: &mut crate::webhook::Delivery) {
    if d.claim_id.is_some() {
        return;
    }
    let Some(payout) = signal_fields(d).payout else {
        return;
    };
    d.claim_id = sqlx::query_scalar::<_, String>("SELECT claim_id FROM payouts WHERE id = $1")
        .bind(payout.to_string())
        .fetch_optional(&mut **tx)
        .await
        .ok()
        .flatten()
        .and_then(|c| uuid::Uuid::parse_str(&c).ok())
        .map(ClaimId);
}

// Counts pending deliveries that are not verified, with their stored bytes. The SQL text match is
// only a prefilter: serde escapes quotes inside strings, so no string field can forge the literal,
// yet any row the filter calls verified is decoded and the authoritative `verified` field decides.
// Pending rows are bounded by the active cap, so the scan is bounded.
async fn unverified_backlog(tx: &mut Transaction<'_, Any>) -> Result<(i64, i64), ApiError> {
    let counted = sqlx::query(&format!("SELECT COUNT(*), COALESCE(SUM(LENGTH(w.payload)),0) FROM inbox_schedule s JOIN webhook_inbox w ON w.id = s.id WHERE s.status = 'pending' AND w.payload NOT LIKE '{VERIFIED_LITERAL}'"))
        .fetch_one(&mut **tx).await.map_err(storage_error)?;
    let mut rows: i64 = counted.try_get(0).map_err(storage_error)?;
    let mut bytes: i64 = counted.try_get(1).map_err(storage_error)?;
    let claimed = sqlx::query(&format!("SELECT w.payload FROM inbox_schedule s JOIN webhook_inbox w ON w.id = s.id WHERE s.status = 'pending' AND w.payload LIKE '{VERIFIED_LITERAL}' LIMIT {}", ACTIVE_INBOX_CAP + 1))
        .fetch_all(&mut **tx).await.map_err(storage_error)?;
    for row in claimed {
        let payload: String = row.try_get("payload").map_err(storage_error)?;
        let verified =
            serde_json::from_str::<crate::webhook::Delivery>(&payload).is_ok_and(|d| d.verified);
        if !verified {
            rows += 1;
            bytes += payload.len() as i64;
        }
    }
    Ok((rows, bytes))
}

// Verified-exhaustion signal: one bounded key=value line, written after the transition has
// committed. It carries only server-side identifiers and sanitized, length-limited tokens, never
// the body, headers, signature, payee or amounts.
type SignalSink = Arc<dyn Fn(&str) + Send + Sync>;
const VIA_QUERY_FAILED: &str = "query_failed";
const VIA_RECONCILE: &str = "reconcile_unmatched";
const VIA_RECOVERY: &str = "crash_recovery";
const SIGNAL_TOKEN_MAX: usize = 64;
const SIGNAL_BODY_MAX: usize = 256 * 1024;
const SIGNAL_TYPE_PREFIX: &str = "PAYMENT.PAYOUTS-ITEM.";

fn stderr_sink() -> SignalSink {
    Arc::new(|line| write_signal(&mut std::io::stderr().lock(), line))
}
// A closed or full stderr must never panic or fail an already committed operation, so unlike
// eprintln! write errors are dropped.
fn write_signal(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}
fn safe_token(s: &str, extra: &[char]) -> Option<String> {
    (!s.is_empty()
        && s.len() <= SIGNAL_TOKEN_MAX
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') || extra.contains(&c)
        }))
    .then(|| s.to_owned())
}
struct SignalFields {
    event: String,
    kind: String,
    payout: Option<uuid::Uuid>,
}
// Reads the decoded delivery body, already bounded by intake. Every value is validated or
// replaced by a fixed word, so nothing attacker-shaped reaches the log.
fn signal_fields(d: &crate::webhook::Delivery) -> SignalFields {
    let body = (d.body.len() <= SIGNAL_BODY_MAX)
        .then(|| serde_json::from_slice::<serde_json::Value>(&d.body).ok())
        .flatten();
    let text = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str().map(str::to_owned));
    let event = match text(body.as_ref().map(|b| &b["id"])) {
        None => "none".to_owned(),
        Some(id) => safe_token(&id, &[':']).unwrap_or_else(|| "invalid".into()),
    };
    let kind = text(body.as_ref().map(|b| &b["event_type"]))
        .filter(|t| t.starts_with(SIGNAL_TYPE_PREFIX))
        .and_then(|t| safe_token(&t, &[]))
        .unwrap_or_else(|| "other".into());
    let payout = text(
        body.as_ref()
            .map(|b| &b["resource"]["payout_item"]["sender_item_id"]),
    )
    .and_then(|s| uuid::Uuid::parse_str(&s).ok());
    SignalFields {
        event,
        kind,
        payout,
    }
}
// Pure formatter. Returns None unless the decoded row is verified, so forged traffic can never
// write to the log.
fn verified_exhausted_line(
    d: &crate::webhook::Delivery,
    via: &str,
    payout: Option<uuid::Uuid>,
    claim: Option<uuid::Uuid>,
) -> Option<String> {
    if !d.verified {
        return None;
    }
    let f = signal_fields(d);
    let id = |u: Option<uuid::Uuid>, none: &str| u.map_or(none.to_owned(), |u| u.to_string());
    Some(format!(
        "webhook_verified_exhausted delivery={} event={} type={} via={} attempts={} received_at={} payout={} claim={}",
        safe_token(&d.id, &[]).unwrap_or_else(|| "invalid".into()),
        f.event,
        f.kind,
        via,
        d.attempts,
        d.received_at,
        id(payout, "none"),
        id(claim, "unknown"),
    ))
}

async fn live_bytes(tx: &mut Transaction<'_, Any>, sqlite: bool) -> Result<i64, ApiError> {
    // Count bytes in SQL; do not transfer and deserialize ordinary pending bodies (which
    // serialize as large integer arrays). Fetch only possible terminal envelopes for the
    // exact Rust exclusion check. An escaped status necessarily contains a backslash, so
    // the fallback preserves noncanonical JSON too. Substring matches are candidates only:
    // malformed, verified and uncleared payloads still count in full.
    let total_sql = if sqlite {
        "SELECT COALESCE(SUM(LENGTH(CAST(payload AS BLOB))),0) FROM webhook_inbox"
    } else {
        "SELECT COALESCE(SUM(OCTET_LENGTH(payload)),0) FROM webhook_inbox"
    };
    let total: i64 = sqlx::query_scalar(total_sql)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage_error)?;
    let candidates_sql = if sqlite {
        "SELECT payload FROM webhook_inbox WHERE INSTR(payload,'rejected')>0 OR INSTR(payload,'exhausted')>0 OR INSTR(payload,CHAR(92))>0"
    } else {
        "SELECT payload FROM webhook_inbox WHERE STRPOS(payload,'rejected')>0 OR STRPOS(payload,'exhausted')>0 OR STRPOS(payload,CHR(92))>0"
    };
    let payloads: Vec<String> = sqlx::query_scalar(candidates_sql)
        .fetch_all(&mut **tx)
        .await
        .map_err(storage_error)?;
    let excluded: i64 = payloads
        .iter()
        .filter(|payload| {
            serde_json::from_str::<crate::webhook::Delivery>(payload).is_ok_and(|d| {
                !d.verified
                    && matches!(d.status.as_str(), "rejected" | "exhausted")
                    && d.body.is_empty()
                    && d.headers.is_empty()
            })
        })
        .map(|payload| payload.len() as i64)
        .sum();
    Ok(total - excluded)
}

/// Copies a bounded batch of verified, finished deliveries into the archive and removes from the
/// live inbox only rows whose archived payload is byte-identical. A crash or retry at any point
/// leaves every row in at least one table, and repeating the call converges.
async fn archive_verified(
    tx: &mut Transaction<'_, Any>,
    received_before: i64,
    now: i64,
) -> Result<(), ApiError> {
    sqlx::query(&format!("INSERT INTO webhook_inbox_archive(id,payload,status,received,claim_id,archived_at) SELECT w.id,w.payload,s.status,s.received,s.claim_id,$2 FROM webhook_inbox w JOIN inbox_schedule s ON s.id = w.id WHERE w.payload LIKE '{VERIFIED_LITERAL}' AND ((s.status IN {VERIFIED_TERMINAL} AND s.received < $1) OR (s.status = 'exhausted' AND w.payload LIKE '%\"verified\":true%')) AND w.id NOT IN (SELECT id FROM webhook_inbox_archive) ORDER BY s.received, s.id LIMIT {ARCHIVE_BATCH}"))
        .bind(received_before).bind(now).execute(&mut **tx).await.map_err(storage_error)?;
    let copies = sqlx::query("SELECT w.id,w.payload FROM webhook_inbox_archive a JOIN webhook_inbox w ON w.id=a.id WHERE a.payload=w.payload")
        .fetch_all(&mut **tx).await.map_err(storage_error)?;
    for row in copies {
        let payload: String = row.try_get("payload").map_err(storage_error)?;
        if serde_json::from_str::<crate::webhook::Delivery>(&payload).is_ok_and(|d| d.verified) {
            let id: String = row.try_get("id").map_err(storage_error)?;
            sqlx::query("DELETE FROM webhook_inbox WHERE id=$1")
                .bind(id)
                .execute(&mut **tx)
                .await
                .map_err(storage_error)?;
        }
    }
    Ok(())
}

async fn inbox_row(
    tx: &mut Transaction<'_, Any>,
    id: &str,
) -> Result<crate::webhook::Delivery, ApiError> {
    let payload: String = sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id = $1")
        .bind(id)
        .fetch_one(&mut **tx)
        .await
        .map_err(storage_error)?;
    serde_json::from_str(&payload).map_err(storage_error)
}
async fn put_inbox(
    tx: &mut Transaction<'_, Any>,
    d: &crate::webhook::Delivery,
) -> Result<(), ApiError> {
    let mut d = d.clone();
    // Only unverified history is redacted. A verified delivery is provider audit evidence.
    if matches!(d.status.as_str(), "rejected" | "exhausted") && !d.verified {
        d.body.clear();
        d.headers.clear();
    }
    upsert(tx, "webhook_inbox", d.id.clone(), &d, &[]).await?;
    sqlx::query("INSERT INTO inbox_schedule(id,status,received,due,attempts,claim_id) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(id) DO UPDATE SET status=excluded.status,due=excluded.due,attempts=excluded.attempts,claim_id=excluded.claim_id")
        .bind(&d.id).bind(&d.status).bind(d.received_at).bind(d.next_attempt).bind(i64::from(d.attempts)).bind(d.claim_id.map(|id| id.to_string())).execute(&mut **tx).await.map_err(storage_error)?;
    Ok(())
}
impl SqlStore {
    // Called only after the transition committed. Attribution is best effort: a failed or empty
    // lookup yields claim=unknown and can neither fail nor undo the stored evidence.
    async fn emit_verified_exhausted(&self, rows: Vec<(crate::webhook::Delivery, &'static str)>) {
        for (d, via) in rows {
            if !d.verified {
                continue;
            }
            let payout = signal_fields(&d).payout;
            // Attribution committed with the transition wins; the lookup is only a fallback.
            let claim = match payout {
                _ if d.claim_id.is_some() => d.claim_id.map(|c| c.0),
                Some(p) => {
                    sqlx::query_scalar::<_, String>("SELECT claim_id FROM payouts WHERE id = $1")
                        .bind(p.to_string())
                        .fetch_optional(&self.pool)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|c| uuid::Uuid::parse_str(&c).ok())
                }
                None => None,
            };
            if let Some(line) = verified_exhausted_line(&d, via, payout, claim) {
                (self.signal)(&line);
            }
        }
    }
    async fn payout_known(&self, payout: uuid::Uuid) -> Result<bool, ApiError> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM payouts WHERE id = $1")
            .bind(payout.to_string())
            .fetch_one(&self.pool)
            .await
            .map_err(storage_error)?;
        Ok(n > 0)
    }
    // One bounded line: how many verified deliveries sit exhausted, how many of those have no
    // claim, and the oldest. Read-only, no schema or retention change, and it repeats until the
    // rows are gone, so one lost line no longer hides the state. Nothing yet clears a row; see
    // docs/H5-P6F-STATIC-REPAIR.md (open question on acknowledgement).
    async fn report_verified_exhausted(&self, now: i64) -> Result<(), ApiError> {
        let last = self.gauge_at.load(Ordering::Relaxed);
        if last != i64::MIN && now.saturating_sub(last) < GAUGE_INTERVAL_SECS {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(storage_error)?;
        if !self.sqlite {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
                .execute(&mut *tx)
                .await
                .map_err(storage_error)?;
        }
        let mut count = 0i64;
        let mut unattributed = 0i64;
        let mut oldest: Option<i64> = None;
        for sql in [
            format!("SELECT COUNT(*), COALESCE(SUM(CASE WHEN s.claim_id IS NULL THEN 1 ELSE 0 END),0), COALESCE(MIN(s.received),0) FROM inbox_schedule s JOIN webhook_inbox w ON w.id = s.id WHERE s.status = 'exhausted' AND w.payload LIKE '{VERIFIED_LITERAL}'"),
            format!("SELECT COUNT(*), COALESCE(SUM(CASE WHEN claim_id IS NULL THEN 1 ELSE 0 END),0), COALESCE(MIN(received),0) FROM webhook_inbox_archive WHERE status = 'exhausted' AND payload LIKE '{VERIFIED_LITERAL}'"),
        ] {
            let row = sqlx::query(&sql)
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
            let n: i64 = row.try_get(0).map_err(storage_error)?;
            count += n;
            unattributed += row.try_get::<i64, _>(1).map_err(storage_error)?;
            if n > 0 {
                let first: i64 = row.try_get(2).map_err(storage_error)?;
                oldest = Some(oldest.map_or(first, |o| o.min(first)));
            }
        }
        tx.rollback().await.map_err(storage_error)?;
        if count > 0 {
            self.gauge_at.store(now, Ordering::Relaxed);
            (self.signal)(&format!(
                "webhook_verified_exhausted_outstanding count={count} unattributed={unattributed} oldest_received_at={}",
                oldest.unwrap_or(0)
            ));
        }
        Ok(())
    }
    #[cfg(test)]
    async fn make_inbox_due(&self) -> Result<(), ApiError> {
        let mut tx = self.begin_locked().await?;
        let ids =
            sqlx::query("SELECT id FROM inbox_schedule WHERE status IN ('pending','unmatched')")
                .fetch_all(&mut *tx)
                .await
                .map_err(storage_error)?;
        for row in ids {
            let mut d = inbox_row(
                &mut tx,
                &row.try_get::<String, _>("id").map_err(storage_error)?,
            )
            .await?;
            d.next_attempt = 0;
            put_inbox(&mut tx, &d).await?;
        }
        tx.commit().await.map_err(storage_error)?;
        Ok(())
    }
    /// Automatic deletion requires ALL: never verified, rejected/exhausted, body and headers
    /// already cleared, and received more than 24h ago. Verified events are never deleted;
    /// live verified rows move only to an identical archive copy. Archive rows are never pruned.
    async fn inbox(&self, command: Command, now: i64) -> Result<Output, ApiError> {
        let pending_read = matches!(command, Command::InboxPending);
        let mut exhausted = Vec::new();
        let mut tx = if pending_read {
            self.pool.begin().await.map_err(storage_error)?
        } else {
            self.begin_locked().await?
        };
        // Maintenance is explicit; listing eligible deliveries remains read-only.
        if !pending_read {
            // Recover a crash after reserving the last attempt. put_inbox redacts exhausted rows.
            if matches!(command, Command::InboxPrune) {
                let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM inbox_schedule WHERE status IN ('pending','unmatched') AND attempts >= 8 AND due <= $1")
                .bind(now).fetch_all(&mut *tx).await.map_err(storage_error)?;
                for id in ids {
                    let mut row = inbox_row(&mut tx, &id).await?;
                    row.status = "exhausted".into();
                    if row.verified {
                        attribute_claim(&mut tx, &mut row).await;
                        exhausted.push((row.clone(), VIA_RECOVERY));
                    }
                    put_inbox(&mut tx, &row).await?;
                }
                // Repeat until a pass moves nothing, so a backlog above one batch still drains.
                loop {
                    let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox")
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                    archive_verified(&mut tx, now - INBOX_HOT_WINDOW, now).await?;
                    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox")
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                    if after == before {
                        break;
                    }
                }
            }
            // Delete only never-verified, rejected/exhausted rows with body AND headers already
            // cleared, received more than 24h ago. Decode explicitly: missing/malformed state
            // is not permission to delete. Verified evidence only moves to an identical archive;
            // no prune path deletes archive rows.
            let rows = sqlx::query("SELECT w.id,w.payload FROM webhook_inbox w JOIN inbox_schedule s ON s.id=w.id WHERE s.status IN ('rejected','exhausted') AND s.received < $1")
                .bind(now - 86400).fetch_all(&mut *tx).await.map_err(storage_error)?;
            for row in rows {
                let payload: String = row.try_get("payload").map_err(storage_error)?;
                if let Ok(d) = serde_json::from_str::<crate::webhook::Delivery>(&payload) {
                    if !d.verified
                        && matches!(d.status.as_str(), "rejected" | "exhausted")
                        && d.body.is_empty()
                        && d.headers.is_empty()
                    {
                        let id: String = row.try_get("id").map_err(storage_error)?;
                        sqlx::query("DELETE FROM webhook_inbox WHERE id=$1")
                            .bind(id)
                            .execute(&mut *tx)
                            .await
                            .map_err(storage_error)?;
                    }
                }
            }
            sqlx::query(
                "DELETE FROM inbox_schedule WHERE NOT EXISTS (SELECT 1 FROM webhook_inbox w WHERE w.id=inbox_schedule.id)",
            )
            .execute(&mut *tx)
            .await
            .map_err(storage_error)?;
        }
        let result = match command {
            Command::InboxPrune => Output::Reconciled("pruned".into()),
            Command::Inbox(mut d) => {
                let exists: i64 =
                    sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox WHERE id = $1")
                        .bind(&d.id)
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                // Shed brand-new unverified work before it is written, so a flood neither fills
                // the inbox nor spends verification budget. Existing and verified rows pass.
                if exists == 0 && !d.verified && d.status == "pending" {
                    // Classify here, under the lock, from the local payouts table. The incoming
                    // flag is never trusted. A row that names a payout we hold and fits the
                    // correlated bounds skips the shed below; anything else is shed as before.
                    let new_bytes = serde_json::to_string(&d).map_err(storage_error)?.len() as i64;
                    d.correlated = false;
                    if let Some(payout) = correlated_payout(&d.body) {
                        d.correlated = payout_exists(&mut tx, payout).await?
                            && correlated_has_room(&mut tx, payout, new_bytes).await?;
                    }
                    let (rows, bytes) = if d.correlated {
                        (0, 0)
                    } else {
                        unverified_backlog(&mut tx).await?
                    };
                    if rows >= UNVERIFIED_BACKLOG_ROWS
                        || bytes + new_bytes > UNVERIFIED_BACKLOG_BYTES
                    {
                        return Err(ApiError::new(
                            ErrorCode::AdapterUnavailable,
                            "webhook intake backlog full",
                        ));
                    }
                }
                // Only work still awaiting verification or matching consumes admission slots;
                // terminal history is bounded by retention and never blocks new deliveries.
                let active: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM inbox_schedule WHERE status IN ('pending','unmatched')",
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(storage_error)?;
                let previous: Option<String> =
                    sqlx::query_scalar("SELECT payload FROM webhook_inbox WHERE id = $1")
                        .bind(&d.id)
                        .fetch_optional(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                let previous_bytes = previous.map_or(0, |p| {
                    if serde_json::from_str::<crate::webhook::Delivery>(&p).is_ok_and(|old| {
                        !old.verified
                            && matches!(old.status.as_str(), "rejected" | "exhausted")
                            && old.body.is_empty()
                            && old.headers.is_empty()
                    }) {
                        0
                    } else {
                        p.len() as i64
                    }
                });
                // put_inbox clears unverified terminal content before storing it.
                let new_bytes =
                    if !d.verified && matches!(d.status.as_str(), "rejected" | "exhausted") {
                        0
                    } else {
                        serde_json::to_string(&d).map_err(storage_error)?.len() as i64
                    };
                let mut bytes = live_bytes(&mut tx, self.sqlite).await?;
                // Verified history inside the hot window must not close intake. Under byte
                // pressure it moves to the archive early instead of being refused or deleted.
                if new_bytes > previous_bytes
                    && bytes - previous_bytes + new_bytes > INBOX_BYTES_CAP
                {
                    for _ in 0..64 {
                        archive_verified(&mut tx, i64::MAX, now).await?;
                        let moved = bytes;
                        bytes = live_bytes(&mut tx, self.sqlite).await?;
                        if bytes == moved || bytes - previous_bytes + new_bytes <= INBOX_BYTES_CAP {
                            break;
                        }
                    }
                }
                if (exists == 0 && active >= ACTIVE_INBOX_CAP)
                    || (new_bytes > previous_bytes
                        && bytes - previous_bytes + new_bytes > INBOX_BYTES_CAP)
                {
                    return Err(ApiError::new(
                        ErrorCode::AdapterUnavailable,
                        "webhook inbox capacity reached",
                    ));
                }
                put_inbox(&mut tx, &d).await?;
                Output::Reconciled("stored".into())
            }
            Command::InboxPending => {
                let ids = sqlx::query("SELECT id FROM inbox_schedule WHERE status IN ('pending','unmatched') AND due <= $1 AND attempts < 8 ORDER BY received,id LIMIT 100").bind(now).fetch_all(&mut *tx).await.map_err(storage_error)?;
                let mut rows = Vec::new();
                for id in ids {
                    rows.push(
                        inbox_row(
                            &mut tx,
                            &id.try_get::<String, _>("id").map_err(storage_error)?,
                        )
                        .await?,
                    );
                }
                Output::Inbox(rows)
            }
            Command::InboxAttempt(id) => {
                let mut d = inbox_row(&mut tx, &id).await?;
                if !matches!(d.status.as_str(), "pending" | "unmatched")
                    || d.attempts >= 8
                    || d.next_attempt > now
                {
                    Output::Inbox(vec![])
                } else {
                    let used = budget_used(&mut tx, BUDGET_GLOBAL, now / 60).await?;
                    // Unverified rows may use all but the reserved slots; verified rows never call
                    // the remote verifier and may use the whole minute. A correlated, still
                    // unverified row also may use the reserve, but only up to its own sub-budget,
                    // so forged correlated traffic cannot take more than that share of the minute.
                    let correlated = d.correlated && !d.verified;
                    let ceiling = if d.verified || d.correlated {
                        VERIFY_BUDGET_PER_MIN
                    } else {
                        VERIFY_BUDGET_PER_MIN - VERIFIED_RESERVED_PER_MIN
                    };
                    let lane_used = if correlated {
                        budget_used(&mut tx, BUDGET_CORRELATED, now / 60).await?
                    } else {
                        0
                    };
                    if used >= ceiling || (correlated && lane_used >= CORRELATED_BUDGET_PER_MIN) {
                        return Err(ApiError::new(
                            ErrorCode::AdapterUnavailable,
                            "verification budget exhausted",
                        ));
                    }
                    if correlated {
                        sqlx::query("UPDATE inbox_budget SET used=used+1 WHERE id=$1")
                            .bind(BUDGET_CORRELATED)
                            .execute(&mut *tx)
                            .await
                            .map_err(storage_error)?;
                    }
                    sqlx::query("UPDATE inbox_budget SET used=used+1 WHERE id=$1")
                        .bind(BUDGET_GLOBAL)
                        .execute(&mut *tx)
                        .await
                        .map_err(storage_error)?;
                    d.attempts += 1;
                    d.next_attempt = now + 60 * (1i64 << (d.attempts - 1));

                    put_inbox(&mut tx, &d).await?;
                    Output::Inbox(vec![d])
                }
            }
            Command::InboxFailure(id) => {
                let mut d = inbox_row(&mut tx, &id).await?;
                if d.attempts >= 8 && matches!(d.status.as_str(), "pending" | "unmatched") {
                    d.status = "exhausted".into();
                    if d.verified {
                        attribute_claim(&mut tx, &mut d).await;
                        exhausted.push((d.clone(), VIA_QUERY_FAILED));
                    }
                    put_inbox(&mut tx, &d).await?;
                }
                Output::Reconciled("deferred".into())
            }
            Command::InboxVerified(id) => {
                let mut d = inbox_row(&mut tx, &id).await?;
                d.verified = true;
                put_inbox(&mut tx, &d).await?;
                Output::Reconciled("verified".into())
            }
            _ => unreachable!(),
        };
        tx.commit().await.map_err(storage_error)?;
        self.emit_verified_exhausted(exhausted).await;
        Ok(result)
    }
}

async fn rows<T: DeserializeOwned>(
    tx: &mut Transaction<'_, Any>,
    table: &str,
) -> Result<Vec<T>, ApiError> {
    let rows = sqlx::query(&format!("SELECT payload FROM {table} ORDER BY id"))
        .fetch_all(&mut **tx)
        .await
        .map_err(storage_error)?;
    rows.into_iter()
        .map(|row| {
            let payload: String = row.try_get("payload").map_err(storage_error)?;
            serde_json::from_str(&payload).map_err(storage_error)
        })
        .collect()
}
async fn load(tx: &mut Transaction<'_, Any>) -> Result<Ledger, ApiError> {
    let mut ledger = Ledger::default();
    for row in rows::<BudgetLine>(tx, "budget_lines").await? {
        ledger.budgets.insert(row.id, row);
    }
    for row in rows::<Payee>(tx, "payees").await? {
        ledger.payees.insert(row.id, row);
    }
    for row in rows::<Claim>(tx, "claims").await? {
        ledger.claims.insert(row.id, row);
    }
    for row in rows::<Approval>(tx, "approvals").await? {
        ledger.approvals.insert(row.claim_id, row);
    }
    for row in rows::<Payout>(tx, "payouts").await? {
        ledger.payouts.insert(row.claim_id, row);
    }
    for row in rows::<StoredEvent>(tx, "payout_events").await? {
        ledger.events.insert(row.event.paypal_event_id.clone(), row);
    }
    for (id, text) in rows::<(ClaimId, String)>(tx, "receipt_sources").await? {
        ledger.receipts.insert(id, text);
    }
    for (id, request) in rows::<(ClaimId, SubmitClaimRequest)>(tx, "submitted_requests").await? {
        ledger.submissions.insert(id, request);
    }
    for (key, count) in rows::<(String, i64)>(tx, "policy_counters").await? {
        ledger.policy.insert(key, count);
    }
    for request in rows::<domain::ImmutableRequest>(tx, "immutable_requests").await? {
        ledger.requests.insert(request.claim_id, request);
    }
    for (id, evidence) in rows::<(String, serde_json::Value)>(tx, "provider_evidence").await? {
        ledger.evidence.insert(id, evidence);
    }
    for (key, hash) in rows::<(String, String)>(tx, "sandbox_events").await? {
        ledger.sandbox_events.insert(key, hash);
    }
    Ok(ledger)
}
async fn upsert<T: Serialize>(
    tx: &mut Transaction<'_, Any>,
    table: &str,
    id: String,
    row: &T,
    extra: &[(&str, String)],
) -> Result<(), ApiError> {
    let payload = serde_json::to_string(row).map_err(storage_error)?;
    let mut columns = vec!["id", "payload"];
    columns.extend(extra.iter().map(|(key, _)| *key));
    let placeholders = (1..=columns.len())
        .map(|n| format!("${n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("INSERT INTO {table} ({}) VALUES ({placeholders}) ON CONFLICT (id) DO UPDATE SET payload = excluded.payload", columns.join(", "));
    let mut query = sqlx::query(&sql).bind(id).bind(payload);
    for (_, value) in extra {
        query = query.bind(value.clone());
    }
    query.execute(&mut **tx).await.map_err(storage_error)?;
    Ok(())
}
async fn save(tx: &mut Transaction<'_, Any>, ledger: &Ledger) -> Result<(), ApiError> {
    for (key, hash) in &ledger.sandbox_events {
        upsert(tx, "sandbox_events", key.clone(), &(key, hash), &[]).await?;
    }
    for (id, evidence) in &ledger.evidence {
        upsert(tx, "provider_evidence", id.clone(), &(id, evidence), &[]).await?;
    }
    for (id, request) in &ledger.requests {
        upsert(tx, "immutable_requests", id.to_string(), request, &[]).await?;
    }
    for (key, count) in &ledger.policy {
        upsert(tx, "policy_counters", key.clone(), &(key, count), &[]).await?;
    }
    for row in ledger.budgets.values() {
        upsert(tx, "budget_lines", row.id.to_string(), row, &[]).await?;
    }
    for row in ledger.payees.values() {
        upsert(tx, "payees", row.id.to_string(), row, &[]).await?;
    }
    for row in ledger.claims.values() {
        upsert(
            tx,
            "claims",
            row.id.to_string(),
            row,
            &[("budget_line_id", row.budget_line_id.to_string())],
        )
        .await?;
    }
    for row in ledger.approvals.values() {
        upsert(
            tx,
            "approvals",
            row.id.to_string(),
            row,
            &[("claim_id", row.claim_id.to_string())],
        )
        .await?;
    }
    for row in ledger.payouts.values() {
        upsert(
            tx,
            "payouts",
            row.id.to_string(),
            row,
            &[
                ("claim_id", row.claim_id.to_string()),
                ("approval_id", row.approval_id.to_string()),
                ("sender_batch_id", row.sender_batch_id.clone()),
            ],
        )
        .await?;
    }
    for row in ledger.events.values() {
        upsert(
            tx,
            "payout_events",
            row.event.paypal_event_id.clone(),
            row,
            &[("payout_id", row.event.payout_id.to_string())],
        )
        .await?;
    }
    for (id, text) in &ledger.receipts {
        upsert(tx, "receipt_sources", id.to_string(), &(id, text), &[]).await?;
    }
    for (id, request) in &ledger.submissions {
        upsert(
            tx,
            "submitted_requests",
            id.to_string(),
            &(id, request),
            &[],
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BUDGET_ID, CLAIMANT_ID, EVENT_ID};

    #[tokio::test]
    async fn reads_leave_lock_version_unchanged() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let id = ClaimId::new();
        store
            .execute(Command::Create {
                id,
                request: CreateClaimRequest {
                    event_id: EVENT_ID,
                    claimant_id: CLAIMANT_ID,
                    budget_line_id: BUDGET_ID,
                    receipt_text: "Synthetic receipt".into(),
                },
            })
            .await
            .unwrap();
        let version =
            || sqlx::query_scalar::<_, i64>("SELECT version FROM ledger_lock WHERE id = 1");
        let before = version().fetch_one(&store.0.pool).await.unwrap();
        for command in [
            Command::Get(id),
            Command::Receipt(id),
            Command::Budgets(EVENT_ID),
            Command::Payees,
            Command::Claims(EVENT_ID),
            Command::Review(id),
            Command::InboxPending,
        ] {
            store.execute(command).await.unwrap();
            assert_eq!(version().fetch_one(&store.0.pool).await.unwrap(), before);
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[tokio::test]
    async fn extraction_counters_are_durable_and_reset_at_taipei_midnight() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("quota.db").display()
        );
        let store = SqliteStore::connect(&url).await.unwrap();
        for _ in 0..10 {
            store
                .execute(Command::ExtractionQuota {
                    account: "demo-claimant".into(),
                    now: 0,
                })
                .await
                .unwrap();
        }
        drop(store);
        let store = SqliteStore::connect(&url).await.unwrap();
        assert!(store
            .execute(Command::ExtractionQuota {
                account: "demo-claimant".into(),
                now: 1
            })
            .await
            .is_err());
        for account in 0..4 {
            for _ in 0..10 {
                store
                    .execute(Command::ExtractionQuota {
                        account: format!("future-account-{account}"),
                        now: 1,
                    })
                    .await
                    .unwrap();
            }
        }
        assert!(store
            .execute(Command::ExtractionQuota {
                account: "demo-finance".into(),
                now: 1
            })
            .await
            .is_err());
        store
            .execute(Command::ExtractionQuota {
                account: "demo-claimant".into(),
                now: 16 * 3600,
            })
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod inbox_tests {
    use super::*;
    use crate::webhook::Delivery;
    use axum::http::HeaderMap;
    #[tokio::test]
    async fn inbox_oldest_first_backoff_attempt_cap_and_budget() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let mut newer = Delivery::new(b"{}", &HeaderMap::new());
        newer.received_at = 20;
        let mut older = newer.clone();
        older.id = "older".into();
        older.received_at = 10;
        for d in [newer.clone(), older.clone()] {
            store.0.inbox(Command::Inbox(d), 100).await.unwrap();
        }
        let Output::Inbox(rows) = store.0.inbox(Command::InboxPending, 100).await.unwrap() else {
            panic!()
        };
        assert_eq!(rows[0].id, older.id);
        let Output::Inbox(rows) = store
            .0
            .inbox(Command::InboxAttempt(older.id.clone()), 100)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(rows[0].next_attempt, 160);
        let Output::Inbox(rows) = store
            .0
            .inbox(Command::InboxAttempt(older.id.clone()), 101)
            .await
            .unwrap()
        else {
            panic!()
        };
        assert!(rows.is_empty());
        store
            .0
            .inbox(Command::InboxVerified(older.id.clone()), 101)
            .await
            .unwrap();
        let mut tx = store.0.begin_locked().await.unwrap();
        let mut d = inbox_row(&mut tx, &older.id).await.unwrap();
        assert!(d.verified);
        d.attempts = 8;
        put_inbox(&mut tx, &d).await.unwrap();
        tx.commit().await.unwrap();
        store
            .0
            .inbox(Command::InboxFailure(older.id.clone()), 101)
            .await
            .unwrap();
        sqlx::query("UPDATE inbox_budget SET used=60")
            .execute(&store.0.pool)
            .await
            .unwrap();
        assert!(store
            .0
            .inbox(Command::InboxAttempt(newer.id.clone()), 101)
            .await
            .is_err());
        assert!(store
            .0
            .inbox(Command::InboxAttempt(newer.id.clone()), 180)
            .await
            .is_ok());
    }
    #[tokio::test]
    async fn inbox_capacity_rejects_before_insert_and_rejections_are_redacted_pruned() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let mut d = Delivery::new(b"private body", &HeaderMap::new());
        store.execute(Command::Inbox(d.clone())).await.unwrap();
        store
            .execute(Command::Reconcile {
                delivery_id: d.id.clone(),
                verified: false,
                confirmation: None,
            })
            .await
            .unwrap();
        let mut tx = store.0.begin_locked().await.unwrap();
        let row = inbox_row(&mut tx, &d.id).await.unwrap();
        assert!(row.body.is_empty());
        assert!(row.headers.is_empty());
        assert_eq!(row.hash, d.hash);
        tx.commit().await.unwrap();
        store
            .0
            .inbox(Command::InboxPrune, d.received_at + 86401)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox")
            .fetch_one(&store.0.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        let mut tx = store.0.begin_locked().await.unwrap();
        for n in 0..1000 {
            d.id = format!("cap-{n}");
            put_inbox(&mut tx, &d).await.unwrap();
        }
        tx.commit().await.unwrap();
        d.id = "overflow".into();
        assert_eq!(
            store.execute(Command::Inbox(d)).await.unwrap_err().code,
            ErrorCode::AdapterUnavailable
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM webhook_inbox")
            .fetch_one(&store.0.pool)
            .await
            .unwrap();
        assert_eq!(count, 1000);
        // A single retained large payload also closes intake before row-count cap.
        sqlx::query("DELETE FROM webhook_inbox WHERE id <> 'cap-0'")
            .execute(&store.0.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM inbox_schedule WHERE id <> 'cap-0'")
            .execute(&store.0.pool)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE webhook_inbox SET payload = printf('%.*c', 33554432, 'x') WHERE id='cap-0'",
        )
        .execute(&store.0.pool)
        .await
        .unwrap();
        assert_eq!(
            store
                .execute(Command::Inbox(Delivery::new(b"{}", &HeaderMap::new())))
                .await
                .unwrap_err()
                .code,
            ErrorCode::AdapterUnavailable
        );
    }
    #[tokio::test]
    async fn ledger_reads_ignore_inbox_payload_and_same_day_reset_does_not_write() {
        let store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        store.execute(Command::ResetDemo { now: 0 }).await.unwrap();
        let before: i64 = sqlx::query_scalar("SELECT version FROM ledger_lock")
            .fetch_one(&store.0.pool)
            .await
            .unwrap();
        store.execute(Command::ResetDemo { now: 1 }).await.unwrap();
        let after: i64 = sqlx::query_scalar("SELECT version FROM ledger_lock")
            .fetch_one(&store.0.pool)
            .await
            .unwrap();
        assert_eq!(before, after);
        sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES('corrupt','invalid json')")
            .execute(&store.0.pool)
            .await
            .unwrap();
        store.execute(Command::Payees).await.unwrap();
        store
            .execute(Command::ResetDemo { now: 86400 })
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod p6e_tests {
    use super::*;
    use crate::webhook::Delivery;
    use axum::http::HeaderMap;
    use std::sync::Mutex;

    // A whole minute, so budget assertions never straddle a boundary.
    const T0: i64 = 60 * 16_666_667;
    const PAYOUT: &str = "6f1c1f0e-0d6e-4b34-9a52-6d7f0f6a2c11";
    const CLAIM: &str = "0b6c5a2e-4d1b-4f0e-8d57-3a1f7e9c4b22";

    type Lines = Arc<Mutex<Vec<String>>>;

    async fn store() -> (SqliteStore, Lines) {
        let mut store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let lines: Lines = Arc::default();
        let sink = lines.clone();
        store.0.signal = Arc::new(move |line| sink.lock().unwrap().push(line.to_owned()));
        (store, lines)
    }
    fn delivery(n: usize) -> Delivery {
        let mut d = Delivery::new(b"{}", &HeaderMap::new());
        d.id = format!("d-{n}");
        d.received_at = n as i64;
        d
    }
    fn event_body(event_id: &str, payout: &str) -> Vec<u8> {
        serde_json::json!({
            "id": event_id,
            "event_type": "PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
            "resource": {
                "payout_item": {"sender_item_id": payout, "receiver": "SECRET-MARKER@example.test"},
                "payout_batch_id": "BATCH-1"
            }
        })
        .to_string()
        .into_bytes()
    }
    fn verified_event(n: usize, event_id: &str, payout: &str) -> Delivery {
        let mut d = delivery(n);
        d.body = event_body(event_id, payout);
        d.headers = vec![("paypal-transmission-sig".into(), b"SECRET-MARKER".to_vec())];
        d.verified = true;
        d.attempts = 8;
        d
    }
    async fn put(store: &SqliteStore, d: Delivery) -> Result<Output, ApiError> {
        store.0.inbox(Command::Inbox(d), T0).await
    }
    async fn attempt(store: &SqliteStore, id: &str, now: i64) -> Result<bool, ApiError> {
        match store.0.inbox(Command::InboxAttempt(id.into()), now).await? {
            Output::Inbox(rows) => Ok(!rows.is_empty()),
            _ => unreachable!(),
        }
    }
    async fn fill(store: &SqliteStore, n: usize) {
        for i in 0..n {
            put(store, delivery(i)).await.unwrap();
        }
    }
    async fn count(store: &SqliteStore, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&store.0.pool)
            .await
            .unwrap()
    }
    async fn used(store: &SqliteStore) -> i64 {
        count(store, "SELECT COALESCE(SUM(used),0) FROM inbox_budget").await
    }
    async fn backlog(store: &SqliteStore) -> (i64, i64) {
        let mut tx = store.0.begin_locked().await.unwrap();
        let got = unverified_backlog(&mut tx).await.unwrap();
        tx.commit().await.unwrap();
        got
    }
    async fn row(store: &SqliteStore, id: &str) -> Delivery {
        let mut tx = store.0.begin_locked().await.unwrap();
        let d = inbox_row(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        d
    }
    fn is_backlog_full(e: &ApiError) -> bool {
        e.code == ErrorCode::AdapterUnavailable && e.message.contains("intake backlog full")
    }
    async fn failures(store: &SqliteStore, id: &str) {
        store
            .0
            .inbox(Command::InboxFailure(id.into()), T0)
            .await
            .unwrap();
    }
    async fn raw_pending(store: &SqliteStore, id: &str, received: i64, payload: &str) {
        sqlx::query("INSERT INTO webhook_inbox(id,payload) VALUES($1,$2)")
            .bind(id)
            .bind(payload)
            .execute(&store.0.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO inbox_schedule(id,status,received,due,attempts,claim_id) VALUES($1,'pending',$2,0,0,NULL)")
            .bind(id)
            .bind(received)
            .execute(&store.0.pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn shed_caps_unverified_backlog_rows_before_writing_or_spending_budget() {
        let (store, _) = store().await;
        fill(&store, 100).await;
        let err = put(&store, delivery(100)).await.unwrap_err();
        assert!(is_backlog_full(&err), "{err:?}");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM webhook_inbox").await,
            100
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM inbox_schedule").await,
            100
        );
        assert_eq!(used(&store).await, 0);
    }

    #[tokio::test]
    async fn shed_caps_unverified_backlog_bytes_before_row_cap() {
        let (store, _) = store().await;
        let mut admitted = 0;
        let err = loop {
            let mut d = delivery(admitted);
            d.body = vec![100u8; 131_072];
            match put(&store, d).await {
                Ok(_) => admitted += 1,
                Err(e) => break e,
            }
            assert!(admitted < 100, "byte cap never fired");
        };
        assert!(is_backlog_full(&err), "{err:?}");
        assert!((20..100).contains(&admitted), "admitted {admitted}");
        let (rows, bytes) = backlog(&store).await;
        assert_eq!(rows, admitted as i64);
        assert!(bytes <= UNVERIFIED_BACKLOG_BYTES);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM webhook_inbox").await,
            admitted as i64
        );
    }

    #[tokio::test]
    async fn shed_never_blocks_verified_or_existing_rows() {
        let (store, _) = store().await;
        fill(&store, 100).await;
        // A new verified delivery is not unverified backlog.
        let mut verified = delivery(500);
        verified.verified = true;
        put(&store, verified).await.unwrap();
        // An existing row can be rewritten in place while the backlog is full.
        let mut again = row(&store, "d-2").await;
        again.attempts = 3;
        put(&store, again).await.unwrap();
        assert_eq!(row(&store, "d-2").await.attempts, 3);
        // A brand-new unverified one is still refused, and the refusal wrote nothing.
        assert!(is_backlog_full(
            &put(&store, delivery(501)).await.unwrap_err()
        ));
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM webhook_inbox").await,
            101
        );
        // Marking a pending row verified moves it out of the unverified backlog.
        store
            .0
            .inbox(Command::InboxVerified("d-1".into()), T0)
            .await
            .unwrap();
        assert_eq!(backlog(&store).await.0, 99);
        put(&store, delivery(502)).await.unwrap();
    }

    #[tokio::test]
    async fn shed_reopens_exactly_as_rows_resolve() {
        let (store, _) = store().await;
        fill(&store, 100).await;
        for id in ["d-0", "d-1"] {
            store
                .execute(Command::Reconcile {
                    delivery_id: id.into(),
                    verified: false,
                    confirmation: None,
                })
                .await
                .unwrap();
        }
        put(&store, delivery(200)).await.unwrap();
        put(&store, delivery(201)).await.unwrap();
        assert!(is_backlog_full(
            &put(&store, delivery(202)).await.unwrap_err()
        ));
    }

    #[tokio::test]
    async fn string_fields_cannot_spoof_verified_classification() {
        let (store, _) = store().await;
        for n in 0..100 {
            let mut d = delivery(n);
            d.id = format!(r#"a","verified":true,"n":"{n}"#);
            d.hash = r#"{"verified":true}"#.into();
            put(&store, d).await.unwrap();
        }
        assert_eq!(backlog(&store).await.0, 100);
        let mut next = delivery(100);
        next.id = r#"z","verified":true,"n":""#.into();
        assert!(is_backlog_full(&put(&store, next).await.unwrap_err()));
    }

    #[tokio::test]
    async fn decoded_verified_field_overrides_a_matching_text_literal() {
        let (store, _) = store().await;
        // Payloads that contain the exact literal in an unknown nested key while the real field
        // is false. Only the decoded field may decide.
        for n in 0..100 {
            let d = delivery(n);
            let mut v = serde_json::to_value(&d).unwrap();
            v["extra"] = serde_json::json!({"verified": true});
            let payload = v.to_string();
            assert!(payload.contains(r#""verified":true"#));
            raw_pending(&store, &d.id, d.received_at, &payload).await;
        }
        assert_eq!(backlog(&store).await.0, 100);
        assert!(is_backlog_full(
            &put(&store, delivery(100)).await.unwrap_err()
        ));
    }

    #[tokio::test]
    async fn backlog_counts_only_unverified_pending() {
        let (store, _) = store().await;
        put(&store, delivery(0)).await.unwrap();
        let mut verified = delivery(1);
        verified.verified = true;
        put(&store, verified).await.unwrap();
        let mut rejected = delivery(2);
        rejected.status = "rejected".into();
        put(&store, rejected).await.unwrap();
        let mut unmatched = delivery(3);
        unmatched.status = "unmatched".into();
        unmatched.verified = true;
        put(&store, unmatched).await.unwrap();
        let (rows, bytes) = backlog(&store).await;
        assert_eq!(rows, 1);
        assert!(bytes > 0);
    }

    #[test]
    fn verified_literal_is_serialized_compactly() {
        let mut d = delivery(0);
        d.verified = true;
        assert!(serde_json::to_string(&d)
            .unwrap()
            .contains(r#""verified":true"#));
        d.verified = false;
        assert!(!serde_json::to_string(&d)
            .unwrap()
            .contains(r#""verified":true"#));
    }

    #[tokio::test]
    async fn byte_cap_still_closes_intake_for_verified_pending_payloads() {
        let (store, _) = store().await;
        let mut big = delivery(0);
        big.verified = true;
        let mut v = serde_json::to_value(&big).unwrap();
        v["pad"] = "x".repeat(INBOX_BYTES_CAP as usize).into();
        raw_pending(&store, &big.id, 0, &v.to_string()).await;
        let err = put(&store, delivery(1)).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::AdapterUnavailable);
        assert!(err.message.contains("capacity reached"), "{err:?}");
    }

    #[tokio::test]
    async fn budget_reserves_slots_for_verified_rows() {
        let (store, _) = store().await;
        for n in 0..51 {
            put(&store, delivery(n)).await.unwrap();
        }
        for n in 0..50 {
            assert!(attempt(&store, &format!("d-{n}"), T0).await.unwrap());
        }
        let err = attempt(&store, "d-50", T0).await.unwrap_err();
        assert!(err.message.contains("verification budget exhausted"));
        assert_eq!(used(&store).await, 50);
        assert_eq!(
            row(&store, "d-50").await.attempts,
            0,
            "a refusal burns no attempt"
        );
        for n in 100..111 {
            put(&store, delivery(n)).await.unwrap();
            store
                .0
                .inbox(Command::InboxVerified(format!("d-{n}")), T0)
                .await
                .unwrap();
        }
        for n in 100..110 {
            assert!(attempt(&store, &format!("d-{n}"), T0).await.unwrap());
        }
        assert_eq!(used(&store).await, 60);
        assert!(attempt(&store, "d-110", T0).await.is_err());
        assert_eq!(used(&store).await, 60);
    }

    #[tokio::test]
    async fn budget_rolls_over_with_the_clock_and_backlog_drains() {
        let (store, _) = store().await;
        for n in 0..60 {
            put(&store, delivery(n)).await.unwrap();
        }
        let mut served = 0;
        for n in 0..60 {
            if attempt(&store, &format!("d-{n}"), T0)
                .await
                .unwrap_or(false)
            {
                served += 1;
            }
        }
        assert_eq!(served, 50);
        for n in 0..60 {
            let id = format!("d-{n}");
            if row(&store, &id).await.attempts == 0 {
                assert!(attempt(&store, &id, T0 + 60).await.unwrap());
            }
        }
        assert_eq!(used(&store).await, 10);
    }

    #[test]
    fn formatter_emits_only_for_verified_rows_with_exact_fields() {
        let mut d = verified_event(0, "WH-1:a.b_c", PAYOUT);
        d.id = "11111111-2222-3333-4444-555555555555".into();
        d.received_at = 1234;
        let payout = uuid::Uuid::parse_str(PAYOUT).unwrap();
        let claim = uuid::Uuid::parse_str(CLAIM).unwrap();
        assert_eq!(
            verified_exhausted_line(&d, VIA_QUERY_FAILED, Some(payout), Some(claim)).unwrap(),
            format!("webhook_verified_exhausted delivery=11111111-2222-3333-4444-555555555555 event=WH-1:a.b_c type=PAYMENT.PAYOUTS-ITEM.SUCCEEDED via=query_failed attempts=8 received_at=1234 payout={PAYOUT} claim={CLAIM}")
        );
        let line = verified_exhausted_line(&d, VIA_RECOVERY, None, None).unwrap();
        assert!(line.ends_with("payout=none claim=unknown"), "{line}");
        assert!(!line.contains("SECRET-MARKER"));
        d.verified = false;
        assert!(verified_exhausted_line(&d, VIA_QUERY_FAILED, None, None).is_none());
    }

    #[test]
    fn formatter_sanitizes_untrusted_fields_to_one_bounded_line() {
        let mut d = verified_event(0, "bad\nid\u{1b}[31m", "not-a-uuid");
        d.id = format!("{}\n", "x".repeat(200));
        let line = verified_exhausted_line(&d, VIA_QUERY_FAILED, None, None).unwrap();
        assert!(
            line.contains("delivery=invalid event=invalid type=PAYMENT.PAYOUTS-ITEM.SUCCEEDED"),
            "{line:?}"
        );
        assert!(line.contains("payout=none"));
        assert!(!line.contains('\n') && !line.contains('\u{1b}'), "{line:?}");
        assert!(line.len() < 512);
        for (id, kind, want) in [
            (
                "a".repeat(65),
                "PAYMENT.PAYOUTS-ITEM.OK".to_owned(),
                "event=invalid type=PAYMENT.PAYOUTS-ITEM.OK",
            ),
            (
                "ok".into(),
                format!("PAYMENT.PAYOUTS-ITEM.{}", "X".repeat(64)),
                "event=ok type=other",
            ),
            (
                "ok".into(),
                "PAYMENT.PAYOUTS-ITEM.X\nY".into(),
                "event=ok type=other",
            ),
            ("ok".into(), "OTHER.TYPE".into(), "event=ok type=other"),
        ] {
            let mut d = verified_event(0, &id, PAYOUT);
            d.body = serde_json::json!({"id": id, "event_type": kind})
                .to_string()
                .into_bytes();
            let line = verified_exhausted_line(&d, VIA_QUERY_FAILED, None, None).unwrap();
            assert!(!line.contains('\n'), "{line:?}");
            assert!(line.len() < 512, "{line}");
            assert!(line.contains(want), "{line}");
        }
        // A non-JSON or oversized body yields fixed words, never an error.
        d.body = b"not json".to_vec();
        assert!(verified_exhausted_line(&d, VIA_QUERY_FAILED, None, None)
            .unwrap()
            .contains("event=none type=other"));
        d.body = vec![b' '; SIGNAL_BODY_MAX + 1];
        assert!(verified_exhausted_line(&d, VIA_QUERY_FAILED, None, None)
            .unwrap()
            .contains("event=none type=other"));
    }

    #[test]
    fn emission_boundary_survives_a_broken_writer_and_writes_one_line() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }
        write_signal(&mut Broken, "webhook_verified_exhausted x=1");
        let mut out = Vec::new();
        write_signal(&mut out, "webhook_verified_exhausted x=1");
        assert_eq!(out, b"webhook_verified_exhausted x=1\n");
    }

    #[tokio::test]
    async fn query_failed_path_signals_once_after_commit_and_keeps_evidence() {
        let (store, lines) = store().await;
        let d = verified_event(0, "WH-1", PAYOUT);
        put(&store, d.clone()).await.unwrap();
        failures(&store, &d.id).await;
        failures(&store, &d.id).await;
        let seen = lines.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].starts_with("webhook_verified_exhausted delivery=d-0 event=WH-1 "));
        assert!(seen[0].contains("via=query_failed attempts=8"));
        assert!(seen[0].contains(&format!("payout={PAYOUT} claim=unknown")));
        assert!(!seen[0].contains("SECRET-MARKER"));
        let kept = row(&store, &d.id).await;
        assert_eq!(kept.status, "exhausted");
        assert_eq!((kept.body, kept.headers), (d.body, d.headers));
    }

    #[tokio::test]
    async fn unverified_exhaustion_and_non_exhaustion_emit_nothing() {
        let (store, lines) = store().await;
        let mut forged = delivery(0);
        forged.attempts = 8;
        put(&store, forged).await.unwrap();
        failures(&store, "d-0").await;
        assert_eq!(row(&store, "d-0").await.status, "exhausted");
        let mut early = verified_event(1, "WH-2", PAYOUT);
        early.attempts = 3;
        put(&store, early).await.unwrap();
        failures(&store, "d-1").await;
        assert_eq!(row(&store, "d-1").await.status, "pending");
        assert!(lines.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attribution_reads_claim_and_failure_does_not_affect_persistence() {
        let (store, lines) = store().await;
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&store.0.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO payouts(id,claim_id,approval_id,sender_batch_id,payload) VALUES($1,$2,'ap','sb','{}')")
            .bind(PAYOUT)
            .bind(CLAIM)
            .execute(&store.0.pool)
            .await
            .unwrap();
        let before = count(&store, "SELECT COUNT(*) FROM payouts").await;
        let d = verified_event(0, "WH-1", PAYOUT);
        put(&store, d.clone()).await.unwrap();
        failures(&store, &d.id).await;
        assert!(lines.lock().unwrap()[0].ends_with(&format!("payout={PAYOUT} claim={CLAIM}")));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM payouts").await, before);
        // The attribution query now fails; the transition must still persist and still signal.
        sqlx::raw_sql("DROP TABLE payout_events; DROP TABLE payouts")
            .execute(&store.0.pool)
            .await
            .unwrap();
        let second = verified_event(1, "WH-2", PAYOUT);
        put(&store, second.clone()).await.unwrap();
        failures(&store, &second.id).await;
        assert_eq!(row(&store, &second.id).await.status, "exhausted");
        let seen = lines.lock().unwrap().clone();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].ends_with(&format!("payout={PAYOUT} claim=unknown")));
    }

    #[tokio::test]
    async fn reconcile_path_signals_when_verified_event_stays_unmatched_at_attempt_cap() {
        let (store, lines) = store().await;
        let d = verified_event(0, "WH-9", PAYOUT);
        put(&store, d.clone()).await.unwrap();
        store
            .execute(Command::Reconcile {
                delivery_id: d.id.clone(),
                verified: true,
                confirmation: None,
            })
            .await
            .unwrap();
        assert_eq!(row(&store, &d.id).await.status, "exhausted");
        let seen = lines.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("event=WH-9") && seen[0].contains("via=reconcile_unmatched"));
    }

    #[tokio::test]
    async fn crash_recovery_path_signals_once_and_archives_the_evidence_intact() {
        let (store, lines) = store().await;
        let d = verified_event(0, "WH-7", PAYOUT);
        put(&store, d.clone()).await.unwrap();
        store.0.inbox(Command::InboxPrune, T0).await.unwrap();
        store.0.inbox(Command::InboxPrune, T0 + 1).await.unwrap();
        let seen = lines.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("event=WH-7") && seen[0].contains("via=crash_recovery"));
        let archived: String = sqlx::query_scalar(
            "SELECT payload FROM webhook_inbox_archive WHERE id = 'd-0' AND status = 'exhausted'",
        )
        .fetch_one(&store.0.pool)
        .await
        .unwrap();
        let kept: Delivery = serde_json::from_str(&archived).unwrap();
        assert_eq!(
            (kept.body, kept.headers, kept.verified),
            (d.body, d.headers, true)
        );
    }
}

// P6F static candidate tests. UNRUN: written under a build hold and never compiled or executed.
#[cfg(test)]
mod p6f_tests {
    use super::*;
    use crate::webhook::Delivery;
    use axum::http::HeaderMap;
    use std::sync::Mutex;

    const T0: i64 = 60 * 16_666_667;
    const PAYOUT: &str = "6f1c1f0e-0d6e-4b34-9a52-6d7f0f6a2c11";
    const CLAIM: &str = "0b6c5a2e-4d1b-4f0e-8d57-3a1f7e9c4b22";
    const OUTSTANDING: &str = "webhook_verified_exhausted_outstanding ";

    type Lines = Arc<Mutex<Vec<String>>>;

    async fn store() -> (SqliteStore, Lines) {
        let mut store = SqliteStore::connect("sqlite::memory:").await.unwrap();
        let lines: Lines = Arc::default();
        let sink = lines.clone();
        store.0.signal = Arc::new(move |line| sink.lock().unwrap().push(line.to_owned()));
        // The single in-memory connection is reused, so the pragma holds for the whole test.
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&store.0.pool)
            .await
            .unwrap();
        (store, lines)
    }
    async fn seed_payout(store: &SqliteStore, payout: &str, claim: &str) {
        sqlx::query("INSERT INTO payouts(id,claim_id,approval_id,sender_batch_id,payload) VALUES($1,$2,$3,$4,'{}')")
            .bind(payout)
            .bind(claim)
            .bind(format!("ap-{payout}"))
            .bind(format!("sb-{payout}"))
            .execute(&store.0.pool)
            .await
            .unwrap();
    }
    fn body(event_id: &str, payout: &str, kind: &str) -> Vec<u8> {
        serde_json::json!({
            "id": event_id,
            "event_type": kind,
            "resource": {"payout_item": {"sender_item_id": payout}}
        })
        .to_string()
        .into_bytes()
    }
    fn delivery(n: usize) -> Delivery {
        let mut d = Delivery::new(b"{}", &HeaderMap::new());
        d.id = format!("d-{n}");
        d.received_at = n as i64;
        d
    }
    fn named(n: usize, payout: &str) -> Delivery {
        let mut d = delivery(n);
        d.body = body(&format!("EV-{n}"), payout, "PAYMENT.PAYOUTS-ITEM.SUCCEEDED");
        d
    }
    fn exhausted_verified(n: usize, payout: &str) -> Delivery {
        let mut d = named(n, payout);
        d.verified = true;
        d.attempts = 8;
        d.received_at = 5 + n as i64;
        d
    }
    async fn put(store: &SqliteStore, d: Delivery) -> Result<Output, ApiError> {
        store.0.inbox(Command::Inbox(d), T0).await
    }
    async fn fail(store: &SqliteStore, id: &str) {
        store
            .0
            .inbox(Command::InboxFailure(id.into()), T0)
            .await
            .unwrap();
    }
    async fn row(store: &SqliteStore, id: &str) -> Delivery {
        let mut tx = store.0.begin_locked().await.unwrap();
        let d = inbox_row(&mut tx, id).await.unwrap();
        tx.commit().await.unwrap();
        d
    }
    async fn scalar(store: &SqliteStore, sql: &str) -> i64 {
        sqlx::query_scalar(sql)
            .fetch_one(&store.0.pool)
            .await
            .unwrap()
    }
    async fn attempt(store: &SqliteStore, id: &str) -> Result<bool, ApiError> {
        match store.0.inbox(Command::InboxAttempt(id.into()), T0).await? {
            Output::Inbox(rows) => Ok(!rows.is_empty()),
            _ => unreachable!(),
        }
    }
    fn backlog_full(e: &ApiError) -> bool {
        e.code == ErrorCode::AdapterUnavailable && e.message.contains("intake backlog full")
    }
    async fn saturate(store: &SqliteStore) {
        for n in 0..100 {
            put(store, delivery(1000 + n)).await.unwrap();
        }
        assert!(backlog_full(&put(store, delivery(2000)).await.unwrap_err()));
    }
    fn outstanding(lines: &Lines) -> Vec<String> {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(OUTSTANDING))
            .cloned()
            .collect()
    }

    // MAJ-2: attribution is part of the transition's transaction, on all three paths.
    #[tokio::test]
    async fn exhaustion_commits_claim_with_the_transition_and_the_archive_carries_it() {
        let (store, lines) = store().await;
        seed_payout(&store, PAYOUT, CLAIM).await;
        let d = exhausted_verified(0, PAYOUT);
        put(&store, d.clone()).await.unwrap();
        assert!(row(&store, &d.id).await.claim_id.is_none());
        fail(&store, &d.id).await;
        let kept = row(&store, &d.id).await;
        assert_eq!(kept.status, "exhausted");
        assert_eq!(kept.claim_id.map(|c| c.to_string()).as_deref(), Some(CLAIM));
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT claim_id FROM inbox_schedule WHERE id='d-0'")
                .fetch_one(&store.0.pool)
                .await
                .unwrap(),
            CLAIM
        );
        assert!(lines.lock().unwrap()[0].ends_with(&format!("payout={PAYOUT} claim={CLAIM}")));
        store.0.inbox(Command::InboxPrune, T0).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT claim_id FROM webhook_inbox_archive WHERE id='d-0' AND status='exhausted'"
            )
            .fetch_one(&store.0.pool)
            .await
            .unwrap(),
            CLAIM
        );
        // The crash-recovery path attributes the same way. The reconcile path is NOT covered here:
        // it loads the whole ledger, and the raw payout fixture above is not a decodable Payout.
        let r = exhausted_verified(1, PAYOUT);
        put(&store, r.clone()).await.unwrap();
        store.0.inbox(Command::InboxPrune, T0 + 1).await.unwrap();
        assert_eq!(
            row_or_archived_claim(&store, &r.id).await.as_deref(),
            Some(CLAIM)
        );
    }
    async fn row_or_archived_claim(store: &SqliteStore, id: &str) -> Option<String> {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT claim_id FROM inbox_schedule WHERE id=$1 UNION ALL SELECT claim_id FROM webhook_inbox_archive WHERE id=$1",
        )
        .bind(id)
        .fetch_optional(&store.0.pool)
        .await
        .unwrap()
        .flatten()
    }
    #[tokio::test]
    async fn exhaustion_without_a_local_payout_leaves_claim_empty_and_still_persists() {
        let (store, lines) = store().await;
        let d = exhausted_verified(0, PAYOUT);
        put(&store, d.clone()).await.unwrap();
        fail(&store, &d.id).await;
        let kept = row(&store, &d.id).await;
        assert_eq!((kept.status.as_str(), kept.claim_id), ("exhausted", None));
        assert!(lines.lock().unwrap()[0].ends_with("claim=unknown"));
    }

    // MAJ-2: level-triggered line, repeated until nothing is outstanding. Read-only.
    #[tokio::test]
    async fn outstanding_line_counts_live_and_archived_verified_rows_and_is_throttled() {
        let (store, lines) = store().await;
        store.0.report_verified_exhausted(T0).await.unwrap();
        assert!(
            outstanding(&lines).is_empty(),
            "nothing outstanding, no line"
        );
        seed_payout(&store, PAYOUT, CLAIM).await;
        let archived = exhausted_verified(0, PAYOUT);
        put(&store, archived.clone()).await.unwrap();
        fail(&store, &archived.id).await;
        store.0.inbox(Command::InboxPrune, T0).await.unwrap();
        let live = exhausted_verified(1, "00000000-0000-4000-8000-000000000000");
        put(&store, live.clone()).await.unwrap();
        fail(&store, &live.id).await;
        let mut forged = delivery(9);
        forged.attempts = 8;
        put(&store, forged).await.unwrap();
        fail(&store, "d-9").await;
        let before = scalar(&store, "SELECT COUNT(*) FROM webhook_inbox_archive").await;
        store.0.report_verified_exhausted(T0).await.unwrap();
        assert_eq!(
            outstanding(&lines),
            vec![format!(
                "{OUTSTANDING}count=2 unattributed=1 oldest_received_at=5"
            )]
        );
        store.0.report_verified_exhausted(T0 + 10).await.unwrap();
        assert_eq!(outstanding(&lines).len(), 1, "throttled");
        store
            .0
            .report_verified_exhausted(T0 + GAUGE_INTERVAL_SECS)
            .await
            .unwrap();
        assert_eq!(outstanding(&lines).len(), 2, "repeats while outstanding");
        assert_eq!(
            scalar(&store, "SELECT COUNT(*) FROM webhook_inbox_archive").await,
            before,
            "reporting writes nothing"
        );
    }

    // MAJ-1 partial mitigation: correlated lane. Never evidence of authentication.
    #[tokio::test]
    async fn correlated_unverified_row_is_stored_while_backlog_is_saturated() {
        let (store, _) = store().await;
        seed_payout(&store, PAYOUT, CLAIM).await;
        saturate(&store).await;
        put(&store, named(1, PAYOUT)).await.unwrap();
        assert!(row(&store, "d-1").await.correlated);
        // The residual stays visible: other traffic is still shed.
        for refused in [
            named(2, "11111111-1111-4111-8111-111111111111"),
            named(3, &PAYOUT.to_uppercase()),
            {
                let mut d = named(4, PAYOUT);
                d.body = body("EV-4", PAYOUT, "PAYMENT.PAYOUTSBATCH.SUCCESS");
                d
            },
            {
                let mut d = delivery(5);
                d.correlated = true; // a caller cannot assert the lane
                d
            },
        ] {
            assert!(backlog_full(&put(&store, refused).await.unwrap_err()));
        }
    }
    #[tokio::test]
    async fn correlated_rows_are_capped_per_payout_and_in_total() {
        let (store, _) = store().await;
        seed_payout(&store, PAYOUT, CLAIM).await;
        saturate(&store).await;
        for n in 0..CORRELATED_PER_PAYOUT as usize {
            put(&store, named(n, PAYOUT)).await.unwrap();
        }
        assert!(backlog_full(
            &put(&store, named(50, PAYOUT)).await.unwrap_err()
        ));
        // Other payouts still have room, up to the total.
        let mut n = 100;
        let mut payouts = Vec::new();
        for k in 1..(CORRELATED_TOTAL_ROWS / CORRELATED_PER_PAYOUT) {
            let p = format!("00000000-0000-4000-8000-{k:012}");
            seed_payout(&store, &p, &format!("claim-{k}")).await;
            payouts.push(p);
        }
        for p in &payouts {
            for _ in 0..CORRELATED_PER_PAYOUT {
                put(&store, named(n, p)).await.unwrap();
                n += 1;
            }
        }
        let last = "00000000-0000-4000-8000-0000000000ff";
        seed_payout(&store, last, "claim-last").await;
        assert!(backlog_full(
            &put(&store, named(n, last)).await.unwrap_err()
        ));
    }
    #[tokio::test]
    async fn correlated_bytes_cannot_take_the_shared_byte_cap() {
        let (store, _) = store().await;
        seed_payout(&store, PAYOUT, CLAIM).await;
        let other = "00000000-0000-4000-8000-000000000001";
        seed_payout(&store, other, "other-claim").await;
        saturate(&store).await;
        let mut admitted = 0;
        let refused_bytes = loop {
            // Stay within HTTP intake's body bound; distribute across payouts so the byte
            // cap, rather than the per-payout row cap, is the first exhausted resource.
            let mut d = named(admitted, if admitted % 2 == 0 { PAYOUT } else { other });
            let mut v: serde_json::Value = serde_json::from_slice(&d.body).unwrap();
            v["pad"] = "d".repeat(120_000).into();
            d.body = v.to_string().into_bytes();
            assert!(d.body.len() <= 131_072);
            let new_bytes = serde_json::to_string(&d).unwrap().len() as i64;
            if let Err(e) = put(&store, d).await {
                assert!(backlog_full(&e), "unexpected refusal: {e:?}");
                break new_bytes;
            }
            admitted += 1;
            assert!(
                admitted < 2 * CORRELATED_PER_PAYOUT as usize,
                "bytes cap never fired before per-payout limits"
            );
        };
        assert!(admitted >= 1);
        let bytes = scalar(
            &store,
            "SELECT COALESCE(SUM(LENGTH(w.payload)),0) FROM webhook_inbox w WHERE w.payload LIKE '%\"correlated\":true%'",
        )
        .await;
        assert!(bytes <= CORRELATED_TOTAL_BYTES, "{bytes}");
        assert!(bytes + refused_bytes > CORRELATED_TOTAL_BYTES);
    }
    #[tokio::test]
    async fn correlated_rows_use_their_own_sub_budget_and_leave_ordinary_rows_alone() {
        let (store, _) = store().await;
        let mut ids = Vec::new();
        for k in 0..4 {
            let p = format!("00000000-0000-4000-8000-{k:012}");
            seed_payout(&store, &p, &format!("claim-{k}")).await;
            for j in 0..6 {
                let n = k * 10 + j;
                put(&store, named(n, &p)).await.unwrap();
                ids.push(format!("d-{n}"));
            }
        }
        put(&store, delivery(900)).await.unwrap();
        for id in ids.iter().take(CORRELATED_BUDGET_PER_MIN as usize) {
            assert!(attempt(&store, id).await.unwrap());
        }
        let refused = attempt(&store, &ids[CORRELATED_BUDGET_PER_MIN as usize])
            .await
            .unwrap_err();
        assert!(refused.message.contains("verification budget exhausted"));
        assert_eq!(
            scalar(&store, "SELECT used FROM inbox_budget WHERE id=2").await,
            20
        );
        assert_eq!(
            scalar(&store, "SELECT used FROM inbox_budget WHERE id=1").await,
            20
        );
        // The refusal burned no attempt, and an ordinary row is unaffected by the lane limit.
        assert_eq!(row(&store, &ids[20]).await.attempts, 0);
        assert!(attempt(&store, "d-900").await.unwrap());
    }
    #[tokio::test]
    async fn correlated_lane_never_lowers_the_ordinary_ceiling() {
        let (store, _) = store().await;
        seed_payout(&store, PAYOUT, CLAIM).await;
        for n in 0..51 {
            put(&store, delivery(n)).await.unwrap();
        }
        for n in 0..50 {
            assert!(attempt(&store, &format!("d-{n}")).await.unwrap());
        }
        assert!(
            attempt(&store, "d-50").await.is_err(),
            "ordinary ceiling is still 50"
        );
        put(&store, named(500, PAYOUT)).await.unwrap();
        assert!(
            attempt(&store, "d-500").await.unwrap(),
            "correlated may use the reserve"
        );
    }
    #[test]
    fn correlation_requires_item_event_and_canonical_payout_uuid() {
        let ok = body("E", PAYOUT, "PAYMENT.PAYOUTS-ITEM.SUCCEEDED");
        assert_eq!(correlated_payout(&ok), uuid::Uuid::parse_str(PAYOUT).ok());
        for bad in [
            body(
                "E",
                &PAYOUT.to_uppercase(),
                "PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
            ),
            body(
                "E",
                &PAYOUT.replace('-', ""),
                "PAYMENT.PAYOUTS-ITEM.SUCCEEDED",
            ),
            body("E", PAYOUT, "PAYMENT.PAYOUTSBATCH.SUCCESS"),
            body("E", "nobody", "PAYMENT.PAYOUTS-ITEM.SUCCEEDED"),
            b"not json".to_vec(),
            vec![b' '; SIGNAL_BODY_MAX + 1],
        ] {
            assert_eq!(correlated_payout(&bad), None);
        }
    }
}
