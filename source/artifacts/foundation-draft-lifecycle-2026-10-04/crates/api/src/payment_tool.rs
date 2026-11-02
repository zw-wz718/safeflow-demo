//! Trusted Agent dispatch facade. Full facts/confirmation live in PaymentPreparationService.
use safeflow_contracts::*;
use serde_json::Value;

pub(crate) struct PaymentIntentTool;
impl AgentTool for PaymentIntentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(ToolName::parse(PAYMENT_INTENT_TOOL).unwrap(),
            "Request payment of the current saved draft. Planning and persistent independent confirmation are available; funds execution is disabled. The Agent dispatcher returns the full payment display and safe next actions; it never reports a submitted payment.",
            PaymentDraftLocator::schema(), 1, true, "PAYMENT_INTENT").unwrap()
    }
    fn execute<'a>(
        &'a self,
        _: &'a ExecutionContext,
        _: ToolExecutionAuthorization<'a>,
        _: Value,
    ) -> ContractFuture<'a, Result<ToolResult, ToolError>> {
        Box::pin(async {
            Err(ToolError {
                reason_code: ReasonCode::parse("PAYMENT_DISPATCH_REQUIRED").unwrap(),
                message: "Use the trusted Agent payment dispatcher and independent confirmation"
                    .into(),
            })
        })
    }
}

pub(crate) struct PaymentIntentPolicy(pub ToolName);
impl DomainPolicy for PaymentIntentPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.0
    }
    fn evaluate<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        // Locator inputs cannot stand in for the complete facts consumed by FundsBusinessPolicy.
        Box::pin(async {
            Err(SecurityError { reason_code: ReasonCode::parse("PAYMENT_PROJECTION_REQUIRED").unwrap(), message: "Full saved payment facts and independent confirmation must be resolved by the trusted dispatcher".into() })
        })
    }
    fn verify_execution<'a>(
        &'a self,
        _: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async {
            Err(SecurityError {
                reason_code: ReasonCode::parse("PAYMENT_DISPATCH_REQUIRED").unwrap(),
                message: "No funds execution capability is installed".into(),
            })
        })
    }
}
