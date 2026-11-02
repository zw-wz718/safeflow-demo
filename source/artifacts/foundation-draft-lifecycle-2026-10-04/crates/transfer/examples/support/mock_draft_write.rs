use safeflow_bank_gateway::BankGateway;
use safeflow_contracts::{
    ExecutionContext, MockDraftAction, NodeId, OperationId, PolicyVersion, SecurityLevel,
    SessionRef, TaskId, ToolName, UserId, MOCK_DRAFT_CREATE_TOOL,
};
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, MockDraftAuthority, MockDraftClock,
    RuntimeAssembly, RuntimeBootstrap, StaticSessionResolver, ToolRegistry, ToolSecurityCatalog,
    ToolSecurityProfile,
};
use safeflow_transfer::{fixtures::ManualClock, Clock};
use safeflow_transfer_integration::runtime_tools::{
    CreateMockDraftPolicy, CreateMockDraftTool, MockDraftLifecyclePolicy, MockDraftLifecycleTool,
    MockDraftStore, MOCK_DRAFT_LIFECYCLE_POLICY_VERSION, MOCK_DRAFT_POLICY_VERSION,
    MOCK_DRAFT_SCHEMA_VERSION,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct DemoClock(pub ManualClock);
impl Clock for DemoClock {
    fn now(&self) -> u64 {
        self.0.now()
    }
}
impl MockDraftClock for DemoClock {
    fn now_seconds(&self) -> u64 {
        self.now()
    }
}

pub struct App {
    pub assembly: RuntimeAssembly,
    pub authority: Arc<MockDraftAuthority>,
    pub store: Arc<MockDraftStore<DemoClock>>,
    pub audit: Arc<InMemoryAuditRecorder>,
    pub sessions: Arc<StaticSessionResolver>,
}

pub fn assemble(
    bank: Arc<dyn BankGateway>,
    clock: DemoClock,
) -> Result<App, Box<dyn std::error::Error>> {
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(
        SessionRef::parse("mock_session_001")?,
        UserId::parse("user_001")?,
    );
    sessions.insert(
        SessionRef::parse("mock_session_002")?,
        UserId::parse("user_002")?,
    );
    let sessions = Arc::new(sessions);
    let authority = Arc::new(MockDraftAuthority::new(
        sessions.clone(),
        Arc::new(clock.clone()),
    ));
    let store = Arc::new(MockDraftStore::new(bank, clock, 300)?);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(CreateMockDraftTool::new(
        store.clone(),
        authority.clone(),
    )))?;
    let mut policies = DomainPolicyRegistry::new();
    policies.register(
        Arc::new(CreateMockDraftPolicy::new(store.clone())),
        PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION)?,
        MOCK_DRAFT_SCHEMA_VERSION,
    )?;
    let mut catalog = ToolSecurityCatalog::new();
    catalog.register(ToolSecurityProfile {
        tool_name: ToolName::parse(MOCK_DRAFT_CREATE_TOOL)?,
        tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
        policy_version: PolicyVersion::parse(MOCK_DRAFT_POLICY_VERSION)?,
        minimum_level: SecurityLevel::Green,
    })?;
    for (action, tool, policy) in [
        (
            MockDraftAction::Modify,
            MockDraftLifecycleTool::modify(store.clone(), authority.clone()),
            MockDraftLifecyclePolicy::modify(store.clone()),
        ),
        (
            MockDraftAction::Cancel,
            MockDraftLifecycleTool::cancel(store.clone(), authority.clone()),
            MockDraftLifecyclePolicy::cancel(store.clone()),
        ),
    ] {
        tools.register(Arc::new(tool))?;
        policies.register(
            Arc::new(policy),
            PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION)?,
            MOCK_DRAFT_SCHEMA_VERSION,
        )?;
        catalog.register(ToolSecurityProfile {
            tool_name: ToolName::parse(action.tool_name())?,
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION)?,
            minimum_level: SecurityLevel::Green,
        })?;
    }
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly = RuntimeBootstrap::build_mock_draft(
        tools,
        policies,
        catalog,
        sessions.clone(),
        audit.clone(),
        authority.clone(),
    )?;
    Ok(App {
        assembly,
        authority,
        store,
        audit,
        sessions,
    })
}

pub fn context(task: &str) -> ExecutionContext {
    ExecutionContext {
        task_id: TaskId::parse(task).unwrap(),
        node_id: NodeId::parse("read_references").unwrap(),
        operation_id: OperationId::parse("read_references_only").unwrap(),
        authenticated_user_id: UserId::parse("user_001").unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}
