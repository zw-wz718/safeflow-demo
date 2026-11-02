use crate::session::SimulationSessions;
use safeflow_bank_gateway::{
    BankGateway, FindPayeesPolicy, FindPayeesTool, GetAccountBalancePolicy, GetAccountBalanceTool,
    BALANCE_POLICY_VERSION, BANK_QUERY_SCHEMA_VERSION, FIND_PAYEES, GET_ACCOUNT_BALANCE,
    PAYEES_POLICY_VERSION,
};
use safeflow_contracts::*;
use safeflow_runtime::*;
use safeflow_transfer_integration::{runtime_tools::*, SystemClock};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;

/// Trusted startup approval, never deserialized from HTTP or model output.
/// The caller supplies the reviewed implementation revision and central approval explicitly.
#[derive(Clone)]
pub struct ReadOnlyToolRegistration {
    pub tool: Arc<dyn AgentTool>,
    pub policy: Arc<dyn DomainPolicy>,
    pub policy_version: PolicyVersion,
    pub compatible_schema_version: u32,
    pub profile: ToolSecurityProfile,
    pub input: PreflightInputContract,
    pub approved_action: String,
    pub implementation_revision: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionPath {
    ReadOnly,
    MockDraft(MockDraftAction),
    PaymentIntent,
}
impl serde::Serialize for ExecutionPath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(match self {
            Self::ReadOnly => "READ_ONLY",
            Self::PaymentIntent => "PAYMENT_INTENT",
            Self::MockDraft(action) => action.action(),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssemblyScope {
    Balance,
    LegacyDraft,
    AgentLifecycle,
}

// Only these delivered adapters have an approved API execution path. No plugin defaults.
#[derive(Clone, Copy)]
enum ApprovedTool {
    Balance,
    Payees,
    Create,
    Modify,
    Cancel,
    Payment,
}
const APPROVED: [ApprovedTool; 6] = [
    ApprovedTool::Payment,
    ApprovedTool::Create,
    ApprovedTool::Modify,
    ApprovedTool::Cancel,
    ApprovedTool::Balance,
    ApprovedTool::Payees,
];
struct Approval {
    name: &'static str,
    schema: u32,
    policy: &'static str,
    minimum_level: SecurityLevel,
    execution: ExecutionPath,
    action: &'static str,
}
impl ApprovedTool {
    fn approval(self) -> Approval {
        let (name, schema, policy, minimum_level, execution, action) = match self {
            Self::Payment => (
                PAYMENT_INTENT_TOOL,
                1,
                PAYMENT_INTENT_POLICY,
                SecurityLevel::Yellow,
                ExecutionPath::PaymentIntent,
                "PAYMENT_INTENT",
            ),
            Self::Balance => (
                GET_ACCOUNT_BALANCE,
                BANK_QUERY_SCHEMA_VERSION,
                BALANCE_POLICY_VERSION,
                SecurityLevel::Green,
                ExecutionPath::ReadOnly,
                "READ_BALANCE",
            ),
            Self::Payees => (
                FIND_PAYEES,
                BANK_QUERY_SCHEMA_VERSION,
                PAYEES_POLICY_VERSION,
                SecurityLevel::Green,
                ExecutionPath::ReadOnly,
                "READ_PAYEES",
            ),
            Self::Create => (
                MockDraftAction::Create.tool_name(),
                MOCK_DRAFT_SCHEMA_VERSION,
                MOCK_DRAFT_POLICY_VERSION,
                SecurityLevel::Green,
                ExecutionPath::MockDraft(MockDraftAction::Create),
                MockDraftAction::Create.action(),
            ),
            Self::Modify => (
                MockDraftAction::Modify.tool_name(),
                MOCK_DRAFT_SCHEMA_VERSION,
                MOCK_DRAFT_LIFECYCLE_POLICY_VERSION,
                SecurityLevel::Green,
                ExecutionPath::MockDraft(MockDraftAction::Modify),
                MockDraftAction::Modify.action(),
            ),
            Self::Cancel => (
                MockDraftAction::Cancel.tool_name(),
                MOCK_DRAFT_SCHEMA_VERSION,
                MOCK_DRAFT_LIFECYCLE_POLICY_VERSION,
                SecurityLevel::Green,
                ExecutionPath::MockDraft(MockDraftAction::Cancel),
                MockDraftAction::Cancel.action(),
            ),
        };
        Approval {
            name,
            schema,
            policy,
            minimum_level,
            execution,
            action,
        }
    }
}
impl AssemblyScope {
    fn tools(self) -> &'static [ApprovedTool] {
        match self {
            Self::Balance => &[ApprovedTool::Balance],
            Self::LegacyDraft => &[ApprovedTool::Create],
            Self::AgentLifecycle => &APPROVED,
        }
    }
    fn projection(self) -> &'static str {
        match self {
            Self::Balance => "bank-direct-input-v1",
            Self::LegacyDraft => "existing-create-direct-v1",
            Self::AgentLifecycle => "bounded-direct-input-v2",
        }
    }
    pub(crate) fn contracts(self) -> Vec<PreflightInputContract> {
        self.tools()
            .iter()
            .map(|tool| {
                let approval = tool.approval();
                PreflightInputContract {
                    tool_name: ToolName::parse(approval.name).expect("approved tool"),
                    tool_schema_version: approval.schema,
                    policy_version: PolicyVersion::parse(approval.policy).expect("approved policy"),
                    projection_revision: if approval.execution == ExecutionPath::PaymentIntent {
                        PAYMENT_LOCATOR_PROJECTION
                    } else {
                        self.projection()
                    }
                    .into(),
                }
            })
            .collect()
    }
    pub(crate) fn draft_contracts(self) -> Vec<PreflightInputContract> {
        self.contracts()
            .into_iter()
            .filter(|c| {
                self.tools().iter().any(|t| {
                    let a = t.approval();
                    a.name == c.tool_name.as_str()
                        && matches!(a.execution, ExecutionPath::MockDraft(_))
                })
            })
            .collect()
    }
}

pub(crate) struct CheckedAssembly {
    assembly: RuntimeAssembly,
    contracts: Vec<PreflightInputContract>,
    executions: HashMap<ToolName, ExecutionPath>,
    definitions: Vec<ToolDefinition>,
}
impl CheckedAssembly {
    pub(crate) fn with_final_payment(
        mut self,
        profile: ToolSecurityProfile,
    ) -> Result<Self, &'static str> {
        let registration = safeflow_storage::payment_final_registration_for_backend(profile)
            .map_err(|_| "invalid final payment registration")?;
        self.assembly = self
            .assembly
            .with_final_payment_policy_for_backend(registration)
            .map_err(|_| "final payment registration unavailable")?;
        Ok(self)
    }
    pub(crate) fn runtime(&self) -> &Runtime {
        self.assembly.runtime()
    }
    pub(crate) fn contracts(&self) -> Vec<PreflightInputContract> {
        self.contracts.clone()
    }
    pub(crate) fn draft_contracts(&self) -> Vec<PreflightInputContract> {
        self.contracts
            .iter()
            .filter(|c| {
                matches!(
                    self.execution(&c.tool_name),
                    Some(ExecutionPath::MockDraft(_))
                )
            })
            .cloned()
            .collect()
    }
    pub(crate) fn execution(&self, name: &ToolName) -> Option<ExecutionPath> {
        self.executions.get(name).copied()
    }
    pub(crate) fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }
}

