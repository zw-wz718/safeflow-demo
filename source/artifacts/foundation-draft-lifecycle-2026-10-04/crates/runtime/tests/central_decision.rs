#[path = "support/central_risk.rs"]
mod probe;
use probe::*;
use safeflow_contracts::*;
use safeflow_runtime::*;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

async fn evaluate(
    level: SecurityLevel,
    findings: Vec<SecurityFinding>,
    checks: &[&str],
) -> SecurityDecision {
    let session = SessionRef::parse("probe_session").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_test").unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let assembly = assemble(Arc::new(sessions), level, checks, findings, calls.clone());
    let plan = AgentPlan::new(
        TaskId::parse("task_test").unwrap(),
        1,
        vec![PlanNode::Tool {
            id: NodeId::parse("probe").unwrap(),
            tool: ToolName::parse(TOOL).unwrap(),
            arguments: json!({}),
            depends_on: vec![],
        }],
    )
    .unwrap();
    let request = OperationSecurityRequest {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: plan.task_id.clone(),
        node_id: NodeId::parse("probe").unwrap(),
        operation_id: OperationId::parse("operation_test").unwrap(),
        facts_version: 1,
        phase: SecurityPhase::PreAuth,
        tool_name: ToolName::parse(TOOL).unwrap(),
        plan_version: 1,
        session_ref: session,
        normalized_arguments: json!({}),
        arguments_hash: ArgumentsHash::parse(format!("{:x}", Sha256::digest(b"{}"))).unwrap(),
        idempotency_key: None,
        confirmation_ref: None,
        auth_grant_ref: None,
    };
    let reviewed = assembly
        .runtime()
        .review_preflight_security_for_backend(&plan, request.clone(), &input_contract())
        .await
        .unwrap();
    let repeated = assembly
        .runtime()
        .review_preflight_security_for_backend(&plan, request, &input_contract())
        .await
        .unwrap();
    assert_eq!(
        reviewed.decision().risk_score,
        repeated.decision().risk_score
    );
    assert_eq!(reviewed.decision().findings, repeated.decision().findings);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    reviewed.decision().clone()
}
fn hard_finding(kind: FindingKind) -> SecurityFinding {
    SecurityFinding {
        check_id: CheckId::parse("EXISTING_HARD_RULE").unwrap(),
        policy_version: PolicyVersion::parse(POLICY).unwrap(),
        source: SourceName::parse(SOURCE).unwrap(),
        kind,
        reason_code: ReasonCode::parse("EXISTING_RULE_RESULT").unwrap(),
        evidence_refs: vec![],
        observed_value: None,
        event_ids: vec![],
        dedup_key: None,
        window_start_ms: None,
        window_end_ms: None,
    }
}
#[tokio::test]
async fn kernel_scores_trusted_policy_without_changing_soft_signal_execution_path() {
    let checks = [CHECK, "COMMON_AUTH_FAILURE_10M", "COMMON_HIGH_RISK_OP_10M"];
    let decision = evaluate(
        SecurityLevel::Green,
        vec![
            count_finding(CHECK, 20),
            count_finding(checks[1], 2),
            count_finding(checks[2], 3),
        ],
        &checks,
    )
    .await;
    assert_eq!(decision.risk_score, 55);
    assert_eq!(decision.risk_tier, RiskTier::High);
    assert_eq!(decision.outcome, SecurityOutcome::Allow);
    assert_eq!(decision.minimum_level, SecurityLevel::Green);
    assert!(decision.required_gates.is_empty());
    assert_eq!(
        decision
            .findings
            .iter()
            .filter(|f| f.applied_weight.is_some())
            .count(),
        3
    );
}
#[tokio::test]
async fn require_gate_and_risk_signal_never_downgrade_red_or_critical() {
    for level in [SecurityLevel::Red, SecurityLevel::Critical] {
        let decision = evaluate(
            level,
            vec![
                hard_finding(FindingKind::RequireGate),
                count_finding(CHECK, 20),
            ],
            &[CHECK],
        )
        .await;
        assert_eq!(
            decision.outcome,
            if level == SecurityLevel::Red {
                SecurityOutcome::WaitingAuth
            } else {
                SecurityOutcome::Blocked
            }
        );
        assert_eq!(
            decision.required_gates,
            vec![
                SecurityGate::ExplicitConfirmation,
                SecurityGate::StrongAuthentication
            ]
        );
    }
}
#[tokio::test]
async fn hard_deny_and_needs_input_keep_priority_over_soft_scores_and_gates() {
    for (kind, outcome) in [
        (FindingKind::NeedsInput, SecurityOutcome::NeedsInput),
        (FindingKind::HardDeny, SecurityOutcome::Blocked),
    ] {
        let decision = evaluate(
            SecurityLevel::Red,
            vec![hard_finding(kind), count_finding(CHECK, 20)],
            &[CHECK],
        )
        .await;
        assert_eq!(decision.outcome, outcome);
        assert_eq!(decision.risk_score, 15);
    }
    let decision = evaluate(
        SecurityLevel::Green,
        vec![
            hard_finding(FindingKind::NeedsInput),
            hard_finding(FindingKind::HardDeny),
        ],
        &[],
    )
    .await;
    assert_eq!(decision.outcome, SecurityOutcome::Blocked);
}
#[tokio::test]
async fn legacy_hard_ids_remain_compatible_and_no_configuration_is_not_verified_low() {
    let decision = evaluate(SecurityLevel::Green, vec![], &[]).await;
    assert_eq!(decision.outcome, SecurityOutcome::Allow);
    assert_eq!(
        decision
            .findings
            .last()
            .unwrap()
            .finding
            .reason_code
            .as_str(),
        "RISK_SCORING_DISABLED_NOT_ASSESSED"
    );
    let decision = evaluate(
        SecurityLevel::Green,
        vec![hard_finding(FindingKind::RequireGate)],
        &[],
    )
    .await;
    assert_eq!(decision.outcome, SecurityOutcome::ReadyToConfirm);
}

#[tokio::test]
async fn kernel_rejections_are_safe_decisions_for_the_existing_audit_chain() {
    for mutation in 0..4 {
        let mut f = count_finding(CHECK, 20);
        match mutation {
            0 => f.check_id = CheckId::parse("UNKNOWN_RISK").unwrap(),
            1 => f.policy_version = PolicyVersion::parse("unapproved").unwrap(),
            2 => f.source = SourceName::parse("client").unwrap(),
            _ => f.evidence_refs.clear(),
        }
        let decision = evaluate(SecurityLevel::Green, vec![f], &[CHECK]).await;
        assert_eq!(decision.outcome, SecurityOutcome::Blocked);
        assert_eq!(decision.findings[0].finding.check_id.as_str(), "COMMON-17");
        assert_eq!(
            decision.findings[0].finding.policy_version.as_str(),
            CENTRAL_SCORING_VERSION
        );
        assert!(decision.findings[0].finding.evidence_refs.is_empty());
    }
}

#[test]
fn startup_refuses_risk_bindings_to_missing_tools() {
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .risk_catalog_for_backend()
        .enable_distinct_count_for_backend(
            CHECK,
            ToolName::parse(TOOL).unwrap(),
            SourceName::parse(SOURCE).unwrap(),
        )
        .unwrap();
    let result = RuntimeBootstrap::build(
        ToolRegistry::new(),
        DomainPolicyRegistry::new(),
        catalog,
        Arc::new(StaticSessionResolver::new()),
        Arc::new(InMemoryAuditRecorder::new()),
    );
    assert!(matches!(
        result,
        Err(StartupValidationError::OrphanSecurityProfile(_))
    ));
}
