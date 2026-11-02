//! Application assembly for examples/tests only. It executes two READ tools,
//! never registers or executes a draft-storage Tool. Not a new common Runtime.

use safeflow_bank_gateway::{
    BankGateway, FindPayeesOutput, FindPayeesPolicy, FindPayeesTool, GetAccountBalanceOutput,
    GetAccountBalancePolicy, GetAccountBalanceTool, BALANCE_POLICY_VERSION,
    BANK_QUERY_SCHEMA_VERSION, FIND_PAYEES, GET_ACCOUNT_BALANCE, PAYEES_POLICY_VERSION,
};
use safeflow_contracts::{
    AgentPlan, ExecutionContext, NodeId, OperationId, PlanNode, PlanningResult, PolicyVersion,
    ResourceId, SecurityLevel, SessionRef, TaskId, ToolName, ToolResultStatus, Understanding,
    UnderstandingDecision,
};
use safeflow_runtime::{
    DomainPolicyRegistry, InMemoryAuditRecorder, MockLlm, PlanningService, RuntimeBootstrap,
    RuntimeOutcome, SessionResolver, StaticSessionResolver, ToolRegistry, ToolSecurityCatalog,
    ToolSecurityProfile,
};
use serde_json::json;
use std::sync::Arc;

pub struct ReadReferences {
    pub context: ExecutionContext,
    pub account: GetAccountBalanceOutput,
    pub payees: FindPayeesOutput,
    pub query_operations: Vec<OperationId>,
    pub audit_event_count: usize,
}

/// Server-owned task/session and explicit account selection. A fixed MockLlm
/// supplies the demo plan; it is not natural-language understanding or login.
pub async fn query_bank(
    bank: Arc<dyn BankGateway>,
    sessions: Arc<StaticSessionResolver>,
    session: SessionRef,
    task: TaskId,
    account: ResourceId,
    query: &str,
) -> Result<ReadReferences, Box<dyn std::error::Error>> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GetAccountBalanceTool::new(bank.clone())))?;
    tools.register(Arc::new(FindPayeesTool::new(bank.clone())))?;
    let mut policies = DomainPolicyRegistry::new();
    policies.register(
        Arc::new(GetAccountBalancePolicy::new(bank.clone())),
        PolicyVersion::parse(BALANCE_POLICY_VERSION)?,
        BANK_QUERY_SCHEMA_VERSION,
    )?;
    policies.register(
        Arc::new(FindPayeesPolicy::new(bank)),
        PolicyVersion::parse(PAYEES_POLICY_VERSION)?,
        BANK_QUERY_SCHEMA_VERSION,
    )?;
    let mut catalog = ToolSecurityCatalog::new();
    for (tool, version) in [
        (GET_ACCOUNT_BALANCE, BALANCE_POLICY_VERSION),
        (FIND_PAYEES, PAYEES_POLICY_VERSION),
    ] {
        catalog.register(ToolSecurityProfile {
            tool_name: ToolName::parse(tool)?,
            tool_schema_version: BANK_QUERY_SCHEMA_VERSION,
            policy_version: PolicyVersion::parse(version)?,
            minimum_level: SecurityLevel::Green,
        })?;
    }
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, sessions.clone(), audit.clone())?;
    let balance_node = NodeId::parse("read_balance")?;
    let plan = AgentPlan::new(
        task.clone(),
        1,
        vec![
            PlanNode::Tool {
                id: balance_node.clone(),
                tool: ToolName::parse(GET_ACCOUNT_BALANCE)?,
                arguments: json!({"account_id": account}),
                depends_on: vec![],
            },
            PlanNode::Tool {
                id: NodeId::parse("read_payees")?,
                tool: ToolName::parse(FIND_PAYEES)?,
                arguments: json!({"query": query}),
                depends_on: vec![],
            },
        ],
    )?;
    let llm = Arc::new(MockLlm::new(PlanningResult::for_plan(
        Understanding {
            intent: "READ_TRANSFER_REFERENCES".into(),
            entities: json!({"account_id": account, "query": query}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    )));
    let planning = PlanningService::new(llm, assembly.tool_registry());
    let planned = planning
        .plan("查询已明确选择的付款账户和已有收款人")
        .await?;
    let outcome = assembly.runtime().execute(planned, session.clone()).await?;
    let RuntimeOutcome::Completed {
        task_id, results, ..
    } = outcome
    else {
        return Err("bank-reference query did not complete; do not create a draft".into());
    };
    if task_id != task
        || results.len() != 2
        || results
            .iter()
            .any(|result| result.status != ToolResultStatus::Succeeded)
    {
        return Err("bank-reference results do not match the completed task".into());
    }
    let balance_result = results
        .iter()
        .find(|result| result.tool_name.as_str() == GET_ACCOUNT_BALANCE)
        .ok_or("missing balance result")?;
    let payees_result = results
        .iter()
        .find(|result| result.tool_name.as_str() == FIND_PAYEES)
        .ok_or("missing payee result")?;
    let account_output: GetAccountBalanceOutput =
        serde_json::from_value(balance_result.data.clone())?;
    account_output.validate_for(&account)?;
    let payees: FindPayeesOutput = serde_json::from_value(payees_result.data.clone())?;
    payees.validate()?;
    // Same immutable server fixture resolver used by Runtime. Neither the
    // business input nor the model supplies the authenticated identity.
    let context = ExecutionContext {
        authenticated_user_id: sessions.resolve(&session)?,
        task_id: task,
        node_id: balance_node,
        operation_id: balance_result.operation_id.clone(),
        plan_version: 1,
        idempotency_key: None,
    };
    Ok(ReadReferences {
        context,
        account: account_output,
        payees,
        query_operations: results
            .iter()
            .map(|result| result.operation_id.clone())
            .collect(),
        audit_event_count: audit.events()?.len(),
    })
}
