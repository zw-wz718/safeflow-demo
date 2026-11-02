mod support;

use safeflow_contracts::ResourceId;
use safeflow_mock_bank::MockBankGateway;
use safeflow_transfer_integration::{
    DraftChanges, DraftError, DraftInput, DraftRestore, DraftStatus, IntegrationError,
    PayeeSelection, StoredDraft, TransferDrafts,
};
use std::sync::Arc;
use support::{block_on, context, ManualClock};

struct OfflineBank;
impl safeflow_bank_gateway::BankGateway for OfflineBank {
    fn get_account_balance<'a>(
        &'a self,
        _: &'a safeflow_contracts::UserId,
        _: &'a ResourceId,
    ) -> safeflow_contracts::ContractFuture<
        'a,
        Result<safeflow_bank_gateway::GetAccountBalanceOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async { Err(safeflow_bank_gateway::BankError::Unavailable) })
    }
    fn find_payees<'a>(
        &'a self,
        _: &'a safeflow_contracts::UserId,
        _: &'a str,
    ) -> safeflow_contracts::ContractFuture<
        'a,
        Result<safeflow_bank_gateway::FindPayeesOutput, safeflow_bank_gateway::BankError>,
    > {
        Box::pin(async { Err(safeflow_bank_gateway::BankError::Unavailable) })
    }
}

fn original(clock: ManualClock) -> StoredDraft {
    let mut service = TransferDrafts::new(Arc::new(MockBankGateway::new()), clock, 300).unwrap();
    block_on(service.create(
        &context("user_001", "restore_task"),
        DraftInput {
            payer_account_id: Some(ResourceId::parse("account_001").unwrap()),
            payee_query: Some("小李".into()),
            payee: Some(PayeeSelection::Id(ResourceId::parse("payee_008").unwrap())),
            amount: Some("200.00".into()),
        },
        "原备注",
    ))
    .unwrap()
    .saved
    .unwrap()
}

fn restore_input(draft: StoredDraft) -> DraftRestore {
    DraftRestore {
        expected_version: draft.plan_version,
        draft,
        submitted: false,
        payee_query: "小李".into(),
    }
}

