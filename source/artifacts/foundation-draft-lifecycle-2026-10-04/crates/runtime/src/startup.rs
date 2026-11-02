use crate::security::DeterministicSecurityKernel;
use crate::{
    AuditRecorder, DomainPolicyRegistry, MockDraftAuthority, Runtime, SessionResolver,
    ToolRegistry, ToolSecurityCatalog,
};
use safeflow_contracts::{PolicyVersion, ToolName};
use std::fmt;
use std::sync::Arc;

pub fn validate_tool_policy_bindings(
    tools: &ToolRegistry,
    policies: &DomainPolicyRegistry,
    catalog: &ToolSecurityCatalog,
) -> Result<(), StartupValidationError> {
    for tool in tools.definitions() {
        let policy = policies
            .definition(&tool.name)
            .ok_or_else(|| StartupValidationError::MissingPolicy(tool.name.clone()))?;
        let profile = catalog
            .get(&tool.name)
            .ok_or_else(|| StartupValidationError::MissingSecurityProfile(tool.name.clone()))?;

        if policy.compatible_tool_schema_version != tool.schema_version {
            return Err(StartupValidationError::PolicySchemaMismatch {
                tool_name: tool.name,
                tool_schema_version: tool.schema_version,
                policy_schema_version: policy.compatible_tool_schema_version,
            });
        }
        if profile.tool_schema_version != tool.schema_version {
            return Err(StartupValidationError::CatalogSchemaMismatch {
                tool_name: tool.name,
                tool_schema_version: tool.schema_version,
                catalog_schema_version: profile.tool_schema_version,
            });
        }
        if profile.policy_version != policy.policy_version {
            return Err(StartupValidationError::PolicyVersionMismatch {
                tool_name: tool.name,
                registered: policy.policy_version.clone(),
                approved: profile.policy_version.clone(),
            });
        }
    }

    for policy in policies.definitions() {
        if tools.get(&policy.tool_name).is_none() {
            return Err(StartupValidationError::OrphanPolicy(policy.tool_name));
        }
    }
    for profile in catalog.profiles() {
        if tools.get(&profile.tool_name).is_none() {
            return Err(StartupValidationError::OrphanSecurityProfile(
                profile.tool_name,
            ));
        }
    }
    for tool in catalog.risk.bound_tools() {
        if tools.get(tool).is_none() || catalog.get(tool).is_none() {
            return Err(StartupValidationError::OrphanSecurityProfile(tool.clone()));
        }
    }
    Ok(())
}

pub struct RuntimeBootstrap;

impl RuntimeBootstrap {
    pub fn build_mock_draft(
        tools: ToolRegistry,
        policies: DomainPolicyRegistry,
        catalog: ToolSecurityCatalog,
        sessions: Arc<dyn SessionResolver>,
        audit: Arc<dyn AuditRecorder>,
        authority: Arc<MockDraftAuthority>,
    ) -> Result<RuntimeAssembly, StartupValidationError> {
        if !Arc::ptr_eq(&sessions, &authority.sessions) {
            return Err(StartupValidationError::MockDraftSessionResolverMismatch);
        }
        let mut assembly = Self::build(tools, policies, catalog, sessions, audit)?;
        assembly.runtime.mock_drafts = Some(authority);
        Ok(assembly)
    }

    pub fn build(
        tools: ToolRegistry,
        policies: DomainPolicyRegistry,
        catalog: ToolSecurityCatalog,
        sessions: Arc<dyn SessionResolver>,
        audit: Arc<dyn AuditRecorder>,
    ) -> Result<RuntimeAssembly, StartupValidationError> {
        validate_tool_policy_bindings(&tools, &policies, &catalog)?;

        let tools = Arc::new(tools);
        let policies = Arc::new(policies);
        let catalog = Arc::new(catalog);
        let security = Arc::new(DeterministicSecurityKernel::new(
            tools.clone(),
            policies.clone(),
            catalog.clone(),
            sessions.clone(),
        ));
        let mut runtime = Runtime::new(
            tools.clone(),
            security.clone(),
            audit,
            sessions,
            policies,
            catalog,
        );
        runtime.final_kernel = Some(security);
        Ok(RuntimeAssembly { runtime, tools })
    }
}

pub struct RuntimeAssembly {
    runtime: Runtime,
    tools: Arc<ToolRegistry>,
}

impl RuntimeAssembly {
    /// Explicit backend-only policy adoption; adds no model Tool or execution path.
    pub fn with_final_payment_policy_for_backend(
        mut self,
        registration: crate::BackendFinalPolicyRegistration,
    ) -> Result<Self, safeflow_contracts::SecurityError> {
        self.runtime.validate_final_registration(&registration)?;
        if self.runtime.final_payment.is_some() {
            return Err(crate::security::contract_security_error(
                "duplicate final payment registration",
            ));
        }
        self.runtime.final_payment = Some(registration);
        Ok(self)
    }
    /// Bind server assembly provenance before publishing the Runtime. This grants no authority.
    pub fn with_registration_metadata_for_backend(mut self, metadata: serde_json::Value) -> Self {
        self.runtime.backend_registration = Some(metadata);
        self
    }

    pub fn runtime(&self) -> &Runtime {
        &self.runtime
    }

    pub fn tool_registry(&self) -> Arc<ToolRegistry> {
        self.tools.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupValidationError {
    MockDraftSessionResolverMismatch,
    MissingPolicy(ToolName),
    MissingSecurityProfile(ToolName),
    OrphanPolicy(ToolName),
    OrphanSecurityProfile(ToolName),
    PolicySchemaMismatch {
        tool_name: ToolName,
        tool_schema_version: u32,
        policy_schema_version: u32,
    },
    CatalogSchemaMismatch {
        tool_name: ToolName,
        tool_schema_version: u32,
        catalog_schema_version: u32,
    },
    PolicyVersionMismatch {
        tool_name: ToolName,
        registered: PolicyVersion,
        approved: PolicyVersion,
    },
}

impl fmt::Display for StartupValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MockDraftSessionResolverMismatch => formatter.write_str("mock draft authority must share Runtime's trusted session resolver"),
            Self::MissingPolicy(name) => write!(formatter, "tool has no domain policy: {name}"),
            Self::MissingSecurityProfile(name) => {
                write!(formatter, "tool has no minimum security profile: {name}")
            }
            Self::OrphanPolicy(name) => {
                write!(formatter, "domain policy points to an unknown tool: {name}")
            }
            Self::OrphanSecurityProfile(name) => {
                write!(formatter, "security profile points to an unknown tool: {name}")
            }
            Self::PolicySchemaMismatch {
                tool_name,
                tool_schema_version,
                policy_schema_version,
            } => write!(
                formatter,
                "policy schema mismatch for {tool_name}: tool={tool_schema_version}, policy={policy_schema_version}"
            ),
            Self::CatalogSchemaMismatch {
                tool_name,
                tool_schema_version,
                catalog_schema_version,
            } => write!(
                formatter,
                "catalog schema mismatch for {tool_name}: tool={tool_schema_version}, catalog={catalog_schema_version}"
            ),
            Self::PolicyVersionMismatch {
                tool_name,
                registered,
                approved,
            } => write!(
                formatter,
                "policy version mismatch for {tool_name}: registered={registered}, approved={approved}"
            ),
        }
    }
}

impl std::error::Error for StartupValidationError {}
