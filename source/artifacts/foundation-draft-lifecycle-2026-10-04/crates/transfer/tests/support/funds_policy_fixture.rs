use safeflow_bank_gateway::{CnyCurrency, DecimalAmount, MaskedAccountView, PayeeView};
use safeflow_contracts::*;
use safeflow_transfer_integration::{
    funds_policy::{DailyFacts, FundsPolicyFacts, FundsPolicySource, FundsStoredFacts},
    mock_execution::{MockAccountFunds, MockOutgoingState, MockTransferRequest},
    DraftDetails, DraftStatus, StoredDraft,
};

pub struct Source(pub Result<FundsPolicyFacts, SecurityError>);
impl FundsPolicySource for Source {
    fn read<'a>(
        &'a self,
        _context: &'a SecurityContext,
    ) -> ContractFuture<'a, Result<FundsPolicyFacts, SecurityError>> {
        Box::pin(async { self.0.clone() })
    }
}

pub fn fixture() -> (SecurityContext, FundsPolicyFacts) {
    let request = MockTransferRequest {
        task_id: TaskId::parse("policy-task").unwrap(),
        operation_id: OperationId::parse("stable-business-operation").unwrap(),
        draft_id: ResourceId::parse("policy-draft").unwrap(),
        draft_version: 2,
        payer_account_id: ResourceId::parse("policy-payer").unwrap(),
        payee_id: ResourceId::parse("policy-payee").unwrap(),
        amount: DecimalAmount::parse("200.00").unwrap(),
        currency: CnyCurrency::Cny,
        remark: "模拟业务检查，不扣款".into(),
    };
    // Synthetic backend context: not a real session, confirmation or permit.
    let context = SecurityContext {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: request.task_id.clone(),
        node_id: NodeId::parse("policy-node").unwrap(),
        operation_id: OperationId::parse("runtime-operation-different-from-business").unwrap(),
        facts_version: 3,
        phase: SecurityPhase::PreTool,
        tool_name: ToolName::parse("execute_transfer").unwrap(),
        plan_version: 7,
        authenticated_user_id: UserId::parse("policy-owner").unwrap(),
        normalized_arguments: serde_json::to_value(&request).unwrap(),
        arguments_hash: ArgumentsHash::parse("a".repeat(64)).unwrap(),
        idempotency_key: Some(IdempotencyKey::parse("policy-key").unwrap()),
        confirmation_ref: None,
        auth_grant_ref: None,
    };
    let draft = StoredDraft {
        user_id: context.authenticated_user_id.clone(),
        task_id: request.task_id.clone(),
        operation_id: request.operation_id.clone(),
        draft_id: request.draft_id.clone(),
        plan_version: 2,
        status: DraftStatus::ReadyToConfirm,
        expires_at: 1792296000,
        details: Some(DraftDetails {
            account: MaskedAccountView {
                account_id: request.payer_account_id.clone(),
                display_name: "模拟付款账户".into(),
                masked_account_no: "**** 6628".into(),
                currency: CnyCurrency::Cny,
            },
            payee: PayeeView {
                payee_id: request.payee_id.clone(),
                display_name: "模拟小李".into(),
                bank_name: "模拟银行".into(),
                masked_account_no: "**** 1024".into(),
            },
            amount: request.amount.clone(),
            currency: CnyCurrency::Cny,
            remark: request.remark.clone(),
            available_balance: DecimalAmount::parse("1288.00").unwrap(),
            balance_as_of: "2026-10-18T10:00:00+08:00".into(),
        }),
    };
    let facts = FundsPolicyFacts {
        binding: context.clone(),
        checked_at: "2026-10-18T10:00:00+08:00".into(),
        stored: FundsStoredFacts {
            owner_id: context.authenticated_user_id.clone(),
            request,
            key: context.idempotency_key.clone().unwrap(),
            draft: Some(draft),
            submitted: false,
            payer_owner_id: Some(context.authenticated_user_id.clone()),
            funds: Some(MockAccountFunds {
                booked: DecimalAmount::parse("1288.00").unwrap(),
                reserved: DecimalAmount::parse("0.00").unwrap(),
                available: DecimalAmount::parse("1288.00").unwrap(),
            }),
            recipient_account_id: Some(ResourceId::parse("policy-recipient").unwrap()),
            original_request: None,
            original_key: None,
            key_operation_id: None,
        },
        outgoing: MockOutgoingState::Allowed,
        daily: Some(DailyFacts {
            complete: true,
            submitted: vec![],
        }),
    };
    (context, facts)
}
