mod support {
    include!("support/funds_policy_fixture.rs");
}
use futures::executor::block_on;
use safeflow_contracts::DomainPolicy;
use safeflow_contracts::*;
use safeflow_transfer_integration::funds_policy::{FundsBusinessPolicy, FundsPolicyFacts};

fn check(context: &SecurityContext, facts: FundsPolicyFacts, kind: FindingKind, reason: &str) {
    let policy = FundsBusinessPolicy::new(context.tool_name.clone(), support::Source(Ok(facts)));
    let result = block_on(policy.evaluate(context)).unwrap();
    assert_eq!(result.len(), 1, "expected {reason}: {result:?}");
    assert_eq!(result[0].kind, kind);
    assert_eq!(result[0].reason_code.as_str(), reason);
    result[0].validate().unwrap();
}

#[test]
fn current_draft_and_business_request_must_match() {
    for case in 0..8 {
        let (context, mut facts) = support::fixture();
        let reason = match case {
            0 => {
                facts.stored.owner_id = UserId::parse("other").unwrap();
                "FUNDS_OWNER_MISMATCH"
            }
            1 => {
                facts.stored.request.remark = "changed".into();
                "FUNDS_ARGUMENTS_MISMATCH"
            }
            2 => {
                facts.stored.draft = None;
                "FUNDS_DRAFT_UNAVAILABLE"
            }
            3 => {
                facts.stored.draft.as_mut().unwrap().status =
                    safeflow_transfer_integration::DraftStatus::Cancelled;
                "FUNDS_DRAFT_NOT_EXECUTABLE"
            }
            4 => {
                facts.stored.draft.as_mut().unwrap().expires_at = 1;
                "FUNDS_DRAFT_NOT_EXECUTABLE"
            }
            5 => {
                facts.stored.draft.as_mut().unwrap().plan_version += 1;
                "FUNDS_DRAFT_NOT_EXECUTABLE"
            }
            6 => {
                facts.stored.draft.as_mut().unwrap().operation_id =
                    OperationId::parse("other-operation").unwrap();
                "FUNDS_DRAFT_NOT_EXECUTABLE"
            }
            _ => {
                facts.stored.submitted = true;
                "FUNDS_ORIGINAL_UNAVAILABLE"
            }
        };
        let kind = if matches!(case, 2 | 7) {
            FindingKind::NeedsInput
        } else {
            FindingKind::HardDeny
        };
        check(&context, facts, kind, reason);
    }
}

#[test]
fn account_route_balance_and_outgoing_facts_block_unsafe_execution() {
    use safeflow_bank_gateway::DecimalAmount;
    use safeflow_transfer_integration::mock_execution::MockOutgoingState;
    for case in 0..9 {
        let (context, mut facts) = support::fixture();
        let (kind, reason) = match case {
            0 => {
                facts.stored.payer_owner_id = Some(UserId::parse("other").unwrap());
                (FindingKind::HardDeny, "ACCOUNT_NOT_ACCESSIBLE")
            }
            1 => {
                facts.stored.payer_owner_id = None;
                (FindingKind::HardDeny, "ACCOUNT_NOT_ACCESSIBLE")
            }
            2 => {
                facts.stored.recipient_account_id = None;
                (FindingKind::NeedsInput, "PAYEE_ROUTE_UNAVAILABLE")
            }
            3 => {
                facts.stored.recipient_account_id =
                    Some(facts.stored.request.payer_account_id.clone());
                (FindingKind::HardDeny, "INVALID_SELF_ROUTE")
            }
            4 => {
                facts.stored.funds = None;
                (FindingKind::NeedsInput, "BALANCE_UNAVAILABLE")
            }
            5 => {
                let f = facts.stored.funds.as_mut().unwrap();
                f.booked = DecimalAmount::parse("100.00").unwrap();
                f.available = f.booked.clone();
                (FindingKind::HardDeny, "INSUFFICIENT_FUNDS")
            }
            6 => {
                facts.stored.funds.as_mut().unwrap().reserved =
                    DecimalAmount::parse("2000.00").unwrap();
                (FindingKind::NeedsInput, "BALANCE_INCONSISTENT")
            }
            7 => {
                facts.outgoing = MockOutgoingState::Blocked;
                (FindingKind::HardDeny, "ACCOUNT_OUTGOING_BLOCKED")
            }
            _ => {
                facts.outgoing = MockOutgoingState::Unknown;
                (FindingKind::NeedsInput, "OUTGOING_STATE_UNKNOWN")
            }
        };
        check(&context, facts, kind, reason);
    }
}

#[test]
fn incomplete_daily_facts_and_untrusted_time_are_not_zero_usage() {
    for case in 0..4 {
        let (context, mut facts) = support::fixture();
        let reason = match case {
            0 => {
                facts.daily = None;
                "DAILY_USAGE_UNAVAILABLE"
            }
            1 => {
                facts.daily.as_mut().unwrap().complete = false;
                "DAILY_USAGE_UNAVAILABLE"
            }
            2 => {
                facts.daily.as_mut().unwrap().submitted.push(
                    safeflow_transfer_integration::daily_usage::SubmittedTransferFact {
                        owner_id: context.authenticated_user_id.clone(),
                        operation_id: OperationId::parse("older").unwrap(),
                        payer_account_id: facts.stored.request.payer_account_id.clone(),
                        amount: facts.stored.request.amount.clone(),
                        submitted_at: None,
                        status: ToolResultStatus::Processing,
                    },
                );
                "DAILY_USAGE_UNAVAILABLE"
            }
            _ => {
                facts.checked_at = "client-says-today".into();
                "FUNDS_TIME_UNAVAILABLE"
            }
        };
        check(&context, facts, FindingKind::NeedsInput, reason);
    }
}

