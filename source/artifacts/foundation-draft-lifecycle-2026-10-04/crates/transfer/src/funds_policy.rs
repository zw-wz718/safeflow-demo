//! Read-only transfer business checks (contract 06), not permission to pay.
//! Sources MUST be trusted backend adapters, never client/model supplied facts.
//! Freshness and protection through COMMIT remain the foundation's responsibility.
use crate::{
    daily_usage::{calculate_daily_usage, DailyUsageInput, SubmittedTransferFact},
    mock_execution::{amount_cents, MockAccountFunds, MockOutgoingState, MockTransferRequest},
    StoredDraft,
};
use chrono::DateTime;
use safeflow_contracts::*;

/// Owner-scoped SQL facts. No Deserialize: this is not a wire/input contract.
#[derive(Clone, Debug)]
pub struct FundsStoredFacts {
    pub owner_id: UserId,
    pub request: MockTransferRequest,
    pub key: IdempotencyKey,
    pub draft: Option<StoredDraft>,
    pub submitted: bool,
    pub payer_owner_id: Option<UserId>,
    pub funds: Option<MockAccountFunds>,
    pub recipient_account_id: Option<ResourceId>,
    pub original_request: Option<MockTransferRequest>,
    pub original_key: Option<IdempotencyKey>,
    pub key_operation_id: Option<OperationId>,
}

#[derive(Clone, Debug)]
pub struct DailyFacts {
    pub complete: bool,
    pub submitted: Vec<SubmittedTransferFact>,
}

#[derive(Clone, Debug)]
pub struct FundsPolicyFacts {
    /// The exact context for which this fresh backend read was performed.
    /// Equality detects misbinding, not authenticity or freshness by itself.
    pub binding: SecurityContext,
    pub checked_at: String,
    pub stored: FundsStoredFacts,
    pub outgoing: MockOutgoingState,
    pub daily: Option<DailyFacts>,
}

pub trait FundsPolicySource: Send + Sync {
    fn read<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<FundsPolicyFacts, SecurityError>>;
}

pub struct FundsBusinessPolicy<S> {
    tool_name: ToolName,
    source: S,
}
impl<S: FundsPolicySource> FundsBusinessPolicy<S> {
    /// Name comes from the foundation's adopted tool registration, not a default.
    pub fn new(tool_name: ToolName, source: S) -> Self {
        Self { tool_name, source }
    }
}
impl<S: FundsPolicySource> DomainPolicy for FundsBusinessPolicy<S> {
    fn tool_name(&self) -> &ToolName {
        &self.tool_name
    }
    fn evaluate<'a>(
        &'a self,
        context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async move {
            let facts = self.source.read(context).await?;
            if context.tool_name != self.tool_name || facts.binding != *context {
                return Ok(finding(FindingKind::HardDeny, "FUNDS_CONTEXT_MISMATCH"));
            }
            Ok(check_business(context, &facts))
        })
    }
    fn verify_execution<'a>(
        &'a self,
        _context: &'a PostExecuteContext,
    ) -> ContractFuture<'a, Result<Vec<SecurityFinding>, SecurityError>> {
        Box::pin(async {
            Err(SecurityError {
                reason_code: ReasonCode::parse("FUNDS_POST_EXECUTION_NOT_CONNECTED").unwrap(),
                message: "Use the foundation's trusted original-order/result reconciliation; this adapter only checks pre-execution business facts.".into(),
            })
        })
    }
}

