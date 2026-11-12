use chrono::NaiveDate;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! ids {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
    )+};
}
ids!(
    EventId,
    BudgetLineId,
    ClaimantId,
    ClaimId,
    ApprovalId,
    ApproverId,
    PayeeId,
    PayoutId
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct Cents(i64);
impl Cents {
    pub const ZERO: Self = Self(0);
    pub fn get(self) -> i64 {
        self.0
    }
    pub fn checked_add(self, other: Self) -> Result<Self, ApiError> {
        self.0
            .checked_add(other.0)
            .map(Self)
            .ok_or_else(|| ApiError::new(ErrorCode::InvalidInput, "amount overflow"))
    }
    pub fn checked_mul(self, quantity: u32) -> Result<Self, ApiError> {
        self.0
            .checked_mul(i64::from(quantity))
            .map(Self)
            .ok_or_else(|| ApiError::new(ErrorCode::InvalidInput, "amount overflow"))
    }
}
impl TryFrom<i64> for Cents {
    type Error = String;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value < 0 {
            Err("cents must be nonnegative".into())
        } else {
            Ok(Self(value))
        }
    }
}
impl From<Cents> for i64 {
    fn from(value: Cents) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ReceiptHash(String);
impl ReceiptHash {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ReceiptHash {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value))
        } else {
            Err("expected lowercase SHA-256 hex".into())
        }
    }
}
impl From<ReceiptHash> for String {
    fn from(value: ReceiptHash) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Currency {
    USD,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetLine {
    pub id: BudgetLineId,
    pub event_id: EventId,
    pub category: String,
    pub approved_cents: Cents,
    pub reserved_cents: Cents,
    pub paid_cents: Cents,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiptFields {
    pub merchant: Option<String>,
    pub date: Option<NaiveDate>,
    pub total_cents: Option<Cents>,
    pub currency: Option<Currency>,
    pub uncertain: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ClaimState {
    Draft,
    NeedsInfo,
    Submitted,
    DuplicateBlocked,
    OverBudget,
    Approved,
    PayoutPending,
    Paid,
    PayoutFailed,
    PayoutUnknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PayoutState {
    PayoutPending,
    Paid,
    PayoutFailed,
    PayoutUnknown,
}

pub const CLAIM_TRANSITIONS: &[(ClaimState, ClaimState)] = &[
    (ClaimState::Draft, ClaimState::NeedsInfo),
    (ClaimState::Draft, ClaimState::Submitted),
    (ClaimState::Draft, ClaimState::DuplicateBlocked),
    (ClaimState::Draft, ClaimState::OverBudget),
    (ClaimState::NeedsInfo, ClaimState::Submitted),
    (ClaimState::NeedsInfo, ClaimState::DuplicateBlocked),
    (ClaimState::NeedsInfo, ClaimState::OverBudget),
    (ClaimState::Submitted, ClaimState::Approved),
    (ClaimState::Approved, ClaimState::PayoutPending),
    (ClaimState::PayoutPending, ClaimState::Paid),
    (ClaimState::PayoutPending, ClaimState::PayoutFailed),
    (ClaimState::PayoutPending, ClaimState::PayoutUnknown),
    (ClaimState::PayoutUnknown, ClaimState::Paid),
    (ClaimState::PayoutUnknown, ClaimState::PayoutFailed),
];
pub const PAYOUT_TRANSITIONS: &[(PayoutState, PayoutState)] = &[
    (PayoutState::PayoutPending, PayoutState::Paid),
    (PayoutState::PayoutPending, PayoutState::PayoutFailed),
    (PayoutState::PayoutPending, PayoutState::PayoutUnknown),
    (PayoutState::PayoutUnknown, PayoutState::Paid),
    (PayoutState::PayoutUnknown, PayoutState::PayoutFailed),
];
impl ClaimState {
    pub fn transition(self, next: Self) -> Result<Self, ApiError> {
        if self == next || CLAIM_TRANSITIONS.contains(&(self, next)) {
            Ok(next)
        } else {
            Err(ApiError::new(
                ErrorCode::InvalidTransition,
                "claim transition forbidden",
            ))
        }
    }
}
impl PayoutState {
    pub fn transition(self, next: Self) -> Result<Self, ApiError> {
        if self == next || PAYOUT_TRANSITIONS.contains(&(self, next)) {
            Ok(next)
        } else {
            Err(ApiError::new(
                ErrorCode::InvalidTransition,
                "payout transition forbidden",
            ))
        }
    }
    pub fn claim_state(self) -> ClaimState {
        match self {
            Self::PayoutPending => ClaimState::PayoutPending,
            Self::Paid => ClaimState::Paid,
            Self::PayoutFailed => ClaimState::PayoutFailed,
            Self::PayoutUnknown => ClaimState::PayoutUnknown,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub id: ClaimId,
    pub event_id: EventId,
    pub claimant_id: ClaimantId,
    pub budget_line_id: BudgetLineId,
    pub receipt_sha256: ReceiptHash,
    pub fields: ReceiptFields,
    pub amount_cents: Cents,
    pub revision: u32,
    pub state: ClaimState,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub id: ApprovalId,
    pub claim_id: ClaimId,
    pub claim_revision: u32,
    pub approver_id: ApproverId,
    pub payee_id: PayeeId,
    pub amount_cents: Cents,
    pub receipt_sha256: ReceiptHash,
    pub expires_at: i64,
    /// Immutable receiver approved by finance; legacy approvals fail closed before send.
    #[serde(default)]
    pub receiver: String,
}
impl Approval {
    pub fn is_valid(&self, claim: &Claim, payee: PayeeId, now: i64) -> bool {
        self.claim_id == claim.id
            && self.claim_revision == claim.revision
            && self.payee_id == payee
            && self.amount_cents == claim.amount_cents
            && self.receipt_sha256 == claim.receipt_sha256
            && now < self.expires_at
    }
    pub fn sender_batch_id(&self) -> String {
        format!("rr-{}", self.id)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderBinding {
    pub sender_batch_id: String,
    pub sender_item_id: String,
    pub receiver: String,
    pub recipient_type: String,
    pub amount_cents: Cents,
    pub currency: Currency,
    pub payee_id: PayeeId,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReconciliationRecord {
    pub source: String,
    pub status: String,
    pub paypal_batch_id: Option<String>,
    pub item_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Payout {
    pub id: PayoutId,
    pub claim_id: ClaimId,
    pub approval_id: ApprovalId,
    pub sender_batch_id: String,
    pub paypal_batch_id: Option<String>,
    pub item_id: Option<String>,
    pub state: PayoutState,
    #[serde(default)]
    pub binding: Option<ProviderBinding>,
    #[serde(default)]
    pub reconciliation: Vec<ReconciliationRecord>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PayoutEventKind {
    Pending,
    Succeeded,
    Failed,
    Refunded,
    Reversed,
    Unknown,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayoutEvent {
    pub paypal_event_id: String,
    pub payout_id: PayoutId,
    pub kind: PayoutEventKind,
    pub verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Payee {
    pub id: PayeeId,
    pub claimant_id: ClaimantId,
    pub display_name: String,
    pub sandbox_email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateClaimRequest {
    pub event_id: EventId,
    pub claimant_id: ClaimantId,
    pub budget_line_id: BudgetLineId,
    pub receipt_text: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExtractClaimRequest {}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReceiptItem {
    pub quantity: u32,
    pub unit_cents: Cents,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubmitClaimRequest {
    pub fields: ReceiptFields,
    pub items: Vec<ReceiptItem>,
}
impl SubmitClaimRequest {
    pub fn amount(&self) -> Result<Cents, ApiError> {
        if self.items.is_empty() || self.items.iter().any(|i| i.quantity == 0) {
            return Err(ApiError::new(
                ErrorCode::InvalidInput,
                "nonempty items with positive quantities required",
            ));
        }
        let sum = self.items.iter().try_fold(Cents::ZERO, |sum, item| {
            sum.checked_add(item.unit_cents.checked_mul(item.quantity)?)
        })?;
        if sum == Cents::ZERO
            || self.fields.total_cents != Some(sum)
            || self.fields.currency != Some(Currency::USD)
            || !self.fields.uncertain.is_empty()
            || self
                .fields
                .merchant
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
            || self.fields.date.is_none()
        {
            return Err(ApiError::new(
                ErrorCode::NeedsInfo,
                "complete USD fields must match server-computed item sum",
            ));
        }
        Ok(sum)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApproveClaimRequest {
    pub payee_id: PayeeId,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayoutClaimRequest {}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimResponse {
    pub claim: Claim,
    pub approval: Option<Approval>,
    pub payout: Option<Payout>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetLinesResponse {
    pub budget_lines: Vec<BudgetLine>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayeesResponse {
    pub payees: Vec<Payee>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PayPalWebhookRequest {
    pub paypal_event_id: String,
    pub payout_id: PayoutId,
    pub sender_batch_id: String,
    pub paypal_batch_id: String,
    pub item_id: String,
    pub amount_cents: Cents,
    pub currency: Currency,
    pub payee_id: PayeeId,
    pub kind: PayoutEventKind,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebhookResponse {
    pub duplicate: bool,
    pub payout: Payout,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidInput,
    NotFound,
    InvalidTransition,
    NeedsInfo,
    DuplicateReceipt,
    OverBudget,
    ApprovalInvalid,
    PayeeNotAllowed,
    Forbidden,
    WebhookUnverified,
    WebhookMismatch,
    EventConflict,
    ModeNotImplemented,
    StorageUnavailable,
    AdapterUnavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}
impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}
impl std::error::Error for ApiError {}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimsResponse {
    pub claims: Vec<ClaimResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClaimReviewResponse {
    pub claim: ClaimResponse,
    pub receipt_text: String,
    pub submission: Option<SubmitClaimRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeMode {
    Mock,
    Sandbox,
    Manual,
    #[serde(rename = "gemini-free")]
    GeminiFree,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublicConfigResponse {
    pub paypal_mode: RuntimeMode,
    pub llm_mode: RuntimeMode,
    pub event_id: EventId,
    pub claimant_id: ClaimantId,
}

pub fn schemas() -> Vec<(&'static str, String)> {
    macro_rules! export { ($($ty:ty),+ $(,)?) => { vec![$((stringify!($ty), format!("{}\n", serde_json::to_string_pretty(&schemars::schema_for!($ty)).unwrap()))),+] }; }
    export!(
        ClaimsResponse,
        ClaimReviewResponse,
        PublicConfigResponse,
        BudgetLine,
        ReceiptFields,
        Claim,
        ClaimState,
        Approval,
        Payout,
        PayoutState,
        PayoutEvent,
        Payee,
        CreateClaimRequest,
        ExtractClaimRequest,
        SubmitClaimRequest,
        ApproveClaimRequest,
        PayoutClaimRequest,
        ClaimResponse,
        BudgetLinesResponse,
        PayeesResponse,
        PayPalWebhookRequest,
        WebhookResponse,
        ApiError
    )
}

#[cfg(test)]
mod tests;

impl JsonSchema for Cents {
    fn schema_name() -> String {
        "Cents".into()
    }
    fn json_schema(generator: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let mut schema = i64::json_schema(generator).into_object();
        schema.number().minimum = Some(0.0);
        schema.into()
    }
}
impl JsonSchema for ReceiptHash {
    fn schema_name() -> String {
        "ReceiptHash".into()
    }
    fn json_schema(generator: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let mut schema = String::json_schema(generator).into_object();
        schema.string().pattern = Some("^[0-9a-f]{64}$".into());
        schema.into()
    }
}
