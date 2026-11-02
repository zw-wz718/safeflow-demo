use super::*;

fn context() -> SecurityContext {
    SecurityContext::from_request(
        OperationSecurityRequest {
            contract_version: SECURITY_CONTRACT_VERSION,
            task_id: TaskId::parse("task_test").unwrap(),
            node_id: NodeId::parse("node_test").unwrap(),
            operation_id: OperationId::parse("op_test").unwrap(),
            facts_version: 1,
            phase: SecurityPhase::PreAuth,
            tool_name: ToolName::parse("execute_transfer").unwrap(),
            plan_version: 1,
            session_ref: SessionRef::parse("session_test").unwrap(),
            normalized_arguments: json!({}),
            arguments_hash: ArgumentsHash::parse("a".repeat(64)).unwrap(),
            idempotency_key: None,
            confirmation_ref: None,
            auth_grant_ref: None,
        },
        UserId::parse("user_test").unwrap(),
    )
    .unwrap()
}
fn catalog(checks: &[&str]) -> CentralPolicyCatalog {
    let mut catalog = CentralPolicyCatalog::default();
    for check in checks {
        catalog
            .enable_distinct_count_for_backend(
                check,
                context().tool_name,
                SourceName::parse("trusted_probe").unwrap(),
            )
            .unwrap();
    }
    catalog
}
fn signal(check: &str, count: u64) -> SecurityFinding {
    SecurityFinding {
        check_id: CheckId::parse(check).unwrap(),
        policy_version: PolicyVersion::parse(DEMO_RISK_VERSION).unwrap(),
        source: SourceName::parse("trusted_probe").unwrap(),
        kind: FindingKind::RiskSignal,
        reason_code: ReasonCode::parse("DISTINCT_EVENTS_OBSERVED").unwrap(),
        evidence_refs: vec![EvidenceRef::parse("evidence_redacted_count_1").unwrap()],
        observed_value: Some(json!(count)),
        event_ids: (0..count)
            .map(|n| EventId::parse(format!("opaque_event_{n}")).unwrap())
            .collect(),
        dedup_key: Some(format!("op_test:{check}:1")),
        window_start_ms: Some(100),
        window_end_ms: Some(600100),
    }
}
const VELOCITY: &str = "TRANSFER_VELOCITY_10M";

