use safeflow_subscription_evidence::{peer_review::inspect_transfer_fixture, Transaction};
use serde_json::{json, Value};

const PEER: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../../fixtures/subscription/peer_sources/transfer_mock_200_2a72b5d.json"
));

#[test]
fn pinned_actual_peer_fixture_is_reviewed_without_claiming_integration() {
    let review = inspect_transfer_fixture(PEER).unwrap();
    let saved: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../fixtures/subscription/SafeFlow_SUB-01_转账样例核对实测_v0.1.json"
    )))
    .unwrap();
    assert_eq!(serde_json::to_value(&review).unwrap(), saved);
    assert_eq!(review.amount_minor_units, 20_000);
    assert!(!review.direct_subscription_input_compatible);
    assert!(!review.recognition_executed);
    assert!(!review.production_registration);
    assert!(!review.card_reference_present);
    assert!(!review.merchant_reference_present);
    assert!(review
        .unresolved
        .contains(&"CATEGORY_CODE_MISSING_FROM_SOURCE"));
    assert!(review
        .cautions
        .contains(&"TRANSFER_IS_NOT_SUBSCRIPTION_CONSUMPTION"));
    // This fixture cannot currently be deserialized as subscription input.
    let value: Value = serde_json::from_str(PEER).unwrap();
    assert!(
        serde_json::from_value::<Transaction>(value["success_debit_transaction"].clone()).is_err()
    );
}

#[test]
fn lost_reply_remains_distinct_from_pending_settlement_and_ledger_completion() {
    let value: Value = serde_json::from_str(PEER).unwrap();
    assert_eq!(value["lost_reply"]["status"], "PROCESSING");
    assert_eq!(value["lost_reply"]["reason_code"], "OUTCOME_UNKNOWN");
    assert_eq!(value["processing"]["reason_code"], "AWAITING_BANK_RESULT");
    assert_eq!(value["success_debit_transaction"]["status"], "COMPLETED");
    let review = inspect_transfer_fixture(PEER).unwrap();
    assert!(review
        .cautions
        .contains(&"UNKNOWN_RECEIPT_DOES_NOT_PROVE_NO_COMPLETED_LEDGER"));
}

#[test]
fn review_never_exports_fixture_identity_or_operation_identifiers() {
    let review = serde_json::to_string(&inspect_transfer_fixture(PEER).unwrap()).unwrap();
    for private in [
        "user_demo",
        "account_001",
        "payee_008",
        "business-transfer-001",
        "draft_demo",
    ] {
        assert!(!review.contains(private));
    }
}

#[test]
fn changed_money_currency_account_time_or_transaction_kind_requires_review() {
    for (key, value) in [
        ("amount", json!("200.01")),
        ("currency", json!("USD")),
        ("account_id", json!("another-account")),
        ("completed_at", json!("not-a-time")),
        ("transaction_type", json!("CONSUMPTION")),
        ("direction", json!("OUT")),
        ("status", json!("SUCCEEDED")),
        ("card_id", json!("guessed-card")),
        ("merchant_id", json!("guessed-merchant")),
        ("category_code", Value::Null),
    ] {
        let mut source: Value = serde_json::from_str(PEER).unwrap();
        source["success_debit_transaction"][key] = value;
        assert!(
            inspect_transfer_fixture(&source.to_string()).is_err(),
            "{key}"
        );
    }
}

#[test]
fn conflicting_receipt_operation_and_completion_facts_are_rejected() {
    for (receipt, key, value) in [
        ("lost_reply", "operation_id", json!("different-operation")),
        ("processing", "completed_at", json!("2026-10-07T00:00:00Z")),
        ("failure", "status", json!("PROCESSING")),
        ("success", "reason_code", json!("OUTCOME_UNKNOWN")),
        ("success", "proposal_version", json!(2)),
    ] {
        let mut source: Value = serde_json::from_str(PEER).unwrap();
        source[receipt][key] = value;
        assert!(inspect_transfer_fixture(&source.to_string()).is_err());
    }
}

#[test]
fn absence_of_receipt_or_ledger_is_not_inferred_as_success_or_no_debit() {
    for key in [
        "request",
        "lost_reply",
        "success",
        "success_debit_transaction",
    ] {
        let mut source: Value = serde_json::from_str(PEER).unwrap();
        source.as_object_mut().unwrap().remove(key);
        assert!(inspect_transfer_fixture(&source.to_string()).is_err());
    }
    assert!(inspect_transfer_fixture("not-json").is_err());
}
