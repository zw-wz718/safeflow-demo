use safeflow_contracts::*;
use serde_json::json;

fn request() -> PreCommitSecurityRequestV2 {
    PreCommitSecurityRequestV2 {
        contract_version: PRE_COMMIT_SECURITY_CONTRACT_VERSION,
        task_id: TaskId::parse("task_001").unwrap(),
        node_id: NodeId::parse("node_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        facts_version: 1,
        phase: SecurityPhase::PreCommit,
        tool_name: ToolName::parse("contract_write").unwrap(),
        plan_version: 1,
        session_ref: SessionRef::parse("session_001").unwrap(),
        normalized_arguments: json!({"amount": "200.00"}),
        arguments_hash: ArgumentsHash::parse("a".repeat(64)).unwrap(),
        idempotency_key: IdempotencyKey::parse("idem_001").unwrap(),
        confirmation_ref: ConfirmationRef::parse("confirmation_001").unwrap(),
        authentication: PreCommitAuthenticationV2::NotRequired {},
    }
}

#[test]
fn conditional_modes_roundtrip_without_inventing_a_grant() {
    let mut request = request();
    assert_eq!(
        request.validate_for_required_gates(&[SecurityGate::ExplicitConfirmation]),
        Ok(())
    );
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        value["authentication"],
        json!({"requirement": "NOT_REQUIRED"})
    );
    assert_eq!(
        serde_json::from_value::<PreCommitSecurityRequestV2>(value).unwrap(),
        request
    );
    request.authentication = PreCommitAuthenticationV2::Required {
        auth_grant_ref: AuthGrantRef::parse("grant_reference_only").unwrap(),
    };
    assert_eq!(
        request.validate_for_required_gates(&[
            SecurityGate::ExplicitConfirmation,
            SecurityGate::StrongAuthentication,
        ]),
        Ok(())
    );
    let value = serde_json::to_value(&request).unwrap();
    assert_eq!(
        serde_json::from_value::<PreCommitSecurityRequestV2>(value).unwrap(),
        request
    );
}

#[test]
fn contradictory_authentication_cannot_satisfy_trusted_gates() {
    let mut request = request();
    let strong = [
        SecurityGate::ExplicitConfirmation,
        SecurityGate::StrongAuthentication,
    ];
    assert_eq!(
        request.validate_for_required_gates(&strong),
        Err(PreCommitContractError::AuthenticationGateMismatch)
    );
    request.authentication = PreCommitAuthenticationV2::Required {
        auth_grant_ref: AuthGrantRef::parse("grant_reference_only").unwrap(),
    };
    assert_eq!(
        request.validate_for_required_gates(&[SecurityGate::ExplicitConfirmation]),
        Err(PreCommitContractError::AuthenticationGateMismatch)
    );
}

#[test]
fn missing_duplicate_or_noncanonical_gates_are_rejected() {
    for gates in [
        vec![],
        vec![SecurityGate::StrongAuthentication],
        vec![
            SecurityGate::ExplicitConfirmation,
            SecurityGate::ExplicitConfirmation,
        ],
        vec![
            SecurityGate::StrongAuthentication,
            SecurityGate::ExplicitConfirmation,
        ],
        vec![
            SecurityGate::ExplicitConfirmation,
            SecurityGate::StrongAuthentication,
            SecurityGate::StrongAuthentication,
        ],
    ] {
        assert_eq!(
            request().validate_for_required_gates(&gates),
            Err(PreCommitContractError::InvalidRequiredGates)
        );
    }
}

#[test]
fn versions_and_phase_cannot_downgrade_conditional_pre_commit() {
    for version in [0, 1, 3] {
        let mut request = request();
        request.contract_version = version;
        assert_eq!(
            request.validate(),
            Err(PreCommitContractError::UnsupportedVersion(version))
        );
    }
    for phase in [SecurityPhase::PreTool, SecurityPhase::PreAuth] {
        let mut request = request();
        request.phase = phase;
        assert_eq!(
            request.validate(),
            Err(PreCommitContractError::InvalidPhase)
        );
    }
    let mut request = request();
    request.facts_version = 0;
    assert_eq!(request.validate(), Err(PreCommitContractError::ZeroVersion));
    request.facts_version = 1;
    request.plan_version = 0;
    assert_eq!(request.validate(), Err(PreCommitContractError::ZeroVersion));
}

#[test]
fn required_references_and_identity_are_strict_at_the_json_boundary() {
    let value = serde_json::to_value(request()).unwrap();
    for field in ["idempotency_key", "confirmation_ref", "authentication"] {
        let mut missing = value.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<PreCommitSecurityRequestV2>(missing).is_err());
        let mut null = value.clone();
        null[field] = serde_json::Value::Null;
        assert!(serde_json::from_value::<PreCommitSecurityRequestV2>(null).is_err());
    }
    for authentication in [
        json!({"requirement":"REQUIRED"}),
        json!({"requirement":"REQUIRED","auth_grant_ref":null}),
        json!({"requirement":"REQUIRED","auth_grant_ref":""}),
        json!({"requirement":"NOT_REQUIRED","auth_grant_ref":"grant_001"}),
        json!({"requirement":"UNKNOWN"}),
        json!({"requirement":"REQUIRED","auth_grant_ref":"grant_001","verified":true}),
    ] {
        let mut raw = value.clone();
        raw["authentication"] = authentication;
        assert!(serde_json::from_value::<PreCommitSecurityRequestV2>(raw).is_err());
    }
    for field in [
        "user_id",
        "authenticated_user_id",
        "required_gates",
        "verified",
        "auth_grant_ref",
    ] {
        let mut raw = value.clone();
        raw[field] = json!("untrusted");
        assert!(serde_json::from_value::<PreCommitSecurityRequestV2>(raw).is_err());
    }
}

#[test]
fn isolated_simulation_reference_cannot_enter_conditional_pre_commit() {
    let mut request = request();
    request.authentication = PreCommitAuthenticationV2::Required {
        auth_grant_ref: AuthGrantRef::parse("sim_grant_001").unwrap(),
    };
    assert_eq!(
        request.validate(),
        Err(PreCommitContractError::IsolatedAuthentication)
    );
}

#[test]
fn frozen_v1_still_requires_authentication_and_rejects_v2_wire_shape() {
    let request = request();
    let v2_json = serde_json::to_value(&request).unwrap();
    let mut legacy = OperationSecurityRequest {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: request.task_id,
        node_id: request.node_id,
        operation_id: request.operation_id,
        facts_version: request.facts_version,
        phase: request.phase,
        tool_name: request.tool_name,
        plan_version: request.plan_version,
        session_ref: request.session_ref,
        normalized_arguments: request.normalized_arguments,
        arguments_hash: request.arguments_hash,
        idempotency_key: Some(request.idempotency_key),
        confirmation_ref: Some(request.confirmation_ref),
        auth_grant_ref: None,
    };
    assert_eq!(SECURITY_CONTRACT_VERSION, 1);
    assert_eq!(
        legacy.validate(),
        Err(SecurityContractError::MissingPreCommitReference(
            "auth_grant_ref"
        ))
    );
    legacy.auth_grant_ref = Some(AuthGrantRef::parse("grant_001").unwrap());
    assert_eq!(legacy.validate(), Ok(()));
    let value = serde_json::to_value(&legacy).unwrap();
    assert_eq!(
        serde_json::from_value::<OperationSecurityRequest>(value.clone()).unwrap(),
        legacy
    );
    assert!(serde_json::from_value::<PreCommitSecurityRequestV2>(value).is_err());
    assert!(serde_json::from_value::<OperationSecurityRequest>(v2_json).is_err());
}
