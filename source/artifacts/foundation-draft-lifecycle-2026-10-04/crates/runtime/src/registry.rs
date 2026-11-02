use safeflow_contracts::{
    AgentTool, DomainPolicy, PolicyVersion, SecurityLevel, ToolDefinition, ToolName,
};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<ToolName, Arc<dyn AgentTool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn AgentTool>) -> Result<(), RegistrationError> {
        let name = tool.definition().name;
        if self.tools.contains_key(&name) {
            return Err(RegistrationError::DuplicateTool(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn AgentTool>> {
        self.tools.get(name).cloned()
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions: Vec<_> = self.tools.values().map(|tool| tool.definition()).collect();
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        definitions
    }
}

#[derive(Default)]
pub struct DomainPolicyRegistry {
    policies: HashMap<ToolName, RegisteredDomainPolicy>,
}

struct RegisteredDomainPolicy {
    policy: Arc<dyn DomainPolicy>,
    definition: DomainPolicyDefinition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainPolicyDefinition {
    pub tool_name: ToolName,
    pub policy_version: PolicyVersion,
    pub compatible_tool_schema_version: u32,
}

impl DomainPolicyRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &mut self,
        policy: Arc<dyn DomainPolicy>,
        policy_version: PolicyVersion,
        compatible_tool_schema_version: u32,
    ) -> Result<(), RegistrationError> {
        let name = policy.tool_name().clone();
        if self.policies.contains_key(&name) {
            return Err(RegistrationError::DuplicatePolicy(name));
        }
        if compatible_tool_schema_version == 0 {
            return Err(RegistrationError::ZeroSchemaVersion(name));
        }
        self.policies.insert(
            name.clone(),
            RegisteredDomainPolicy {
                policy,
                definition: DomainPolicyDefinition {
                    tool_name: name,
                    policy_version,
                    compatible_tool_schema_version,
                },
            },
        );
        Ok(())
    }

    pub fn get(&self, name: &ToolName) -> Option<Arc<dyn DomainPolicy>> {
        self.policies
            .get(name)
            .map(|registered| registered.policy.clone())
    }

    pub fn definition(&self, name: &ToolName) -> Option<&DomainPolicyDefinition> {
        self.policies
            .get(name)
            .map(|registered| &registered.definition)
    }

    pub fn definitions(&self) -> Vec<DomainPolicyDefinition> {
        let mut definitions: Vec<_> = self
            .policies
            .values()
            .map(|registered| registered.definition.clone())
            .collect();
        definitions.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        definitions
    }
}

#[derive(Default)]
pub struct ToolSecurityCatalog {
    profiles: HashMap<ToolName, ToolSecurityProfile>,
    pub(crate) risk: crate::CentralPolicyCatalog,
}

impl ToolSecurityCatalog {
    pub fn risk_catalog_for_backend(&mut self) -> &mut crate::CentralPolicyCatalog {
        &mut self.risk
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, profile: ToolSecurityProfile) -> Result<(), RegistrationError> {
        if profile.tool_schema_version == 0 {
            return Err(RegistrationError::ZeroSchemaVersion(
                profile.tool_name.clone(),
            ));
        }
        if self.profiles.contains_key(&profile.tool_name) {
            return Err(RegistrationError::DuplicateSecurityProfile(
                profile.tool_name,
            ));
        }
        self.profiles.insert(profile.tool_name.clone(), profile);
        Ok(())
    }

    pub fn get(&self, name: &ToolName) -> Option<&ToolSecurityProfile> {
        self.profiles.get(name)
    }

    pub fn profiles(&self) -> Vec<ToolSecurityProfile> {
        let mut profiles: Vec<_> = self.profiles.values().cloned().collect();
        profiles.sort_by(|left, right| left.tool_name.cmp(&right.tool_name));
        profiles
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolSecurityProfile {
    pub tool_name: ToolName,
    pub tool_schema_version: u32,
    pub policy_version: PolicyVersion,
    pub minimum_level: SecurityLevel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistrationError {
    DuplicateTool(ToolName),
    DuplicatePolicy(ToolName),
    DuplicateSecurityProfile(ToolName),
    ZeroSchemaVersion(ToolName),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTool(name) => write!(formatter, "tool is already registered: {name}"),
            Self::DuplicatePolicy(name) => {
                write!(formatter, "domain policy is already registered: {name}")
            }
            Self::DuplicateSecurityProfile(name) => {
                write!(formatter, "security profile is already registered: {name}")
            }
            Self::ZeroSchemaVersion(name) => {
                write!(
                    formatter,
                    "schema version must be greater than zero for {name}"
                )
            }
        }
    }
}

impl std::error::Error for RegistrationError {}
