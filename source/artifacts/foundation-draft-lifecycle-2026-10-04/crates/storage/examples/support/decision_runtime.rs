use safeflow_bank_gateway::*;
use safeflow_contracts::*;
use safeflow_runtime::*;
use serde_json::Value;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

// Observation only: the real Tool and Policy are retained; no success is synthesized.
struct CountedTool {
    inner: Arc<dyn AgentTool>,
    calls: Arc<AtomicUsize>,
}
impl AgentTool for CountedTool {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.execute(context, authorization, arguments)
    }
}
pub fn counted(tool: Arc<dyn AgentTool>, calls: Arc<AtomicUsize>) -> Arc<dyn AgentTool> {
    Arc::new(CountedTool { inner: tool, calls })
}
pub fn assemble(
    sessions: Arc<dyn SessionResolver>,
    bank: Arc<dyn BankGateway>,
    minimum: SecurityLevel,
    calls: Arc<AtomicUsize>,
) -> RuntimeAssembly {
    let mut tools = ToolRegistry::new();
    let mut policies = DomainPolicyRegistry::new();
    let mut catalog = ToolSecurityCatalog::new();
    for (tool, policy, version) in [
        (
            Arc::new(GetAccountBalanceTool::new(bank.clone())) as Arc<dyn AgentTool>,
            Arc::new(GetAccountBalancePolicy::new(bank.clone())) as Arc<dyn DomainPolicy>,
            BALANCE_POLICY_VERSION,
        ),
        (
            Arc::new(FindPayeesTool::new(bank.clone())) as Arc<dyn AgentTool>,
            Arc::new(FindPayeesPolicy::new(bank)) as Arc<dyn DomainPolicy>,
            PAYEES_POLICY_VERSION,
        ),
    ] {
        let definition = tool.definition();
        tools.register(counted(tool, calls.clone())).unwrap();
        policies
            .register(
                policy,
                PolicyVersion::parse(version).unwrap(),
                definition.schema_version,
            )
            .unwrap();
        catalog
            .register(ToolSecurityProfile {
                tool_name: definition.name,
                tool_schema_version: definition.schema_version,
                policy_version: PolicyVersion::parse(version).unwrap(),
                minimum_level: minimum,
            })
            .unwrap();
    }
    RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        sessions,
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap()
}
pub fn input_contracts() -> Vec<PreflightInputContract> {
    [
        (GET_ACCOUNT_BALANCE, BALANCE_POLICY_VERSION),
        (FIND_PAYEES, PAYEES_POLICY_VERSION),
    ]
    .into_iter()
    .map(|(tool, policy)| PreflightInputContract {
        tool_name: ToolName::parse(tool).unwrap(),
        tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
        policy_version: PolicyVersion::parse(policy).unwrap(),
        projection_revision: "bank-direct-input-v1".into(),
    })
    .collect()
}