fn check_business(context: &SecurityContext, facts: &FundsPolicyFacts) -> Vec<SecurityFinding> {
    use FindingKind::{HardDeny, NeedsInput};
    let stored = &facts.stored;
    if context.contract_version != SECURITY_CONTRACT_VERSION
        || context.plan_version == 0
        || context.facts_version == 0
    {
        return finding(NeedsInput, "FUNDS_CONTEXT_INVALID");
    }
    if stored.owner_id != context.authenticated_user_id {
        return finding(HardDeny, "FUNDS_OWNER_MISMATCH");
    }
    if context.idempotency_key.as_ref() != Some(&stored.key)
        || serde_json::from_value::<MockTransferRequest>(context.normalized_arguments.clone())
            .ok()
            .as_ref()
            != Some(&stored.request)
        || context.task_id != stored.request.task_id
    {
        return finding(HardDeny, "FUNDS_ARGUMENTS_MISMATCH");
    }
    if stored
        .key_operation_id
        .as_ref()
        .is_some_and(|op| op != &stored.request.operation_id)
        || stored
            .original_key
            .as_ref()
            .is_some_and(|key| key != &stored.key)
        || stored
            .original_request
            .as_ref()
            .is_some_and(|original| original != &stored.request)
    {
        return finding(HardDeny, "FUNDS_IDEMPOTENCY_CONFLICT");
    }
    match (
        &stored.original_request,
        &stored.original_key,
        &stored.key_operation_id,
    ) {
        (Some(_), Some(_), Some(_)) => return finding(HardDeny, "FUNDS_ORIGINAL_REQUIRES_QUERY"),
        (None, None, None) => {}
        _ => return finding(NeedsInput, "FUNDS_ORIGINAL_UNAVAILABLE"),
    }
    let now = match DateTime::parse_from_rfc3339(&facts.checked_at)
        .ok()
        .and_then(|t| u64::try_from(t.timestamp()).ok())
    {
        Some(now) => now,
        None => return finding(NeedsInput, "FUNDS_TIME_UNAVAILABLE"),
    };
    if stored.submitted {
        return finding(NeedsInput, "FUNDS_ORIGINAL_UNAVAILABLE");
    }
    let Some(draft) = &stored.draft else {
        return finding(NeedsInput, "FUNDS_DRAFT_UNAVAILABLE");
    };
    let execution = ExecutionContext {
        task_id: context.task_id.clone(),
        node_id: context.node_id.clone(),
        operation_id: context.operation_id.clone(),
        authenticated_user_id: context.authenticated_user_id.clone(),
        plan_version: context.plan_version,
        idempotency_key: context.idempotency_key.clone(),
    };
    if MockTransferRequest::from_trusted_draft(&execution, draft, stored.request.draft_version, now)
        .ok()
        .as_ref()
        != Some(&stored.request)
    {
        return finding(HardDeny, "FUNDS_DRAFT_NOT_EXECUTABLE");
    }
    if stored.payer_owner_id.as_ref() != Some(&context.authenticated_user_id) {
        return finding(HardDeny, "ACCOUNT_NOT_ACCESSIBLE");
    }
    let Some(recipient) = &stored.recipient_account_id else {
        return finding(NeedsInput, "PAYEE_ROUTE_UNAVAILABLE");
    };
    if recipient == &stored.request.payer_account_id {
        return finding(HardDeny, "INVALID_SELF_ROUTE");
    }
    let Some(funds) = &stored.funds else {
        return finding(NeedsInput, "BALANCE_UNAVAILABLE");
    };
    let amounts = (
        amount_cents(&funds.booked),
        amount_cents(&funds.reserved),
        amount_cents(&funds.available),
    );
    let available = match amounts {
        (Ok(booked), Ok(reserved), Ok(available))
            if booked.checked_sub(reserved) == Some(available) =>
        {
            available
        }
        _ => return finding(NeedsInput, "BALANCE_INCONSISTENT"),
    };
    match facts.outgoing {
        MockOutgoingState::Blocked => return finding(HardDeny, "ACCOUNT_OUTGOING_BLOCKED"),
        MockOutgoingState::Unknown => return finding(NeedsInput, "OUTGOING_STATE_UNKNOWN"),
        MockOutgoingState::Allowed => {}
    }
    if amount_cents(&stored.request.amount).is_ok_and(|amount| amount > available) {
        return finding(HardDeny, "INSUFFICIENT_FUNDS");
    }
    let Some(daily) = &facts.daily else {
        return finding(NeedsInput, "DAILY_USAGE_UNAVAILABLE");
    };
    if daily
        .submitted
        .iter()
        .any(|f| f.operation_id == stored.request.operation_id)
    {
        return finding(NeedsInput, "FUNDS_ORIGINAL_UNAVAILABLE");
    }
    if calculate_daily_usage(DailyUsageInput {
        owner_id: &context.authenticated_user_id,
        as_of: &facts.checked_at,
        complete: daily.complete,
        submitted: &daily.submitted,
    })
    .is_err()
    {
        return finding(NeedsInput, "DAILY_USAGE_UNAVAILABLE");
    }
    // No new cap, risk score or security level is chosen here.
    vec![]
}

fn finding(kind: FindingKind, reason: &str) -> Vec<SecurityFinding> {
    vec![SecurityFinding {
        check_id: CheckId::parse("transfer-funds-business").unwrap(),
        policy_version: PolicyVersion::parse("transfer-funds-business-2026-10-18-v1").unwrap(),
        source: SourceName::parse("transfer-business").unwrap(),
        kind,
        reason_code: ReasonCode::parse(reason).expect("static reason"),
        evidence_refs: vec![],
        observed_value: None,
        event_ids: vec![],
        dedup_key: None,
        window_start_ms: None,
        window_end_ms: None,
    }]
}
