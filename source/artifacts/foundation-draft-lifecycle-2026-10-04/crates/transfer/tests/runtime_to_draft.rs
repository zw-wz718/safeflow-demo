#[path = "../examples/support/read_references.rs"]
mod read_references;
#[path = "../../mock-bank/examples/support/mod.rs"]
mod support;

use safeflow_contracts::SecurityOutcome;
use safeflow_contracts::{ResourceId, SessionRef, TaskId, UserId};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{RuntimeError, StaticSessionResolver};
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{
    DraftInput, DraftStatus, DraftView, PayeeSelection, TransferDrafts,
};
use std::sync::Arc;
use support::block_on;

fn sessions() -> Arc<StaticSessionResolver> {
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(
        SessionRef::parse("server_session").unwrap(),
        UserId::parse("user_001").unwrap(),
    );
    Arc::new(sessions)
}

#[test]
fn real_runtime_queries_feed_a_draft_in_the_same_task_without_reusing_query_operation_ids() {
    let bank = Arc::new(MockBankGateway::new());
    let refs = block_on(read_references::query_bank(
        bank.clone(),
        sessions(),
        SessionRef::parse("server_session").unwrap(),
        TaskId::parse("server_task").unwrap(),
        ResourceId::parse("account_001").unwrap(),
        "小李",
    ))
    .unwrap();
    assert_eq!(refs.audit_event_count, 10);
    assert_eq!(refs.account.available_balance.as_str(), "1288.00");
    assert_eq!(refs.payees.candidates[0].masked_account_no, "**** 1024");
    let mut drafts = TransferDrafts::new(bank, ManualClock::new(1000), 300).unwrap();
    let draft = block_on(drafts.create(
        &refs.context,
        DraftInput {
            payer_account_id: Some(refs.account.account.account_id),
            payee_query: Some("小李".into()),
            payee: Some(PayeeSelection::Id(
                refs.payees.candidates[0].payee_id.clone(),
            )),
            amount: Some("200.00".into()),
        },
        "",
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(draft.task_id.as_str(), "server_task");
    assert_eq!(draft.user_id.as_str(), "user_001");
    assert_eq!(draft.status, DraftStatus::ReadyToConfirm);
    assert!(!refs.query_operations.contains(&draft.operation_id));
    assert_eq!(draft.details.unwrap().amount.as_str(), "200.00");
}

#[test]
fn runtime_same_name_results_preserve_selection_and_continue_the_original_task() {
    let bank = Arc::new(MockBankGateway::with_same_name_payees());
    let refs = block_on(read_references::query_bank(
        bank.clone(),
        sessions(),
        SessionRef::parse("server_session").unwrap(),
        TaskId::parse("same_name_task").unwrap(),
        ResourceId::parse("account_001").unwrap(),
        "小李",
    ))
    .unwrap();
    assert_eq!(refs.audit_event_count, 10);
    assert_eq!(refs.payees.candidates.len(), 2);
    let mut drafts = TransferDrafts::new(bank, ManualClock::new(1000), 300).unwrap();
    let pending = block_on(drafts.create(
        &refs.context,
        DraftInput {
            payer_account_id: Some(refs.account.account.account_id),
            payee_query: Some("小李".into()),
            payee: Some(PayeeSelection::Name("小李".into())),
            amount: Some("200.00".into()),
        },
        "",
    ))
    .unwrap();
    assert!(pending.saved.is_none());
    let DraftView::NeedPayee(choices) = pending.view else {
        panic!("must retain ambiguity")
    };
    assert_eq!(choices.len(), 2);
    let choice = refs
        .payees
        .candidates
        .iter()
        .find(|payee| payee.payee_id.as_str() == "payee_009")
        .unwrap();
    let selected = block_on(drafts.create(
        &refs.context,
        DraftInput {
            payee: Some(PayeeSelection::Id(choice.payee_id.clone())),
            ..Default::default()
        },
        "",
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(selected.task_id.as_str(), "same_name_task");
    assert_eq!(
        selected.details.unwrap().payee.masked_account_no,
        "**** 2024"
    );
}

#[test]
fn a_missing_server_session_does_not_return_references_for_draft_creation() {
    let result = block_on(read_references::query_bank(
        Arc::new(MockBankGateway::new()),
        sessions(),
        SessionRef::parse("unknown_session").unwrap(),
        TaskId::parse("missing_session_task").unwrap(),
        ResourceId::parse("account_001").unwrap(),
        "小李",
    ));
    let Err(error) = result else {
        panic!("must not receive references")
    };
    assert!(matches!(
        error.downcast_ref::<RuntimeError>(),
        Some(RuntimeError::Security(_))
    ));
}

#[test]
fn runtime_denies_a_foreign_account_before_returning_draft_references() {
    let result = block_on(read_references::query_bank(
        Arc::new(MockBankGateway::new()),
        sessions(),
        SessionRef::parse("server_session").unwrap(),
        TaskId::parse("foreign_account_task").unwrap(),
        ResourceId::parse("account_002").unwrap(),
        "小李",
    ));
    let Err(error) = result else {
        panic!("must not receive references")
    };
    assert_eq!(
        error.downcast_ref::<RuntimeError>(),
        Some(&RuntimeError::SecurityGate(SecurityOutcome::Blocked))
    );
}
