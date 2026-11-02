use safeflow_contracts::{
    AgentPlan, AgentTool, CheckId, ContractFuture, DomainPolicy, ExecutionContext, FindingKind,
    NodeId, PlanNode, PlanningResult, PolicyVersion, PostExecuteContext, ReasonCode,
    SecurityContext, SecurityError, SecurityFinding, SecurityLevel, SessionRef, SourceName, TaskId,
    ToolDefinition, ToolError, ToolExecutionAuthorization, ToolName, ToolResult, ToolResultRef,
    ToolResultStatus, Understanding, UnderstandingDecision, UserId,
};
use safeflow_runtime::{
    AuditEventKind, DomainPolicyRegistry, InMemoryAuditRecorder, RegistrationError,
    RuntimeBootstrap, RuntimeError, StartupValidationError, StaticSessionResolver, ToolRegistry,
    ToolSecurityCatalog, ToolSecurityProfile,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

const TOOL_NAME: &str = "get_account_balance";
const POLICY_VERSION: &str = "balance-policy-v1";

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

struct CountingTool {
    name: ToolName,
    schema_version: u32,
    calls: AtomicUsize,
}

impl CountingTool {
    fn new(name: ToolName, schema_version: u32) -> Self {
        Self {
            name,
            schema_version,
            calls: AtomicUsize::new(0),
        }
    }
}

impl AgentTool for CountingTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            "Read a balance",
            json!({"type": "object"}),
            self.schema_version,
            false,
            "READ_BALANCE",
        )
        .unwrap()
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        _authorization: ToolExecutionAuthorization<'a>,
        _arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult::new(
                self.name.clone(),
                context.operation_id.clone(),
                ToolResultRef::parse("result_001").unwrap(),
                ToolResultStatus::Succeeded,
                json!({"balance": "100.00"}),
            ))
        })
    }
}

struct FixedPolicy {
    name: ToolName,
    findings: Vec<SecurityFinding>,
}

impl FixedPolicy {
    fn allow(name: ToolName) -> Self {
        Self {
            name,
            findings: vec![],
        }
    }

    fn deny(name: ToolName) -> Self {
        Self {
            name,
            findings: vec![SecurityFinding {
                check_id: CheckId::parse("ACCOUNT_ACCESS_DENIED").unwrap(),
                policy_version: PolicyVersion::parse(POLICY_VERSION).unwrap(),
                source: SourceName::parse("account_policy").unwrap(),
                kind: FindingKind::HardDeny,
                reason_code: ReasonCode::parse("RESOURCE_NOT_OWNED").unwrap(),
                evidence_refs: vec![],
                observed_value: None,
                event_ids: vec![],
                dedup_key: None,
                window_start_ms: None,
                window_end_ms: None,
            }],
        }
    }
}

impl DomainPolicy for FixedPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        _context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        let findings = self.findings.clone();
        Box::pin(async move { Ok(findings) })
    }

    fn verify_execution<'a>(
        &'a self,
        _context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move { Ok(vec![]) })
    }
}

fn tool_name() -> ToolName {
    ToolName::parse(TOOL_NAME).unwrap()
}

fn policy_version() -> PolicyVersion {
    PolicyVersion::parse(POLICY_VERSION).unwrap()
}

fn profile(name: ToolName, schema_version: u32, policy: PolicyVersion) -> ToolSecurityProfile {
    ToolSecurityProfile {
        tool_name: name,
        tool_schema_version: schema_version,
        policy_version: policy,
        minimum_level: SecurityLevel::Green,
    }
}

fn startup_dependencies() -> (Arc<StaticSessionResolver>, Arc<InMemoryAuditRecorder>) {
    (
        Arc::new(StaticSessionResolver::new()),
        Arc::new(InMemoryAuditRecorder::new()),
    )
}

#[test]
fn startup_rejects_a_tool_without_a_domain_policy() {
    let name = tool_name();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CountingTool::new(name.clone(), 1)))
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(name.clone(), 1, policy_version()))
        .unwrap();
    let (sessions, audit) = startup_dependencies();

    let result =
        RuntimeBootstrap::build(tools, DomainPolicyRegistry::new(), catalog, sessions, audit);

    assert!(matches!(
        result,
        Err(StartupValidationError::MissingPolicy(found)) if found == name
    ));
}

#[test]
fn startup_rejects_a_tool_without_a_security_profile() {
    let name = tool_name();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CountingTool::new(name.clone(), 1)))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let (sessions, audit) = startup_dependencies();

    let result =
        RuntimeBootstrap::build(tools, policies, ToolSecurityCatalog::new(), sessions, audit);

    assert!(matches!(
        result,
        Err(StartupValidationError::MissingSecurityProfile(found)) if found == name
    ));
}

#[test]
fn startup_rejects_orphan_policy_and_security_profile() {
    let name = tool_name();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let (sessions, audit) = startup_dependencies();
    let result = RuntimeBootstrap::build(
        ToolRegistry::new(),
        policies,
        ToolSecurityCatalog::new(),
        sessions,
        audit,
    );
    assert!(matches!(
        result,
        Err(StartupValidationError::OrphanPolicy(found)) if found == name
    ));

    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(name.clone(), 1, policy_version()))
        .unwrap();
    let (sessions, audit) = startup_dependencies();
    let result = RuntimeBootstrap::build(
        ToolRegistry::new(),
        DomainPolicyRegistry::new(),
        catalog,
        sessions,
        audit,
    );
    assert!(matches!(
        result,
        Err(StartupValidationError::OrphanSecurityProfile(found)) if found == name
    ));
}

