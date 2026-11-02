mod support;
use safeflow_card_runtime_adapter::{
    GetAccountCardFactsPolicy, GetAccountCardFactsTool, GET_ACCOUNT_CARD_FACTS,
};
use safeflow_contracts::{
    AgentTool, AttemptId, BusinessOperationRef, DomainPolicy, ExecutionContext,
    ExecutionObservation, NodeId, OperationId, PostExecuteContext, PostExecuteRequest,
    SecurityLevel, SecurityOutcome, TaskId, ToolExecutionAuthorization, ToolName, ToolResultRef,
    UserId,
};
use safeflow_runtime::{AuditEventKind, RuntimeError};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use support::*;

fn context() -> ExecutionContext {
    ExecutionContext {
        task_id: TaskId::parse("t1").unwrap(),
        node_id: NodeId::parse("n1").unwrap(),
        operation_id: OperationId::parse("op1").unwrap(),
        authenticated_user_id: UserId::parse("U1").unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}

fn direct(
    source: std::sync::Arc<Source>,
    arguments: Value,
) -> Result<safeflow_contracts::ToolResult, safeflow_contracts::ToolError> {
    block_on(GetAccountCardFactsTool::new(source).execute(
        &context(),
        ToolExecutionAuthorization::ReadOnly,
        arguments,
    ))
}

#[test]
fn runtime_returns_all_card_locks_and_account_wide_d02_rules() {
    let source = Source::new(Mode::Locked);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    let outcome = block_on(
        app.runtime()
            .execute(plan(json!({"account_id": "A"})), session()),
    )
    .unwrap();
    let safeflow_runtime::RuntimeOutcome::Completed { results, .. } = outcome else {
        panic!("expected a completed read");
    };
    assert_eq!(results.len(), 1);
    let data = &results[0].data;
    assert_eq!(data["cards"].as_array().unwrap().len(), 2);
    assert_eq!(data["cards"][0]["active_locks"], json!([]));
    assert_eq!(data["active_card_lock_ids"], json!(["L0", "L1", "L2"]));
    for use_name in [
        "all_outgoing",
        "transfer",
        "card_spend",
        "automatic_debit",
        "other_outgoing",
    ] {
        assert_eq!(data["card_lock_restrictions"][use_name], true);
    }
    for use_name in ["query", "receive"] {
        assert_eq!(data["card_lock_restrictions"][use_name], false);
    }
    assert_eq!(data["unlock_route"], "OFFLINE_BANK");
    assert_eq!(data["all_active_locks_must_be_released"], true);
    // Existing Runtime evaluates PRE_TOOL and PRE_AUTH, then executes the Tool.
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
    assert_eq!(source.revisions.load(Ordering::SeqCst), 3);
    let events: Vec<_> = audit
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert!(events.contains(&AuditEventKind::ToolCompleted));
    assert!(events.contains(&AuditEventKind::TaskCompleted));
}

#[test]
fn zero_locks_is_only_a_scoped_fact_and_not_payment_permission() {
    let result = direct(Source::new(Mode::Clear), json!({"account_id": "A"})).unwrap();
    assert_eq!(result.data["scope"], "CARD_LOCK_FACTS_ONLY");
    assert_eq!(result.data["card_lock_restrictions"]["all_outgoing"], false);
    for field in [
        "permit",
        "can_transfer",
        "authorized",
        "account_healthy",
        "user_id",
        "internal_evidence",
    ] {
        assert!(result.data.get(field).is_none());
    }
}

#[test]
fn another_owned_account_is_not_restricted_by_a_locks() {
    let source = Source::new(Mode::Locked);
    assert_eq!(
        direct(source.clone(), json!({"account_id":"A"}))
            .unwrap()
            .data["card_lock_restrictions"]["all_outgoing"],
        true
    );
    assert_eq!(
        direct(source, json!({"account_id":"C"})).unwrap().data["card_lock_restrictions"]
            ["all_outgoing"],
        false
    );
}

#[test]
fn foreign_missing_and_tail_number_queries_have_same_outward_error() {
    for account in ["B", "missing", "1234"] {
        let error = direct(Source::new(Mode::Locked), json!({"account_id":account})).unwrap_err();
        assert_eq!(error.reason_code.as_str(), "CARD_RESOURCE_UNAVAILABLE");
        assert!(!error.message.contains(account));
    }
}

#[test]
fn foreign_account_is_blocked_before_tool_starts() {
    let source = Source::new(Mode::Foreign);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    assert_eq!(
        block_on(
            app.runtime()
                .execute(plan(json!({"account_id":"A"})), session())
        ),
        Err(RuntimeError::SecurityGate(SecurityOutcome::Blocked))
    );
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::ToolStarted));
}

