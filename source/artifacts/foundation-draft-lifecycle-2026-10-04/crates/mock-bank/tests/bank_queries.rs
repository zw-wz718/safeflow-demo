#[path = "../examples/support/mod.rs"]
mod support;

use safeflow_bank_gateway::{
    BankError, BankGateway, FindPayeesOutput, FindPayeesPolicy, FindPayeesTool,
    GetAccountBalanceOutput, GetAccountBalancePolicy, GetAccountBalanceTool,
    BALANCE_POLICY_VERSION, BANK_QUERY_SCHEMA_VERSION, FIND_PAYEES, GET_ACCOUNT_BALANCE,
    PAYEES_POLICY_VERSION,
};
use safeflow_contracts::{
    AgentPlan, AgentTool, CheckId, ContractFuture, DomainPolicy, ExecutionContext, FindingKind,
    NodeId, OperationId, PlanNode, PlanningResult, PolicyVersion, PostExecuteContext, ReasonCode,
    ResourceId, SecurityContext, SecurityError, SecurityFinding, SecurityLevel, SecurityOutcome,
    SessionRef, SourceName, TaskId, ToolDefinition, ToolError, ToolExecutionAuthorization,
    ToolName, ToolResult, Understanding, UnderstandingDecision, UserId,
};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{
    AuditEventKind, DomainPolicyRegistry, InMemoryAuditRecorder, MockLlm, PlanningService,
    RuntimeAssembly, RuntimeBootstrap, RuntimeError, RuntimeOutcome, StartupValidationError,
    StaticSessionResolver, ToolRegistry, ToolSecurityCatalog, ToolSecurityProfile,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use support::block_on;

struct ObservedGateway {
    inner: Arc<dyn BankGateway>,
    calls: AtomicUsize,
    users: Mutex<Vec<UserId>>,
}

impl ObservedGateway {
    fn new(inner: Arc<dyn BankGateway>) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            users: Mutex::new(vec![]),
        }
    }

    fn observe(&self, user: &UserId) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.users.lock().unwrap().push(user.clone());
    }
}

impl BankGateway for ObservedGateway {
    fn get_account_balance<'a>(
        &'a self,
        user: &'a UserId,
        account: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        self.observe(user);
        self.inner.get_account_balance(user, account)
    }

    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        query: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        self.observe(user);
        self.inner.find_payees(user, query)
    }
}

struct Harness {
    assembly: RuntimeAssembly,
    gateway: Arc<ObservedGateway>,
    audit: Arc<InMemoryAuditRecorder>,
}

fn registries(
    gateway: Arc<dyn BankGateway>,
) -> (ToolRegistry, DomainPolicyRegistry, ToolSecurityCatalog) {
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(GetAccountBalanceTool::new(gateway.clone())))
        .unwrap();
    tools
        .register(Arc::new(FindPayeesTool::new(gateway.clone())))
        .unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(GetAccountBalancePolicy::new(gateway.clone())),
            PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            BANK_QUERY_SCHEMA_VERSION,
        )
        .unwrap();
    policies
        .register(
            Arc::new(FindPayeesPolicy::new(gateway)),
            PolicyVersion::parse(PAYEES_POLICY_VERSION).unwrap(),
            BANK_QUERY_SCHEMA_VERSION,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    for (name, version) in [
        (GET_ACCOUNT_BALANCE, BALANCE_POLICY_VERSION),
        (FIND_PAYEES, PAYEES_POLICY_VERSION),
    ] {
        catalog
            .register(profile(name, version, SecurityLevel::Green))
            .unwrap();
    }
    (tools, policies, catalog)
}

fn profile(name: &str, version: &str, level: SecurityLevel) -> ToolSecurityProfile {
    ToolSecurityProfile {
        tool_name: ToolName::parse(name).unwrap(),
        tool_schema_version: 1,
        policy_version: PolicyVersion::parse(version).unwrap(),
        minimum_level: level,
    }
}

