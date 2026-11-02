use safeflow_contracts::{
    compute_operation_fingerprint, DecisionId, ExecutionObservation, ExecutionPermit,
    ExecutionPermitStatus, FingerprintPayloadV1, OperationEvent, OperationId, OperationState,
    PermitError, PermitId, PermitIssuanceRequest, PermitUseRequest, ReasonCode, ResourceId,
    SecurityOutcome, ToolName, UserId,
};
use serde_json::json;

fn fingerprint(amount: &str) -> safeflow_contracts::OperationFingerprint {
    compute_operation_fingerprint(&FingerprintPayloadV1 {
        user_id: UserId::parse("user_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        resource_id: ResourceId::parse("draft_001").unwrap(),
        normalized_arguments: json!({
            "amount": amount,
            "currency": "CNY",
            "payee_id": "payee_008"
        }),
        plan_version: 1,
        schema_version: 1,
    })
    .unwrap()
}

fn issue_permit(
    operation_fingerprint: safeflow_contracts::OperationFingerprint,
) -> ExecutionPermit {
    ExecutionPermit::issue(PermitIssuanceRequest {
        permit_id: PermitId::parse("permit_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        decision_id: DecisionId::parse("decision_001").unwrap(),
        user_id: UserId::parse("user_001").unwrap(),
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        fingerprint: operation_fingerprint,
        facts_version: 1,
        issued_at_ms: 1_000,
        expires_at_ms: 2_000,
        security_outcome: SecurityOutcome::ReadyToExecute,
    })
    .unwrap()
}

fn use_request(
    operation_fingerprint: safeflow_contracts::OperationFingerprint,
) -> PermitUseRequest {
    PermitUseRequest {
        operation_id: OperationId::parse("operation_001").unwrap(),
        user_id: UserId::parse("user_001").unwrap(),
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        fingerprint: operation_fingerprint,
        facts_version: 1,
        now_ms: 1_500,
    }
}

#[test]
fn a_write_permit_can_only_be_issued_from_ready_to_execute() {
    let result = ExecutionPermit::issue(PermitIssuanceRequest {
        permit_id: PermitId::parse("permit_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        decision_id: DecisionId::parse("decision_001").unwrap(),
        user_id: UserId::parse("user_001").unwrap(),
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        fingerprint: fingerprint("200.00"),
        facts_version: 1,
        issued_at_ms: 1_000,
        expires_at_ms: 2_000,
        security_outcome: SecurityOutcome::WaitingAuth,
    });
    assert!(matches!(
        result,
        Err(PermitError::DecisionNotReady(SecurityOutcome::WaitingAuth))
    ));
}

#[test]
fn changing_the_amount_invalidates_the_permit() {
    let permit = issue_permit(fingerprint("200.00"));
    let result = permit.validate_for_use(&use_request(fingerprint("2000.00")));
    assert_eq!(
        result,
        Err(PermitError::BindingMismatch("operation_fingerprint"))
    );
}

#[test]
fn a_consumed_permit_cannot_be_used_again() {
    let operation_fingerprint = fingerprint("200.00");
    let request = use_request(operation_fingerprint.clone());
    let permit = issue_permit(operation_fingerprint)
        .consume(&request)
        .unwrap();
    assert_eq!(permit.status(), ExecutionPermitStatus::Consumed);
    assert_eq!(
        permit.validate_for_use(&request),
        Err(PermitError::NotActive(ExecutionPermitStatus::Consumed))
    );
}

#[test]
fn an_expired_permit_is_rejected() {
    let operation_fingerprint = fingerprint("200.00");
    let permit = issue_permit(operation_fingerprint.clone());
    let mut request = use_request(operation_fingerprint);
    request.now_ms = 2_000;
    assert_eq!(permit.validate_for_use(&request), Err(PermitError::Expired));
}

#[test]
fn an_unknown_submission_outcome_cannot_return_to_ready() {
    let processing = OperationState::Ready
        .transition(OperationEvent::SubmissionRecorded)
        .unwrap()
        .transition(OperationEvent::OutcomeUnknown)
        .unwrap();
    assert_eq!(processing, OperationState::Processing);
    assert!(processing
        .transition(OperationEvent::SubmissionRecorded)
        .is_err());
}

#[test]
fn timeout_observation_never_allows_resubmission() {
    let observation = ExecutionObservation::OutcomeUnknown {
        attempt_id: safeflow_contracts::AttemptId::parse("attempt_001").unwrap(),
        business_operation_ref: safeflow_contracts::BusinessOperationRef::parse("bank_op_001")
            .unwrap(),
        reason_code: ReasonCode::parse("UPSTREAM_TIMEOUT").unwrap(),
    };
    assert!(!observation.submission_is_proven_absent());
    assert!(observation.requires_reconciliation());
}
