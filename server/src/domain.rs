use contracts::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const EVENT_ID: EventId = EventId(Uuid::from_u128(1));
pub const BUDGET_ID: BudgetLineId = BudgetLineId(Uuid::from_u128(2));
pub const CLAIMANT_ID: ClaimantId = ClaimantId(Uuid::from_u128(3));
pub const PAYEE_ID: PayeeId = PayeeId(Uuid::from_u128(4));
pub const APPROVER_ID: ApproverId = ApproverId(Uuid::from_u128(5));

#[derive(Debug, Clone, Default)]
pub struct Ledger {
    pub budgets: BTreeMap<BudgetLineId, BudgetLine>,
    pub payees: BTreeMap<PayeeId, Payee>,
    pub claims: BTreeMap<ClaimId, Claim>,
    pub approvals: BTreeMap<ClaimId, Approval>,
    pub payouts: BTreeMap<ClaimId, Payout>,
    pub events: BTreeMap<String, StoredEvent>,
    pub receipts: BTreeMap<ClaimId, String>,
    pub submissions: BTreeMap<ClaimId, SubmitClaimRequest>,
    pub policy: BTreeMap<String, i64>,
    pub requests: BTreeMap<ClaimId, ImmutableRequest>,
    pub evidence: BTreeMap<String, serde_json::Value>,
    pub inbox: BTreeMap<String, crate::webhook::Delivery>,
    pub sandbox_events: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImmutableRequest {
    pub claim_id: ClaimId,
    pub first_attempt_at: i64,
    pub body: serde_json::Value,
    pub sha256: String,
    pub request_id: String,
    pub environment: String,
    pub version: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEvent {
    pub event: PayoutEvent,
    pub envelope: PayPalWebhookRequest,
}
impl Ledger {
    pub fn seed(&mut self) {
        self.budgets.entry(BUDGET_ID).or_insert_with(|| BudgetLine {
            id: BUDGET_ID,
            event_id: EVENT_ID,
            category: "Miso the cat | clinic estimate USD 100.00 | demo".into(),
            approved_cents: Cents::try_from(10_000).unwrap(),
            reserved_cents: Cents::ZERO,
            paid_cents: Cents::ZERO,
        });
        self.payees.entry(PAYEE_ID).or_insert_with(|| Payee {
            id: PAYEE_ID,
            claimant_id: CLAIMANT_ID,
            display_name: "Synthetic Pawhaven Animal Clinic".into(),
            sandbox_email: "sandbox-pawhaven-clinic@example.invalid".into(),
        });
    }
    pub fn response(&self, id: ClaimId) -> Result<ClaimResponse, ApiError> {
        Ok(ClaimResponse {
            claim: self.claims.get(&id).ok_or_else(not_found)?.clone(),
            approval: self.approvals.get(&id).cloned(),
            payout: self.payouts.get(&id).cloned(),
        })
    }
    pub fn receipt(&self, id: ClaimId) -> Result<String, ApiError> {
        Ok(self.receipts.get(&id).ok_or_else(not_found)?.clone())
    }
}
#[derive(Debug, Clone)]
pub enum Command {
    Inbox(crate::webhook::Delivery),
    InboxPending,
    InboxPrune,
    InboxAttempt(String),
    InboxVerified(String),
    InboxFailure(String),
    WebhookCandidate(String),
    Reconcile {
        delivery_id: String,
        verified: bool,
        confirmation: Option<Observation>,
    },
    QueryDue {
        id: ClaimId,
        now: i64,
        age: i64,
    },
    ExtractionInput(ClaimId),
    ResetDemo {
        now: i64,
    },
    ConfigureRecipient(String),
    ConfigureSandbox(bool),
    ExtractionQuota {
        account: String,
        now: i64,
    },
    Create {
        id: ClaimId,
        request: CreateClaimRequest,
    },
    Extract {
        id: ClaimId,
        fields: ReceiptFields,
    },
    Submit {
        id: ClaimId,
        request: SubmitClaimRequest,
    },
    Approve {
        approval_id: ApprovalId,
        id: ClaimId,
        payee_id: PayeeId,
        now: i64,
    },
    PreparePayout {
        payout_id: PayoutId,
        id: ClaimId,
        now: i64,
    },
    Observe {
        id: ClaimId,
        observation: Observation,
    },
    NotSent(ClaimId),
    Disposition {
        id: ClaimId,
        actor: String,
        request: DispositionRequest,
        now: i64,
    },
    Webhook(PayPalWebhookRequest),
    Get(ClaimId),
    Receipt(ClaimId),
    Budgets(EventId),
    Payees,
    Claims(EventId),
    Review(ClaimId),
}
/// Finance records explicit accounting adjustments without rewriting provider history.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DispositionRequest {
    pub disposition_id: String,
    pub reason: String,
    pub reserved_adjustment_cents: i64,
    pub paid_adjustment_cents: i64,
}
pub fn frozen(ledger: &Ledger, id: ClaimId) -> bool {
    ledger.payouts.get(&id).is_some_and(|p| {
        !p.reconciliation.is_empty()
            && ledger.policy.get(&format!("disposed:{id}")).copied()
                != Some(p.reconciliation.len() as i64)
    })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayoutAction {
    SendNew,
    QueryExisting,
    Complete,
}
#[derive(Debug, Clone)]
pub enum Output {
    Inbox(Vec<crate::webhook::Delivery>),
    Candidate(Option<ClaimResponse>),
    Reconciled(String),
    QueryDue(bool),
    ClaimantConfigured,
    Claim(ClaimResponse),
    Prepared {
        response: ClaimResponse,
        action: PayoutAction,
    },
    Webhook(WebhookResponse),
    Budgets(BudgetLinesResponse),
    Payees(PayeesResponse),
    Receipt(String),
    Claims(ClaimsResponse),
    Review(ClaimReviewResponse),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub state: PayoutState,
    pub paypal_batch_id: Option<String>,
    pub item_id: Option<String>,
    pub binding: Option<ProviderBinding>,
    pub source: String,
    pub status: String,
}
pub fn not_found() -> ApiError {
    ApiError::new(ErrorCode::NotFound, "record not found")
}
fn transition_error() -> ApiError {
    ApiError::new(
        ErrorCode::InvalidTransition,
        "command forbidden in current state",
    )
}
fn merchant(fields: &ReceiptFields) -> String {
    fields
        .merchant
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}
fn editable(state: ClaimState) -> bool {
    matches!(state, ClaimState::Draft | ClaimState::NeedsInfo)
}
fn occupied(state: ClaimState) -> bool {
    !matches!(
        state,
        ClaimState::Draft
            | ClaimState::NeedsInfo
            | ClaimState::DuplicateBlocked
            | ClaimState::OverBudget
    )
}
fn next_revision(claim: &mut Claim) -> Result<(), ApiError> {
    claim.revision = claim
        .revision
        .checked_add(1)
        .ok_or_else(|| ApiError::new(ErrorCode::InvalidInput, "revision overflow"))?;
    Ok(())
}

pub fn apply(ledger: &mut Ledger, command: Command) -> Result<Output, ApiError> {
    let editable_id = match &command {
        Command::Extract { id, .. } | Command::Submit { id, .. } | Command::Approve { id, .. } => {
            Some(*id)
        }
        Command::PreparePayout { id, .. } if !ledger.payouts.contains_key(id) => Some(*id),
        _ => None,
    };
    if editable_id.is_some_and(|id| ledger.policy.contains_key(&format!("archived:{id}"))) {
        return Err(ApiError::new(
            ErrorCode::InvalidInput,
            "demo claim archived by daily reset",
        ));
    }
    match command {
        Command::InboxPrune => Ok(Output::Reconciled("pruned".into())),
        Command::InboxAttempt(id) => {
            let row = ledger.inbox.get_mut(&id).ok_or_else(not_found)?;
            row.attempts += 1;
            Ok(Output::Inbox(vec![row.clone()]))
        }
        Command::InboxFailure(id) => {
            if let Some(row) = ledger.inbox.get_mut(&id) {
                if row.attempts >= 8 {
                    row.status = "exhausted".into();
                }
            }
            Ok(Output::Reconciled("deferred".into()))
        }
        Command::InboxVerified(id) => {
            ledger.inbox.get_mut(&id).ok_or_else(not_found)?.verified = true;
            Ok(Output::Reconciled("verified".into()))
        }
        Command::Inbox(delivery) => {
            ledger.inbox.insert(delivery.id.clone(), delivery);
            Ok(Output::Reconciled("stored".into()))
        }
        Command::InboxPending => Ok(Output::Inbox(
            ledger
                .inbox
                .values()
                .filter(|d| d.status == "pending" || d.status == "unmatched")
                .take(100)
                .cloned()
                .collect(),
        )),
        Command::Reconcile {
            delivery_id,
            verified,
            confirmation,
        } => crate::reconcile::apply(ledger, &delivery_id, verified, confirmation),
        Command::WebhookCandidate(sender) => {
            let response = ledger
                .payouts
                .values()
                .find(|p| {
                    p.binding
                        .as_ref()
                        .is_some_and(|b| b.sender_item_id == sender)
                })
                .map(|p| ledger.response(p.claim_id))
                .transpose()?;
            Ok(Output::Candidate(response))
        }
        Command::QueryDue { id, now, age } => {
            let latest_event = ledger
                .inbox
                .values()
                .filter(|d| d.claim_id == Some(id) && d.status == "applied")
                .map(|d| d.received_at)
                .max();
            let due = !frozen(ledger, id)
                && ledger.payouts.get(&id).is_some_and(|p| {
                    matches!(
                        p.state,
                        PayoutState::PayoutPending | PayoutState::PayoutUnknown
                    )
                })
                && ledger.requests.get(&id).is_some_and(|r| {
                    now.saturating_sub(latest_event.unwrap_or(r.first_attempt_at)) >= age
                });
            Ok(Output::QueryDue(due))
        }

        Command::ExtractionInput(id) => {
            let claim = ledger.claims.get(&id).ok_or_else(not_found)?;
            if !editable(claim.state) || ledger.policy.contains_key(&format!("archived:{id}")) {
                return Err(transition_error());
            }
            Ok(Output::Receipt(ledger.receipt(id)?))
        }
        Command::ResetDemo { now } => {
            // 04:00 Taipei is 20:00 UTC on the preceding date.
            let day = (now + 4 * 3600).div_euclid(86400);
            match ledger.policy.get("reset-day").copied() {
                None => {
                    ledger.policy.insert("reset-day".into(), day);
                }
                Some(previous) if previous < day => {
                    for claim in ledger.claims.values() {
                        let key = format!("archived:{}", claim.id);
                        if ledger.policy.contains_key(&key) {
                            continue;
                        }
                        if !ledger.payouts.contains_key(&claim.id)
                            && matches!(claim.state, ClaimState::Submitted | ClaimState::Approved)
                        {
                            let budget = ledger
                                .budgets
                                .get_mut(&claim.budget_line_id)
                                .ok_or_else(not_found)?;
                            budget.reserved_cents = Cents::try_from(
                                budget.reserved_cents.get() - claim.amount_cents.get(),
                            )
                            .map_err(|_| {
                                ApiError::new(ErrorCode::InvalidInput, "reservation underflow")
                            })?;
                        }
                        ledger.policy.insert(key, day);
                        // Keep claim facts/hashes and all payout evidence; remove raw demo input.
                        ledger.receipts.insert(
                            claim.id,
                            "[daily demo reset: raw synthetic input removed]".into(),
                        );
                    }
                    let budget = ledger.budgets.get_mut(&BUDGET_ID).ok_or_else(not_found)?;
                    let used = budget.reserved_cents.checked_add(budget.paid_cents)?;
                    let target = used.checked_add(Cents::try_from(10_000).unwrap())?;
                    let before = budget.approved_cents;
                    budget.approved_cents = target;
                    ledger.evidence.entry(format!("budget-refill:{day}")).or_insert_with(||
                        serde_json::json!({"kind":"daily-budget-refill", "day":day, "at":now,
                            "budget_id":BUDGET_ID, "approved_before":before,
                            "approved_after":budget.approved_cents, "reserved":budget.reserved_cents,
                            "paid":budget.paid_cents, "actor":"daily-demo-maintenance"}));
                    ledger.policy.insert("reset-day".into(), day);
                }
                _ => {}
            }
            Ok(Output::ClaimantConfigured)
        }
        Command::ConfigureSandbox(sandbox) => {
            ledger
                .policy
                .insert("sandbox-mode".into(), i64::from(sandbox));
            Ok(Output::ClaimantConfigured)
        }
        Command::ConfigureRecipient(receiver) => {
            ledger
                .payees
                .get_mut(&PAYEE_ID)
                .ok_or_else(not_found)?
                .sandbox_email = receiver;
            Ok(Output::ClaimantConfigured)
        }
        Command::ExtractionQuota { account, now } => {
            let day = (now + 8 * 3600).div_euclid(86400);
            let global = format!("ai:{day}");
            let user = format!("ai:{day}:{account}");
            if ledger.policy.get(&global).copied().unwrap_or(0) >= 50
                || ledger.policy.get(&user).copied().unwrap_or(0) >= 10
            {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "extraction quota exhausted; use manual text entry",
                ));
            }
            *ledger.policy.entry(global).or_default() += 1;
            *ledger.policy.entry(user).or_default() += 1;
            Ok(Output::ClaimantConfigured)
        }
        Command::Create { id, request } => {
            if request.receipt_text.trim().is_empty() || request.receipt_text.chars().count() > 4000
            {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "receipt text must be 1..4000 characters",
                ));
            }
            let budget = ledger
                .budgets
                .get(&request.budget_line_id)
                .ok_or_else(not_found)?;
            if budget.event_id != request.event_id {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "budget belongs to another event",
                ));
            }
            if !ledger
                .payees
                .values()
                .any(|p| p.claimant_id == request.claimant_id)
            {
                return Err(ApiError::new(
                    ErrorCode::PayeeNotAllowed,
                    "claimant has no preset payee",
                ));
            }
            let claim = Claim {
                id,
                event_id: request.event_id,
                claimant_id: request.claimant_id,
                budget_line_id: request.budget_line_id,
                receipt_sha256: ReceiptHash::try_from(format!(
                    "{:x}",
                    Sha256::digest(request.receipt_text.as_bytes())
                ))
                .unwrap(),
                fields: ReceiptFields::default(),
                amount_cents: Cents::ZERO,
                revision: 1,
                state: ClaimState::Draft,
            };
            ledger.claims.insert(id, claim);
            ledger.receipts.insert(id, request.receipt_text);
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::Extract { id, fields } => {
            let claim = ledger.claims.get_mut(&id).ok_or_else(not_found)?;
            if !editable(claim.state) {
                return Err(transition_error());
            }
            let complete = fields
                .merchant
                .as_ref()
                .is_some_and(|m| !m.trim().is_empty())
                && fields.date.is_some()
                && fields.total_cents.is_some_and(|c| c > Cents::ZERO)
                && fields.currency == Some(Currency::USD)
                && fields.uncertain.is_empty();
            if !complete {
                claim.state = claim.state.transition(ClaimState::NeedsInfo)?;
            }
            next_revision(claim)?;
            claim.fields = fields;
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::Submit { id, request } => {
            let claim = ledger.claims.get(&id).ok_or_else(not_found)?.clone();
            if !editable(claim.state) {
                if ledger.submissions.get(&id).is_some_and(|old| {
                    serde_json::to_value(old).unwrap() == serde_json::to_value(&request).unwrap()
                }) {
                    return Ok(Output::Claim(ledger.response(id)?));
                }
                return Err(transition_error());
            }
            let amount = request.amount()?;
            if amount.get() > 1000 {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "maximum claim is USD 10.00",
                ));
            }
            let duplicate = ledger.claims.values().any(|other| {
                other.id != id
                    && (!ledger
                        .policy
                        .contains_key(&format!("archived:{}", other.id))
                        || ledger.payouts.contains_key(&other.id))
                    && occupied(other.state)
                    && (other.receipt_sha256 == claim.receipt_sha256
                        || (merchant(&other.fields) == merchant(&request.fields)
                            && other.fields.date == request.fields.date
                            && other.amount_cents == amount))
            });
            let next = if duplicate {
                ClaimState::DuplicateBlocked
            } else {
                let budget = ledger
                    .budgets
                    .get(&claim.budget_line_id)
                    .ok_or_else(not_found)?;
                let used = budget
                    .reserved_cents
                    .checked_add(budget.paid_cents)?
                    .checked_add(amount)?;
                if used > budget.approved_cents {
                    ClaimState::OverBudget
                } else {
                    ClaimState::Submitted
                }
            };
            let claim = ledger.claims.get_mut(&id).unwrap();
            claim.state = claim.state.transition(next)?;
            next_revision(claim)?;
            claim.amount_cents = amount;
            claim.fields = request.fields.clone();
            if next == ClaimState::DuplicateBlocked {
                return Err(ApiError::new(
                    ErrorCode::DuplicateReceipt,
                    "receipt already submitted",
                ));
            }
            if next == ClaimState::OverBudget {
                return Err(ApiError::new(
                    ErrorCode::OverBudget,
                    "budget capacity exceeded",
                ));
            }
            let budget = ledger.budgets.get_mut(&claim.budget_line_id).unwrap();
            budget.reserved_cents = budget.reserved_cents.checked_add(amount)?;
            ledger.submissions.insert(id, request);
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::Approve {
            approval_id,
            id,
            payee_id,
            now,
        } => {
            let claim = ledger.claims.get(&id).ok_or_else(not_found)?;
            let payee = ledger
                .payees
                .get(&payee_id)
                .filter(|p| p.id == PAYEE_ID && p.claimant_id == claim.claimant_id)
                .ok_or_else(|| {
                    ApiError::new(
                        ErrorCode::PayeeNotAllowed,
                        "payee must be preset for this claimant",
                    )
                })?;
            if let Some(approval) = ledger.approvals.get(&id) {
                if approval.is_valid(claim, payee_id, now) {
                    return Ok(Output::Claim(ledger.response(id)?));
                }
                return Err(ApiError::new(
                    ErrorCode::ApprovalInvalid,
                    "approval changed or expired",
                ));
            }
            if claim.state != ClaimState::Submitted {
                return Err(transition_error());
            }
            let expires_at = now
                .checked_add(900)
                .ok_or_else(|| ApiError::new(ErrorCode::InvalidInput, "time overflow"))?;
            let approval = Approval {
                id: approval_id,
                claim_id: id,
                claim_revision: claim.revision,
                approver_id: APPROVER_ID,
                payee_id: payee.id,
                amount_cents: claim.amount_cents,
                receipt_sha256: claim.receipt_sha256.clone(),
                expires_at,
                receiver: payee.sandbox_email.clone(),
            };
            ledger.approvals.insert(id, approval);
            let claim = ledger.claims.get_mut(&id).unwrap();
            claim.state = claim.state.transition(ClaimState::Approved)?;
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::PreparePayout { payout_id, id, now } => {
            if let Some(payout) = ledger.payouts.get(&id) {
                let action = if frozen(ledger, id)
                    || matches!(payout.state, PayoutState::Paid | PayoutState::PayoutFailed)
                {
                    PayoutAction::Complete
                } else {
                    PayoutAction::QueryExisting
                };
                return Ok(Output::Prepared {
                    response: ledger.response(id)?,
                    action,
                });
            }
            let claim = ledger.claims.get(&id).ok_or_else(not_found)?;
            let approval = ledger
                .approvals
                .get(&id)
                .ok_or_else(|| ApiError::new(ErrorCode::ApprovalInvalid, "approval required"))?;
            if !approval.is_valid(claim, approval.payee_id, now)
                || approval.receiver.is_empty()
                || !ledger
                    .payees
                    .get(&approval.payee_id)
                    .is_some_and(|p| p.id == PAYEE_ID && p.claimant_id == claim.claimant_id)
            {
                return Err(ApiError::new(
                    ErrorCode::ApprovalInvalid,
                    "approval binding or expiry invalid",
                ));
            }
            if claim.state != ClaimState::Approved {
                return Err(transition_error());
            }
            if approval.amount_cents.get() > 1000 {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "maximum payout is USD 10.00",
                ));
            }
            let day = (now + 8 * 3600).div_euclid(86400);
            let key = format!("payout:{day}");
            if ledger.policy.get(&key).copied().unwrap_or(0) >= 20 {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "daily payout limit reached",
                ));
            }
            *ledger.policy.entry(key).or_default() += 1;
            let payout = Payout {
                id: payout_id,
                claim_id: id,
                approval_id: approval.id,
                sender_batch_id: approval.sender_batch_id(),
                paypal_batch_id: None,
                item_id: None,
                state: PayoutState::PayoutPending,
                binding: Some(ProviderBinding {
                    sender_batch_id: approval.sender_batch_id(),
                    sender_item_id: payout_id.to_string(),
                    receiver: approval.receiver.clone(),
                    recipient_type: "EMAIL".into(),
                    amount_cents: approval.amount_cents,
                    currency: Currency::USD,
                    payee_id: approval.payee_id,
                }),
                reconciliation: vec![],
            };
            let body = crate::sandbox::request_body(&payout);
            let bytes = serde_json::to_vec(&body).unwrap();
            ledger.requests.insert(
                id,
                ImmutableRequest {
                    claim_id: id,
                    first_attempt_at: now,
                    sha256: format!("{:x}", Sha256::digest(&bytes)),
                    body,
                    request_id: payout.sender_batch_id.clone(),
                    environment: if ledger.policy.get("sandbox-mode") == Some(&1) {
                        "sandbox"
                    } else {
                        "mock"
                    }
                    .into(),
                    version: 1,
                },
            );
            ledger.payouts.insert(id, payout);
            let claim = ledger.claims.get_mut(&id).unwrap();
            claim.state = claim.state.transition(ClaimState::PayoutPending)?;
            Ok(Output::Prepared {
                response: ledger.response(id)?,
                action: PayoutAction::SendNew,
            })
        }
        Command::Observe { id, observation } => {
            let evidence = serde_json::json!({"claim_id":id, "observed_at":chrono::Utc::now().timestamp(), "observation":observation});
            observe(ledger, id, observation)?;
            ledger.evidence.insert(Uuid::new_v4().to_string(), evidence);
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::Webhook(envelope) => {
            if envelope.paypal_event_id.is_empty()
                || envelope.paypal_event_id.len() > 256
                || envelope.paypal_batch_id.is_empty()
                || envelope.item_id.is_empty()
            {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "event and provider IDs required",
                ));
            }
            if let Some(old) = ledger.events.get(&envelope.paypal_event_id) {
                if serde_json::to_value(&old.envelope).unwrap()
                    != serde_json::to_value(&envelope).unwrap()
                {
                    return Err(ApiError::new(
                        ErrorCode::EventConflict,
                        "event ID reused with changed facts",
                    ));
                }
                let payout = ledger
                    .payouts
                    .values()
                    .find(|p| p.id == envelope.payout_id)
                    .ok_or_else(not_found)?;
                return Ok(Output::Webhook(WebhookResponse {
                    duplicate: true,
                    payout: payout.clone(),
                }));
            }
            let payout = ledger
                .payouts
                .values()
                .find(|p| p.id == envelope.payout_id)
                .ok_or_else(not_found)?
                .clone();
            let approval = ledger.approvals.get(&payout.claim_id).unwrap();
            if envelope.sender_batch_id != payout.sender_batch_id
                || envelope.amount_cents != approval.amount_cents
                || envelope.payee_id != approval.payee_id
                || envelope.currency != Currency::USD
                || payout
                    .paypal_batch_id
                    .as_ref()
                    .is_some_and(|id| id != &envelope.paypal_batch_id)
                || payout
                    .item_id
                    .as_ref()
                    .is_some_and(|id| id != &envelope.item_id)
            {
                return Err(ApiError::new(
                    ErrorCode::WebhookMismatch,
                    "event does not match approved payout",
                ));
            }
            let state = match envelope.kind {
                PayoutEventKind::Succeeded => PayoutState::Paid,
                PayoutEventKind::Failed => PayoutState::PayoutFailed,
                PayoutEventKind::Pending => PayoutState::PayoutPending,
                PayoutEventKind::Refunded
                | PayoutEventKind::Reversed
                | PayoutEventKind::Unknown => payout.state,
            };
            observe(
                ledger,
                payout.claim_id,
                Observation {
                    state,
                    paypal_batch_id: Some(envelope.paypal_batch_id.clone()),
                    item_id: Some(envelope.item_id.clone()),
                    binding: payout.binding.clone(),
                    source: format!("mock-webhook:{}", envelope.paypal_event_id),
                    status: format!("{:?}", envelope.kind),
                },
            )?;
            let event = PayoutEvent {
                paypal_event_id: envelope.paypal_event_id.clone(),
                payout_id: payout.id,
                kind: envelope.kind,
                verified: true,
            };
            ledger.events.insert(
                envelope.paypal_event_id.clone(),
                StoredEvent { event, envelope },
            );
            Ok(Output::Webhook(WebhookResponse {
                duplicate: false,
                payout: ledger.payouts.get(&payout.claim_id).unwrap().clone(),
            }))
        }
        Command::Claims(event) => {
            if !ledger.budgets.values().any(|line| line.event_id == event) {
                return Err(not_found());
            }
            let claims = ledger
                .claims
                .values()
                .filter(|claim| {
                    claim.event_id == event
                        && (!ledger
                            .policy
                            .contains_key(&format!("archived:{}", claim.id))
                            || frozen(ledger, claim.id))
                })
                .map(|claim| ledger.response(claim.id))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Output::Claims(ClaimsResponse { claims }))
        }
        Command::Review(id) => Ok(Output::Review(ClaimReviewResponse {
            claim: ledger.response(id)?,
            receipt_text: ledger.receipt(id)?,
            submission: ledger.submissions.get(&id).cloned(),
        })),
        Command::Disposition {
            id,
            actor,
            request,
            now,
        } => {
            if actor.trim().is_empty()
                || request.reason.trim().is_empty()
                || request.reason.len() > 2000
                || request.disposition_id.is_empty()
                || request.disposition_id.len() > 128
            {
                return Err(ApiError::new(
                    ErrorCode::InvalidInput,
                    "actor, reason and disposition ID required",
                ));
            }
            let key = format!("disposition:{id}:{}", request.disposition_id);
            let facts = serde_json::json!({"claim_id":id,"actor":actor,"request":request});
            if let Some(old) = ledger.evidence.get(&key) {
                if old["facts"] != facts {
                    return Err(ApiError::new(
                        ErrorCode::EventConflict,
                        "disposition ID reused",
                    ));
                }
                return Ok(Output::Claim(ledger.response(id)?));
            }
            if !frozen(ledger, id) {
                return Err(ApiError::new(
                    ErrorCode::InvalidTransition,
                    "claim has no unresolved reconciliation",
                ));
            }
            let claim = ledger.claims.get(&id).ok_or_else(not_found)?;
            // Zero adjustments release the hold. Nonzero adjustments settle exactly this claim.
            let amount = claim.amount_cents.get();
            let settlement = match (
                request.reserved_adjustment_cents,
                request.paid_adjustment_cents,
            ) {
                (0, 0) => None,
                (r, p) if r == -amount && p == amount => Some(PayoutState::Paid),
                (r, 0) if r == -amount => Some(PayoutState::PayoutFailed),
                _ => {
                    return Err(ApiError::new(
                        ErrorCode::InvalidInput,
                        "adjustment must settle this claim exactly",
                    ))
                }
            };
            if settlement.is_some()
                && matches!(
                    ledger.payouts[&id].state,
                    PayoutState::Paid | PayoutState::PayoutFailed
                )
            {
                return Err(transition_error());
            }
            let budget = ledger
                .budgets
                .get_mut(&claim.budget_line_id)
                .ok_or_else(not_found)?;
            let adjust = |value: Cents, delta: i64| -> Result<Cents, ApiError> {
                value
                    .get()
                    .checked_add(delta)
                    .and_then(|v| Cents::try_from(v).ok())
                    .ok_or_else(|| {
                        ApiError::new(ErrorCode::InvalidInput, "invalid accounting adjustment")
                    })
            };
            let reserved = adjust(budget.reserved_cents, request.reserved_adjustment_cents)?;
            let paid = adjust(budget.paid_cents, request.paid_adjustment_cents)?;
            if reserved.checked_add(paid)? > budget.approved_cents {
                return Err(ApiError::new(
                    ErrorCode::OverBudget,
                    "adjustment exceeds budget",
                ));
            }
            let before = budget.clone();
            budget.reserved_cents = reserved;
            budget.paid_cents = paid;
            let count = ledger.payouts[&id].reconciliation.len() as i64;
            ledger.evidence.insert(key, serde_json::json!({"kind":"finance-disposition",
                "facts":facts,"at":now,"reconciliation_count":count,"before":before,"after":budget}));
            ledger.policy.insert(format!("disposed:{id}"), count);
            if let Some(state) = settlement {
                let claim = ledger.claims.get_mut(&id).unwrap();
                claim.state = claim.state.transition(state.claim_state())?;
                ledger.payouts.get_mut(&id).unwrap().state = state;
            }
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::NotSent(id) => {
            let payout = ledger.payouts.get_mut(&id).ok_or_else(not_found)?;
            let record = ReconciliationRecord {
                source: "create-not-sent".into(),
                status: "NotSent".into(),
                paypal_batch_id: payout.paypal_batch_id.clone(),
                item_id: payout.item_id.clone(),
            };
            if !payout.reconciliation.contains(&record) {
                payout.reconciliation.push(record);
            }
            Ok(Output::Claim(ledger.response(id)?))
        }
        Command::Get(id) => Ok(Output::Claim(ledger.response(id)?)),
        Command::Receipt(id) => Ok(Output::Receipt(ledger.receipt(id)?)),
        Command::Budgets(event) => {
            let budget_lines: Vec<_> = ledger
                .budgets
                .values()
                .filter(|b| b.event_id == event)
                .cloned()
                .collect();
            if budget_lines.is_empty() {
                return Err(not_found());
            }
            Ok(Output::Budgets(BudgetLinesResponse { budget_lines }))
        }
        Command::Payees => Ok(Output::Payees(PayeesResponse {
            payees: ledger.payees.values().cloned().collect(),
        })),
    }
}

