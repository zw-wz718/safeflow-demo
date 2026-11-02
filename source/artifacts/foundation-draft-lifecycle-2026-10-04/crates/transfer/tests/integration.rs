mod support;
use safeflow_contracts::ResourceId;
use safeflow_mock_bank::MockBankGateway;
use safeflow_transfer_integration::{
    DraftChanges, DraftError, DraftInput, DraftStatus, DraftView, IntegrationError, PayeeSelection,
    TransferDrafts,
};
use std::sync::Arc;
use support::{block_on, context, ManualClock};

fn input() -> DraftInput {
    DraftInput {
        payer_account_id: Some(ResourceId::parse("account_001").unwrap()),
        payee: Some(PayeeSelection::Name("小李".into())),
        amount: Some("200".into()),
        ..Default::default()
    }
}

#[test]
fn public_create_rejects_invalid_remark_without_consuming_the_task() {
    for remark in ["字".repeat(141), "转账\n备注".into(), "\u{7f}".into()] {
        let ctx = context("user_001", "remark_create");
        let mut drafts = TransferDrafts::new(
            Arc::new(MockBankGateway::new()),
            ManualClock::new(1000),
            300,
        )
        .unwrap();
        assert_eq!(
            block_on(drafts.create(&ctx, input(), &remark)).unwrap_err(),
            IntegrationError::InvalidRemark
        );
        let saved = block_on(drafts.create(&ctx, input(), "合法备注"))
            .unwrap()
            .saved
            .unwrap();
        assert_eq!(saved.plan_version, 1);
        assert_eq!(saved.details.unwrap().remark, "合法备注");
    }
}

#[test]
fn public_modify_rejects_invalid_remark_without_changing_the_original() {
    let ctx = context("user_001", "remark_modify");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let first = block_on(drafts.create(&ctx, input(), "原备注"))
        .unwrap()
        .saved
        .unwrap();
    for remark in ["字".repeat(141), "新\n备注".into(), "\0".into()] {
        let result = block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount: Some("300".into()),
                    ..Default::default()
                },
                remark: Some(remark),
                ..Default::default()
            },
        ));
        assert_eq!(result.unwrap_err(), IntegrationError::InvalidRemark);
        assert_eq!(drafts.read(&ctx, &first.draft_id).unwrap(), first);
    }
    assert_eq!(
        drafts.cancel(&ctx, &first.draft_id, 1).unwrap().status,
        DraftStatus::Cancelled
    );
}

#[test]
fn public_remark_boundary_counts_characters_and_supports_preserve_and_clear() {
    for remark in ["a".repeat(140), "字".repeat(140), "😀".repeat(140)] {
        let ctx = context("user_001", "remark_boundary");
        let mut drafts = TransferDrafts::new(
            Arc::new(MockBankGateway::new()),
            ManualClock::new(1000),
            300,
        )
        .unwrap();
        let first = block_on(drafts.create(&ctx, input(), &remark))
            .unwrap()
            .saved
            .unwrap();
        assert_eq!(first.details.as_ref().unwrap().remark, remark);
        let unchanged = block_on(drafts.modify(&ctx, &first.draft_id, 1, DraftChanges::default()))
            .unwrap()
            .saved
            .unwrap();
        assert_eq!(unchanged, first);
        let second = block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some(remark.clone()),
                ..Default::default()
            },
        ))
        .unwrap()
        .saved
        .unwrap();
        assert_eq!(second.plan_version, 2);
        assert_eq!(second.expires_at, first.expires_at);
        let cleared = block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            2,
            DraftChanges {
                remark: Some(String::new()),
                ..Default::default()
            },
        ))
        .unwrap()
        .saved
        .unwrap();
        assert_eq!(cleared.plan_version, 3);
        assert_eq!(cleared.operation_id, first.operation_id);
        assert_eq!(cleared.details.unwrap().remark, "");
        assert!(block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            2,
            DraftChanges {
                remark: Some("字".repeat(141)),
                ..Default::default()
            },
        ))
        .is_err());
        assert_eq!(drafts.read(&ctx, &first.draft_id).unwrap().plan_version, 3);
    }
}

