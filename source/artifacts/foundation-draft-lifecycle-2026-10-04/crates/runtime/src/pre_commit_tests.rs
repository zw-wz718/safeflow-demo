use super::*;
use crate::{InMemoryAuditRecorder, RuntimeBootstrap, ToolSecurityProfile};
use safeflow_contracts::{
    AgentTool, ArgumentsHash, AuthGrantRef, ConfirmationRef, DomainPolicy, ExecutionContext,
    IdempotencyKey, NodeId, OperationId, OperationSecurityRequest, PreCommitAuthenticationV2,
    PreCommitSecurityRequestV2, TaskId, ToolDefinition, ToolError, ToolExecutionAuthorization,
    ToolName, ToolResult, PRE_COMMIT_SECURITY_CONTRACT_VERSION,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::AtomicUsize;
use std::task::{Context, Poll, Wake, Waker};

struct NoopWake;
impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}
fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

// Public-mechanism sentinels, never a business or authentication implementation.
struct Sentinel {
    name: ToolName,
    write: bool,
    calls: AtomicUsize,
}
impl AgentTool for Sentinel {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            "contract sentinel",
            json!({"type":"object"}),
            1,
            self.write,
            "CONTRACT_SENTINEL",
        )
        .unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a ExecutionContext,
        _: ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("no tool may execute during security review")
    }
}
impl DomainPolicy for Sentinel {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_ne!(context.phase, SecurityPhase::PreCommit);
        assert_eq!(context.authenticated_user_id.as_str(), "user_001");
        Box::pin(async { Ok(vec![]) })
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        panic!("no post-execution during security review")
    }
}