fn sessions() -> Arc<StaticSessionResolver> {
    let mut sessions = StaticSessionResolver::new();
    for (session, user) in [
        ("session_001", "user_001"),
        ("session_002", "user_002"),
        ("session_unknown_user", "user_unknown"),
    ] {
        sessions.insert(
            SessionRef::parse(session).unwrap(),
            UserId::parse(user).unwrap(),
        );
    }
    Arc::new(sessions)
}

impl Harness {
    fn new(bank: Arc<dyn BankGateway>) -> Self {
        let gateway = Arc::new(ObservedGateway::new(bank));
        let (tools, policies, catalog) = registries(gateway.clone());
        let audit = Arc::new(InMemoryAuditRecorder::new());
        let assembly =
            RuntimeBootstrap::build(tools, policies, catalog, sessions(), audit.clone()).unwrap();
        Self {
            assembly,
            gateway,
            audit,
        }
    }

    fn run(
        &self,
        requests: Vec<(&str, Value)>,
        session: &str,
    ) -> Result<Vec<ToolResult>, RuntimeError> {
        let llm = Arc::new(MockLlm::new(plan(requests)));
        let planning = PlanningService::new(llm.clone(), self.assembly.tool_registry());
        let planned = block_on(planning.plan("bank query test")).unwrap();
        assert_eq!(llm.call_count(), 1);
        assert_eq!(llm.last_input().unwrap().unwrap().tool_definitions.len(), 2);
        match block_on(
            self.assembly
                .runtime()
                .execute(planned, SessionRef::parse(session).unwrap()),
        )? {
            RuntimeOutcome::Completed { results, .. } => Ok(results),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    fn assert_no_tool_started(&self) {
        assert!(!self
            .audit
            .events()
            .unwrap()
            .iter()
            .any(|event| event.kind == AuditEventKind::ToolStarted));
    }
}

fn plan(requests: Vec<(&str, Value)>) -> PlanningResult {
    let nodes = requests
        .into_iter()
        .enumerate()
        .map(|(index, (tool, arguments))| PlanNode::Tool {
            id: NodeId::parse(format!("node_{index}")).unwrap(),
            tool: ToolName::parse(tool).unwrap(),
            arguments,
            depends_on: vec![],
        })
        .collect();
    PlanningResult::for_plan(
        Understanding {
            intent: "READ_BANK".to_owned(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        AgentPlan::new(TaskId::parse("task_server_query").unwrap(), 1, nodes).unwrap(),
    )
}

fn execution_context(user: &str) -> ExecutionContext {
    ExecutionContext {
        task_id: TaskId::parse("task_server_query").unwrap(),
        node_id: NodeId::parse("node_0").unwrap(),
        operation_id: OperationId::parse("operation_query_test").unwrap(),
        authenticated_user_id: UserId::parse(user).unwrap(),
        plan_version: 1,
        idempotency_key: None,
    }
}

#[test]
fn queries_run_through_planning_runtime_security_gateway_and_audit() {
    let harness = Harness::new(Arc::new(MockBankGateway::new()));
    let results = harness
        .run(
            vec![
                (GET_ACCOUNT_BALANCE, json!({"account_id": "account_001"})),
                (FIND_PAYEES, json!({"query": "小李"})),
            ],
            "session_001",
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0].data,
        json!({
            "account": {"account_id": "account_001", "display_name": "工资卡", "masked_account_no": "**** 6628", "currency": "CNY"},
            "available_balance": "1288.00", "as_of": "2026-10-01T14:30:00+08:00"
        })
    );
    assert_eq!(
        results[1].data,
        json!({"candidates": [{"payee_id": "payee_008", "display_name": "小李", "bank_name": "示例银行", "masked_account_no": "**** 1024"}]})
    );
    assert_eq!(
        results[0].operation_id.as_str(),
        "operation_task_server_query_node_0"
    );
    assert_ne!(results[0].operation_id, results[1].operation_id);
    assert_eq!(harness.gateway.calls.load(Ordering::SeqCst), 6);
    assert!(harness
        .gateway
        .users
        .lock()
        .unwrap()
        .iter()
        .all(|user| user.as_str() == "user_001"));
    let events = harness.audit.events().unwrap();
    assert_eq!(events.len(), 10);
    assert!(events
        .iter()
        .all(|event| event.task_id.as_str() == "task_server_query"));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == AuditEventKind::ToolCompleted)
            .count(),
        2
    );
    let audit_json = serde_json::to_string(&events).unwrap();
    for private_value in ["session_001", "1288.00", "小李", "1024", "6628"] {
        assert!(!audit_json.contains(private_value));
    }
}

#[test]
fn cross_user_account_access_is_denied_before_tool_execution() {
    for (session, account) in [
        ("session_002", "account_001"),
        ("session_001", "account_002"),
        ("session_001", "account_missing"),
    ] {
        let harness = Harness::new(Arc::new(MockBankGateway::new()));
        assert_eq!(
            harness
                .run(
                    vec![(GET_ACCOUNT_BALANCE, json!({"account_id": account}))],
                    session
                )
                .unwrap_err(),
            RuntimeError::SecurityGate(SecurityOutcome::Blocked)
        );
        harness.assert_no_tool_started();
        assert_eq!(harness.gateway.calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn mock_gateway_enforces_ownership_without_policy_help() {
    let bank = MockBankGateway::new();
    for (user, account) in [
        ("user_002", "account_001"),
        ("user_001", "account_002"),
        ("user_001", "missing"),
        ("user_missing", "account_001"),
    ] {
        assert_eq!(
            block_on(bank.get_account_balance(
                &UserId::parse(user).unwrap(),
                &ResourceId::parse(account).unwrap()
            ))
            .unwrap_err(),
            BankError::ResourceNotOwned
        );
    }
    assert_eq!(
        block_on(bank.find_payees(&UserId::parse("user_missing").unwrap(), "小李")).unwrap_err(),
        BankError::ResourceNotOwned
    );
}

#[test]
fn direct_tools_repeat_input_and_ownership_checks() {
    let gateway: Arc<dyn BankGateway> = Arc::new(MockBankGateway::new());
    let balance = GetAccountBalanceTool::new(gateway.clone());
    let payees = FindPayeesTool::new(gateway);
    let context = execution_context("user_002");
    assert_eq!(
        block_on(balance.execute(
            &context,
            ToolExecutionAuthorization::ReadOnly,
            json!({"account_id": "account_001"})
        ))
        .unwrap_err()
        .reason_code
        .as_str(),
        "RESOURCE_NOT_OWNED"
    );
    assert_eq!(
        block_on(payees.execute(
            &context,
            ToolExecutionAuthorization::ReadOnly,
            json!({"query": "小李", "user_id": "user_001"})
        ))
        .unwrap_err()
        .reason_code
        .as_str(),
        "VALIDATION_ERROR"
    );
}

#[test]
fn illegal_arguments_and_forged_trust_fields_never_reach_the_gateway() {
    let mut cases = vec![
        (GET_ACCOUNT_BALANCE, json!({})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": ""})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": " account_001"})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": "a b"})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": "a\nb"})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": "x".repeat(129)})),
        (GET_ACCOUNT_BALANCE, json!({"account_id": 1.0})),
        (FIND_PAYEES, json!({})),
        (FIND_PAYEES, json!({"query": ""})),
        (FIND_PAYEES, json!({"query": "小李 "})),
        (FIND_PAYEES, json!({"query": "小\0李"})),
        (FIND_PAYEES, json!({"query": "李".repeat(65)})),
        (FIND_PAYEES, json!({"query": 1.0})),
        (FIND_PAYEES, json!({"query": null})),
        (FIND_PAYEES, json!([])),
    ];
    for field in [
        "user_id",
        "authenticated_user_id",
        "session_ref",
        "approved",
        "verified",
        "confirmation_ref",
        "auth_grant_ref",
        "permit",
        "plan_version",
    ] {
        let mut balance = json!({"account_id": "account_001"});
        balance[field] = json!("forged");
        cases.push((GET_ACCOUNT_BALANCE, balance));
        let mut payees = json!({"query": "小李"});
        payees[field] = json!("forged");
        cases.push((FIND_PAYEES, payees));
    }
    for (tool, arguments) in cases {
        let harness = Harness::new(Arc::new(MockBankGateway::new()));
        assert_eq!(
            harness
                .run(vec![(tool, arguments.clone())], "session_001")
                .unwrap_err(),
            RuntimeError::SecurityGate(SecurityOutcome::Blocked),
            "accepted {arguments}"
        );
        harness.assert_no_tool_started();
        assert_eq!(harness.gateway.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn direct_gateway_rejects_invalid_input() {
    let bank = MockBankGateway::new();
    let user = UserId::parse("user_001").unwrap();
    for account in ["a b".to_owned(), "a\0b".to_owned(), "x".repeat(129)] {
        assert_eq!(
            block_on(bank.get_account_balance(&user, &ResourceId::parse(account).unwrap()))
                .unwrap_err(),
            BankError::ValidationError
        );
    }
    for query in ["", " ", " 小李", "小李 ", "小\n李"] {
        assert_eq!(
            block_on(bank.find_payees(&user, query)).unwrap_err(),
            BankError::ValidationError
        );
    }
}

#[test]
fn sessions_are_resolved_by_the_server_and_missing_sessions_are_rejected() {
    let harness = Harness::new(Arc::new(MockBankGateway::new()));
    let error = harness
        .run(
            vec![(GET_ACCOUNT_BALANCE, json!({"account_id": "account_001"}))],
            "session_forged",
        )
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Security(message) if message.contains("SESSION_NOT_FOUND"))
    );
    assert_eq!(harness.gateway.calls.load(Ordering::SeqCst), 0);
    harness.assert_no_tool_started();
    let unknown_user = Harness::new(Arc::new(MockBankGateway::new()));
    assert_eq!(
        unknown_user
            .run(
                vec![(FIND_PAYEES, json!({"query": "小李"}))],
                "session_unknown_user"
            )
            .unwrap_err(),
        RuntimeError::SecurityGate(SecurityOutcome::Blocked)
    );
    unknown_user.assert_no_tool_started();
}

#[test]
fn same_name_candidates_are_preserved_without_selecting_a_payee() {
    let harness = Harness::new(Arc::new(MockBankGateway::with_same_name_payees()));
    let results = harness
        .run(vec![(FIND_PAYEES, json!({"query": "小李"}))], "session_001")
        .unwrap();
    let candidates = results[0].data["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0]["payee_id"], "payee_008");
    assert_eq!(candidates[1]["payee_id"], "payee_009");
    assert!(results[0].data.get("selected_payee").is_none());
}

#[test]
fn payee_search_filters_by_user_and_returns_no_match_as_an_empty_list() {
    let harness = Harness::new(Arc::new(MockBankGateway::with_same_name_payees()));
    let other_user = harness
        .run(vec![(FIND_PAYEES, json!({"query": "小李"}))], "session_002")
        .unwrap();
    assert_eq!(
        other_user[0].data["candidates"].as_array().unwrap().len(),
        1
    );
    assert_eq!(other_user[0].data["candidates"][0]["payee_id"], "payee_020");
    let no_match = harness
        .run(
            vec![(FIND_PAYEES, json!({"query": "不存在"}))],
            "session_001",
        )
        .unwrap();
    assert_eq!(no_match[0].data, json!({"candidates": []}));
}

#[test]
fn returned_views_are_copies_and_queries_do_not_mutate_bank_data() {
    let bank = MockBankGateway::new();
    let user = UserId::parse("user_001").unwrap();
    let account = ResourceId::parse("account_001").unwrap();
    let mut view = block_on(bank.get_account_balance(&user, &account)).unwrap();
    view.account.display_name = "changed".to_owned();
    let mut payees = block_on(bank.find_payees(&user, "小李")).unwrap();
    payees.candidates.clear();
    assert_eq!(
        block_on(bank.get_account_balance(&user, &account))
            .unwrap()
            .account
            .display_name,
        "工资卡"
    );
    assert_eq!(
        block_on(bank.find_payees(&user, "小李"))
            .unwrap()
            .candidates
            .len(),
        1
    );
}

#[test]
fn real_query_bindings_fail_closed_on_missing_and_incompatible_metadata() {
    let gateway: Arc<dyn BankGateway> = Arc::new(MockBankGateway::new());
    let (tools, _, catalog) = registries(gateway.clone());
    let missing_policy = RuntimeBootstrap::build(
        tools,
        DomainPolicyRegistry::new(),
        catalog,
        sessions(),
        Arc::new(InMemoryAuditRecorder::new()),
    );
    assert!(matches!(
        missing_policy,
        Err(StartupValidationError::MissingPolicy(_))
    ));
    let (tools, policies, _) = registries(gateway.clone());
    let missing_profile = RuntimeBootstrap::build(
        tools,
        policies,
        ToolSecurityCatalog::new(),
        sessions(),
        Arc::new(InMemoryAuditRecorder::new()),
    );
    assert!(matches!(
        missing_profile,
        Err(StartupValidationError::MissingSecurityProfile(_))
    ));
    for (schema, version) in [(2, BALANCE_POLICY_VERSION), (1, "unapproved-v2")] {
        let (tools, policies, _) = registries(gateway.clone());
        let mut catalog = ToolSecurityCatalog::new();
        catalog
            .register(profile(
                FIND_PAYEES,
                PAYEES_POLICY_VERSION,
                SecurityLevel::Green,
            ))
            .unwrap();
        let mut balance_profile = profile(GET_ACCOUNT_BALANCE, version, SecurityLevel::Green);
        balance_profile.tool_schema_version = schema;
        catalog.register(balance_profile).unwrap();
        let result = RuntimeBootstrap::build(
            tools,
            policies,
            catalog,
            sessions(),
            Arc::new(InMemoryAuditRecorder::new()),
        );
        assert!(matches!(
            result,
            Err(StartupValidationError::CatalogSchemaMismatch { .. }
                | StartupValidationError::PolicyVersionMismatch { .. })
        ));
    }
}

struct FixedPolicy {
    name: ToolName,
    deny: bool,
}

impl DomainPolicy for FixedPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            Ok(if self.deny {
                vec![SecurityFinding {
                    check_id: CheckId::parse("TEST-DENY").unwrap(),
                    policy_version: PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
                    source: SourceName::parse("test_policy").unwrap(),
                    kind: FindingKind::HardDeny,
                    reason_code: ReasonCode::parse("RESOURCE_NOT_OWNED").unwrap(),
                    evidence_refs: vec![],
                    observed_value: None,
                    event_ids: vec![],
                    dedup_key: None,
                    window_start_ms: None,
                    window_end_ms: None,
                }]
            } else {
                vec![]
            })
        })
    }

    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async { Ok(vec![]) })
    }
}