struct AssemblyInputs {
    tools: ToolRegistry,
    policies: DomainPolicyRegistry,
    catalog: ToolSecurityCatalog,
    contracts: Vec<PreflightInputContract>,
    executions: Vec<(ToolName, ExecutionPath)>,
    extensions: Vec<(ToolName, String, String)>,
}
type DraftDependencies = (Arc<MockDraftStore<SystemClock>>, Arc<MockDraftAuthority>);

impl AssemblyInputs {
    fn new(
        scope: AssemblyScope,
        bank: Arc<dyn BankGateway>,
        draft: Option<&DraftDependencies>,
    ) -> Result<Self, &'static str> {
        let mut inputs = Self {
            tools: ToolRegistry::new(),
            policies: DomainPolicyRegistry::new(),
            catalog: ToolSecurityCatalog::new(),
            contracts: scope.contracts(),
            executions: vec![],
            extensions: vec![],
        };
        for approved in scope.tools() {
            let a = approved.approval();
            let (tool, policy): (Arc<dyn AgentTool>, Arc<dyn DomainPolicy>) = match approved {
                ApprovedTool::Payment => (
                    Arc::new(crate::payment_tool::PaymentIntentTool),
                    Arc::new(crate::payment_tool::PaymentIntentPolicy(
                        ToolName::parse(PAYMENT_INTENT_TOOL).unwrap(),
                    )),
                ),
                ApprovedTool::Balance => (
                    Arc::new(GetAccountBalanceTool::new(bank.clone())),
                    Arc::new(GetAccountBalancePolicy::new(bank.clone())),
                ),
                ApprovedTool::Payees => (
                    Arc::new(FindPayeesTool::new(bank.clone())),
                    Arc::new(FindPayeesPolicy::new(bank.clone())),
                ),
                ApprovedTool::Create | ApprovedTool::Modify | ApprovedTool::Cancel => {
                    let (store, authority) =
                        draft.ok_or("draft execution dependencies unavailable")?;
                    match approved {
                        ApprovedTool::Create => (
                            Arc::new(CreateMockDraftTool::new(store.clone(), authority.clone())),
                            Arc::new(CreateMockDraftPolicy::new(store.clone())),
                        ),
                        ApprovedTool::Modify => (
                            Arc::new(MockDraftLifecycleTool::modify(
                                store.clone(),
                                authority.clone(),
                            )),
                            Arc::new(MockDraftLifecyclePolicy::modify(store.clone())),
                        ),
                        ApprovedTool::Cancel => (
                            Arc::new(MockDraftLifecycleTool::cancel(
                                store.clone(),
                                authority.clone(),
                            )),
                            Arc::new(MockDraftLifecyclePolicy::cancel(store.clone())),
                        ),
                        _ => unreachable!(),
                    }
                }
            };
            let name = ToolName::parse(a.name).expect("approved tool");
            if tool.definition().name != name || policy.tool_name() != &name {
                return Err("adapter name differs from approved binding");
            }
            inputs
                .tools
                .register(tool)
                .map_err(|_| "duplicate or invalid Tool")?;
            inputs
                .policies
                .register(
                    policy,
                    PolicyVersion::parse(a.policy).expect("approved policy"),
                    a.schema,
                )
                .map_err(|_| "duplicate or invalid Policy")?;
            inputs
                .catalog
                .register(ToolSecurityProfile {
                    tool_name: name.clone(),
                    tool_schema_version: a.schema,
                    policy_version: PolicyVersion::parse(a.policy).expect("approved policy"),
                    minimum_level: a.minimum_level,
                })
                .map_err(|_| "duplicate or invalid Profile")?;
            inputs.executions.push((name, a.execution));
        }
        Ok(inputs)
    }
    fn extend(&mut self, registrations: &[ReadOnlyToolRegistration]) -> Result<(), &'static str> {
        for registration in registrations {
            let definition = registration.tool.definition();
            if definition.has_side_effect
                || definition.action != registration.approved_action
                || registration.approved_action.is_empty()
                || registration.implementation_revision.trim().is_empty()
                || registration.implementation_revision.len() > 256
                || definition.name != *registration.policy.tool_name()
                || definition.name != registration.profile.tool_name
                || definition.name != registration.input.tool_name
                || registration.input.projection_revision != "bounded-direct-input-v2"
            {
                return Err("invalid explicit read-only approval");
            }
            self.tools
                .register(registration.tool.clone())
                .map_err(|_| "duplicate or invalid Tool")?;
            self.policies
                .register(
                    registration.policy.clone(),
                    registration.policy_version.clone(),
                    registration.compatible_schema_version,
                )
                .map_err(|_| "duplicate or invalid Policy")?;
            self.catalog
                .register(registration.profile.clone())
                .map_err(|_| "duplicate or invalid Profile")?;
            self.contracts.push(registration.input.clone());
            self.executions
                .push((definition.name.clone(), ExecutionPath::ReadOnly));
            self.extensions.push((
                definition.name,
                registration.approved_action.clone(),
                registration.implementation_revision.clone(),
            ));
        }
        if !registrations.is_empty() {
            for contract in &mut self.contracts {
                if contract.tool_name.as_str() != PAYMENT_INTENT_TOOL {
                    contract.projection_revision = "bounded-direct-input-v2".into();
                }
            }
        }
        Ok(())
    }
    fn build(
        self,
        scope: AssemblyScope,
        sessions: Arc<SimulationSessions>,
        audit: Arc<InMemoryAuditRecorder>,
        authority: Option<Arc<MockDraftAuthority>>,
    ) -> Result<CheckedAssembly, &'static str> {
        let mut executions = HashMap::new();
        for (name, execution) in &self.executions {
            if executions.insert(name.clone(), *execution).is_some() {
                return Err("duplicate execution path");
            }
        }
        let definitions = self.tools.definitions();
        if definitions.len() != scope.tools().len() + self.extensions.len()
            || self.contracts.len() != definitions.len()
            || executions.len() != definitions.len()
        {
            return Err("incomplete tool, input or execution assembly");
        }
        for definition in &definitions {
            if let Some((_, action, _)) = self
                .extensions
                .iter()
                .find(|(name, _, _)| name == &definition.name)
            {
                if definition.has_side_effect
                    || definition.action != *action
                    || executions.get(&definition.name) != Some(&ExecutionPath::ReadOnly)
                {
                    return Err("extension execution category mismatch");
                }
                continue;
            }
            let a = scope
                .tools()
                .iter()
                .map(|t| t.approval())
                .find(|a| a.name == definition.name.as_str())
                .ok_or("tool has no approved execution path")?;
            if executions.get(&definition.name) != Some(&a.execution)
                || definition.has_side_effect != (a.execution != ExecutionPath::ReadOnly)
                || definition.action != a.action
                || definition.schema_version != a.schema
                || !self.catalog.profiles().iter().any(|profile| {
                    profile.tool_name == definition.name && profile.minimum_level >= a.minimum_level
                })
            {
                return Err("execution category or adapter definition mismatch");
            }
        }
        for input in &self.contracts {
            let projection =
                if executions.get(&input.tool_name) == Some(&ExecutionPath::PaymentIntent) {
                    PAYMENT_LOCATOR_PROJECTION
                } else if self.extensions.is_empty() {
                    scope.projection()
                } else {
                    "bounded-direct-input-v2"
                };
            if input.projection_revision != projection {
                return Err("unsupported input projection");
            }
        }
        let assembly = if scope == AssemblyScope::Balance {
            RuntimeBootstrap::build(self.tools, self.policies, self.catalog, sessions, audit)
        } else {
            RuntimeBootstrap::build_mock_draft(
                self.tools,
                self.policies,
                self.catalog,
                sessions,
                audit,
                authority.ok_or("draft authority unavailable")?,
            )
        }
        .map_err(|_| "Tool/Policy/Profile binding invalid")?;
        for contract in &self.contracts {
            assembly
                .runtime()
                .validate_input_projection_for_backend(contract)
                .map_err(|_| "input schema cannot use approved projection")?;
            if self
                .extensions
                .iter()
                .any(|(name, _, _)| name == &contract.tool_name)
            {
                // Read-only recovery still uses the existing flat direct-input grammar.
                let mut direct = contract.clone();
                direct.projection_revision = "bank-direct-input-v1".into();
                assembly
                    .runtime()
                    .validate_input_projection_for_backend(&direct)
                    .map_err(|_| "read-only recovery cannot use input schema")?;
            }
        }
        let assembly = assembly.with_registration_metadata_for_backend(json!({
            "contract": "api-approved-assembly-v1", "inputs": self.contracts,
            "execution_paths": self.executions,
            "read_only_approvals": self.extensions,
            "assembly_source_digest": format!("{:x}", Sha256::digest(include_str!("assembly.rs").replace("\r\n", "\n").as_bytes())),
            "dispatch_source_digest": format!("{:x}", Sha256::digest(include_str!("http/agent.rs").replace("\r\n", "\n").as_bytes()))
            ,"payment_dispatch_source_digest": format!("{:x}", Sha256::digest(include_str!("payment_tool.rs").replace("\r\n", "\n").as_bytes()))
        }));
        let intake = PlanningIntakeService::for_registered_backend(
            assembly.runtime(),
            None,
            self.contracts.clone(),
        )
        .map_err(|_| "approved input registration invalid")?;
        let approved = intake
            .approved_tools_for_backend()
            .map_err(|_| "approved tool definitions unavailable")?;
        if approved != definitions {
            return Err("model and execution tool sets differ");
        }
        intake
            .registration_hash_for_backend()
            .map_err(|_| "registration provenance unavailable")?;
        Ok(CheckedAssembly {
            assembly,
            contracts: self.contracts,
            executions,
            definitions: approved,
        })
    }
}