#[test]
fn existing_operation_requires_query_even_when_old_draft_and_account_are_unavailable() {
    let (context, mut facts) = support::fixture();
    facts.stored.original_request = Some(facts.stored.request.clone());
    facts.stored.original_key = Some(facts.stored.key.clone());
    facts.stored.key_operation_id = Some(facts.stored.request.operation_id.clone());
    facts.stored.submitted = true;
    facts.stored.draft = None;
    facts.stored.funds = None;
    facts.daily = None;
    facts.outgoing = safeflow_transfer_integration::mock_execution::MockOutgoingState::Unknown;
    check(
        &context,
        facts,
        FindingKind::HardDeny,
        "FUNDS_ORIGINAL_REQUIRES_QUERY",
    );
}

#[test]
fn same_key_changed_parameters_or_changed_key_cannot_authorize_another_payment() {
    for case in 0..4 {
        let (context, mut facts) = support::fixture();
        facts.stored.original_request = Some(facts.stored.request.clone());
        facts.stored.original_key = Some(facts.stored.key.clone());
        facts.stored.key_operation_id = Some(facts.stored.request.operation_id.clone());
        match case {
            0 => facts.stored.original_request.as_mut().unwrap().remark = "different".into(),
            1 => facts.stored.original_key = Some(IdempotencyKey::parse("different-key").unwrap()),
            2 => {
                facts.stored.key_operation_id =
                    Some(OperationId::parse("different-business-op").unwrap())
            }
            _ => facts.stored.original_request = None,
        }
        check(
            &context,
            facts,
            if case == 3 {
                FindingKind::NeedsInput
            } else {
                FindingKind::HardDeny
            },
            if case == 3 {
                "FUNDS_ORIGINAL_UNAVAILABLE"
            } else {
                "FUNDS_IDEMPOTENCY_CONFLICT"
            },
        );
    }
}

#[test]
fn invalid_context_envelope_and_daily_original_conflict_do_not_pass() {
    let (mut context, mut facts) = support::fixture();
    context.plan_version = 0;
    facts.binding = context.clone();
    check(
        &context,
        facts,
        FindingKind::NeedsInput,
        "FUNDS_CONTEXT_INVALID",
    );
    let (context, mut facts) = support::fixture();
    facts.daily.as_mut().unwrap().submitted.push(
        safeflow_transfer_integration::daily_usage::SubmittedTransferFact {
            owner_id: context.authenticated_user_id.clone(),
            operation_id: facts.stored.request.operation_id.clone(),
            payer_account_id: facts.stored.request.payer_account_id.clone(),
            amount: facts.stored.request.amount.clone(),
            submitted_at: Some("2026-10-18T09:00:00+08:00".into()),
            status: ToolResultStatus::Processing,
        },
    );
    check(
        &context,
        facts,
        FindingKind::NeedsInput,
        "FUNDS_ORIGINAL_UNAVAILABLE",
    );
}

#[test]
fn source_failure_and_unconnected_post_execute_never_report_pass() {
    let (context, _) = support::fixture();
    let error = SecurityError {
        reason_code: ReasonCode::parse("STORAGE_UNAVAILABLE").unwrap(),
        message: "fixture unavailable".into(),
    };
    let policy = FundsBusinessPolicy::new(
        context.tool_name.clone(),
        support::Source(Err(error.clone())),
    );
    assert_eq!(block_on(policy.evaluate(&context)).unwrap_err(), error);
    let post = PostExecuteContext {
        contract_version: SECURITY_CONTRACT_VERSION,
        task_id: context.task_id,
        node_id: context.node_id,
        operation_id: context.operation_id,
        tool_name: context.tool_name,
        authenticated_user_id: context.authenticated_user_id,
        observation: ExecutionObservation::NotSubmitted {
            attempt_id: AttemptId::parse("fixture-attempt").unwrap(),
            reason_code: ReasonCode::parse("FIXTURE_ONLY").unwrap(),
        },
    };
    assert_eq!(
        block_on(policy.verify_execution(&post))
            .unwrap_err()
            .reason_code
            .as_str(),
        "FUNDS_POST_EXECUTION_NOT_CONNECTED"
    );
}

#[test]
fn mismatched_context_and_request_cannot_reuse_facts() {
    for change in 0..5 {
        let (mut context, facts) = support::fixture();
        match change {
            0 => {
                context.authenticated_user_id =
                    safeflow_contracts::UserId::parse("other-owner").unwrap()
            }
            1 => context.plan_version += 1,
            2 => context.facts_version += 1,
            3 => context.normalized_arguments["amount"] = "300.00".into(),
            _ => context.tool_name = safeflow_contracts::ToolName::parse("other_tool").unwrap(),
        }
        let policy =
            FundsBusinessPolicy::new(facts.binding.tool_name.clone(), support::Source(Ok(facts)));
        let result = block_on(policy.evaluate(&context)).unwrap();
        assert!(!result.is_empty(), "changed context {change} accepted");
    }
}

#[test]
fn valid_business_facts_have_no_blocking_findings_and_do_not_execute_money() {
    let (context, facts) = support::fixture();
    let policy = FundsBusinessPolicy::new(context.tool_name.clone(), support::Source(Ok(facts)));
    assert!(block_on(policy.evaluate(&context)).unwrap().is_empty());
}
