use safeflow_contracts::{
    AgentPlan, AgentTool, ContractFuture, DomainPolicy, ExecutionContext, NodeId, PlanNode,
    PlanningResult, PolicyVersion, PostExecuteContext, SecurityContext, SecurityError,
    SecurityFinding, SecurityLevel, SecurityPhase, SessionRef, TaskId, TaskState, ToolDefinition,
    ToolError, ToolExecutionAuthorization, ToolName, ToolResult, ToolResultRef, ToolResultStatus,
    Understanding, UnderstandingDecision, UserId,
};
use safeflow_runtime::{
    AuditEventKind, DomainPolicyRegistry, InMemoryAuditRecorder, MockLlm, PlanningService,
    RuntimeBootstrap, RuntimeOutcome, StaticSessionResolver, ToolRegistry, ToolSecurityCatalog,
    ToolSecurityProfile,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

struct MockAgentTool {
    name: ToolName,
    calls: AtomicUsize,
    seen_users: Mutex<Vec<UserId>>,
}

impl MockAgentTool {
    fn new(name: ToolName) -> Self {
        Self {
            name,
            calls: AtomicUsize::new(0),
            seen_users: Mutex::new(vec![]),
        }
    }
}

impl AgentTool for MockAgentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            self.name.clone(),
            "Read the authenticated user's account balance",
            json!({
                "type": "object",
                "properties": {"account_id": {"type": "string"}},
                "required": ["account_id"]
            }),
            1,
            false,
            "READ_BALANCE",
        )
        .unwrap()
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            assert!(matches!(
                authorization,
                ToolExecutionAuthorization::ReadOnly
            ));
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_users
                .lock()
                .unwrap()
                .push(context.authenticated_user_id.clone());
            Ok(ToolResult::new(
                self.name.clone(),
                context.operation_id.clone(),
                ToolResultRef::parse("tool_result_001").unwrap(),
                ToolResultStatus::Succeeded,
                json!({
                    "account_id": arguments["account_id"],
                    "available_balance": "1288.00",
                    "currency": "CNY"
                }),
            ))
        })
    }
}

struct MockDomainPolicy {
    name: ToolName,
    phases: Mutex<Vec<SecurityPhase>>,
    post_execute_calls: AtomicUsize,
    seen_users: Mutex<Vec<UserId>>,
}

impl MockDomainPolicy {
    fn new(name: ToolName) -> Self {
        Self {
            name,
            phases: Mutex::new(vec![]),
            post_execute_calls: AtomicUsize::new(0),
            seen_users: Mutex::new(vec![]),
        }
    }
}

impl DomainPolicy for MockDomainPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            self.phases.lock().unwrap().push(context.phase);
            self.seen_users
                .lock()
                .unwrap()
                .push(context.authenticated_user_id.clone());
            Ok(vec![])
        })
    }

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            self.post_execute_calls.fetch_add(1, Ordering::SeqCst);
            self.seen_users
                .lock()
                .unwrap()
                .push(context.authenticated_user_id.clone());
            Ok(vec![])
        })
    }
}

fn fixed_planning_result(tool_name: ToolName) -> PlanningResult {
    let task_id = TaskId::parse("task_001").unwrap();
    let plan = AgentPlan::new(
        task_id,
        1,
        vec![PlanNode::Tool {
            id: NodeId::parse("node_001").unwrap(),
            tool: tool_name,
            arguments: json!({"account_id": "account_001"}),
            depends_on: vec![],
        }],
    )
    .unwrap();
    PlanningResult::for_plan(
        Understanding {
            intent: "GET_ACCOUNT_BALANCE".to_owned(),
            entities: json!({"account_id": "account_001"}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        plan,
    )
}

#[test]
fn fixed_plan_runs_once_through_runtime_security_tool_and_audit() {
    let tool_name = ToolName::parse("get_account_balance").unwrap();
    let tool = Arc::new(MockAgentTool::new(tool_name.clone()));
    let policy = Arc::new(MockDomainPolicy::new(tool_name.clone()));

    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    let policy_version = PolicyVersion::parse("balance-policy-v1").unwrap();
    policies
        .register(policy.clone(), policy_version.clone(), 1)
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(ToolSecurityProfile {
            tool_name: tool_name.clone(),
            tool_schema_version: 1,
            policy_version,
            minimum_level: SecurityLevel::Green,
        })
        .unwrap();

    let mock_llm = Arc::new(MockLlm::new(fixed_planning_result(tool_name)));

    let session_ref = SessionRef::parse("session_001").unwrap();
    let user_id = UserId::parse("user_001").unwrap();
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session_ref.clone(), user_id.clone());
    let sessions = Arc::new(sessions);
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, sessions.clone(), audit.clone()).unwrap();
    let planning = PlanningService::new(mock_llm.clone(), assembly.tool_registry());
    let planning_result = block_on(planning.plan("查询账户余额")).unwrap();

    let outcome = block_on(assembly.runtime().execute(planning_result, session_ref)).unwrap();

    let RuntimeOutcome::Completed { state, results, .. } = outcome else {
        panic!("expected a completed runtime outcome");
    };
    assert_eq!(state, TaskState::Completed);
    assert_eq!(results.len(), 1);
    assert_eq!(mock_llm.call_count(), 1);
    assert_eq!(
        mock_llm
            .last_input()
            .unwrap()
            .unwrap()
            .tool_definitions
            .len(),
        1
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        tool.seen_users.lock().unwrap().as_slice(),
        &[user_id.clone()]
    );
    assert_eq!(
        policy.phases.lock().unwrap().as_slice(),
        &[SecurityPhase::PreTool, SecurityPhase::PreAuth]
    );
    assert_eq!(policy.post_execute_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        policy.seen_users.lock().unwrap().as_slice(),
        &[user_id.clone(), user_id.clone(), user_id]
    );

    let event_kinds: Vec<_> = audit
        .events()
        .unwrap()
        .into_iter()
        .map(|event| event.kind)
        .collect();
    assert_eq!(
        event_kinds,
        vec![
            AuditEventKind::PlanAccepted,
            AuditEventKind::SecurityEvaluated,
            AuditEventKind::SecurityEvaluated,
            AuditEventKind::ToolStarted,
            AuditEventKind::ToolCompleted,
            AuditEventKind::TaskCompleted,
        ]
    );
}