#[test]
fn central_minimum_and_hard_denial_stop_real_query_tools() {
    for (deny, level, expected) in [
        (true, SecurityLevel::Green, SecurityOutcome::Blocked),
        (false, SecurityLevel::Red, SecurityOutcome::WaitingAuth),
    ] {
        let gateway = Arc::new(ObservedGateway::new(Arc::new(MockBankGateway::new())));
        let mut tools = ToolRegistry::new();
        tools
            .register(Arc::new(GetAccountBalanceTool::new(gateway.clone())))
            .unwrap();
        let mut policies = DomainPolicyRegistry::new();
        policies
            .register(
                Arc::new(FixedPolicy {
                    name: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
                    deny,
                }),
                PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
                1,
            )
            .unwrap();
        let mut catalog = ToolSecurityCatalog::new();
        catalog
            .register(profile(GET_ACCOUNT_BALANCE, BALANCE_POLICY_VERSION, level))
            .unwrap();
        let audit = Arc::new(InMemoryAuditRecorder::new());
        let assembly =
            RuntimeBootstrap::build(tools, policies, catalog, sessions(), audit.clone()).unwrap();
        let result = block_on(assembly.runtime().execute(
            plan(vec![(
                GET_ACCOUNT_BALANCE,
                json!({"account_id": "account_001"}),
            )]),
            SessionRef::parse("session_001").unwrap(),
        ));
        assert_eq!(result.unwrap_err(), RuntimeError::SecurityGate(expected));
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 0);
        assert!(!audit
            .events()
            .unwrap()
            .iter()
            .any(|event| event.kind == AuditEventKind::ToolStarted));
    }
}