#[test]
fn catalog_matches_documented_demo_parameters_and_disables_unknown_formats() {
    let rules = demo_risk_definitions();
    assert_eq!(
        rules.iter().map(|r| r.weight).collect::<Vec<_>>(),
        vec![15, 20, 20, 10, 15, 20, 20, 15]
    );
    assert_eq!(
        rules
            .iter()
            .filter_map(|r| r.distinct_count_threshold)
            .collect::<Vec<_>>(),
        vec![2, 3, 3, 20]
    );
    assert_eq!(
        rules[4].missing_data_policy,
        MissingRiskDataPolicy::AddFixedRisk10
    );
    let mut c = CentralPolicyCatalog::default();
    for id in [
        "UNKNOWN",
        "COMMON_NEW_DEVICE_30D",
        "COMMON_STALE_SESSION_12H",
        "TRANSFER_FIRST_PAYEE_90D",
        "TRANSFER_AMOUNT_DEVIATION_90D",
    ] {
        assert!(c
            .enable_distinct_count_for_backend(
                id,
                context().tool_name,
                SourceName::parse("trusted_probe").unwrap()
            )
            .is_err());
    }
    assert!(c
        .enable_distinct_count_for_backend(
            VELOCITY,
            ToolName::parse("list_cards").unwrap(),
            SourceName::parse("trusted_probe").unwrap()
        )
        .is_err());
}
#[test]
fn tier_boundaries_and_score_cap_are_independent_of_business_weight_combinations() {
    for (score, tier) in [
        (19, RiskTier::Low),
        (20, RiskTier::Medium),
        (39, RiskTier::Medium),
        (40, RiskTier::High),
        (69, RiskTier::High),
        (70, RiskTier::Critical),
    ] {
        assert_eq!(risk_tier_for_score(score), tier);
    }
    assert_eq!(capped_score([20, 20, 20, 20, 20, 20].into_iter()), 100);
    assert_eq!(capped_score([u32::MAX, 20].into_iter()), 100);
}
#[test]
fn actual_count_threshold_and_exact_window_score_deterministically() {
    let c = catalog(&[VELOCITY]);
    for count in [0, 2, 3, 4] {
        let (evaluated, score) = c
            .evaluate(&context(), vec![signal(VELOCITY, count)])
            .unwrap();
        assert_eq!(score, if count >= 3 { 20 } else { 0 });
        assert_eq!(evaluated[0].applied_weight, Some(20));
        assert_eq!(
            evaluated[0].finding.policy_version.as_str(),
            DEMO_RISK_VERSION
        );
    }
}
#[test]
fn repeated_calls_and_reordered_duplicate_events_never_accumulate() {
    let c = catalog(&[VELOCITY]);
    let a = signal(VELOCITY, 3);
    let mut b = a.clone();
    b.event_ids.reverse();
    b.event_ids.push(a.event_ids[0].clone());
    let expected = c.evaluate(&context(), vec![a.clone()]).unwrap();
    assert_eq!(c.evaluate(&context(), vec![a, b]).unwrap(), expected);
    for _ in 0..3 {
        assert_eq!(
            c.evaluate(&context(), vec![signal(VELOCITY, 3)]).unwrap(),
            expected
        );
    }
}
#[test]
fn conflicting_same_rule_observations_are_rejected() {
    assert!(catalog(&[VELOCITY])
        .evaluate(&context(), vec![signal(VELOCITY, 3), signal(VELOCITY, 4)])
        .is_err());
}
#[test]
fn malformed_unapproved_and_out_of_scope_signals_fail_closed() {
    let c = catalog(&[VELOCITY]);
    let base = signal(VELOCITY, 3);
    for mutation in 0..13 {
        let mut f = base.clone();
        match mutation {
            0 => f.check_id = CheckId::parse("UNKNOWN").unwrap(),
            1 => f.policy_version = PolicyVersion::parse("risk-unapproved").unwrap(),
            2 => f.source = SourceName::parse("client").unwrap(),
            3 => f.evidence_refs.clear(),
            4 => f.observed_value = Some(json!({"count":3,"weight":99})),
            5 => f.event_ids.pop().map(|_| ()).unwrap(),
            6 => f.window_start_ms = Some(101),
            7 => f.window_end_ms = Some(-1),
            8 => f.dedup_key = Some("another_operation".into()),
            9 => f.observed_value = Some(json!(-1)),
            10 => f.evidence_refs = vec![EvidenceRef::parse("raw secret text").unwrap()],
            11 => f.kind = FindingKind::Informational,
            _ => f.observed_value = Some(json!(3.0)),
        }
        assert!(
            c.evaluate(&context(), vec![f]).is_err(),
            "mutation {mutation}"
        );
    }
    let mut wrong = context();
    wrong.tool_name = ToolName::parse("list_cards").unwrap();
    assert!(c.evaluate(&wrong, vec![base.clone()]).is_err());
    wrong = context();
    wrong.phase = SecurityPhase::PreTool;
    assert!(c.evaluate(&wrong, vec![base]).is_err());
}
#[test]
fn explicit_missing_policies_never_masquerade_as_verified_low() {
    for check in [
        VELOCITY,
        "COMMON_AUTH_FAILURE_10M",
        "COMMON_HIGH_RISK_OP_10M",
    ] {
        assert!(catalog(&[check]).evaluate(&context(), vec![]).is_err());
    }
    let c = catalog(&["COMMON_RESOURCE_ENUM_10M"]);
    let (findings, score) = c.evaluate(&context(), vec![]).unwrap();
    assert_eq!(score, 0);
    assert_eq!(
        findings[0].finding.reason_code.as_str(),
        "MISSING_AUXILIARY_DATA"
    );
    assert_eq!(
        findings[1].finding.reason_code.as_str(),
        "RISK_SCORING_PARTIAL_COMPUTE_AUDIT_ONLY"
    );
    let (findings, _) = CentralPolicyCatalog::default()
        .evaluate(&context(), vec![])
        .unwrap();
    assert_eq!(
        findings[0].finding.reason_code.as_str(),
        "RISK_SCORING_DISABLED_NOT_ASSESSED"
    );
}
#[test]
fn only_pre_auth_scores_and_configuration_content_is_exposed_to_digest() {
    let c = catalog(&[VELOCITY]);
    let mut pretool = context();
    pretool.phase = SecurityPhase::PreTool;
    let (findings, score) = c.evaluate(&pretool, vec![]).unwrap();
    assert_eq!(score, 0);
    assert_eq!(
        findings[0].finding.reason_code.as_str(),
        "RISK_SCORING_NOT_APPLICABLE_PHASE"
    );
    let other = catalog(&["COMMON_AUTH_FAILURE_10M"]);
    assert_eq!(c.metadata()["version"], other.metadata()["version"]);
    assert_ne!(c.metadata(), other.metadata());
}

#[test]
fn approved_multi_rule_contributions_reach_critical_without_inventing_weights() {
    let checks = [
        "COMMON_AUTH_FAILURE_10M",
        "COMMON_HIGH_RISK_OP_10M",
        VELOCITY,
        "COMMON_RESOURCE_ENUM_10M",
    ];
    let c = catalog(&checks);
    let (findings, score) = c
        .evaluate(
            &context(),
            vec![
                signal(checks[0], 2),
                signal(checks[1], 3),
                signal(checks[2], 3),
                signal(checks[3], 20),
            ],
        )
        .unwrap();
    assert_eq!(score, 75);
    assert_eq!(risk_tier_for_score(score), RiskTier::Critical);
    assert_eq!(
        findings
            .iter()
            .filter_map(|f| f.applied_weight)
            .sum::<u32>(),
        75
    );
}

#[test]
fn legacy_sixteen_finding_budget_does_not_shrink_for_unconfigured_tools() {
    let mut f = signal("EXISTING_HARD_RULE", 0);
    f.kind = FindingKind::Informational;
    f.observed_value = None;
    f.event_ids.clear();
    f.evidence_refs.clear();
    f.dedup_key = None;
    f.window_start_ms = None;
    f.window_end_ms = None;
    let findings = vec![f; 16];
    let c = CentralPolicyCatalog::default();
    assert_eq!(
        c.evaluate(&context(), findings.clone())
            .unwrap()
            .0
            .into_iter()
            .map(|f| f.finding)
            .collect::<Vec<_>>(),
        findings
    );
    assert_eq!(c.metadata()["unbound_status"], "DISABLED_NOT_ASSESSED");
}
