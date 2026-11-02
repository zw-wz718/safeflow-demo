use super::*;
use crate::plugin_test_support::probe;
use safeflow_mock_bank::MockBankGateway;
use std::sync::atomic::Ordering;
use std::time::Duration;
use zeroize::Zeroizing;

#[test]
fn read_only_extension_uses_existing_binding_checks_and_rejects_incomplete_approvals() {
    let fixture = Fixture::new();
    for change in 0..12 {
        let probe = probe(fixture.bank.clone(), false);
        let mut registration = probe.registration;
        match change {
            0 => registration.policy = Arc::new(GetAccountBalancePolicy::new(fixture.bank.clone())),
            1 => registration.profile.tool_name = ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            2 => registration.input.tool_name = ToolName::parse(GET_ACCOUNT_BALANCE).unwrap(),
            3 => registration.compatible_schema_version += 1,
            4 => registration.profile.tool_schema_version += 1,
            5 => registration.input.tool_schema_version += 1,
            6 => registration.policy_version = PolicyVersion::parse("unapproved-v2").unwrap(),
            7 => {
                registration.profile.policy_version = PolicyVersion::parse("unapproved-v2").unwrap()
            }
            8 => registration.input.policy_version = PolicyVersion::parse("unapproved-v2").unwrap(),
            9 => registration.input.projection_revision = "unknown".into(),
            10 => registration.approved_action.clear(),
            11 => registration.implementation_revision.clear(),
            _ => unreachable!(),
        }
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        let result = inputs
            .extend(&[registration])
            .and_then(|_| fixture.build(AssemblyScope::Balance, inputs));
        assert!(result.is_err(), "case {change}");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn read_only_extension_duplicate_and_write_adapters_fail_before_execution() {
    let fixture = Fixture::new();
    let probe = probe(fixture.bank.clone(), false);
    let mut inputs = fixture.inputs(AssemblyScope::Balance);
    assert!(inputs
        .extend(&[probe.registration.clone(), probe.registration.clone()])
        .is_err());
    let mut inputs = fixture.inputs(AssemblyScope::AgentLifecycle);
    assert!(inputs.extend(&[probe.registration.clone()]).is_err());
    let mut registration = probe.registration;
    registration.tool = Arc::new(CreateMockDraftTool::new(
        fixture.draft.0.clone(),
        fixture.draft.1.clone(),
    ));
    let mut inputs = fixture.inputs(AssemblyScope::Balance);
    assert!(inputs.extend(&[registration]).is_err());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn read_only_extension_provenance_is_stable_and_binds_central_approval() {
    let fixture = Fixture::new();
    let build = |registration: ReadOnlyToolRegistration| {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        inputs.extend(&[registration]).unwrap();
        fixture.build(AssemblyScope::Balance, inputs).unwrap()
    };
    let registration = probe(fixture.bank.clone(), false).registration;
    let baseline = build(registration.clone());
    assert_eq!(baseline.definitions().len(), 2);
    assert_eq!(hash(&baseline), hash(&build(registration.clone())));
    let mut changed = registration.clone();
    changed.implementation_revision.push_str("-revised");
    assert_ne!(hash(&baseline), hash(&build(changed)));
    let mut changed = registration;
    changed.profile.minimum_level = SecurityLevel::Yellow;
    assert_ne!(hash(&baseline), hash(&build(changed)));
}

#[tokio::test]
async fn read_only_recovery_snapshot_binds_backend_approval() {
    let fixture = Fixture::new();
    let build = |registration: ReadOnlyToolRegistration| {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        inputs.extend(&[registration]).unwrap();
        fixture.build(AssemblyScope::Balance, inputs).unwrap()
    };
    let registration = probe(fixture.bank.clone(), false).registration;
    let first = build(registration.clone());
    let second = build(registration.clone());
    let mut changed = registration.clone();
    changed.implementation_revision.push_str("-changed");
    let changed = build(changed);
    let mut changed_profile = registration;
    changed_profile.profile.minimum_level = SecurityLevel::Yellow;
    let changed_profile = build(changed_profile);
    let cookie = fixture
        .sessions
        .issue("assembly-test-secret", None)
        .unwrap();
    let cookie = cookie::Cookie::parse(cookie).unwrap();
    let session = crate::session::token_reference(cookie.value()).unwrap();
    let plan = proposal(&TaskId::parse("snapshot-test").unwrap())
        .plan
        .unwrap();
    let baseline = first
        .runtime()
        .review_read_only_execution(plan.clone(), session.clone(), 7, first.contracts())
        .await
        .unwrap();
    let same = second
        .runtime()
        .review_read_only_execution(plan.clone(), session.clone(), 7, second.contracts())
        .await
        .unwrap();
    let different = changed
        .runtime()
        .review_read_only_execution(plan.clone(), session.clone(), 7, changed.contracts())
        .await
        .unwrap();
    assert_eq!(baseline.bindings_hash(), same.bindings_hash());
    assert_ne!(baseline.bindings_hash(), different.bindings_hash());
    let different_profile = changed_profile
        .runtime()
        .review_read_only_execution(plan, session, 7, changed_profile.contracts())
        .await
        .unwrap();
    assert_ne!(baseline.bindings_hash(), different_profile.bindings_hash());
}

struct DefinitionOverride {
    inner: Arc<dyn AgentTool>,
    definition: ToolDefinition,
}
impl AgentTool for DefinitionOverride {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: serde_json::Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        self.inner.execute(context, authorization, arguments)
    }
}
#[test]
fn read_only_extension_rejects_unsupported_schema_and_missing_registered_components() {
    let fixture = Fixture::new();
    for change in 0..6 {
        let probe = probe(fixture.bank.clone(), false);
        let mut registration = probe.registration;
        if change <= 1 {
            let mut definition = registration.tool.definition();
            definition.input_schema = if change == 0 {
                json!({"$ref":"unconfirmed-schema"})
            } else {
                json!({"type":"object","properties":{"nested":{"type":"object","properties":{"query":{"type":"string"}},"required":["query"],"additionalProperties":false}},"required":["nested"],"additionalProperties":false})
            };
            registration.tool = Arc::new(DefinitionOverride {
                inner: registration.tool,
                definition,
            });
        }
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        inputs.extend(&[registration]).unwrap();
        match change {
            2 => inputs.policies = DomainPolicyRegistry::new(),
            3 => inputs.catalog = ToolSecurityCatalog::new(),
            4 => {
                inputs.contracts.pop();
            }
            5 => {
                inputs.executions.pop();
            }
            _ => {}
        }
        assert!(fixture.build(AssemblyScope::Balance, inputs).is_err());
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    }
}

struct Clock;
impl MockDraftClock for Clock {
    fn now_seconds(&self) -> u64 {
        1_800_000_000
    }
}
struct Fixture {
    sessions: Arc<SimulationSessions>,
    bank: Arc<MockBankGateway>,
    draft: DraftDependencies,
}
impl Fixture {
    fn new() -> Self {
        let sessions = Arc::new(
            SimulationSessions::new(
                Zeroizing::new("assembly-test-secret".into()),
                UserId::parse("user_001").unwrap(),
                Duration::from_secs(300),
            )
            .unwrap(),
        );
        let bank = Arc::new(MockBankGateway::new());
        let store = Arc::new(MockDraftStore::new(bank.clone(), SystemClock, 600).unwrap());
        let authority = Arc::new(MockDraftAuthority::new(sessions.clone(), Arc::new(Clock)));
        Self {
            sessions,
            bank,
            draft: (store, authority),
        }
    }
    fn inputs(&self, scope: AssemblyScope) -> AssemblyInputs {
        AssemblyInputs::new(scope, self.bank.clone(), Some(&self.draft)).unwrap()
    }
    fn build(
        &self,
        scope: AssemblyScope,
        inputs: AssemblyInputs,
    ) -> Result<CheckedAssembly, &'static str> {
        inputs.build(
            scope,
            self.sessions.clone(),
            Arc::new(InMemoryAuditRecorder::new()),
            Some(self.draft.1.clone()),
        )
    }
}
fn hash(assembly: &CheckedAssembly) -> ArgumentsHash {
    PlanningIntakeService::for_registered_backend(assembly.runtime(), None, assembly.contracts())
        .unwrap()
        .registration_hash_for_backend()
        .unwrap()
}

#[test]
fn approved_scopes_publish_exact_model_and_dispatch_sets() {
    let fixture = Fixture::new();
    for (scope, count, drafts) in [
        (AssemblyScope::Balance, 1, 0),
        (AssemblyScope::LegacyDraft, 1, 1),
        (AssemblyScope::AgentLifecycle, 6, 3),
    ] {
        let assembled = fixture.build(scope, fixture.inputs(scope)).unwrap();
        let intake = PlanningIntakeService::for_registered_backend(
            assembled.runtime(),
            None,
            assembled.contracts(),
        )
        .unwrap();
        assert_eq!(assembled.definitions().len(), count);
        assert_eq!(assembled.draft_contracts().len(), drafts);
        assert_eq!(
            intake.approved_tools_for_backend().unwrap(),
            assembled.definitions()
        );
        for definition in assembled.definitions() {
            let execution = assembled.execution(&definition.name).unwrap();
            assert_eq!(
                definition.has_side_effect,
                execution != ExecutionPath::ReadOnly
            );
        }
        for forbidden in ["submit_transfer", "unknown_tool"] {
            assert_eq!(
                assembled.execution(&ToolName::parse(forbidden).unwrap()),
                None
            );
        }
    }
}

#[tokio::test]
async fn payment_intent_requires_explicit_pairing_and_cannot_use_direct_execution_or_projection() {
    let fixture = Fixture::new();
    let assembled = fixture
        .build(
            AssemblyScope::AgentLifecycle,
            fixture.inputs(AssemblyScope::AgentLifecycle),
        )
        .unwrap();
    let contract = assembled
        .contracts()
        .into_iter()
        .find(|c| c.tool_name.as_str() == PAYMENT_INTENT_TOOL)
        .unwrap();
    assert_eq!(contract.projection_revision, PAYMENT_LOCATOR_PROJECTION);
    let input = json!({"draft_id":"draft_test","draft_version":1});
    assert!(assembled
        .runtime()
        .direct_preflight_arguments_for_backend(&contract.tool_name, &input, &contract)
        .is_err());
    assert!(assembled
        .runtime()
        .lifecycle_arguments_for_backend(&contract.tool_name, &input, &contract)
        .is_err());
    let context = ExecutionContext {
        task_id: TaskId::parse("task_test").unwrap(),
        node_id: NodeId::parse("payment").unwrap(),
        operation_id: OperationId::parse("payment_test").unwrap(),
        authenticated_user_id: UserId::parse("user_001").unwrap(),
        plan_version: 1,
        idempotency_key: None,
    };
    let error = crate::payment_tool::PaymentIntentTool
        .execute(&context, ToolExecutionAuthorization::ReadOnly, input)
        .await
        .unwrap_err();
    assert_eq!(error.reason_code.as_str(), "PAYMENT_DISPATCH_REQUIRED");
    for change in 0..5 {
        let mut inputs = fixture.inputs(AssemblyScope::AgentLifecycle);
        match change {
            0 => inputs.contracts[0].projection_revision = "bounded-direct-input-v2".into(),
            1 => inputs.executions[0].1 = ExecutionPath::ReadOnly,
            2 => inputs.policies = DomainPolicyRegistry::new(),
            3 => {
                let mut profiles = inputs.catalog.profiles();
                profiles
                    .iter_mut()
                    .find(|p| p.tool_name.as_str() == PAYMENT_INTENT_TOOL)
                    .unwrap()
                    .minimum_level = SecurityLevel::Green;
                inputs.catalog = ToolSecurityCatalog::new();
                for profile in profiles {
                    inputs.catalog.register(profile).unwrap();
                }
            }
            4 => inputs.contracts[0].tool_schema_version += 1,
            _ => unreachable!(),
        }
        assert!(
            fixture
                .build(AssemblyScope::AgentLifecycle, inputs)
                .is_err(),
            "case {change}"
        );
    }
}

#[test]
fn missing_policy_profile_input_or_execution_prevents_publication() {
    let fixture = Fixture::new();
    for missing in ["policy", "profile", "input", "execution"] {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        match missing {
            "policy" => inputs.policies = DomainPolicyRegistry::new(),
            "profile" => inputs.catalog = ToolSecurityCatalog::new(),
            "input" => inputs.contracts.clear(),
            "execution" => inputs.executions.clear(),
            _ => unreachable!(),
        }
        assert!(
            fixture.build(AssemblyScope::Balance, inputs).is_err(),
            "{missing}"
        );
    }
}

#[test]
fn duplicate_inputs_and_execution_paths_prevent_publication() {
    let fixture = Fixture::new();
    let mut inputs = fixture.inputs(AssemblyScope::AgentLifecycle);
    inputs.contracts[1] = inputs.contracts[0].clone();
    assert!(fixture
        .build(AssemblyScope::AgentLifecycle, inputs)
        .is_err());
    let mut inputs = fixture.inputs(AssemblyScope::AgentLifecycle);
    inputs.executions[1] = inputs.executions[0].clone();
    assert!(fixture
        .build(AssemblyScope::AgentLifecycle, inputs)
        .is_err());
    let mut inputs = fixture.inputs(AssemblyScope::Balance);
    assert!(inputs
        .tools
        .register(Arc::new(GetAccountBalanceTool::new(fixture.bank.clone())))
        .is_err());
    assert!(inputs
        .policies
        .register(
            Arc::new(GetAccountBalancePolicy::new(fixture.bank.clone())),
            PolicyVersion::parse(BALANCE_POLICY_VERSION).unwrap(),
            BANK_QUERY_SCHEMA_VERSION
        )
        .is_err());
    let profile = inputs.catalog.profiles()[0].clone();
    assert!(inputs.catalog.register(profile).is_err());
}

#[test]
fn mismatched_input_names_and_versions_prevent_publication() {
    let fixture = Fixture::new();
    for change in 0..3 {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        match change {
            0 => inputs.contracts[0].tool_name = ToolName::parse(FIND_PAYEES).unwrap(),
            1 => inputs.contracts[0].tool_schema_version += 1,
            2 => {
                inputs.contracts[0].policy_version = PolicyVersion::parse("unapproved-v2").unwrap()
            }
            _ => unreachable!(),
        }
        assert!(fixture.build(AssemblyScope::Balance, inputs).is_err());
    }
}

#[test]
fn mismatched_policy_and_profile_bindings_prevent_publication() {
    let fixture = Fixture::new();
    for change in 0..6 {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        if change < 3 {
            inputs.policies = DomainPolicyRegistry::new();
            let policy: Arc<dyn DomainPolicy> = if change == 0 {
                Arc::new(FindPayeesPolicy::new(fixture.bank.clone()))
            } else {
                Arc::new(GetAccountBalancePolicy::new(fixture.bank.clone()))
            };
            inputs
                .policies
                .register(
                    policy,
                    PolicyVersion::parse(if change == 1 {
                        "unapproved-v2"
                    } else {
                        BALANCE_POLICY_VERSION
                    })
                    .unwrap(),
                    if change == 2 { 2 } else { 1 },
                )
                .unwrap();
        } else {
            let mut profile = inputs.catalog.profiles()[0].clone();
            match change {
                3 => profile.tool_name = ToolName::parse(FIND_PAYEES).unwrap(),
                4 => profile.policy_version = PolicyVersion::parse("unapproved-v2").unwrap(),
                5 => profile.tool_schema_version += 1,
                _ => unreachable!(),
            }
            inputs.catalog = ToolSecurityCatalog::new();
            inputs.catalog.register(profile).unwrap();
        }
        assert!(fixture.build(AssemblyScope::Balance, inputs).is_err());
    }
}

#[test]
fn unsupported_or_wrong_scope_projection_prevents_publication() {
    let fixture = Fixture::new();
    for projection in [
        "unknown-projection",
        "bounded-direct-input-v2",
        "existing-create-direct-v1",
        "",
    ] {
        let mut inputs = fixture.inputs(AssemblyScope::Balance);
        inputs.contracts[0].projection_revision = projection.into();
        assert!(fixture.build(AssemblyScope::Balance, inputs).is_err());
    }
    assert!(
        AssemblyInputs::new(AssemblyScope::AgentLifecycle, fixture.bank.clone(), None).is_err()
    );
    let assembled = fixture
        .build(
            AssemblyScope::AgentLifecycle,
            fixture.inputs(AssemblyScope::AgentLifecycle),
        )
        .unwrap();
    let mut modify = assembled
        .contracts()
        .into_iter()
        .find(|c| c.tool_name.as_str() == MockDraftAction::Modify.tool_name())
        .unwrap();
    assert!(assembled
        .runtime()
        .validate_input_projection_for_backend(&modify)
        .is_ok());
    modify.projection_revision = "existing-create-direct-v1".into();
    assert!(assembled
        .runtime()
        .validate_input_projection_for_backend(&modify)
        .is_err());
}

#[test]
fn execution_configuration_cannot_reclassify_reads_drafts_or_funds() {
    let fixture = Fixture::new();
    for (index, path) in [
        (0, ExecutionPath::ReadOnly),
        (0, ExecutionPath::MockDraft(MockDraftAction::Cancel)),
        (3, ExecutionPath::MockDraft(MockDraftAction::Create)),
    ] {
        let mut inputs = fixture.inputs(AssemblyScope::AgentLifecycle);
        inputs.executions[index].1 = path;
        assert!(fixture
            .build(AssemblyScope::AgentLifecycle, inputs)
            .is_err());
    }
    for name in ["submit_transfer", "unknown_tool"] {
        for path in [
            ExecutionPath::ReadOnly,
            ExecutionPath::MockDraft(MockDraftAction::Create),
        ] {
            let mut inputs = fixture.inputs(AssemblyScope::Balance);
            inputs.executions[0] = (ToolName::parse(name).unwrap(), path);
            inputs.contracts[0].tool_name = ToolName::parse(name).unwrap();
            assert!(fixture.build(AssemblyScope::Balance, inputs).is_err());
        }
    }
    // An actual delivered, otherwise valid adapter cannot enter an unapproved scope.
    assert!(fixture
        .build(
            AssemblyScope::Balance,
            fixture.inputs(AssemblyScope::LegacyDraft)
        )
        .is_err());
}

#[test]
fn registration_hash_binds_actual_profiles_inputs_paths_and_assembly_source() {
    let fixture = Fixture::new();
    let baseline = fixture
        .build(
            AssemblyScope::Balance,
            fixture.inputs(AssemblyScope::Balance),
        )
        .unwrap();
    let second = fixture
        .build(
            AssemblyScope::Balance,
            fixture.inputs(AssemblyScope::Balance),
        )
        .unwrap();
    assert_eq!(hash(&baseline), hash(&second));
    let mut inputs = fixture.inputs(AssemblyScope::Balance);
    let mut profile = inputs.catalog.profiles()[0].clone();
    profile.minimum_level = SecurityLevel::Yellow;
    inputs.catalog = ToolSecurityCatalog::new();
    inputs.catalog.register(profile).unwrap();
    let changed = fixture.build(AssemblyScope::Balance, inputs).unwrap();
    assert_ne!(hash(&baseline), hash(&changed));
    let metadata = baseline
        .runtime()
        .preflight_source_metadata_for_backend()
        .unwrap();
    let manifest = &metadata["backend_registration"];
    assert_eq!(manifest["inputs"], json!(baseline.contracts()));
    assert_eq!(
        manifest["execution_paths"],
        json!([[GET_ACCOUNT_BALANCE, "READ_ONLY"]])
    );
    assert_eq!(
        manifest["assembly_source_digest"].as_str().unwrap().len(),
        64
    );
    assert_eq!(
        manifest["dispatch_source_digest"].as_str().unwrap().len(),
        64
    );
}