struct WriteProbe {
    calls: AtomicUsize,
}

impl AgentTool for WriteProbe {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            ToolName::parse("create_transfer_draft").unwrap(),
            "Write probe only; not a draft implementation",
            json!({"type": "object"}),
            1,
            true,
            "SAVE_DRAFT",
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
        Box::pin(async { panic!("runtime must not execute a write tool") })
    }
}

#[test]
fn draft_storage_remains_a_write_and_runtime_never_executes_it() {
    let probe = Arc::new(WriteProbe {
        calls: AtomicUsize::new(0),
    });
    let mut tools = ToolRegistry::new();
    tools.register(probe.clone()).unwrap();
    let mut policies = DomainPolicyRegistry::new();
    policies
        .register(
            Arc::new(FixedPolicy {
                name: ToolName::parse("create_transfer_draft").unwrap(),
                deny: false,
            }),
            PolicyVersion::parse("draft-policy-v1").unwrap(),
            1,
        )
        .unwrap();
    let mut catalog = ToolSecurityCatalog::new();
    catalog
        .register(profile(
            "create_transfer_draft",
            "draft-policy-v1",
            SecurityLevel::Green,
        ))
        .unwrap();
    let audit = Arc::new(InMemoryAuditRecorder::new());
    let assembly =
        RuntimeBootstrap::build(tools, policies, catalog, sessions(), audit.clone()).unwrap();
    let result = block_on(assembly.runtime().execute(
        plan(vec![("create_transfer_draft", json!({"amount": "200.00"}))]),
        SessionRef::parse("session_001").unwrap(),
    ));
    assert_eq!(
        result.unwrap_err(),
        RuntimeError::WriteToolRequiresLaterStage("create_transfer_draft".to_owned())
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert!(!audit
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == AuditEventKind::ToolStarted));
}