#[test]
fn restored_draft_continues_modification_and_cancellation_without_new_ids_or_expiry() {
    let ctx = context("user_001", "restore_task");
    let saved = original(ManualClock::new(1000));
    let mut restarted = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1100),
        9999,
    )
    .unwrap();
    assert_eq!(
        restarted
            .restore_for_backend(&ctx, restore_input(saved.clone()))
            .unwrap(),
        saved
    );
    let modified = block_on(restarted.modify(
        &ctx,
        &saved.draft_id,
        1,
        DraftChanges {
            input: DraftInput {
                amount: Some("300.00".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    let cancelled = restarted.cancel(&ctx, &saved.draft_id, 2).unwrap();
    assert_eq!(modified.plan_version, 2);
    assert_eq!(modified.details.as_ref().unwrap().amount.as_str(), "300.00");
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(cancelled.plan_version, 2);
    assert_eq!(cancelled.task_id, saved.task_id);
    assert_eq!(cancelled.draft_id, saved.draft_id);
    assert_eq!(cancelled.operation_id, saved.operation_id);
    assert_eq!(cancelled.expires_at, 1300);
}

#[test]
fn restore_rejects_wrong_owner_task_version_submitted_and_terminal_records() {
    let ctx = context("user_001", "restore_task");
    let saved = original(ManualClock::new(1000));
    let mut cases = vec![];
    let mut wrong_owner = restore_input(saved.clone());
    wrong_owner.draft.user_id = safeflow_contracts::UserId::parse("user_002").unwrap();
    cases.push((
        wrong_owner,
        IntegrationError::Draft(DraftError::AccessDenied),
    ));
    let mut wrong_task = restore_input(saved.clone());
    wrong_task.draft.task_id = safeflow_contracts::TaskId::parse("other_task").unwrap();
    cases.push((wrong_task, IntegrationError::TaskMismatch));
    let mut wrong_version = restore_input(saved.clone());
    wrong_version.expected_version = 2;
    cases.push((
        wrong_version,
        IntegrationError::Draft(DraftError::VersionConflict {
            expected: 2,
            actual: 1,
        }),
    ));
    let mut submitted = restore_input(saved.clone());
    submitted.submitted = true;
    cases.push((submitted, IntegrationError::DraftAlreadySubmitted));
    for (status, error) in [
        (DraftStatus::Cancelled, DraftError::Cancelled),
        (DraftStatus::Expired, DraftError::Expired),
        (DraftStatus::NeedsInformation, DraftError::NotReady),
    ] {
        let mut terminal = restore_input(saved.clone());
        terminal.draft.status = status;
        cases.push((terminal, IntegrationError::Draft(error)));
    }
    for (bad, expected) in cases {
        let mut restarted = TransferDrafts::new(
            Arc::new(MockBankGateway::new()),
            ManualClock::new(1100),
            300,
        )
        .unwrap();
        assert_eq!(
            restarted.restore_for_backend(&ctx, bad).unwrap_err(),
            expected
        );
        assert!(restarted.read(&ctx, &saved.draft_id).is_err());
    }
}

#[test]
fn restore_never_overwrites_a_task_or_imports_a_missing_or_malformed_selection() {
    let ctx = context("user_001", "restore_task");
    let saved = original(ManualClock::new(1000));
    let mut restarted = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1100),
        300,
    )
    .unwrap();
    for case in 0..5 {
        let mut bad = restore_input(saved.clone());
        match case {
            0 => bad.draft.details = None,
            1 => bad.payee_query.clear(),
            2 => {
                bad.draft
                    .details
                    .as_mut()
                    .unwrap()
                    .account
                    .masked_account_no = "6628".into()
            }
            3 => bad.draft.details.as_mut().unwrap().remark = "bad\nremark".into(),
            _ => {
                bad.draft.plan_version = 0;
                bad.expected_version = 0;
            }
        }
        assert!(restarted.restore_for_backend(&ctx, bad).is_err());
    }
    restarted
        .restore_for_backend(&ctx, restore_input(saved.clone()))
        .unwrap();
    let mut replacing = restore_input(saved.clone());
    replacing.draft.details.as_mut().unwrap().amount =
        safeflow_bank_gateway::DecimalAmount::parse("999.00").unwrap();
    assert_eq!(
        restarted.restore_for_backend(&ctx, replacing).unwrap_err(),
        IntegrationError::Draft(DraftError::AlreadyCreated)
    );
    assert_eq!(restarted.read(&ctx, &saved.draft_id).unwrap(), saved);
    assert!(block_on(restarted.create(&ctx, DraftInput::default(), "")).is_err());
}

#[test]
fn restored_old_versions_cannot_modify_or_cancel_and_expiry_is_not_extended() {
    let ctx = context("user_001", "restore_task");
    let mut saved = original(ManualClock::new(1000));
    saved.plan_version = 2;
    let clock = ManualClock::new(1100);
    let mut restarted =
        TransferDrafts::new(Arc::new(MockBankGateway::new()), clock.clone(), 9999).unwrap();
    restarted
        .restore_for_backend(&ctx, restore_input(saved.clone()))
        .unwrap();
    assert!(block_on(restarted.modify(&ctx, &saved.draft_id, 1, DraftChanges::default())).is_err());
    assert!(restarted.cancel(&ctx, &saved.draft_id, 1).is_err());
    clock.set(1300);
    assert_eq!(
        restarted
            .current_for_review(&ctx, &saved.draft_id, 2)
            .unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    clock.set(1100);
    assert!(restarted
        .current_for_review(&ctx, &saved.draft_id, 2)
        .is_err());
    assert_eq!(
        restarted.read(&ctx, &saved.draft_id).unwrap().status,
        DraftStatus::Expired
    );
}

#[test]
fn expired_restore_cannot_be_retried_after_clock_rollback() {
    let ctx = context("user_001", "restore_task");
    let saved = original(ManualClock::new(1000));
    let clock = ManualClock::new(1300);
    let mut restarted =
        TransferDrafts::new(Arc::new(MockBankGateway::new()), clock.clone(), 300).unwrap();
    assert_eq!(
        restarted
            .restore_for_backend(&ctx, restore_input(saved.clone()))
            .unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    clock.set(1100);
    assert_eq!(
        restarted
            .restore_for_backend(&ctx, restore_input(saved))
            .unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
}

#[test]
fn restored_display_is_not_fresh_bank_authority_and_offline_cancellation_still_works() {
    let ctx = context("user_001", "restore_task");
    let saved = original(ManualClock::new(1000));
    let mut restarted =
        TransferDrafts::new(Arc::new(OfflineBank), ManualClock::new(1100), 300).unwrap();
    restarted
        .restore_for_backend(&ctx, restore_input(saved.clone()))
        .unwrap();
    assert_eq!(
        block_on(restarted.modify(
            &ctx,
            &saved.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount: Some("300.00".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        ))
        .unwrap_err(),
        IntegrationError::Bank(safeflow_bank_gateway::BankError::Unavailable)
    );
    assert_eq!(restarted.read(&ctx, &saved.draft_id).unwrap(), saved);
    let cancelled = restarted.cancel(&ctx, &saved.draft_id, 1).unwrap();
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(cancelled.plan_version, 1);
    assert_eq!(cancelled.expires_at, saved.expires_at);
}
