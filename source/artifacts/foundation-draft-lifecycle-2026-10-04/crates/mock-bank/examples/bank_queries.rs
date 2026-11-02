mod support;

use safeflow_bank_gateway::{
    BankGateway, FindPayeesPolicy, FindPayeesTool, GetAccountBalancePolicy, GetAccountBalanceTool,
    BALANCE_POLICY_VERSION, BANK_QUERY_SCHEMA_VERSION, FIND_PAYEES, GET_ACCOUNT_BALANCE,
    PAYEES_POLICY_VERSION,
};
use safeflow_contracts::{
    AgentPlan, NodeId, PlanNode, PlanningResult, PolicyVersion, SecurityLevel, SessionRef, TaskId,
    ToolName, Understanding, UnderstandingDecision, UserId,
};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, MockLlm, PlanningService, RuntimeBootstrap,
    RuntimeOutcome, StaticSessionResolver, ToolRegistry, ToolSecurityCatalog, ToolSecurityProfile,
};
use serde_json::json;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    support::block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let same_name = std::env::args().skip(1).any(|arg| arg == "--same-name");
    let gateway: Arc<dyn BankGateway> = Arc::new(if same_name {
        MockBankGateway::with_same_name_payees()
    } else {
        MockBankGateway::new()
    });
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GetAccountBalanceTool::new(gateway.clone())))?;
    tools.register(Arc::new(FindPayeesTool::new(gateway.clone())))?;

    let mut policies = DomainPolicyRegistry::new();
    policies.register(
        Arc::new(GetAccountBalancePolicy::new(gateway.clone())),
        PolicyVersion::parse(BALANCE_POLICY_VERSION)?,
        BANK_QUERY_SCHEMA_VERSION,
    )?;
    policies.register(
        Arc::new(FindPayeesPolicy::new(gateway)),
        PolicyVersion::parse(PAYEES_POLICY_VERSION)?,
        BANK_QUERY_SCHEMA_VERSION,
    )?;

    // Central approval is explicit and independent of policy evaluation.
    let mut catalog = ToolSecurityCatalog::new();
    for (tool, policy_version) in [
        (GET_ACCOUNT_BALANCE, BALANCE_POLICY_VERSION),
        (FIND_PAYEES, PAYEES_POLICY_VERSION),
    ] {
        catalog.register(ToolSecurityProfile {
            tool_name: ToolName::parse(tool)?,
            tool_schema_version: 1,
            policy_version: PolicyVersion::parse(policy_version)?,
            minimum_level: SecurityLevel::Green,
        })?;
    }

    // These references belong to the server-side demo fixture, not business input.
    let session_ref = SessionRef::parse("session_bank_query_demo")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session_ref.clone(), UserId::parse("user_001")?);
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, Arc::new(sessions), audit.clone())?;

    let task_id = TaskId::parse("task_bank_query_demo")?;
    let plan = AgentPlan::new(
        task_id,
        1,
        vec![
            PlanNode::Tool {
                id: NodeId::parse("node_balance")?,
                tool: ToolName::parse(GET_ACCOUNT_BALANCE)?,
                arguments: json!({"account_id": "account_001"}),
                depends_on: vec![],
            },
            PlanNode::Tool {
                id: NodeId::parse("node_payees")?,
                tool: ToolName::parse(FIND_PAYEES)?,
                arguments: json!({"query": "小李"}),
                depends_on: vec![],
            },
        ],
    )?;
    let llm = Arc::new(MockLlm::new(PlanningResult::for_plan(
        Understanding {
            intent: "READ_TRANSFER_REFERENCES".to_owned(),
            entities: json!({"account_id": "account_001", "query": "小李"}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    )));
    let planning = PlanningService::new(llm, assembly.tool_registry());
    let planned = planning.plan("查询付款账户和已有收款人小李").await?;
    match assembly.runtime().execute(planned, session_ref).await? {
        RuntimeOutcome::Completed {
            task_id,
            state,
            results,
        } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "task_id": task_id,
                    "state": state,
                    "results": results,
                    "audit_event_count": audit.events()?.len()
                }))?
            );
        }
        other => return Err(format!("unexpected query outcome: {other:?}").into()),
    }
    Ok(())
}
