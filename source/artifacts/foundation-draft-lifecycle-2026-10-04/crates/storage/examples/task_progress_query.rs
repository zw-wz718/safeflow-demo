use safeflow_bank_gateway::{
    BankError, BankGateway, FindPayeesOutput, GetAccountBalanceOutput, GetAccountBalancePolicy,
    GetAccountBalanceTool, BALANCE_POLICY_VERSION, GET_ACCOUNT_BALANCE,
};
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use safeflow_storage::{new_task_id, PgRepository, RuntimeRecoveryService, TaskQueryService};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct ObservedBank {
    inner: MockBankGateway,
    reads: AtomicUsize,
}
impl BankGateway for ObservedBank {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_account_balance(user, account)
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        name: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.find_payees(user, name)
    }
}
struct ObservedTool {
    inner: GetAccountBalanceTool,
    calls: Arc<AtomicUsize>,
}
impl AgentTool for ObservedTool {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if !matches!(args.first().map(String::as_str), Some("prepare" | "query"))
        || (args[0] == "prepare" && args.len() != 1)
        || (args[0] == "query" && args.len() != 2)
    {
        return Err("usage: prepare | query TASK_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "backend database configuration required")?;
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let suffix = schema
        .strip_prefix("sf_task_query_demo_")
        .ok_or("use a fresh sf_task_query_demo_<32 hex> schema")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid demo namespace".into());
    }
    let preparing = args[0] == "prepare";
    let repo = if preparing {
        let repo = PgRepository::connect(&url, &schema).await?;
        repo.migrate().await?;
        repo
    } else {
        PgRepository::connect_existing(&url, &schema).await?
    };
    // Static mappings demonstrate trusted assembly, not a production login protocol.
    let session = SessionRef::parse("backend_task_query_demo_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let bank = Arc::new(ObservedBank {
        inner: MockBankGateway::new(),
        reads: AtomicUsize::new(0),
    });
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ObservedTool {
        inner: GetAccountBalanceTool::new(bank.clone()),
        calls: tool_calls.clone(),
    }))?;
    let mut policies = DomainPolicyRegistry::new();
    let version = PolicyVersion::parse(BALANCE_POLICY_VERSION)?;
    policies.register(
        Arc::new(GetAccountBalancePolicy::new(bank.clone())),
        version.clone(),
        1,
    )?;
    let mut profiles = ToolSecurityCatalog::new();
    profiles.register(ToolSecurityProfile {
        tool_name: ToolName::parse(GET_ACCOUNT_BALANCE)?,
        tool_schema_version: 1,
        policy_version: version,
        minimum_level: SecurityLevel::Green,
    })?;
    let assembly = RuntimeBootstrap::build(
        tools,
        policies,
        profiles,
        Arc::new(sessions),
        Arc::new(InMemoryAuditRecorder::new()),
    )?;
    let task_id = if preparing {
        let task_id = new_task_id();
        let plan = AgentPlan::new(
            task_id.clone(),
            2,
            vec![
                PlanNode::Tool {
                    id: NodeId::parse("read_balance")?,
                    tool: ToolName::parse(GET_ACCOUNT_BALANCE)?,
                    arguments: json!({"account_id":"account_001"}),
                    depends_on: vec![],
                },
                PlanNode::UserDecision {
                    id: NodeId::parse("choose")?,
                    depends_on: vec![NodeId::parse("read_balance")?],
                },
            ],
        )?;
        RuntimeRecoveryService::for_trusted_backend(repo.clone(), assembly.runtime())
            .execute_until_wait(session.clone(), plan, 7)
            .await?;
        task_id
    } else {
        TaskId::parse(&args[1])?
    };
    let prep_tools = tool_calls.swap(0, Ordering::SeqCst);
    let prep_reads = bank.reads.swap(0, Ordering::SeqCst);
    let view = TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
        .query(&session, &task_id)
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"mode":args[0],"view":view,"preparation_wrote_checkpoint":preparing,"preparation_tool_executions":prep_tools,"preparation_bank_reads":prep_reads,"query_tool_executions":tool_calls.load(Ordering::SeqCst),"query_bank_reads":bank.reads.load(Ordering::SeqCst),"automatic_execution":false,"business_result_not_supplied":true})
        )?
    );
    repo.close().await;
    Ok(())
}