fn v1_request(phase: SecurityPhase) -> OperationSecurityRequest {
    OperationSecurityRequest {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: TaskId::parse("task_001").unwrap(),
        node_id: NodeId::parse("node_001").unwrap(),
        operation_id: OperationId::parse("operation_001").unwrap(),
        facts_version: 1,
        phase,
        tool_name: ToolName::parse("contract_sentinel").unwrap(),
        plan_version: 1,
        session_ref: SessionRef::parse("session_001").unwrap(),
        normalized_arguments: json!({}),
        arguments_hash: ArgumentsHash::parse("a".repeat(64)).unwrap(),
        idempotency_key: Some(IdempotencyKey::parse("idem_001").unwrap()),
        confirmation_ref: Some(ConfirmationRef::parse("confirmation_001").unwrap()),
        auth_grant_ref: Some(AuthGrantRef::parse("grant_unverified").unwrap()),
    }
}
fn v2_request(authentication: PreCommitAuthenticationV2) -> PreCommitSecurityRequestV2 {
    let old = v1_request(SecurityPhase::PreCommit);
    PreCommitSecurityRequestV2 {
        contract_version: PRE_COMMIT_SECURITY_CONTRACT_VERSION,
        task_id: old.task_id,
        node_id: old.node_id,
        operation_id: old.operation_id,
        facts_version: old.facts_version,
        phase: old.phase,
        tool_name: old.tool_name,
        plan_version: old.plan_version,
        session_ref: old.session_ref,
        normalized_arguments: old.normalized_arguments,
        arguments_hash: old.arguments_hash,
        idempotency_key: old.idempotency_key.unwrap(),
        confirmation_ref: old.confirmation_ref.unwrap(),
        authentication,
    }
}
fn assembly(write: bool, level: SecurityLevel) -> (crate::RuntimeAssembly, Arc<Sentinel>) {
    let sentinel = Arc::new(Sentinel {
        name: ToolName::parse("contract_sentinel").unwrap(),
        write,
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(sentinel.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(sentinel.clone(), policy_version(), 1)
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: sentinel.name.clone(),
            tool_schema_version: 1,
            policy_version: policy_version(),
            minimum_level: level,
        })
        .unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(
        SessionRef::parse("session_001").unwrap(),
        UserId::parse("user_001").unwrap(),
    );
    (
        RuntimeBootstrap::build(
            tools,
            policies,
            catalog,
            Arc::new(sessions),
            Arc::new(InMemoryAuditRecorder::new()),
        )
        .unwrap(),
        sentinel,
    )
}

#[test]
fn v1_reference_presence_cannot_produce_ready_at_any_profile_level() {
    for write in [false, true] {
        for level in [
            SecurityLevel::Green,
            SecurityLevel::Yellow,
            SecurityLevel::Red,
            SecurityLevel::Critical,
        ] {
            let (assembly, sentinel) = assembly(write, level);
            let request = v1_request(SecurityPhase::PreCommit);
            assert_eq!(request.validate(), Ok(()));
            let error = block_on(assembly.runtime().security.evaluate(request)).unwrap_err();
            assert_eq!(
                error.reason_code.as_str(),
                "TRUSTED_PRE_COMMIT_VERIFICATION_UNAVAILABLE"
            );
            assert_eq!(sentinel.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn v1_missing_reference_remains_a_contract_error_not_a_ready_decision() {
    let (assembly, sentinel) = assembly(true, SecurityLevel::Green);
    let mut request = v1_request(SecurityPhase::PreCommit);
    request.auth_grant_ref = None;
    let error = block_on(assembly.runtime().security.evaluate(request)).unwrap_err();
    assert_eq!(error.reason_code.as_str(), "INVALID_SECURITY_CONTRACT");
    assert_eq!(sentinel.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn both_v2_modes_refuse_without_verifiers_and_do_not_rewrite_policy_context() {
    for authentication in [
        PreCommitAuthenticationV2::NotRequired {},
        PreCommitAuthenticationV2::Required {
            auth_grant_ref: AuthGrantRef::parse("grant_reference_only").unwrap(),
        },
    ] {
        let (assembly, sentinel) = assembly(true, SecurityLevel::Green);
        let error = block_on(
            assembly
                .runtime()
                .evaluate_pre_commit_v2_for_backend(v2_request(authentication)),
        )
        .unwrap_err();
        assert_eq!(
            error.reason_code.as_str(),
            "TRUSTED_PRE_COMMIT_VERIFICATION_UNAVAILABLE"
        );
        assert_eq!(sentinel.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn v2_rejects_invalid_contract_and_unknown_session_before_unavailability() {
    let (assembly, sentinel) = assembly(true, SecurityLevel::Green);
    let mut request = v2_request(PreCommitAuthenticationV2::NotRequired {});
    request.phase = SecurityPhase::PreAuth;
    let error = block_on(
        assembly
            .runtime()
            .evaluate_pre_commit_v2_for_backend(request),
    )
    .unwrap_err();
    assert_eq!(error.reason_code.as_str(), "INVALID_PRE_COMMIT_CONTRACT");
    let mut request = v2_request(PreCommitAuthenticationV2::NotRequired {});
    request.session_ref = SessionRef::parse("unknown_session").unwrap();
    let error = block_on(
        assembly
            .runtime()
            .evaluate_pre_commit_v2_for_backend(request),
    )
    .unwrap_err();
    assert_eq!(error.reason_code.as_str(), "SESSION_NOT_FOUND");
    assert_eq!(sentinel.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn preflight_keeps_original_phase_and_never_returns_ready() {
    for phase in [SecurityPhase::PreTool, SecurityPhase::PreAuth] {
        for (write, level, expected) in [
            (false, SecurityLevel::Green, SecurityOutcome::Allow),
            (true, SecurityLevel::Green, SecurityOutcome::ReadyToConfirm),
            (true, SecurityLevel::Yellow, SecurityOutcome::ReadyToConfirm),
            (true, SecurityLevel::Red, SecurityOutcome::WaitingAuth),
            (true, SecurityLevel::Critical, SecurityOutcome::Blocked),
        ] {
            let (assembly, sentinel) = assembly(write, level);
            let decision =
                block_on(assembly.runtime().security.evaluate(v1_request(phase))).unwrap();
            assert_eq!(decision.outcome, expected);
            assert_eq!(sentinel.calls.load(Ordering::SeqCst), 1);
        }
    }
}
