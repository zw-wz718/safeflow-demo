use super::*;
use safeflow_contracts::{AttemptId, BusinessOperationRef, ReasonCode, ToolResultRef};
use serde_json::json;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/received_transfer_2a72b5d7.json")).unwrap()
}
fn review(
    request: &Value,
    receipt: &Value,
) -> Result<MockTransferReceiptProposalReview, ReceiptProposalError> {
    review_mock_transfer_receipt_proposal(
        &serde_json::to_vec(request).unwrap(),
        &serde_json::to_vec(receipt).unwrap(),
    )
}
fn error(request: &Value, receipt: &Value) -> ReceiptProposalError {
    review(request, receipt).err().unwrap()
}

#[test]
fn received_fixed_four_examples_are_only_unverified_field_comparisons() {
    let fixture = fixture();
    for case in ["success", "failure", "processing", "lost_reply"] {
        let summary = serde_json::to_value(
            review(&fixture["request"], &fixture[case])
                .unwrap()
                .summary(),
        )
        .unwrap();
        assert_eq!(summary["scope"], "UNVERIFIED_RECEIPT_PROPOSAL");
        assert_eq!(summary["binding_scope"], "RECEIPT_FIELDS_ONLY");
        assert_eq!(summary["post_execute_assessed"], false);
        assert_eq!(summary["requires_authoritative_readback"], true);
        assert_eq!(summary["business_operation_id"], "business-transfer-001");
        assert_eq!(summary["declared_status"], fixture[case]["status"]);
        assert!(summary.get("verified_outcome").is_none());
        assert!(summary.get("next_action").is_none());
        assert!(summary["unproved_bindings"]
            .as_array()
            .unwrap()
            .contains(&json!("REMARK")));
    }
}
#[test]
fn echoed_request_fields_and_business_draft_version_must_match_exactly() {
    let fixture = fixture();
    for (field, value) in [
        ("task_id", json!("other_task")),
        ("operation_id", json!("runtime_tool_operation")),
        ("draft_id", json!("other_draft")),
        ("draft_version", json!(2)),
        ("payer_account_id", json!("other_payer")),
        ("payee_id", json!("other_payee")),
        ("amount", json!("201.00")),
    ] {
        let mut receipt = fixture["success"].clone();
        receipt[field] = value;
        assert_eq!(
            error(&fixture["request"], &receipt),
            ReceiptProposalError::BindingMismatch(field)
        );
    }
    let mut version = fixture["success"].clone();
    version["proposal_version"] = json!(2);
    assert_eq!(
        error(&fixture["request"], &version),
        ReceiptProposalError::UnsupportedVersion
    );
    let mut remark = fixture["request"].clone();
    remark["remark"] = json!("different valid remark");
    assert!(review(&remark, &fixture["success"]).is_ok());
    // Receipt v1 cannot distinguish remarks; success above must not claim full binding.
    assert_eq!(
        serde_json::to_value(review(&remark, &fixture["success"]).unwrap().summary()).unwrap()
            ["binding_scope"],
        "RECEIPT_FIELDS_ONLY"
    );
}
#[test]
fn raw_duplicate_unknown_float_null_omission_and_resource_limits_are_rejected() {
    let fixture = fixture();
    let expected = serde_json::to_vec(&fixture["request"]).unwrap();
    let receipt = fixture["success"].to_string();
    let duplicate = receipt.replacen(
        "\"proposal_version\":1",
        "\"proposal_version\":1,\"proposal_version\":1",
        1,
    );
    let escaped = receipt.replacen(
        "\"proposal_version\":1",
        "\"proposal_version\":1,\"\\u0070roposal_version\":1",
        1,
    );
    for raw in [duplicate, escaped, " ".repeat(65537)] {
        assert!(review_mock_transfer_receipt_proposal(&expected, raw.as_bytes()).is_err());
    }
    for field in ["reason_code", "completed_at"] {
        let mut omitted = fixture["success"].clone();
        omitted.as_object_mut().unwrap().remove(field);
        assert_eq!(
            error(&fixture["request"], &omitted),
            ReceiptProposalError::InvalidReceipt
        );
    }
    let mut unknown = fixture["success"].clone();
    unknown["permit"] = json!("claimed");
    assert_eq!(
        error(&fixture["request"], &unknown),
        ReceiptProposalError::InvalidReceipt
    );
    let mut floating = fixture["success"].clone();
    floating["amount"] = json!(200.0);
    assert_eq!(
        error(&fixture["request"], &floating),
        ReceiptProposalError::InvalidJson
    );
    let mut invalid = fixture["request"].clone();
    invalid["authenticated_user_id"] = json!("user_demo");
    assert_eq!(
        error(&invalid, &fixture["success"]),
        ReceiptProposalError::InvalidRequest
    );
    let mut invalid = fixture["request"].clone();
    invalid["idempotency_key"] = json!("transfer-key-001");
    assert_eq!(
        error(&invalid, &fixture["success"]),
        ReceiptProposalError::InvalidRequest
    );
}
#[test]
fn known_state_time_and_reason_combinations_are_checked_without_new_business_codes() {
    let fixture = fixture();
    for (case, field, value) in [
        ("success", "completed_at", Value::Null),
        ("success", "completed_at", json!("not a time")),
        (
            "success",
            "completed_at",
            json!("2026-10-07T08:00:00+08:00"),
        ),
        ("success", "reason_code", json!("BANK_DECLINED")),
        ("failure", "reason_code", Value::Null),
        ("processing", "reason_code", json!("NEW_UNCONFIRMED_CODE")),
        ("processing", "completed_at", json!("2026-10-07T00:00:00Z")),
        ("failure", "reason_code", json!([])),
    ] {
        let mut receipt = fixture[case].clone();
        receipt[field] = value;
        assert_eq!(
            error(&fixture["request"], &receipt),
            ReceiptProposalError::InvalidReceipt
        );
    }
    for amount in ["0.00", "200.0", "0200.00", "18446744073709551615.00"] {
        let mut request = fixture["request"].clone();
        request["amount"] = json!(amount);
        assert_eq!(
            error(&request, &fixture["success"]),
            ReceiptProposalError::InvalidRequest
        );
    }
    for field in [
        "task_id",
        "operation_id",
        "draft_id",
        "payer_account_id",
        "payee_id",
    ] {
        let mut request = fixture["request"].clone();
        request[field] = json!("id with space");
        assert_eq!(
            error(&request, &fixture["success"]),
            ReceiptProposalError::InvalidRequest
        );
    }
}
fn received_observation() -> ExecutionObservation {
    ExecutionObservation::ResultReceived {
        attempt_id: AttemptId::parse("existing_attempt").unwrap(),
        business_operation_ref: BusinessOperationRef::parse("business-transfer-001").unwrap(),
        tool_result_ref: ToolResultRef::parse("existing_result").unwrap(),
    }
}
#[test]
fn result_received_can_describe_failure_and_does_not_prove_success() {
    let fixture = fixture();
    let observation = received_observation();
    let failed = review(&fixture["request"], &fixture["failure"]).unwrap();
    assert_eq!(
        failed
            .compare_existing_observation(
                &observation,
                ReceiptObservationPhase::SameSubmissionAttempt
            )
            .unwrap(),
        ReceiptObservationComparison::SameAttemptFieldsCompatible
    );
    let mut wrong = received_observation();
    if let ExecutionObservation::ResultReceived {
        business_operation_ref,
        ..
    } = &mut wrong
    {
        *business_operation_ref = BusinessOperationRef::parse("runtime_operation_1").unwrap();
    }
    assert_eq!(
        failed
            .compare_existing_observation(&wrong, ReceiptObservationPhase::SameSubmissionAttempt)
            .err()
            .unwrap(),
        ReceiptProposalError::BindingMismatch("business_operation_ref")
    );
    let existing_public_domain = ExecutionObservation::ResultReceived {
        attempt_id: AttemptId::parse(format!("existing attempt {}", "x".repeat(300))).unwrap(),
        business_operation_ref: BusinessOperationRef::parse("business-transfer-001").unwrap(),
        tool_result_ref: ToolResultRef::parse(format!("existing result {}", "x".repeat(300)))
            .unwrap(),
    };
    assert!(failed
        .compare_existing_observation(
            &existing_public_domain,
            ReceiptObservationPhase::SameSubmissionAttempt
        )
        .is_ok());
}
#[test]
fn later_query_can_resolve_prior_unknown_without_rewriting_or_resubmitting() {
    let fixture = fixture();
    let unknown = ExecutionObservation::OutcomeUnknown {
        attempt_id: AttemptId::parse("existing_attempt").unwrap(),
        business_operation_ref: BusinessOperationRef::parse("business-transfer-001").unwrap(),
        reason_code: ReasonCode::parse("OUTCOME_UNKNOWN").unwrap(),
    };
    let success = review(&fixture["request"], &fixture["success"]).unwrap();
    assert_eq!(
        success
            .compare_existing_observation(&unknown, ReceiptObservationPhase::SameSubmissionAttempt)
            .err()
            .unwrap(),
        ReceiptProposalError::ObservationConflict
    );
    assert_eq!(
        success
            .compare_existing_observation(&unknown, ReceiptObservationPhase::LaterQuery)
            .unwrap(),
        ReceiptObservationComparison::LaterQueryFieldsCompatible
    );
    let lost = review(&fixture["request"], &fixture["lost_reply"]).unwrap();
    assert!(lost
        .compare_existing_observation(&unknown, ReceiptObservationPhase::SameSubmissionAttempt)
        .is_ok());
    let absent = ExecutionObservation::NotSubmitted {
        attempt_id: AttemptId::parse("existing_attempt").unwrap(),
        reason_code: ReasonCode::parse("LOCAL_VALIDATION_FAILED").unwrap(),
    };
    assert_eq!(
        success
            .compare_existing_observation(&absent, ReceiptObservationPhase::SameSubmissionAttempt)
            .err()
            .unwrap(),
        ReceiptProposalError::ObservationConflict
    );
    assert_eq!(
        success
            .compare_existing_observation(&absent, ReceiptObservationPhase::LaterQuery)
            .unwrap(),
        ReceiptObservationComparison::EarlierAttemptNotComparable
    );
}
