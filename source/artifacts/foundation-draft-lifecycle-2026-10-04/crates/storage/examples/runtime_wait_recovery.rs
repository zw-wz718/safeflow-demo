use safeflow_bank_gateway::{
    GetAccountBalancePolicy, GetAccountBalanceTool, BALANCE_POLICY_VERSION, GET_ACCOUNT_BALANCE,
};
use safeflow_contracts::{
    AgentPlan, AgentTool, ContractFuture, ExecutionContext, NodeId, PlanNode, PolicyVersion,
    SecurityLevel, SessionRef, TaskId, ToolDefinition, ToolError, ToolExecutionAuthorization,
    ToolName, ToolResult, UserId,
};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, RuntimeBootstrap, StaticSessionResolver,
    ToolRegistry, ToolSecurityCatalog, ToolSecurityProfile,
};
use safeflow_storage::{new_task_id, PgRepository, RuntimeRecoveryService, WaitingTaskVersion};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

struct CountedQuery {
    inner: GetAccountBalanceTool,
    calls: AtomicUsize,
    last_query: Mutex<Option<Value>>,
}
impl AgentTool for CountedQuery {
    fn definition(&self) -> ToolDefinition {
        self.inner.definition()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .inner
                .execute(context, authorization, arguments)
                .await?;
            *self.last_query.lock().unwrap() = Some(result.data.clone());
            Ok(result)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 1 && args.len() != 5 {
        return Err("usage: save | restore TASK_ID REVISION PLAN_VERSION FACTS_VERSION".into());
    }
    let url =
        std::env::var("SAFEFLOW_DATABASE_URL").map_err(|_| "SAFEFLOW_DATABASE_URL is required")?;
    let schema = std::env::var("SAFEFLOW_STORAGE_SCHEMA")
        .map_err(|_| "SAFEFLOW_STORAGE_SCHEMA is required")?;
    let repo = PgRepository::connect(&url, &schema).await?;
    repo.migrate().await?;
    let bank = Arc::new(MockBankGateway::new());
    let query = Arc::new(CountedQuery {
        inner: GetAccountBalanceTool::new(bank.clone()),
        calls: AtomicUsize::new(0),
        last_query: Mutex::new(None),
    });
    let mut tools = ToolRegistry::new();
    tools.register(query.clone())?;
    let mut policies = DomainPolicyRegistry::new();
    let version = PolicyVersion::parse(BALANCE_POLICY_VERSION)?;
    policies.register(
        Arc::new(GetAccountBalancePolicy::new(bank)),
        version.clone(),
        1,
    )?;
    let mut catalog = ToolSecurityCatalog::new();
    catalog.register(ToolSecurityProfile {
        tool_name: ToolName::parse(GET_ACCOUNT_BALANCE)?,
        tool_schema_version: 1,
        policy_version: version,
        minimum_level: SecurityLevel::Green,
    })?;
    let session = SessionRef::parse("server_runtime_wait_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let assembly = RuntimeBootstrap::build(
        tools,
        policies,
        catalog,
        Arc::new(sessions),
        Arc::new(InMemoryAuditRecorder::new()),
    )?;
    let service = RuntimeRecoveryService::for_trusted_backend(repo.clone(), assembly.runtime());
    let outcome = match args[0].as_str() {
        "save" if args.len() == 1 => {
            let plan = AgentPlan::new(
                new_task_id(),
                2,
                vec![
                    PlanNode::Tool {
                        id: NodeId::parse("read_balance")?,
                        tool: ToolName::parse(GET_ACCOUNT_BALANCE)?,
                        arguments: json!({"account_id":"account_001"}),
                        depends_on: vec![],
                    },
                    PlanNode::UserDecision {
                        id: NodeId::parse("choose_payee")?,
                        depends_on: vec![NodeId::parse("read_balance")?],
                    },
                ],
            )?;
            service.execute_until_wait(session, plan, 7).await?
        }
        "restore" if args.len() == 5 => {
            service
                .recover_wait(
                    session,
                    &TaskId::parse(&args[1])?,
                    WaitingTaskVersion {
                        task_revision: args[2].parse()?,
                        plan_version: args[3].parse()?,
                        facts_version: args[4].parse()?,
                    },
                )
                .await?
        }
        _ => {
            return Err("usage: save | restore TASK_ID REVISION PLAN_VERSION FACTS_VERSION".into())
        }
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode": args[0], "task": outcome.task, "revalidated_facts_version": outcome.revalidated_facts_version,
            "tool_execution_count": query.calls.load(Ordering::SeqCst), "last_query": query.last_query.lock().unwrap().clone(),
            "automatic_execution": false, "mock_bank": true,
        }))?
    );
    repo.close().await;
    Ok(())
}
