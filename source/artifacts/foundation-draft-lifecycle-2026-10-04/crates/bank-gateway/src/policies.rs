use crate::tools::{result_ref, tool_name};
use crate::{
    BankError, BankGateway, FindPayeesInput, GetAccountBalanceInput, BALANCE_POLICY_VERSION,
    FIND_PAYEES, GET_ACCOUNT_BALANCE, PAYEES_POLICY_VERSION,
};
use safeflow_contracts::{
    CheckId, ContractFuture, DomainPolicy, ExecutionObservation, FindingKind, PolicyVersion,
    PostExecuteContext, ReasonCode, SecurityContext, SecurityError, SecurityFinding, SourceName,
    ToolName,
};
use std::sync::Arc;

pub struct GetAccountBalancePolicy {
    name: ToolName,
    gateway: Arc<dyn BankGateway>,
}

impl GetAccountBalancePolicy {
    pub fn new(gateway: Arc<dyn BankGateway>) -> Self {
        Self {
            name: tool_name(GET_ACCOUNT_BALANCE),
            gateway,
        }
    }
}

impl DomainPolicy for GetAccountBalancePolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let validation = async {
                if context.tool_name != self.name {
                    return Err(BankError::ValidationError);
                }
                let input: GetAccountBalanceInput =
                    serde_json::from_value(context.normalized_arguments.clone())
                        .map_err(|_| BankError::ValidationError)?;
                input.validate()?;
                let output = self
                    .gateway
                    .get_account_balance(&context.authenticated_user_id, &input.account_id)
                    .await?;
                output.validate_for(&input.account_id)
            }
            .await;
            Ok(findings(validation, BALANCE_POLICY_VERSION))
        })
    }

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            Ok(verify_observation(
                context,
                &self.name,
                BALANCE_POLICY_VERSION,
            ))
        })
    }
}

pub struct FindPayeesPolicy {
    name: ToolName,
    gateway: Arc<dyn BankGateway>,
}

impl FindPayeesPolicy {
    pub fn new(gateway: Arc<dyn BankGateway>) -> Self {
        Self {
            name: tool_name(FIND_PAYEES),
            gateway,
        }
    }
}

impl DomainPolicy for FindPayeesPolicy {
    fn tool_name(&self) -> &ToolName {
        &self.name
    }

    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let validation = async {
                if context.tool_name != self.name {
                    return Err(BankError::ValidationError);
                }
                let input: FindPayeesInput =
                    serde_json::from_value(context.normalized_arguments.clone())
                        .map_err(|_| BankError::ValidationError)?;
                input.validate()?;
                let output = self
                    .gateway
                    .find_payees(&context.authenticated_user_id, &input.query)
                    .await?;
                output.validate()
            }
            .await;
            Ok(findings(validation, PAYEES_POLICY_VERSION))
        })
    }

    fn verify_execution<'a>(
        &'a self,
        context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            Ok(verify_observation(
                context,
                &self.name,
                PAYEES_POLICY_VERSION,
            ))
        })
    }
}

fn verify_observation(
    context: &PostExecuteContext,
    name: &ToolName,
    version: &str,
) -> Vec<SecurityFinding> {
    // PostExecuteContext contains references, not the returned bank data.
    let valid = context.tool_name == *name
        && matches!(&context.observation, ExecutionObservation::ResultReceived {
            tool_result_ref, ..
        } if *tool_result_ref == result_ref(&context.operation_id));
    findings(
        if valid {
            Ok(())
        } else {
            Err(BankError::InvalidResponse)
        },
        version,
    )
}

fn findings(validation: Result<(), BankError>, version: &str) -> Vec<SecurityFinding> {
    match validation {
        Ok(()) => vec![],
        Err(error) => vec![SecurityFinding {
            check_id: CheckId::parse("BANK-READ-01").expect("static check ID"),
            policy_version: PolicyVersion::parse(version).expect("static policy version"),
            source: SourceName::parse("bank_query_policy").expect("static source"),
            kind: FindingKind::HardDeny,
            reason_code: ReasonCode::parse(error.reason_code()).expect("static bank error code"),
            evidence_refs: vec![],
            observed_value: None,
            event_ids: vec![],
            dedup_key: None,
            window_start_ms: None,
            window_end_ms: None,
        }],
    }
}