#[test]
fn injected_identity_and_control_fields_are_rejected_without_source_reads() {
    for field in [
        "user_id",
        "authenticated_user_id",
        "session_ref",
        "permit",
        "confirmed",
        "locks",
        "revision",
    ] {
        let source = Source::new(Mode::Locked);
        let mut input = json!({"account_id":"A"});
        input[field] = json!("injected");
        assert_eq!(
            direct(source.clone(), input)
                .unwrap_err()
                .reason_code
                .as_str(),
            "VALIDATION_ERROR"
        );
        assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn invalid_ids_are_rejected_without_silent_normalization() {
    for value in [
        json!(null),
        json!(23),
        json!(""),
        json!(" A"),
        json!("A B"),
        json!("A\n"),
        json!("A\0"),
        json!("卡".repeat(129)),
    ] {
        let source = Source::new(Mode::Locked);
        assert_eq!(
            direct(source.clone(), json!({"account_id":value}))
                .unwrap_err()
                .reason_code
                .as_str(),
            "VALIDATION_ERROR"
        );
        assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn failed_incomplete_or_changed_reads_cannot_become_zero_lock_success() {
    for (mode, code) in [
        (Mode::Unavailable, "CARD_LOOKUP_UNAVAILABLE"),
        (Mode::Incomplete, "CARD_DATA_INCOMPLETE"),
        (Mode::Changed, "CARD_SNAPSHOT_CHANGED"),
    ] {
        assert_eq!(
            direct(Source::new(mode), json!({"account_id":"A"}))
                .unwrap_err()
                .reason_code
                .as_str(),
            code
        );
        let (app, audit) = assembly(Source::new(mode), SecurityLevel::Green);
        assert_eq!(
            block_on(
                app.runtime()
                    .execute(plan(json!({"account_id":"A"})), session())
            ),
            Err(RuntimeError::SecurityGate(SecurityOutcome::Blocked))
        );
        assert!(!audit
            .events()
            .unwrap()
            .iter()
            .any(|e| e.kind == AuditEventKind::ToolStarted));
    }
}

#[test]
fn ownership_change_after_policy_is_rechecked_by_tool() {
    let source = Source::new(Mode::ChangeAfterPolicy);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    let result = block_on(
        app.runtime()
            .execute(plan(json!({"account_id":"A"})), session()),
    );
    assert!(result.is_err());
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
    let events = audit.events().unwrap();
    assert!(events.iter().any(|e| e.kind == AuditEventKind::ToolStarted));
    assert!(!events
        .iter()
        .any(|e| e.kind == AuditEventKind::TaskCompleted));
}

#[test]
fn unknown_session_never_reads_source() {
    let source = Source::new(Mode::Locked);
    let (app, _) = assembly(source.clone(), SecurityLevel::Green);
    assert!(block_on(app.runtime().execute(
        plan(json!({"account_id":"A"})),
        safeflow_contracts::SessionRef::parse("missing").unwrap()
    ))
    .is_err());
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn card_policy_cannot_lower_a_central_red_requirement() {
    let source = Source::new(Mode::Locked);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Red);
    assert_eq!(
        block_on(
            app.runtime()
                .execute(plan(json!({"account_id":"A"})), session())
        ),
        Err(RuntimeError::SecurityGate(SecurityOutcome::WaitingAuth))
    );
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::ToolStarted));
}

#[test]
fn public_output_contains_only_projected_reasons() {
    let output = direct(Source::new(Mode::Locked), json!({"account_id":"A"}))
        .unwrap()
        .data;
    let reasons = &output["cards"][1]["active_locks"];
    assert_eq!(reasons[2]["public_reason"], "另有安全限制，请联系人工客服");
    let text = output.to_string();
    for private in [
        "RiskControl",
        "risk_score",
        "evidence_refs",
        "U1",
        "U2",
        "event_id",
    ] {
        assert!(!text.contains(private));
    }
}

#[test]
fn postcheck_rejects_wrong_operation_result_reference() {
    let policy = GetAccountCardFactsPolicy::new(Source::new(Mode::Locked));
    for (reference, op, allowed) in [
        ("card_facts_result_op1", "op1", true),
        ("card_facts_result_op2", "op1", false),
        ("card_facts_result_op1", "op2", false),
    ] {
        let post = PostExecuteContext::from_request(
            PostExecuteRequest {
                contract_version: 1,
                task_id: TaskId::parse("t1").unwrap(),
                node_id: NodeId::parse("n1").unwrap(),
                operation_id: OperationId::parse(op).unwrap(),
                tool_name: ToolName::parse(GET_ACCOUNT_CARD_FACTS).unwrap(),
                session_ref: session(),
                observation: ExecutionObservation::ResultReceived {
                    attempt_id: AttemptId::parse("attempt1").unwrap(),
                    business_operation_ref: BusinessOperationRef::parse("read1").unwrap(),
                    tool_result_ref: ToolResultRef::parse(reference).unwrap(),
                },
            },
            UserId::parse("U1").unwrap(),
        );
        assert_eq!(
            block_on(policy.verify_execution(&post)).unwrap().is_empty(),
            allowed
        );
    }
}

#[test]
fn a_write_permit_is_not_accepted_as_read_authorization() {
    use safeflow_contracts::{
        DecisionId, ExecutionPermit, OperationFingerprint, PermitId, PermitIssuanceRequest,
    };
    let permit = ExecutionPermit::issue(PermitIssuanceRequest {
        permit_id: PermitId::parse("mock_permit").unwrap(),
        operation_id: OperationId::parse("op1").unwrap(),
        decision_id: DecisionId::parse("mock_decision").unwrap(),
        user_id: UserId::parse("U1").unwrap(),
        tool_name: ToolName::parse(GET_ACCOUNT_CARD_FACTS).unwrap(),
        fingerprint: OperationFingerprint::parse("0".repeat(64)).unwrap(),
        facts_version: 1,
        issued_at_ms: 1,
        expires_at_ms: 2,
        security_outcome: SecurityOutcome::ReadyToExecute,
    })
    .unwrap();
    let source = Source::new(Mode::Locked);
    let error = block_on(GetAccountCardFactsTool::new(source.clone()).execute(
        &context(),
        ToolExecutionAuthorization::Write(&permit),
        json!({"account_id":"A"}),
    ))
    .unwrap_err();
    assert_eq!(
        error.reason_code.as_str(),
        "READ_ONLY_AUTHORIZATION_REQUIRED"
    );
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn missing_public_policy_or_profile_prevents_startup() {
    use safeflow_contracts::PolicyVersion;
    use safeflow_runtime::{
        DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, StartupValidationError,
        StaticSessionResolver, ToolRegistry, ToolSecurityCatalog,
    };
    use std::sync::Arc;
    let source = Source::new(Mode::Locked);
    for include_policy in [false, true] {
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(GetAccountCardFactsTool::new(source.clone())))
            .unwrap();
        let mut policies = DomainPolicyRegistry::new();
        if include_policy {
            policies
                .register(
                    Arc::new(GetAccountCardFactsPolicy::new(source.clone())),
                    PolicyVersion::parse(safeflow_card_runtime_adapter::CARD_FACTS_POLICY_VERSION)
                        .unwrap(),
                    1,
                )
                .unwrap();
        }
        let result = RuntimeBootstrap::build(
            tools,
            policies,
            ToolSecurityCatalog::new(),
            Arc::new(StaticSessionResolver::new()),
            Arc::new(InMemoryAuditRecorder::new()),
        );
        if include_policy {
            assert!(matches!(
                result,
                Err(StartupValidationError::MissingSecurityProfile(_))
            ));
        } else {
            assert!(matches!(
                result,
                Err(StartupValidationError::MissingPolicy(_))
            ));
        }
    }
}

#[test]
fn runtime_rejects_injected_identity_before_reading() {
    let source = Source::new(Mode::Locked);
    let (app, audit) = assembly(source.clone(), SecurityLevel::Green);
    assert!(block_on(
        app.runtime()
            .execute(plan(json!({"account_id":"A", "user_id":"U2"})), session())
    )
    .is_err());
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|e| e.kind == AuditEventKind::ToolStarted));
}