pub(crate) fn observe(
    ledger: &mut Ledger,
    id: ClaimId,
    observation: Observation,
) -> Result<(), ApiError> {
    let released_count = ledger
        .policy
        .get(&format!("disposed:{id}"))
        .copied()
        .unwrap_or(0);
    let payout = ledger.payouts.get_mut(&id).ok_or_else(not_found)?;
    if observation.source.starts_with("sandbox-item-query:")
        && matches!(
            observation.status.as_str(),
            "Mismatch" | "Refunded" | "Reversed" | "Unknown"
        )
    {
        let record = ReconciliationRecord {
            source: observation.source,
            status: observation.status,
            paypal_batch_id: observation.paypal_batch_id,
            item_id: observation.item_id,
        };
        if !payout
            .reconciliation
            .iter()
            .skip(released_count as usize)
            .any(|old| old == &record)
        {
            payout.reconciliation.push(record);
        }
        return Ok(());
    }
    let expected_status = match observation.state {
        PayoutState::Paid => "Succeeded",
        PayoutState::PayoutFailed => "Failed",
        PayoutState::PayoutPending => "Pending",
        PayoutState::PayoutUnknown => "Ambiguous",
    };
    if observation.status != expected_status
        && !matches!(
            observation.status.as_str(),
            "Refunded" | "Reversed" | "Unknown"
        )
    {
        return Err(ApiError::new(
            ErrorCode::WebhookMismatch,
            "item status and normalized state differ",
        ));
    }
    if observation.state != PayoutState::PayoutUnknown
        || observation.binding.is_some()
        || observation.paypal_batch_id.is_some()
        || observation.item_id.is_some()
    {
        let binding = observation.binding.as_ref().ok_or_else(|| {
            ApiError::new(
                ErrorCode::WebhookMismatch,
                "complete provider facts required",
            )
        })?;
        let expected = payout.binding.as_ref().ok_or_else(|| {
            ApiError::new(
                ErrorCode::WebhookMismatch,
                "legacy intent needs reconciliation",
            )
        })?;
        if serde_json::to_value(binding).unwrap() != serde_json::to_value(expected).unwrap()
            || observation
                .paypal_batch_id
                .as_ref()
                .is_none_or(|s| s.is_empty())
            || (observation.item_id.as_ref().is_none_or(|s| s.is_empty())
                && !(observation.source == "sandbox-create"
                    && observation.state == PayoutState::PayoutPending))
            || observation.source.is_empty()
        {
            return Err(ApiError::new(
                ErrorCode::WebhookMismatch,
                "provider facts differ from immutable intent",
            ));
        }
    }
    if payout
        .paypal_batch_id
        .as_ref()
        .zip(observation.paypal_batch_id.as_ref())
        .is_some_and(|(a, b)| a != b)
        || payout
            .item_id
            .as_ref()
            .zip(observation.item_id.as_ref())
            .is_some_and(|(a, b)| a != b)
    {
        return Err(ApiError::new(
            ErrorCode::WebhookMismatch,
            "provider identity changed",
        ));
    }
    let terminal = matches!(payout.state, PayoutState::Paid | PayoutState::PayoutFailed);
    let adjustment = matches!(
        observation.status.as_str(),
        "Refunded" | "Reversed" | "Unknown"
    );
    let conflict = terminal
        && matches!(
            observation.state,
            PayoutState::Paid | PayoutState::PayoutFailed
        )
        && payout.state != observation.state;
    let unproven_failure = observation.state == PayoutState::PayoutFailed
        && !observation.source.starts_with("mock-item-query")
        && !observation.source.starts_with("sandbox-verified:")
        && !observation.source.starts_with("sandbox-item-query:");
    if adjustment || conflict || (unproven_failure && !terminal) {
        let record = ReconciliationRecord {
            source: observation.source.clone(),
            status: observation.status.clone(),
            paypal_batch_id: observation.paypal_batch_id.clone(),
            item_id: observation.item_id.clone(),
        };
        if !payout
            .reconciliation
            .iter()
            .skip(released_count as usize)
            .any(|old| old == &record)
        {
            payout.reconciliation.push(record);
        }
    }
    if let Some(batch) = observation.paypal_batch_id {
        payout.paypal_batch_id = Some(batch);
    }
    if let Some(item) = observation.item_id {
        payout.item_id = Some(item);
    }
    if terminal || payout.reconciliation.len() as i64 > released_count {
        return Ok(());
    }
    if payout.state == PayoutState::PayoutUnknown && observation.state == PayoutState::PayoutPending
    {
        return Ok(());
    }
    let next = payout.state.transition(observation.state)?;
    if next == payout.state {
        return Ok(());
    }
    let claim = ledger.claims.get_mut(&id).unwrap();
    claim.state = claim.state.transition(next.claim_state())?;
    payout.state = next;
    if matches!(next, PayoutState::Paid | PayoutState::PayoutFailed) {
        let budget = ledger.budgets.get_mut(&claim.budget_line_id).unwrap();
        budget.reserved_cents = Cents::try_from(
            budget
                .reserved_cents
                .get()
                .checked_sub(claim.amount_cents.get())
                .ok_or_else(|| ApiError::new(ErrorCode::InvalidInput, "reservation underflow"))?,
        )
        .map_err(|_| ApiError::new(ErrorCode::InvalidInput, "reservation underflow"))?;
        if next == PayoutState::Paid {
            budget.paid_cents = budget.paid_cents.checked_add(claim.amount_cents)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_exclude_other_events() {
        let mut ledger = Ledger::default();
        ledger.seed();
        let other = EventId::new();
        let other_budget = BudgetLineId::new();
        let mut budget = ledger.budgets[&BUDGET_ID].clone();
        budget.id = other_budget;
        budget.event_id = other;
        ledger.budgets.insert(other_budget, budget);
        let local_claim = ClaimId::new();
        let other_claim = ClaimId::new();
        for (id, event_id, budget_line_id) in [
            (local_claim, EVENT_ID, BUDGET_ID),
            (other_claim, other, other_budget),
        ] {
            apply(
                &mut ledger,
                Command::Create {
                    id,
                    request: CreateClaimRequest {
                        event_id,
                        claimant_id: CLAIMANT_ID,
                        budget_line_id,
                        receipt_text: "Synthetic receipt".into(),
                    },
                },
            )
            .unwrap();
        }
        for (event, expected) in [(EVENT_ID, local_claim), (other, other_claim)] {
            let Output::Claims(response) = apply(&mut ledger, Command::Claims(event)).unwrap()
            else {
                panic!("expected claims");
            };
            assert_eq!(
                response
                    .claims
                    .iter()
                    .map(|row| row.claim.id)
                    .collect::<Vec<_>>(),
                vec![expected]
            );
        }
    }
}
