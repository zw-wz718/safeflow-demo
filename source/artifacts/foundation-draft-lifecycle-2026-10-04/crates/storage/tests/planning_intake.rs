//! Public runtime intake tests use existing account Tool/Policy; no storage or business execution.
#[path = "../examples/support/planning_runtime.rs"]
mod support;
use safeflow_bank_gateway::{GetAccountBalanceTool, BALANCE_POLICY_VERSION, GET_ACCOUNT_BALANCE};
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::*;
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

struct Sessions {
    binding: RwLock<Option<SessionBinding>>,
    calls: AtomicUsize,
    revoke_at: AtomicUsize,
}
impl Sessions {
    fn new() -> Self {
        Self {
            binding: RwLock::new(Some(SessionBinding {
                user_id: UserId::parse("user_001").unwrap(),
                binding_version: 1,
            })),
            calls: AtomicUsize::new(0),
            revoke_at: AtomicUsize::new(0),
        }
    }
    fn revoke(&self) {
        *self.binding.write().unwrap() = None;
    }
    fn rebind(&self, user: &str, version: u64) {
        *self.binding.write().unwrap() = Some(SessionBinding {
            user_id: UserId::parse(user).unwrap(),
            binding_version: version,
        });
    }
}
impl SessionResolver for Sessions {
    fn resolve(&self, session: &SessionRef) -> Result<UserId, SecurityError> {
        Ok(self.resolve_binding(session)?.user_id)
    }
    fn resolve_binding(&self, _: &SessionRef) -> Result<SessionBinding, SecurityError> {
        let point = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if point == self.revoke_at.load(Ordering::SeqCst) {
            self.revoke();
        }
        self.binding
            .read()
            .unwrap()
            .clone()
            .ok_or_else(|| SecurityError {
                reason_code: ReasonCode::parse("SESSION_REVOKED").unwrap(),
                message: "private-session-secret".into(),
            })
    }
}
fn task() -> TaskId {
    TaskId::parse("task_backend_001").unwrap()
}
fn session() -> SessionRef {
    SessionRef::parse("session_private_001").unwrap()
}
fn valid() -> PlanningResult {
    support::proposal(&task(), 2)
}
fn raw(result: &PlanningResult) -> Vec<u8> {
    serde_json::to_vec(result).unwrap()
}
fn error(result: Result<AcceptedPlanningProposal, PlanningIntakeError>) -> PlanningIntakeError {
    match result {
        Err(e) => e,
        Ok(_) => panic!("unsafe proposal accepted"),
    }
}
async fn accept(result: PlanningResult) -> Result<AcceptedPlanningProposal, PlanningIntakeError> {
    let sessions = Arc::new(Sessions::new());
    let (assembly, observed, audit) = support::assemble(sessions, None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    let output = service
        .accept_json(&session(), &task(), 2, &raw(&result))
        .await;
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
    output
}

#[tokio::test]
async fn raw_and_mock_gateway_share_the_same_core_and_do_not_execute() {
    let sessions = Arc::new(Sessions::new());
    let (assembly, observed, audit) = support::assemble(sessions, None);
    let mock = Arc::new(MockLlm::new(valid()));
    let service = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        Some(mock.clone()),
        support::contracts(),
    )
    .unwrap();
    let typed = service
        .plan(&session(), &task(), 2, "查询余额")
        .await
        .unwrap();
    let parsed = service
        .accept_json(&session(), &task(), 2, &raw(&valid()))
        .await
        .unwrap();
    assert_eq!(typed.planning_result(), parsed.planning_result());
    assert_eq!(typed.contract_version(), 1);
    assert_eq!(typed.task_id(), &task());
    assert_eq!(typed.plan_version(), 2);
    assert_eq!(mock.call_count(), 1);
    let input = mock.last_input().unwrap().unwrap();
    let encoded = format!("{input:?}");
    for secret in [
        "user_001",
        "session_private_001",
        "task_backend_001",
        BALANCE_POLICY_VERSION,
    ] {
        assert!(!encoded.contains(secret));
    }
    assert_eq!(input.tool_definitions.len(), 1);
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}
#[tokio::test]
async fn clarify_is_bound_to_backend_context_without_plan_or_execution() {
    let mut result = valid();
    result.understanding.decision = UnderstandingDecision::Clarify;
    result.understanding.missing_fields = vec!["account_id".into()];
    result.plan = None;
    result.clarification = Some(Clarification {
        question: "查询哪一个账户？".into(),
    });
    let accepted = accept(result.clone()).await.unwrap();
    assert_eq!(accepted.task_id(), &task());
    assert_eq!(accepted.planning_result(), &result);
    result.clarification.as_mut().unwrap().question = " ".into();
    assert_eq!(
        error(accept(result).await),
        PlanningIntakeError::InvalidResult
    );
}
#[tokio::test]
async fn task_and_version_must_equal_the_trusted_backend_expectation() {
    for (id, version) in [("task_other", 2), ("task_backend_001", 1)] {
        let result = support::proposal(&TaskId::parse(id).unwrap(), version);
        assert_eq!(
            error(accept(result).await),
            PlanningIntakeError::TaskBinding
        );
    }
}
#[tokio::test]
async fn invalid_pass_clarify_and_contract_version_combinations_reject() {
    let value = serde_json::to_value(valid()).unwrap();
    for (pointer, changed) in [
        ("/contract_version", json!(9)),
        ("/plan/contract_version", json!(2)),
        ("/plan/plan_version", json!(0)),
        ("/plan", Value::Null),
        ("/clarification", json!({"question":"which account?"})),
        ("/understanding/decision", json!("CLARIFY")),
        ("/understanding/missing_fields", json!(["account_id"])),
        ("/understanding/intent", json!("")),
        ("/understanding/context_refs", json!(["prior_result"])),
    ] {
        let mut v = value.clone();
        *v.pointer_mut(pointer).unwrap() = changed;
        let typed: PlanningResult = serde_json::from_value(v).unwrap();
        assert!(accept(typed).await.is_err(), "{pointer}");
    }
}
#[tokio::test]
async fn strict_json_rejects_duplicate_keys_at_every_level_and_escaped_aliases() {
    let sessions = Arc::new(Sessions::new());
    let (assembly, observed, audit) = support::assemble(sessions, None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    let original = String::from_utf8(raw(&valid())).unwrap();
    for text in [
        original.replacen(
            "\"contract_version\":1",
            "\"contract_version\":1,\"contract_version\":1",
            1,
        ),
        original.replace(
            "\"account_id\":\"account_001\"",
            "\"account_id\":\"account_001\",\"account_id\":\"account_001\"",
        ),
        original.replacen(
            "\"entities\":{",
            "\"entities\":{\"nested\":{\"x\":1,\"x\":2},",
            1,
        ),
        original.replacen("\"entities\":{", "\"entities\":{\"a\":1,\"\\u0061\":2,", 1),
        original.replacen(
            "\"depends_on\":[]",
            "\"depends_on\":[],\"depends_on\":[]",
            1,
        ),
    ] {
        assert_eq!(
            error(
                service
                    .accept_json(&session(), &task(), 2, text.as_bytes())
                    .await
            ),
            PlanningIntakeError::InvalidJson
        );
    }
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}
#[tokio::test]
async fn raw_json_rejects_unknown_fields_text_wrappers_trailing_and_malformed_content() {
    let sessions = Arc::new(Sessions::new());
    let (assembly, observed, _) = support::assemble(sessions, None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    let original = String::from_utf8(raw(&valid())).unwrap();
    for text in [
        "{bad".into(),
        format!("```json\n{original}\n```"),
        format!("{original} {{}}"),
        original.replacen('{', "{\"owner\":\"user_001\",", 1),
        original.replacen("\"intent\":", "\"confirmed\":true,\"intent\":", 1),
        original.replacen(
            "\"plan_version\":",
            "\"permit\":\"forged\",\"plan_version\":",
            1,
        ),
        original.replacen("\"type\":\"TOOL\"", "\"type\":\"TOOL\",\"success\":true", 1),
    ] {
        assert!(service
            .accept_json(&session(), &task(), 2, text.as_bytes())
            .await
            .is_err());
    }
    assert!(service
        .accept_json(&session(), &task(), 2, &[0xff])
        .await
        .is_err());
    observed.assert_intake_only();
}
#[tokio::test]
async fn unknown_unapproved_tools_and_invalid_direct_arguments_reject_in_both_paths() {
    for arguments in [
        json!({}),
        json!({"account_id":1}),
        json!({"account_id":200.0}),
        json!({"account_id":"account_001","owner":"user_001"}),
        json!({"account_id":"${read.balance}"}),
        json!({"account_id":"{{result}}"}),
        json!({"account_id":"$ref:read"}),
        json!({"account_id":" account_001"}),
    ] {
        let mut result = valid();
        if let PlanNode::Tool { arguments: a, .. } = &mut result.plan.as_mut().unwrap().nodes[0] {
            *a = arguments;
        }
        let (assembly, observed, _) = support::assemble(Arc::new(Sessions::new()), None);
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            Some(Arc::new(MockLlm::new(result.clone()))),
            support::contracts(),
        )
        .unwrap();
        assert!(service
            .accept_json(&session(), &task(), 2, &raw(&result))
            .await
            .is_err());
        assert!(service.plan(&session(), &task(), 2, "query").await.is_err());
        observed.assert_intake_only();
    }
    let mut result = valid();
    if let PlanNode::Tool { tool, .. } = &mut result.plan.as_mut().unwrap().nodes[0] {
        *tool = ToolName::parse("execute_funds").unwrap();
    }
    assert_eq!(
        error(accept(result).await),
        PlanningIntakeError::Registration
    );
    let (assembly, _, _) = support::assemble(Arc::new(Sessions::new()), None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, vec![]).unwrap();
    assert_eq!(
        error(
            service
                .accept_json(&session(), &task(), 2, &raw(&valid()))
                .await
        ),
        PlanningIntakeError::Registration
    );
}
#[tokio::test]
async fn node_duplicates_bad_dependencies_self_cycles_and_duplicate_edges_reject() {
    let mut variants = vec![];
    let mut p = valid();
    p.plan.as_mut().unwrap().nodes[1] = p.plan.as_ref().unwrap().nodes[0].clone();
    variants.push(p);
    for dependencies in [
        vec!["missing"],
        vec!["read"],
        vec!["choose"],
        vec!["choose", "choose"],
    ] {
        let mut p = valid();
        if let PlanNode::Tool { depends_on, .. } = &mut p.plan.as_mut().unwrap().nodes[0] {
            *depends_on = dependencies
                .into_iter()
                .map(|s| NodeId::parse(s).unwrap())
                .collect();
        }
        variants.push(p);
    }
    for p in variants {
        assert_eq!(error(accept(p).await), PlanningIntakeError::InvalidResult);
    }
}
#[tokio::test]
async fn boundary_node_counts_identifiers_depth_and_cumulative_size_are_bounded() {
    let mut p = valid();
    p.plan.as_mut().unwrap().nodes = (0..256)
        .map(|n| PlanNode::UserDecision {
            id: NodeId::parse(format!("node_{n}")).unwrap(),
            depends_on: vec![],
        })
        .collect();
    assert!(accept(p.clone()).await.is_ok());
    p.plan.as_mut().unwrap().nodes.push(PlanNode::UserDecision {
        id: NodeId::parse("extra").unwrap(),
        depends_on: vec![],
    });
    assert!(accept(p).await.is_err());
    let mut p = valid();
    p.plan.as_mut().unwrap().nodes.clear();
    assert!(accept(p).await.is_err());
    for id in ["x".repeat(201), "bad\nid".into()] {
        let mut p = valid();
        if let PlanNode::Tool { id: node, .. } = &mut p.plan.as_mut().unwrap().nodes[0] {
            *node = NodeId::parse(id).unwrap();
        }
        assert!(accept(p).await.is_err());
    }
    let mut nested = json!({});
    for _ in 0..16 {
        nested = json!({"child":nested});
    }
    let mut p = valid();
    p.understanding.entities = nested;
    assert!(accept(p).await.is_err());
    let mut p = valid();
    p.understanding.entities = Value::Object(
        (0..32)
            .map(|n| (format!("field_{n}"), json!("x".repeat(4096))))
            .collect(),
    );
    assert!(accept(p).await.is_err());
    let (assembly, _, _) = support::assemble(Arc::new(Sessions::new()), None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    let mut padded = raw(&valid());
    padded.resize(65537, b' ');
    assert_eq!(
        error(service.accept_json(&session(), &task(), 2, &padded).await),
        PlanningIntakeError::Limit
    );
}
#[tokio::test]
async fn schema_constraints_are_not_ignored_and_ecmascript_classes_are_not_broadened() {
    for property in [
        json!({"type":"string","pattern":"^allowed_only$"}),
        json!({"type":"string","minLength":20}),
        json!({"type":"string","maxLength":2}),
        json!({"type":"string","minimum":1}),
        json!({"type":"string","pattern":"["}),
        json!({"type":"string","pattern":1}),
        json!({"type":"string","format":"email"}),
    ] {
        let schema = json!({"type":"object","properties":{"account_id":property},"required":["account_id"],"additionalProperties":false});
        let override_schema = Arc::new(RwLock::new(Some(schema)));
        let (assembly, observed, _) =
            support::assemble(Arc::new(Sessions::new()), Some(override_schema));
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        assert_eq!(
            error(
                service
                    .accept_json(&session(), &task(), 2, &raw(&valid()))
                    .await
            ),
            PlanningIntakeError::InputContract
        );
        observed.assert_intake_only();
    }
    for (pattern, good, bad) in [(r"^\d+$", "12", "١٢"), (r"^\w+$", "a_1", "汉字")] {
        let schema = json!({"type":"object","properties":{"account_id":{"type":"string","pattern":pattern}},"required":["account_id"],"additionalProperties":false});
        let (assembly, _, _) = support::assemble(
            Arc::new(Sessions::new()),
            Some(Arc::new(RwLock::new(Some(schema)))),
        );
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            None,
            support::contracts(),
        )
        .unwrap();
        for (value, succeeds) in [(good, true), (bad, false)] {
            let mut p = valid();
            if let PlanNode::Tool { arguments, .. } = &mut p.plan.as_mut().unwrap().nodes[0] {
                *arguments = json!({"account_id":value});
            }
            assert_eq!(
                service
                    .accept_json(&session(), &task(), 2, &raw(&p))
                    .await
                    .is_ok(),
                succeeds
            );
        }
    }
}
struct WaitingGateway {
    mock: Arc<MockLlm>,
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

#[tokio::test]
async fn typed_gateway_cannot_bypass_raw_depth_float_count_or_aggregate_limits() {
    let mut decimal = valid();
    decimal.understanding.entities = json!({"untrusted_amount_text":"200.00"});
    let mut floating = valid();
    floating.understanding.entities = json!({"nested":{"value":200.0}});
    let mut deep = valid();
    let mut nested = json!({});
    for _ in 0..16 {
        nested = json!({"child":nested});
    }
    deep.understanding.entities = nested;
    let mut aggregate = valid();
    aggregate.understanding.entities = Value::Object(
        (0..32)
            .map(|n| (format!("field_{n}"), json!("x".repeat(4096))))
            .collect(),
    );
    let mut count = valid();
    count.understanding.entities = json!({"counts":vec![vec![Value::Null;16];256]});
    // Each argument fits its local budget; only accumulation across nodes exceeds it.
    let mut argument_bytes = valid();
    argument_bytes.plan.as_mut().unwrap().nodes = (0..33)
        .map(|n| PlanNode::Tool {
            id: NodeId::parse(format!("read_{n}")).unwrap(),
            tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            arguments: json!({"account_id":"x".repeat(2048)}),
            depends_on: vec![],
        })
        .collect();
    assert!(33 * 2048 > 65536);
    let mut argument_values = valid();
    argument_values.plan.as_mut().unwrap().nodes = (0..17)
        .map(|n| PlanNode::Tool {
            id: NodeId::parse(format!("read_{n}")).unwrap(),
            tool: ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            arguments: json!({"account_id":vec![Value::Null;256]}),
            depends_on: vec![],
        })
        .collect();
    assert!(17 * (1 + 1 + 256) > 4096);
    assert!(raw(&argument_values).len() < 65536);
    for result in [&argument_bytes, &argument_values] {
        for node in &result.plan.as_ref().unwrap().nodes {
            if let PlanNode::Tool { arguments, .. } = node {
                assert!(serde_json::to_vec(arguments).unwrap().len() < 4096);
            }
        }
    }
    let mut nodes = valid();
    nodes.plan.as_mut().unwrap().nodes = (0..257)
        .map(|n| PlanNode::UserDecision {
            id: NodeId::parse(format!("node_{n}")).unwrap(),
            depends_on: vec![],
        })
        .collect();
    for (result, expected, cumulative_limit) in [
        (decimal, true, false),
        (floating, false, false),
        (deep, false, false),
        (aggregate, false, false),
        (count, false, false),
        (argument_bytes, false, true),
        (argument_values, false, true),
        (nodes, false, false),
    ] {
        let (assembly, observed, audit) = support::assemble(Arc::new(Sessions::new()), None);
        let mock = Arc::new(MockLlm::new(result.clone()));
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            Some(mock.clone()),
            support::contracts(),
        )
        .unwrap();
        let raw_result = service
            .accept_json(&session(), &task(), 2, &raw(&result))
            .await;
        let typed_result = service.plan(&session(), &task(), 2, "query").await;
        assert_eq!(raw_result.is_ok(), expected);
        assert_eq!(typed_result.is_ok(), expected);
        if cumulative_limit {
            // The raw visitor maps resource failures to InvalidJson; typed intake
            // must hit the aggregate budget before parameter contract validation.
            assert_eq!(error(typed_result), PlanningIntakeError::Limit);
        }
        assert_eq!(mock.call_count(), 1);
        observed.assert_intake_only();
        assert!(audit.events().unwrap().is_empty());
    }
}
impl LlmGateway for WaitingGateway {
    fn generate<'a>(
        &'a self,
        input: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        Box::pin(async move {
            let result = self.mock.generate(input).await;
            self.entered.notify_one();
            self.released.notified().await;
            result
        })
    }
}
fn waiting() -> Arc<WaitingGateway> {
    Arc::new(WaitingGateway {
        mock: Arc::new(MockLlm::new(valid())),
        entered: tokio::sync::Notify::new(),
        released: tokio::sync::Notify::new(),
    })
}
#[tokio::test]
async fn revoke_or_rebind_during_gateway_wait_rejects_without_retry() {
    for user in [None, Some("user_001"), Some("user_other")] {
        let sessions = Arc::new(Sessions::new());
        let (assembly, observed, audit) = support::assemble(sessions.clone(), None);
        let gateway = waiting();
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            Some(gateway.clone()),
            support::contracts(),
        )
        .unwrap();
        let session = session();
        let task = task();
        let mut request = Box::pin(service.plan(&session, &task, 2, "query"));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! { _ = gateway.entered.notified() => {}, _ = &mut request => panic!("gateway did not wait") }
        }).await.unwrap();
        if let Some(user) = user {
            sessions.rebind(user, 2);
        } else {
            sessions.revoke();
        }
        gateway.released.notify_one();
        assert_eq!(error(request.await), PlanningIntakeError::Session);
        assert_eq!(gateway.mock.call_count(), 1);
        observed.assert_intake_only();
        assert!(audit.events().unwrap().is_empty());
    }
}
#[tokio::test]
async fn cancellation_drops_the_single_pending_gateway_call_without_execution() {
    let (assembly, observed, audit) = support::assemble(Arc::new(Sessions::new()), None);
    let gateway = waiting();
    let service = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        Some(gateway.clone()),
        support::contracts(),
    )
    .unwrap();
    let session = session();
    let task = task();
    let mut request = Box::pin(service.plan(&session, &task, 2, "query"));
    tokio::select! { _ = gateway.entered.notified() => {}, _ = &mut request => panic!("gateway did not wait") }
    drop(request);
    gateway.released.notify_one();
    assert_eq!(gateway.mock.call_count(), 1);
    observed.assert_intake_only();
    assert!(audit.events().unwrap().is_empty());
}
#[tokio::test]
async fn every_session_observation_including_the_final_one_is_checked() {
    for raw_path in [false, true] {
        let sessions = Arc::new(Sessions::new());
        let (assembly, _, _) = support::assemble(sessions.clone(), None);
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            Some(Arc::new(MockLlm::new(valid()))),
            support::contracts(),
        )
        .unwrap();
        if raw_path {
            service
                .accept_json(&session(), &task(), 2, &raw(&valid()))
                .await
                .unwrap();
        } else {
            service.plan(&session(), &task(), 2, "query").await.unwrap();
        }
        let observations = sessions.calls.load(Ordering::SeqCst);
        for point in 1..=observations {
            let sessions = Arc::new(Sessions::new());
            sessions.revoke_at.store(point, Ordering::SeqCst);
            let (assembly, observed, _) = support::assemble(sessions, None);
            let service = PlanningIntakeService::for_trusted_backend(
                assembly.runtime(),
                Some(Arc::new(MockLlm::new(valid()))),
                support::contracts(),
            )
            .unwrap();
            let result = if raw_path {
                service
                    .accept_json(&session(), &task(), 2, &raw(&valid()))
                    .await
            } else {
                service.plan(&session(), &task(), 2, "query").await
            };
            assert_eq!(error(result), PlanningIntakeError::Session, "point {point}");
            observed.assert_intake_only();
        }
    }
}
struct FailingGateway(AtomicUsize);
impl LlmGateway for FailingGateway {
    fn generate<'a>(
        &'a self,
        _: &'a PlanningInput,
    ) -> ContractFuture<'a, Result<PlanningResult, PlanningError>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(PlanningError::Gateway(
                "token-secret postgresql://private user-input".into(),
            ))
        })
    }
}
#[tokio::test]
async fn absent_and_failing_gateways_are_explicit_and_errors_do_not_leak() {
    let (assembly, observed, _) = support::assemble(Arc::new(Sessions::new()), None);
    let service =
        PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, support::contracts())
            .unwrap();
    assert_eq!(
        error(service.plan(&session(), &task(), 2, "query").await),
        PlanningIntakeError::GatewayNotConfigured
    );
    let gateway = Arc::new(FailingGateway(AtomicUsize::new(0)));
    let service = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        Some(gateway.clone()),
        support::contracts(),
    )
    .unwrap();
    let failure = error(service.plan(&session(), &task(), 2, "query").await);
    assert_eq!(failure, PlanningIntakeError::GatewayFailed);
    assert_eq!(gateway.0.load(Ordering::SeqCst), 1);
    for secret in ["token-secret", "postgresql", "user-input", "private"] {
        assert!(!failure.to_string().contains(secret));
    }
    observed.assert_intake_only();
}
#[tokio::test]
async fn invalid_input_or_unversioned_session_never_calls_gateway() {
    struct Unversioned;
    impl SessionResolver for Unversioned {
        fn resolve(&self, _: &SessionRef) -> Result<UserId, SecurityError> {
            Ok(UserId::parse("user_001").unwrap())
        }
    }
    for sessions in [Arc::new(Unversioned) as Arc<dyn SessionResolver>, {
        let s = Sessions::new();
        s.rebind("user_001", 0);
        Arc::new(s)
    }] {
        let (assembly, observed, _) = support::assemble(sessions, None);
        let mock = Arc::new(MockLlm::new(valid()));
        let service = PlanningIntakeService::for_trusted_backend(
            assembly.runtime(),
            Some(mock.clone()),
            support::contracts(),
        )
        .unwrap();
        assert_eq!(
            error(service.plan(&session(), &task(), 2, "query").await),
            PlanningIntakeError::Session
        );
        assert_eq!(mock.call_count(), 0);
        observed.assert_intake_only();
    }
    let (assembly, _, _) = support::assemble(Arc::new(Sessions::new()), None);
    let mock = Arc::new(MockLlm::new(valid()));
    let service = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        Some(mock.clone()),
        support::contracts(),
    )
    .unwrap();
    for input in [" ".into(), "x".repeat(4097)] {
        assert!(service.plan(&session(), &task(), 2, &input).await.is_err());
    }
    assert!(service.plan(&session(), &task(), 0, "query").await.is_err());
    assert_eq!(mock.call_count(), 0);
}
#[tokio::test]
async fn registration_or_schema_change_during_wait_rejects() {
    let override_schema = Arc::new(RwLock::new(None));
    let (assembly, observed, _) =
        support::assemble(Arc::new(Sessions::new()), Some(override_schema.clone()));
    let gateway = waiting();
    let service = PlanningIntakeService::for_trusted_backend(
        assembly.runtime(),
        Some(gateway.clone()),
        support::contracts(),
    )
    .unwrap();
    let session = session();
    let task = task();
    let mut request = Box::pin(service.plan(&session, &task, 2, "query"));
    tokio::select! { _ = gateway.entered.notified() => {}, _ = &mut request => panic!("gateway did not wait") }
    let mut schema = GetAccountBalanceTool::new(Arc::new(MockBankGateway::new()))
        .definition()
        .input_schema;
    schema["properties"]["account_id"]["minLength"] = json!(2);
    *override_schema.write().unwrap() = Some(schema);
    gateway.released.notify_one();
    assert_eq!(error(request.await), PlanningIntakeError::Registration);
    observed.assert_intake_only();
}
#[tokio::test]
async fn explicit_projection_versions_and_duplicates_cannot_be_forged_by_model() {
    let (assembly, _, _) = support::assemble(Arc::new(Sessions::new()), None);
    for contracts in [
        vec![
            support::contracts()[0].clone(),
            support::contracts()[0].clone(),
        ],
        {
            let mut c = support::contracts();
            c[0].policy_version = PolicyVersion::parse("unapproved").unwrap();
            c
        },
        {
            let mut c = support::contracts();
            c[0].tool_schema_version = 2;
            c
        },
        {
            let mut c = support::contracts();
            c[0].projection_revision = String::new();
            c
        },
    ] {
        assert!(matches!(
            PlanningIntakeService::for_trusted_backend(assembly.runtime(), None, contracts),
            Err(PlanningIntakeError::Registration)
        ));
    }
}
#[tokio::test]
async fn runtime_still_rejects_unregistered_execution_after_a_valid_intake() {
    let accepted = accept(valid()).await.unwrap();
    let tools = ToolRegistry::new();
    // Empty trusted assembly: another Runtime cannot treat intake as execution authority.
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session(), UserId::parse("user_001").unwrap());
    let assembly = RuntimeBootstrap::build(
        tools,
        DomainPolicyRegistry::new(),
        ToolSecurityCatalog::new(),
        Arc::new(sessions),
        Arc::new(InMemoryAuditRecorder::new()),
    )
    .unwrap();
    assert!(matches!(
        assembly
            .runtime()
            .execute(accepted.into_planning_result(), session())
            .await,
        Err(RuntimeError::PlanRejected)
    ));
}