struct UnavailableGateway;

impl BankGateway for UnavailableGateway {
    fn get_account_balance<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async { Err(BankError::Unavailable) })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        Box::pin(async { Err(BankError::Unavailable) })
    }
}

#[test]
fn replacing_the_gateway_propagates_failure_without_fallback_data() {
    for (tool, arguments) in [
        (GET_ACCOUNT_BALANCE, json!({"account_id": "account_001"})),
        (FIND_PAYEES, json!({"query": "小李"})),
    ] {
        let harness = Harness::new(Arc::new(UnavailableGateway));
        assert_eq!(
            harness
                .run(vec![(tool, arguments)], "session_001")
                .unwrap_err(),
            RuntimeError::SecurityGate(SecurityOutcome::Blocked)
        );
        harness.assert_no_tool_started();
    }
}

#[test]
fn returned_views_reject_wrong_resources_unmasked_accounts_and_duplicate_ids() {
    let bank = MockBankGateway::new();
    let user = UserId::parse("user_001").unwrap();
    let account = ResourceId::parse("account_001").unwrap();
    let balance = block_on(bank.get_account_balance(&user, &account)).unwrap();
    assert_eq!(
        balance
            .validate_for(&ResourceId::parse("account_002").unwrap())
            .unwrap_err(),
        BankError::InvalidResponse
    );
    let mut unmasked = balance;
    unmasked.account.masked_account_no = "622200006628".to_owned();
    assert_eq!(
        unmasked.validate_for(&account).unwrap_err(),
        BankError::InvalidResponse
    );
    let mut payees = block_on(bank.find_payees(&user, "小李")).unwrap();
    payees.candidates.push(payees.candidates[0].clone());
    assert_eq!(payees.validate().unwrap_err(), BankError::InvalidResponse);
}
