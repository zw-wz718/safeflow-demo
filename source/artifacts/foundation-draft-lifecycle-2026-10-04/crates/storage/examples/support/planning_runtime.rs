use safeflow_bank_gateway::*;
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, RwLock,
};

pub struct Observations {
    pub tool: AtomicUsize,
    pub policy: AtomicUsize,
    pub bank: AtomicUsize,
}
impl Observations {
    pub fn assert_intake_only(&self) {
        assert_eq!(self.tool.load(Ordering::SeqCst), 0);
        assert_eq!(self.policy.load(Ordering::SeqCst), 0);
        assert_eq!(self.bank.load(Ordering::SeqCst), 0);
    }
}
struct Bank {
    inner: MockBankGateway,
    observations: Arc<Observations>,
}
impl BankGateway for Bank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        self.observations.bank.fetch_add(1, Ordering::SeqCst);
        self.inner.get_account_balance(user, account)
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.observations.bank.fetch_add(1, Ordering::SeqCst);
        self.inner.find_payees(user, query)
    }
}
struct Tool {
    inner: GetAccountBalanceTool,
    observations: Arc<Observations>,
    // Tests can corrupt declared schema without inventing another business tool.
    schema_override: Option<Arc<RwLock<Option<Value>>>>,
}
impl AgentTool for Tool {
    fn definition(&self) -> ToolDefinition {
        let mut definition = self.inner.definition();
        if let Some(schema) = self
            .schema_override
            .as_ref()
            .and_then(|s| s.read().unwrap().clone())
        {
            definition.input_schema = schema;
        }
        definition
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.observations.tool.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(context, authorization, arguments)
    }
}
struct Policy {
    inner: GetAccountBalancePolicy,
    observations: Arc<Observations>,
}
impl DomainPolicy for Policy {
    fn tool_name(&self) -> &ToolName {
        self.inner.tool_name()
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        self.observations.policy.fetch_add(1, Ordering::SeqCst);
        self.inner.evaluate(context)
    }
    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        self.observations.policy.fetch_add(1, Ordering::SeqCst);
        self.inner.verify_execution(context)
    }
}
pub fn assemble(
    sessions: Arc<dyn SessionResolver>,
    schema_override: Option<Arc<RwLock<Option<Value>>>>,
) -> (
    RuntimeAssembly,
    Arc<Observations>,
    Arc<InMemoryAuditRecorder>,
) {
    let observations = Arc::new(Observations {
        tool: AtomicUsize::new(0),
        policy: AtomicUsize::new(0),
        bank: AtomicUsize::new(0),
    });
    let bank = Arc::new(Bank {
        inner: MockBankGateway::new(),
        observations: observations.clone(),
    });
    let tool = Arc::new(Tool {
        inner: GetAccountBalanceTool::new(bank.clone()),
        observations: observations.clone(),
        schema_override,
    });
    let policy = Arc::new(Policy {
        inner: GetAccountBalancePolicy::new(bank),
        observations: observations.clone(),
    });
    let definition = tool.definition();
    let mut tools = ToolRegistry::new();
    tools.register(tool).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            policy,
            PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            definition.schema_version,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: definition.name,
            tool_schema_version: definition.schema_version,
            policy_version: PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            minimum_level: SecurityLevel::Green,
        })
        .unwrap();
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, sessions, audit.clone()).unwrap();
    (assembly, observations, audit)
}
pub fn contracts() -> Vec<PreflightInputContract> {
    vec![PreflightInputContract {
        tool_name: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
        tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
        policy_version: PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
        projection_revision: "bank-direct-input-v1".into(),
    }]
}
pub fn proposal(task: &TaskId, version: u32) -> PlanningResult {
    PlanningResult::for_plan(
        Understanding {
            intent: "READ_BALANCE".into(),
            entities: json!({"account_id":"account_001"}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        AgentPlan::new(
            task.clone(),
            version,
            vec![
                PlanNode::Tool {
                    id: NodeId::parse("read").unwrap(),
                    tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                    arguments: json!({"account_id":"account_001"}),
                    depends_on: vec![],
                },
                PlanNode::UserDecision {
                    id: NodeId::parse("choose").unwrap(),
                    depends_on: vec![NodeId::parse("read").unwrap()],
                },
            ],
        )
        .unwrap(),
    )
}