#[test]
fn missing_amount_waits_and_continues_on_the_same_task_without_a_ready_draft() {
    let ctx = context("user_001", "missing_amount");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let mut request = input();
    request.amount = None;
    let pending = block_on(drafts.create(&ctx, request, "")).unwrap();
    assert_eq!(pending.task_id, ctx.task_id);
    assert_eq!(pending.view, DraftView::NeedAmount);
    assert!(pending.saved.is_none());
    let ready = block_on(drafts.create(
        &ctx,
        DraftInput {
            amount: Some("200.00".into()),
            ..Default::default()
        },
        "",
    ))
    .unwrap();
    assert_eq!(ready.view, DraftView::ReadyForReview);
    let saved = ready.saved.unwrap();
    assert_eq!(saved.task_id, ctx.task_id);
    assert_eq!(saved.plan_version, 1);
    assert_eq!(saved.details.unwrap().amount.as_str(), "200.00");
}

#[test]
fn unmatched_search_or_wrong_name_never_selects_an_available_payee() {
    for (query, name) in [("查无此人", "查无此人"), ("小李", "小王")] {
        let ctx = context("user_001", "wrong_name");
        let mut drafts = TransferDrafts::new(
            Arc::new(MockBankGateway::new()),
            ManualClock::new(1000),
            300,
        )
        .unwrap();
        let mut request = input();
        request.payee_query = Some(query.into());
        request.payee = Some(PayeeSelection::Name(name.into()));
        let pending = block_on(drafts.create(&ctx, request, "")).unwrap();
        assert_eq!(pending.view, DraftView::NeedPayee(vec![]));
        assert!(pending.saved.is_none());
        let ready = block_on(drafts.create(
            &ctx,
            DraftInput {
                payee_query: Some("小李".into()),
                payee: Some(PayeeSelection::Id(ResourceId::parse("payee_008").unwrap())),
                ..Default::default()
            },
            "",
        ))
        .unwrap()
        .saved
        .unwrap();
        assert_eq!(ready.task_id, ctx.task_id);
        assert_eq!(ready.plan_version, 1);
        assert_eq!(ready.details.unwrap().payee.payee_id.as_str(), "payee_008");
    }
}

