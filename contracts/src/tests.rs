use super::*;

#[test]
fn every_declared_transition_and_identity_is_allowed() {
    for &(from, to) in CLAIM_TRANSITIONS {
        assert_eq!(from.transition(to).unwrap(), to);
        assert_eq!(from.transition(from).unwrap(), from);
    }
    for &(from, to) in PAYOUT_TRANSITIONS {
        assert_eq!(from.transition(to).unwrap(), to);
        assert_eq!(from.transition(from).unwrap(), from);
    }
}
#[test]
fn forbidden_and_terminal_transitions_fail() {
    for state in [
        ClaimState::Draft,
        ClaimState::Submitted,
        ClaimState::Approved,
        ClaimState::PayoutUnknown,
        ClaimState::PayoutFailed,
    ] {
        assert!(ClaimState::Paid.transition(state).is_err());
    }
    assert!(ClaimState::Draft.transition(ClaimState::Paid).is_err());
    assert!(ClaimState::Approved.transition(ClaimState::Draft).is_err());
    assert!(PayoutState::Paid
        .transition(PayoutState::PayoutUnknown)
        .is_err());
    assert!(PayoutState::PayoutFailed
        .transition(PayoutState::Paid)
        .is_err());
}
#[test]
fn money_and_receipt_hash_validate_at_boundary() {
    assert!(serde_json::from_str::<Cents>("-1").is_err());
    assert!(serde_json::from_str::<Cents>("1.2").is_err());
    assert!(Cents::try_from(i64::MAX)
        .unwrap()
        .checked_add(Cents::try_from(1).unwrap())
        .is_err());
    assert!(ReceiptHash::try_from("x".repeat(64)).is_err());
}
#[test]
fn approval_binds_every_fact_and_expiry() {
    let mut claim = Claim {
        id: ClaimId::new(),
        event_id: EventId::new(),
        claimant_id: ClaimantId::new(),
        budget_line_id: BudgetLineId::new(),
        receipt_sha256: ReceiptHash::try_from("a".repeat(64)).unwrap(),
        fields: ReceiptFields::default(),
        amount_cents: Cents::try_from(120).unwrap(),
        revision: 1,
        state: ClaimState::Approved,
    };
    let approval = Approval {
        id: ApprovalId::new(),
        claim_id: claim.id,
        claim_revision: claim.revision,
        approver_id: ApproverId::new(),
        payee_id: PayeeId::new(),
        amount_cents: claim.amount_cents,
        receipt_sha256: claim.receipt_sha256.clone(),
        expires_at: 100,
        receiver: "approved@example.invalid".into(),
    };
    assert!(approval.is_valid(&claim, approval.payee_id, 99));
    assert!(!approval.is_valid(&claim, approval.payee_id, 100));
    assert!(!approval.is_valid(&claim, PayeeId::new(), 99));
    claim.revision += 1;
    assert!(!approval.is_valid(&claim, approval.payee_id, 99));
    claim.revision -= 1;
    claim.amount_cents = Cents::ZERO;
    assert!(!approval.is_valid(&claim, approval.payee_id, 99));
    claim.amount_cents = approval.amount_cents;
    claim.receipt_sha256 = ReceiptHash::try_from("b".repeat(64)).unwrap();
    assert!(!approval.is_valid(&claim, approval.payee_id, 99));
    assert_eq!(approval.sender_batch_id(), format!("rr-{}", approval.id));
    assert_eq!(
        approval.sender_batch_id(),
        approval.clone().sender_batch_id()
    );
}
#[test]
fn amount_is_recomputed_with_checked_integer_math() {
    let mut request = SubmitClaimRequest {
        fields: ReceiptFields {
            merchant: Some("Fake shop".into()),
            date: Some(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            total_cents: Some(Cents::try_from(250).unwrap()),
            currency: Some(Currency::USD),
            uncertain: vec![],
        },
        items: vec![ReceiptItem {
            quantity: 2,
            unit_cents: Cents::try_from(125).unwrap(),
        }],
    };
    assert_eq!(request.amount().unwrap().get(), 250);
    request.fields.total_cents = Some(Cents::try_from(251).unwrap());
    assert_eq!(request.amount().unwrap_err().code, ErrorCode::NeedsInfo);
    request.items[0].unit_cents = Cents::try_from(i64::MAX).unwrap();
    assert_eq!(request.amount().unwrap_err().code, ErrorCode::InvalidInput);
}
#[test]
fn schema_drift() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema");
    let expected = schemas();
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), expected.len());
    for (name, json) in expected {
        assert_eq!(
            std::fs::read_to_string(root.join(format!("{name}.json"))).unwrap(),
            json,
            "schema drift for {name}; run cargo run -p contracts --bin export-schema"
        );
    }
}

#[test]
fn schemas_express_validated_money_and_receipt_constraints() {
    let schema = serde_json::to_value(schemars::schema_for!(Claim)).unwrap();
    assert_eq!(schema["definitions"]["Cents"]["minimum"], 0.0);
    assert_eq!(
        schema["definitions"]["ReceiptHash"]["pattern"],
        "^[0-9a-f]{64}$"
    );
}
