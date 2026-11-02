use crate::{
    ContractFuture, ExecutionPermit, IdempotencyKey, MockDraftPermit, NodeId, OperationId,
    ReasonCode, TaskId, ToolName, ToolResultRef, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const AGENT_TOOL_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDefinition {
    pub contract_version: u16,
    pub name: ToolName,
    pub description: String,
    pub input_schema: Value,
    pub schema_version: u32,
    pub has_side_effect: bool,
    pub action: String,
}

impl ToolDefinition {
    pub fn new(
        name: ToolName,
        description: impl Into<String>,
        input_schema: Value,
        schema_version: u32,
        has_side_effect: bool,
        action: impl Into<String>,
    ) -> Result<Self, ToolContractError> {
        let description = description.into();
        let action = action.into();
        if description.trim().is_empty() {
            return Err(ToolContractError::EmptyDescription);
        }
        if schema_version == 0 {
            return Err(ToolContractError::ZeroSchemaVersion);
        }
        if action.trim().is_empty() || action.trim() != action {
            return Err(ToolContractError::InvalidAction);
        }

        Ok(Self {
            contract_version: AGENT_TOOL_CONTRACT_VERSION,
            name,
            description,
            input_schema,
            schema_version,
            has_side_effect,
            action,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionContext {
    pub task_id: TaskId,
    pub node_id: NodeId,
    pub operation_id: OperationId,
    pub authenticated_user_id: UserId,
    pub plan_version: u32,
    pub idempotency_key: Option<IdempotencyKey>,
}

#[derive(Debug)]
pub enum ToolExecutionAuthorization<'a> {
    ReadOnly,
    Write(&'a ExecutionPermit),
    MockDraftWriteV1(&'a MockDraftPermit),
}

impl ToolExecutionAuthorization<'_> {
    pub fn permits_side_effects(&self) -> bool {
        // This v1 helper must not turn a mock storage grant into generic write authority.
        matches!(self, Self::Write(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ToolResultStatus {
    Succeeded,
    Failed,
    Processing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResult {
    pub contract_version: u16,
    pub tool_name: ToolName,
    pub operation_id: OperationId,
    pub result_ref: ToolResultRef,
    pub status: ToolResultStatus,
    pub data: Value,
    /// In-process evidence from an adopted backend adapter. Never accepted from
    /// wire JSON or serialized to clients, audit DTOs or historical result bodies.
    #[serde(skip)]
    pub backend_evidence: Option<Value>,
}

impl ToolResult {
    pub fn new(
        tool_name: ToolName,
        operation_id: OperationId,
        result_ref: ToolResultRef,
        status: ToolResultStatus,
        data: Value,
    ) -> Self {
        Self {
            contract_version: AGENT_TOOL_CONTRACT_VERSION,
            tool_name,
            operation_id,
            result_ref,
            status,
            data,
            backend_evidence: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub reason_code: ReasonCode,
    pub message: String,
}

impl fmt::Display for ToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.reason_code, self.message)
    }
}

impl std::error::Error for ToolError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolContractError {
    EmptyDescription,
    ZeroSchemaVersion,
    InvalidAction,
}

impl fmt::Display for ToolContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDescription => formatter.write_str("tool description must not be empty"),
            Self::ZeroSchemaVersion => {
                formatter.write_str("tool schema_version must be greater than zero")
            }
            Self::InvalidAction => formatter
                .write_str("tool action must not be empty or contain surrounding whitespace"),
        }
    }
}

impl std::error::Error for ToolContractError {}

pub trait AgentTool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>>;
}

#[cfg(test)]
mod backend_evidence_tests {
    use super::*;
    #[test]
    fn backend_evidence_never_roundtrips_on_wire_and_cannot_be_uploaded() {
        let mut result = ToolResult::new(
            ToolName::parse("report").unwrap(),
            OperationId::parse("operation").unwrap(),
            ToolResultRef::parse("result").unwrap(),
            ToolResultStatus::Succeeded,
            serde_json::json!({"display":true}),
        );
        result.backend_evidence =
            Some(serde_json::json!({"private_pages":["not a wire credential"]}));
        let mut wire = serde_json::to_value(&result).unwrap();
        assert!(wire.get("backend_evidence").is_none());
        assert!(serde_json::from_value::<ToolResult>(wire.clone())
            .unwrap()
            .backend_evidence
            .is_none());
        wire["backend_evidence"] = serde_json::json!({"forged":true});
        assert!(serde_json::from_value::<ToolResult>(wire).is_err());
    }
}
