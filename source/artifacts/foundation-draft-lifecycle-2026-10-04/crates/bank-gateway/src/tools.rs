use crate::{BankError, BankGateway, FindPayeesInput, GetAccountBalanceInput};
use safeflow_contracts::{
    AgentTool, ContractFuture, ExecutionContext, ReasonCode, ToolDefinition, ToolError,
    ToolExecutionAuthorization, ToolName, ToolResult, ToolResultRef, ToolResultStatus,
};
use serde_json::{json, Value};
use std::sync::Arc;

pub const GET_ACCOUNT_BALANCE: &str = "get_account_balance";
pub const FIND_PAYEES: &str = "find_payees";
pub const BANK_QUERY_SCHEMA_VERSION: u32 = 1;
pub const BALANCE_POLICY_VERSION: &str = "balance-policy-v1";
pub const PAYEES_POLICY_VERSION: &str = "payees-policy-v1";

pub struct GetAccountBalanceTool {
    gateway: Arc<dyn BankGateway>,
}

impl GetAccountBalanceTool {
    pub fn new(gateway: Arc<dyn BankGateway>) -> Self {
        Self { gateway }
    }
}

impl AgentTool for GetAccountBalanceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            tool_name(GET_ACCOUNT_BALANCE),
            "Read a masked account and balance owned by the authenticated user",
            json!({
                "type": "object",
                "properties": {"account_id": {
                    "type": "string", "minLength": 1,
                    "maxLength": crate::MAX_RESOURCE_ID_LENGTH,
                    "pattern": "^[^\\s\\u0000-\\u001F\\u007F-\\u009F]+$"
                }},
                "required": ["account_id"],
                "additionalProperties": false
            }),
            BANK_QUERY_SCHEMA_VERSION,
            false,
            "READ_BALANCE",
        )
        .expect("static bank query definition")
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            require_read_only(authorization)?;
            let input: GetAccountBalanceInput = serde_json::from_value(arguments)
                .map_err(|_| tool_error(BankError::ValidationError))?;
            input.validate().map_err(tool_error)?;
            let output = self
                .gateway
                .get_account_balance(&context.authenticated_user_id, &input.account_id)
                .await
                .map_err(tool_error)?;
            output.validate_for(&input.account_id).map_err(tool_error)?;
            result(GET_ACCOUNT_BALANCE, context, &output)
        })
    }
}

pub struct FindPayeesTool {
    gateway: Arc<dyn BankGateway>,
}

impl FindPayeesTool {
    pub fn new(gateway: Arc<dyn BankGateway>) -> Self {
        Self { gateway }
    }
}

impl AgentTool for FindPayeesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(
            tool_name(FIND_PAYEES),
            "Find saved payees of the authenticated user, preserving all matching candidates",
            json!({
                "type": "object",
                "properties": {"query": {
                    "type": "string", "minLength": 1,
                    "maxLength": crate::MAX_QUERY_LENGTH,
                    "pattern": "^\\S([^\\u0000-\\u001F\\u007F-\\u009F]*\\S)?$"
                }},
                "required": ["query"],
                "additionalProperties": false
            }),
            BANK_QUERY_SCHEMA_VERSION,
            false,
            "READ_PAYEES",
        )
        .expect("static bank query definition")
    }

    fn execute<'a>(
        &'a self,
        context: &'a ExecutionContext,
        authorization: ToolExecutionAuthorization<'a>,
        arguments: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            require_read_only(authorization)?;
            let input: FindPayeesInput = serde_json::from_value(arguments)
                .map_err(|_| tool_error(BankError::ValidationError))?;
            input.validate().map_err(tool_error)?;
            let output = self
                .gateway
                .find_payees(&context.authenticated_user_id, &input.query)
                .await
                .map_err(tool_error)?;
            output.validate().map_err(tool_error)?;
            result(FIND_PAYEES, context, &output)
        })
    }
}

pub(crate) fn tool_name(value: &str) -> ToolName {
    ToolName::parse(value).expect("static bank query tool name")
}

pub(crate) fn result_ref(context_operation_id: &safeflow_contracts::OperationId) -> ToolResultRef {
    ToolResultRef::parse(format!("read_result_{context_operation_id}"))
        .expect("operation ID is nonempty")
}

fn result(
    name: &str,
    context: &ExecutionContext,
    output: &impl serde::Serialize,
) -> Result<ToolResult, ToolError> {
    Ok(ToolResult::new(
        tool_name(name),
        context.operation_id.clone(),
        result_ref(&context.operation_id),
        ToolResultStatus::Succeeded,
        serde_json::to_value(output).map_err(|_| tool_error(BankError::InvalidResponse))?,
    ))
}

fn require_read_only(authorization: ToolExecutionAuthorization<'_>) -> Result<(), ToolError> {
    if !matches!(authorization, ToolExecutionAuthorization::ReadOnly) {
        return Err(ToolError {
            reason_code: ReasonCode::parse("READ_ONLY_AUTHORIZATION_REQUIRED")
                .expect("static code"),
            message: "bank query tools require read-only authorization".to_owned(),
        });
    }
    Ok(())
}

fn tool_error(error: BankError) -> ToolError {
    ToolError {
        reason_code: ReasonCode::parse(error.reason_code()).expect("static bank error code"),
        message: error.to_string(),
    }
}