#[test]
fn registries_reject_duplicate_tool_policy_and_profile_names() {
    let name = tool_name();
    let tool = Arc::new(CountingTool::new(name.clone(), 1));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    assert_eq!(
        tools.register(tool),
        Err(RegistrationError::DuplicateTool(name.clone()))
    );

    let policy = Arc::new(FixedPolicy::allow(name.clone()));
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(policy.clone(), policy_version(), 1)
        .unwrap();
    assert_eq!(
        policies.register(policy, policy_version(), 1),
        Err(RegistrationError::DuplicatePolicy(name.clone()))
    );

    let profile = profile(name.clone(), 1, policy_version());
    let mut catalog = ToolSecurityCatalog::new();
    catalog.register(profile.clone()).unwrap();
    assert_eq!(
        catalog.register(profile),
        Err(RegistrationError::DuplicateSecurityProfile(name))
    );
}

#[test]
fn startup_rejects_schema_and_policy_version_mismatches() {
    let name = tool_name();
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CountingTool::new(name.clone(), 1)))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            2,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(name.clone(), 1, policy_version()))
        .unwrap();
    let (sessions, audit) = startup_dependencies();
    assert!(matches!(
        RuntimeBootstrap::build(tools, policies, catalog, sessions, audit),
        Err(StartupValidationError::PolicySchemaMismatch { tool_name: found, .. }) if found == name
    ));

    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CountingTool::new(name.clone(), 1)))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(
            name.clone(),
            1,
            PolicyVersion::parse("unapproved-policy-v2").unwrap(),
        ))
        .unwrap();
    let (sessions, audit) = startup_dependencies();
    assert!(matches!(
        RuntimeBootstrap::build(tools, policies, catalog, sessions, audit),
        Err(StartupValidationError::PolicyVersionMismatch { tool_name: found, .. }) if found == name
    ));

    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(CountingTool::new(name.clone(), 1)))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(name.clone(), 2, policy_version()))
        .unwrap();
    let (sessions, audit) = startup_dependencies();
    assert!(matches!(
        RuntimeBootstrap::build(tools, policies, catalog, sessions, audit),
        Err(StartupValidationError::CatalogSchemaMismatch { tool_name: found, .. }) if found == name
    ));
}

#[test]
fn runtime_cannot_bypass_a_hard_deny_policy() {
    let name = tool_name();
    let tool = Arc::new(CountingTool::new(name.clone(), 1));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::deny(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(name.clone(), 1, policy_version()))
        .unwrap();
    let session_ref = SessionRef::parse("session_001").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session_ref.clone(), UserId::parse("user_001").unwrap());
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, Arc::new(sessions), audit.clone())
            .unwrap();

    let plan = AgentPlan::new(
        TaskId::parse("task_001").unwrap(),
        1,
        vec![PlanNode::Tool {
            id: NodeId::parse("node_001").unwrap(),
            tool: name,
            arguments: json!({"account_id": "account_001"}),
            depends_on: vec![],
        }],
    )
    .unwrap();
    let result = PlanningResult::for_plan(
        Understanding {
            intent: "GET_ACCOUNT_BALANCE".to_owned(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    );

    let outcome = block_on(assembly.runtime().execute(result, session_ref));

    assert_eq!(
        outcome,
        Err(RuntimeError::SecurityGate(
            safeflow_contracts::SecurityOutcome::Blocked
        ))
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        audit
            .events()
            .unwrap()
            .into_iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            AuditEventKind::PlanAccepted,
            AuditEventKind::SecurityEvaluated
        ]
    );
}

#[test]
fn central_minimum_security_level_cannot_be_lowered_by_policy() {
    let name = tool_name();
    let tool = Arc::new(CountingTool::new(name.clone(), 1));
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy::allow(name.clone())),
            policy_version(),
            1,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    let mut red_profile = profile(name.clone(), 1, policy_version());
    red_profile.minimum_level = SecurityLevel::Red;
    catalog.register(red_profile).unwrap();
    let session_ref = SessionRef::parse("session_001").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session_ref.clone(), UserId::parse("user_001").unwrap());
    let assembly = RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        Arc::new(sessions),
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap();
    let plan = AgentPlan::new(
        TaskId::parse("task_001").unwrap(),
        1,
        vec![PlanNode::Tool {
            id: NodeId::parse("node_001").unwrap(),
            tool: name,
            arguments: json!({}),
            depends_on: vec![],
        }],
    )
    .unwrap();
    let result = PlanningResult::for_plan(
        Understanding {
            intent: "GET_ACCOUNT_BALANCE".to_owned(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    );

    let outcome = block_on(assembly.runtime().execute(result, session_ref));

    assert_eq!(
        outcome,
        Err(RuntimeError::SecurityGate(
            safeflow_contracts::SecurityOutcome::WaitingAuth
        ))
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}
