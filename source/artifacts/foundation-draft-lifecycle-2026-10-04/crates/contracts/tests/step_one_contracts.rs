use safeflow_contracts::{
    AgentPlan, AgentTool, ArgumentsHash, AuthGrantRef, ConfirmationRef, DecisionId, DomainPolicy,
    ExecutionContext, IdempotencyKey, NodeId, OperationId, OperationSecurityRequest,
    PlanAcceptanceRequest, PlanContractError, PolicyVersion, RiskTier, SecurityContractError,
    SecurityDecision, SecurityGate, SecurityKernel, SecurityLevel, SecurityOutcome, SecurityPhase,
    SessionRef, TaskId, ToolDefinition, ToolError, ToolExecutionAuthorization, ToolName,
    ToolResult, UserId, AGENT_TOOL_CONTRACT_VERSION, SECURITY_CONTRACT_VERSION,
};
use serde_json::{json, Value};

fn operation_request(phase: SecurityPhase) -> OperationSecurityRequest {
    OperationSecurityRequest {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: TaskId::parse("task_001").unwrap(),
        node_id: NodeId::parse("node_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        facts_version: 1,
        phase,
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        plan_version: 1,
        session_ref: SessionRef::parse("session_001").unwrap(),
        normalized_arguments: json!({"amount": "200.00", "currency": "CNY"}),
        arguments_hash: ArgumentsHash::parse("a".repeat(64)).unwrap(),
        idempotency_key: None,
        confirmation_ref: None,
        auth_grant_ref: None,
    }
}

#[test]
fn plan_acceptance_envelope_must_match_the_typed_plan() {
    let plan = AgentPlan::new(TaskId::parse("task_001").unwrap(), 1, vec![]).unwrap();
    let mut request = PlanAcceptanceRequest::new(plan).unwrap();
    request.task_id = TaskId::parse("task_002").unwrap();

    assert_eq!(
        request.validate(),
        Err(SecurityContractError::Plan(PlanContractError::TaskMismatch))
    );
}

#[test]
fn operation_request_rejects_model_supplied_user_identity() {
    let value = json!({
        "contract_version": 1,
        "task_id": "task_001",
        "node_id": "node_001",
        "operation_id": "operation_001",
        "facts_version": 1,
        "phase": "PRE_TOOL",
        "tool_name": "execute_transfer",
        "plan_version": 1,
        "session_ref": "session_001",
        "normalized_arguments": {"amount": "200.00"},
        "arguments_hash": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "idempotency_key": null,
        "confirmation_ref": null,
        "auth_grant_ref": null,
        "user_id": "attacker_selected_user"
    });

    assert!(serde_json::from_value::<OperationSecurityRequest>(value).is_err());
}

#[test]
fn security_context_replaces_session_reference_with_trusted_user() {
    let context = safeflow_contracts::SecurityContext::from_request(
        operation_request(SecurityPhase::PreTool),
        UserId::parse("trusted_user_001").unwrap(),
    )
    .unwrap();
    let serialized = serde_json::to_value(context).unwrap();

    assert_eq!(serialized["authenticated_user_id"], "trusted_user_001");
    assert!(serialized.get("session_ref").is_none());
    assert!(serialized.get("user_id").is_none());
}

#[test]
fn pre_commit_requires_idempotency_confirmation_and_authentication() {
    let mut request = operation_request(SecurityPhase::PreCommit);
    assert_eq!(
        request.validate(),
        Err(SecurityContractError::MissingPreCommitReference(
            "idempotency_key"
        ))
    );

    request.idempotency_key = Some(IdempotencyKey::parse("idem_001").unwrap());
    assert_eq!(
        request.validate(),
        Err(SecurityContractError::MissingPreCommitReference(
            "confirmation_ref"
        ))
    );

    request.confirmation_ref = Some(ConfirmationRef::parse("confirmation_001").unwrap());
    assert_eq!(
        request.validate(),
        Err(SecurityContractError::MissingPreCommitReference(
            "auth_grant_ref"
        ))
    );

    request.auth_grant_ref = Some(AuthGrantRef::parse("auth_grant_001").unwrap());
    assert_eq!(request.validate(), Ok(()));
}

#[test]
fn security_decision_rejects_scores_above_the_frozen_range() {
    let decision = SecurityDecision {
        contract_version: SECURITY_CONTRACT_VERSION,
        decision_id: DecisionId::parse("decision_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        facts_version: 1,
        policy_version: PolicyVersion::parse("risk-v0.1").unwrap(),
        outcome: SecurityOutcome::Blocked,
        minimum_level: SecurityLevel::Critical,
        risk_score: 101,
        risk_tier: RiskTier::Critical,
        required_gates: vec![SecurityGate::StrongAuthentication],
        findings: vec![],
        expires_at_ms: 2_000,
    };

    assert_eq!(
        decision.validate(),
        Err(SecurityContractError::RiskScoreOutOfRange)
    );
}

#[test]
fn public_interfaces_are_object_safe_for_future_registries() {
    struct MockTool;

    impl AgentTool for MockTool {
        fn definition(&self) -> ToolDefinition {
            ToolDefinition::new(
                ToolName::parse("get_account_balance").unwrap(),
                "Read the authenticated user's account balance",
                json!({"type": "object"}),
                1,
                false,
                "READ_BALANCE",
            )
            .unwrap()
        }

        fn execute<'a>(
            &'a self,
            _context: &'a ExecutionContext,
            authorization: ToolExecutionAuthorization<'a>,
            _arguments: Value,
        ) -> safeflow_contracts::ContractFuture<'a, Result<ToolResult, ToolError>> {
            Box::pin(async move {
                assert!(!authorization.permits_side_effects());
                panic!("the object-safety test must not poll the execution future")
            })
        }
    }

    let tool: &dyn AgentTool = &MockTool;
    let _policy: Option<&dyn DomainPolicy> = None;
    let _kernel: Option<&dyn SecurityKernel> = None;
    assert_eq!(
        tool.definition().contract_version,
        AGENT_TOOL_CONTRACT_VERSION
    );
    assert!(!tool.definition().has_side_effect);
}