pub(crate) fn assemble(
    scope: AssemblyScope,
    sessions: Arc<SimulationSessions>,
    audit: Arc<InMemoryAuditRecorder>,
    bank: Arc<dyn BankGateway>,
    draft: Option<DraftDependencies>,
) -> Result<CheckedAssembly, &'static str> {
    assemble_with_read_only(scope, sessions, audit, bank, draft, &[])
}

pub(crate) fn assemble_with_read_only(
    scope: AssemblyScope,
    sessions: Arc<SimulationSessions>,
    audit: Arc<InMemoryAuditRecorder>,
    bank: Arc<dyn BankGateway>,
    draft: Option<DraftDependencies>,
    registrations: &[ReadOnlyToolRegistration],
) -> Result<CheckedAssembly, &'static str> {
    let mut inputs = AssemblyInputs::new(scope, bank, draft.as_ref())?;
    inputs.extend(registrations)?;
    inputs.build(
        scope,
        sessions,
        audit,
        draft.map(|(_, authority)| authority),
    )
}

#[cfg(test)]
mod tests;

pub(crate) fn proposal(id: &TaskId) -> PlanningResult {
    let node = |value: &str| NodeId::parse(value.to_owned()).expect("fixed node");
    let read = |id, dependency: Option<&str>| PlanNode::Tool {
        id: node(id),
        tool: ToolName::parse(GET_ACCOUNT_BALANCE).expect("fixed tool"),
        arguments: json!({"account_id":"account_001"}),
        depends_on: dependency.into_iter().map(node).collect(),
    };
    PlanningResult::for_plan(
        Understanding {
            intent: "READ_BALANCE".into(),
            entities: json!({}),
            missing_fields: vec![],
            context_refs: vec![],
            decision: UnderstandingDecision::Pass,
        },
        AgentPlan::new(
            id.clone(),
            2,
            vec![
                read("read", None),
                PlanNode::UserDecision {
                    id: node("choose"),
                    depends_on: vec![node("read")],
                },
                read("read_two", Some("choose")),
                PlanNode::UserDecision {
                    id: node("choose_two"),
                    depends_on: vec![node("read_two")],
                },
                read("read_three", Some("choose_two")),
            ],
        )
        .expect("fixed read-only plan"),
    )
}