#[test]
fn official_queries_create_and_read_a_typed_200_yuan_draft() {
    let ctx = context("user_001", "original_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let progress = block_on(drafts.create(&ctx, input(), "午饭AA")).unwrap();
    let saved = progress.saved.unwrap();
    assert_eq!(saved.task_id, ctx.task_id);
    assert_ne!(saved.operation_id, ctx.operation_id);
    assert_eq!(saved.plan_version, 1);
    assert_eq!(saved.status, DraftStatus::ReadyToConfirm);
    assert_eq!(saved.expires_at, 1300);
    let details = saved.details.as_ref().unwrap();
    assert_eq!(details.amount.as_str(), "200.00");
    assert_eq!(details.account.account_id.as_str(), "account_001");
    assert_eq!(details.account.masked_account_no, "**** 6628");
    assert_eq!(details.payee.payee_id.as_str(), "payee_008");
    assert_eq!(details.payee.bank_name, "示例银行");
    assert_eq!(details.available_balance.as_str(), "1288.00");
    assert_eq!(details.balance_as_of, "2026-10-01T14:30:00+08:00");
    assert_eq!(drafts.read(&ctx, &saved.draft_id).unwrap(), saved);
}

#[test]
fn modifying_the_amount_keeps_ids_and_rejects_old_review_update_and_cancel() {
    let ctx = context("user_001", "same_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let first = block_on(drafts.create(&ctx, input(), "AA"))
        .unwrap()
        .saved
        .unwrap();
    let second = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
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
    assert_eq!(
        (
            &second.task_id,
            &second.draft_id,
            &second.operation_id,
            second.expires_at
        ),
        (&first.task_id, &first.draft_id, &first.operation_id, 1300)
    );
    assert_eq!(second.plan_version, 2);
    assert_eq!(second.details.unwrap().amount.as_str(), "300.00");
    let conflict = IntegrationError::Draft(DraftError::VersionConflict {
        expected: 1,
        actual: 2,
    });
    assert_eq!(
        drafts
            .current_for_review(&ctx, &first.draft_id, 1)
            .unwrap_err(),
        conflict
    );
    assert_eq!(
        block_on(drafts.modify(&ctx, &first.draft_id, 1, DraftChanges::default())).unwrap_err(),
        conflict
    );
    assert_eq!(
        drafts.cancel(&ctx, &first.draft_id, 1).unwrap_err(),
        conflict
    );
    assert_eq!(
        drafts
            .current_for_review(&ctx, &first.draft_id, 2)
            .unwrap()
            .plan_version,
        2
    );
}

#[test]
fn same_name_candidates_require_an_explicit_owned_id_and_keep_the_task() {
    let ctx = context("user_001", "ambiguous_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::with_same_name_payees()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let progress = block_on(drafts.create(&ctx, input(), "")).unwrap();
    assert!(progress.saved.is_none());
    let DraftView::NeedPayee(choices) = progress.view else {
        panic!("must request selection")
    };
    assert_eq!(choices.len(), 2);
    assert_eq!(choices[0].masked_account_no, "**** 1024");
    assert_eq!(choices[1].masked_account_no, "**** 2024");
    assert!(!choices
        .iter()
        .any(|payee| payee.payee_id.as_str() == "payee_020"));
    let selected = block_on(drafts.create(
        &ctx,
        DraftInput {
            payee: Some(PayeeSelection::Id(ResourceId::parse("payee_009").unwrap())),
            ..Default::default()
        },
        "",
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(selected.task_id, ctx.task_id);
    assert_eq!(
        selected.details.unwrap().payee.payee_id.as_str(),
        "payee_009"
    );
}

#[test]
fn absent_account_source_is_not_replaced_by_a_hardcoded_salary_card() {
    let ctx = context("user_001", "no_account_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let mut request = input();
    request.payer_account_id = None;
    let pending = block_on(drafts.create(&ctx, request, "")).unwrap();
    assert_eq!(pending.view, DraftView::NeedAccountSource);
    assert!(pending.saved.is_none());
    let complete = block_on(drafts.create(
        &ctx,
        DraftInput {
            payer_account_id: Some(ResourceId::parse("account_001").unwrap()),
            ..Default::default()
        },
        "",
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(complete.details.unwrap().amount.as_str(), "200.00");
}

#[test]
fn an_id_without_a_search_and_another_users_id_never_become_ready() {
    let ctx = context("user_001", "untrusted_payee_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let mut request = input();
    request.payee = Some(PayeeSelection::Id(ResourceId::parse("payee_008").unwrap()));
    let progress = block_on(drafts.create(&ctx, request, "")).unwrap();
    assert_eq!(progress.view, DraftView::NeedPayeeSearch);
    assert!(progress.saved.is_none());
    let progress = block_on(drafts.create(
        &ctx,
        DraftInput {
            payee_query: Some("小李".into()),
            payee: Some(PayeeSelection::Id(ResourceId::parse("payee_020").unwrap())),
            ..Default::default()
        },
        "",
    ))
    .unwrap();
    assert_eq!(progress.view, DraftView::NeedPayee(vec![]));
    assert!(progress.saved.is_none());
}

#[test]
fn ambiguous_change_invalidates_previous_version_and_resolves_on_the_same_operation() {
    let ctx = context("user_001", "switch_payee_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::with_same_name_payees()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let mut request = input();
    request.payee_query = Some("小李".into());
    request.payee = Some(PayeeSelection::Id(ResourceId::parse("payee_008").unwrap()));
    let first = block_on(drafts.create(&ctx, request, ""))
        .unwrap()
        .saved
        .unwrap();
    let pending = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        1,
        DraftChanges {
            input: DraftInput {
                payee: Some(PayeeSelection::Name("小李".into())),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(pending.plan_version, 2);
    assert_eq!(pending.status, DraftStatus::NeedsInformation);
    assert!(pending.details.is_none());
    assert!(matches!(
        drafts.current_for_review(&ctx, &first.draft_id, 1),
        Err(IntegrationError::Draft(DraftError::VersionConflict { .. }))
    ));
    assert_eq!(
        drafts
            .current_for_review(&ctx, &first.draft_id, 2)
            .unwrap_err(),
        IntegrationError::Draft(DraftError::NotReady)
    );
    let third = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        2,
        DraftChanges {
            input: DraftInput {
                payee: Some(PayeeSelection::Id(ResourceId::parse("payee_009").unwrap())),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(third.plan_version, 3);
    assert_eq!(third.operation_id, first.operation_id);
    assert_eq!(third.details.unwrap().payee.payee_id.as_str(), "payee_009");
}

#[test]
fn expiry_at_the_deadline_is_latched_and_cancelled_drafts_cannot_resume() {
    let ctx = context("user_001", "expire_task");
    let clock = ManualClock::new(1000);
    let mut drafts =
        TransferDrafts::new(Arc::new(MockBankGateway::new()), clock.clone(), 300).unwrap();
    let saved = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    clock.set(1300);
    assert_eq!(
        drafts.read(&ctx, &saved.draft_id).unwrap().status,
        DraftStatus::Expired
    );
    clock.set(1000);
    assert_eq!(
        drafts
            .current_for_review(&ctx, &saved.draft_id, 1)
            .unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    assert_eq!(
        block_on(drafts.modify(&ctx, &saved.draft_id, 1, DraftChanges::default())).unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    assert_eq!(
        drafts.cancel(&ctx, &saved.draft_id, 1).unwrap_err(),
        IntegrationError::Draft(DraftError::Expired)
    );
    let ctx = context("user_001", "cancel_task");
    let saved = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    let cancelled = drafts.cancel(&ctx, &saved.draft_id, 1).unwrap();
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(drafts.cancel(&ctx, &saved.draft_id, 1).unwrap(), cancelled);
    assert_eq!(
        drafts
            .current_for_review(&ctx, &saved.draft_id, 1)
            .unwrap_err(),
        IntegrationError::Draft(DraftError::Cancelled)
    );
    assert_eq!(
        block_on(drafts.modify(&ctx, &saved.draft_id, 1, DraftChanges::default())).unwrap_err(),
        IntegrationError::Draft(DraftError::Cancelled)
    );
    assert_eq!(
        block_on(drafts.create(&ctx, input(), "")).unwrap_err(),
        IntegrationError::Draft(DraftError::AlreadyCreated)
    );
}

#[test]
fn foreign_accounts_drafts_and_wrong_tasks_are_rejected() {
    let ctx = context("user_001", "owned_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let mut request = input();
    request.payer_account_id = Some(ResourceId::parse("account_002").unwrap());
    assert_eq!(
        block_on(drafts.create(&ctx, request, "")).unwrap_err(),
        IntegrationError::Bank(safeflow_bank_gateway::BankError::ResourceNotOwned)
    );
    let saved = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    let foreign = context("user_002", "owned_task");
    assert_eq!(
        drafts.read(&foreign, &saved.draft_id).unwrap_err(),
        IntegrationError::Draft(DraftError::AccessDenied)
    );
    assert_eq!(
        block_on(drafts.modify(&foreign, &saved.draft_id, 1, DraftChanges::default())).unwrap_err(),
        IntegrationError::Draft(DraftError::AccessDenied)
    );
    assert_eq!(
        drafts.cancel(&foreign, &saved.draft_id, 1).unwrap_err(),
        IntegrationError::Draft(DraftError::AccessDenied)
    );
    assert_eq!(
        block_on(drafts.create(&foreign, input(), "")).unwrap_err(),
        IntegrationError::Draft(DraftError::AccessDenied)
    );
    let wrong_task = context("user_001", "another_task");
    assert_eq!(
        drafts.cancel(&wrong_task, &saved.draft_id, 1).unwrap_err(),
        IntegrationError::TaskMismatch
    );
    assert_eq!(drafts.read(&ctx, &saved.draft_id).unwrap(), saved);
}

#[test]
fn opaque_selected_payee_ids_still_reject_whitespace_and_control_characters() {
    let ctx = context("user_001", "invalid_id_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    for invalid in ["payee 008", "payee\u{0007}008", &"x".repeat(129)] {
        let mut request = input();
        request.payee_query = Some("小李".into());
        request.payee = Some(PayeeSelection::Id(ResourceId::parse(invalid).unwrap()));
        assert_eq!(
            block_on(drafts.create(&ctx, request, "")).unwrap_err(),
            IntegrationError::Bank(safeflow_bank_gateway::BankError::ValidationError)
        );
    }
}

#[test]
fn invalid_new_amount_or_unknown_selected_payee_clears_current_details() {
    let ctx = context("user_001", "invalid_change_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let first = block_on(drafts.create(&ctx, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    let invalid = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        1,
        DraftChanges {
            input: DraftInput {
                amount: Some("0.00".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap();
    assert_eq!(invalid.view, DraftView::InvalidAmount);
    assert_eq!(invalid.saved.unwrap().details, None);
    let recovered = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        2,
        DraftChanges {
            input: DraftInput {
                amount: Some("200.00".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    let invalid = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        3,
        DraftChanges {
            input: DraftInput {
                payee: Some(PayeeSelection::Id(
                    ResourceId::parse("unknown_payee").unwrap(),
                )),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(invalid.plan_version, 4);
    assert_eq!(invalid.status, DraftStatus::NeedsInformation);
    assert!(invalid.details.is_none());
    assert_eq!(invalid.operation_id, recovered.operation_id);
}

#[test]
fn currency_remark_and_search_edits_increment_once_but_an_empty_edit_does_not() {
    let ctx = context("user_001", "metadata_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let first = block_on(drafts.create(&ctx, input(), "旧备注"))
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(
        block_on(drafts.modify(&ctx, &first.draft_id, 1, DraftChanges::default()))
            .unwrap()
            .saved
            .unwrap(),
        first
    );
    assert_eq!(
        block_on(drafts.modify(
            &ctx,
            &first.draft_id,
            1,
            DraftChanges {
                currency: Some("USD".into()),
                ..Default::default()
            }
        ))
        .unwrap_err(),
        IntegrationError::Draft(DraftError::UnsupportedCurrency)
    );
    assert_eq!(drafts.read(&ctx, &first.draft_id).unwrap(), first);
    let second = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        1,
        DraftChanges {
            input: DraftInput {
                payer_account_id: Some(ResourceId::parse("account_001").unwrap()),
                ..Default::default()
            },
            currency: Some("CNY".into()),
            remark: Some("".into()),
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(second.plan_version, 2);
    assert_eq!(second.details.unwrap().remark, "");
    let third = block_on(drafts.modify(
        &ctx,
        &first.draft_id,
        2,
        DraftChanges {
            input: DraftInput {
                payee_query: Some("没人叫这个名字".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(third.plan_version, 3);
    assert_eq!(third.status, DraftStatus::NeedsInformation);
    assert!(third.details.is_none());
}

#[test]
fn separate_users_and_tasks_keep_independent_bank_snapshots_and_identifiers() {
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    let ctx1 = context("user_001", "first_task");
    let first = block_on(drafts.create(&ctx1, input(), ""))
        .unwrap()
        .saved
        .unwrap();
    let ctx2 = context("user_002", "second_task");
    let second = block_on(drafts.create(
        &ctx2,
        DraftInput {
            payer_account_id: Some(ResourceId::parse("account_002").unwrap()),
            amount: Some("20.00".into()),
            payee: Some(PayeeSelection::Name("小李".into())),
            ..Default::default()
        },
        "",
    ))
    .unwrap()
    .saved
    .unwrap();
    assert_eq!(
        second.details.as_ref().unwrap().payee.payee_id.as_str(),
        "payee_020"
    );
    assert_eq!(
        second.details.as_ref().unwrap().account.masked_account_no,
        "**** 7788"
    );
    assert_ne!(first.operation_id, second.operation_id);
    assert_ne!(first.draft_id, second.draft_id);
    assert_eq!(drafts.read(&ctx1, &first.draft_id).unwrap(), first);
    assert_eq!(drafts.read(&ctx2, &second.draft_id).unwrap(), second);
}

#[test]
fn the_server_module_and_its_async_create_future_can_be_sent_to_an_executor() {
    fn require_send<T: Send>(_: T) {}
    let ctx = context("user_001", "async_task");
    let mut drafts = TransferDrafts::new(
        Arc::new(MockBankGateway::new()),
        ManualClock::new(1000),
        300,
    )
    .unwrap();
    require_send(drafts.create(&ctx, input(), ""));
    require_send(drafts);
}
