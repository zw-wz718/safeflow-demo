use safeflow_transfer::fixtures::{demo_bank, ManualClock, MemoryBankGateway};
use safeflow_transfer::{
    Account, BankAccount, BankGateway, DraftChanges, DraftError, DraftInput, DraftService,
    DraftStatus, DraftView, GatewayError, Payee, Selection,
};

fn input() -> DraftInput {
    DraftInput {
        account: Some(Selection::Id("account_001".into())),
        payee: Some(Selection::Id("payee_008".into())),
        amount_yuan: Some("200.00".into()),
    }
}

#[test]
fn changing_amount_saves_version_two_and_rejects_the_old_version() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", Some("task-existing-001"), input(), "午饭AA")
        .unwrap()
        .saved
        .unwrap();
    let changed = drafts
        .modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount_yuan: Some("300.00".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(changed.plan_version, 2);
    assert_eq!(changed.draft_id, first.draft_id);
    assert_eq!(changed.operation_id, first.operation_id);
    assert_eq!(changed.task_id, first.task_id);
    assert_eq!(changed.details.as_ref().unwrap().amount, "300.00");
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), changed);
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 1),
        Err(DraftError::VersionConflict {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(
        drafts
            .current_for_review("user_001", &first.draft_id, 2)
            .unwrap(),
        changed
    );
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some("旧页面改备注".into()),
                ..Default::default()
            }
        ),
        Err(DraftError::VersionConflict {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), changed);
}

#[test]
fn expiry_at_the_exact_deadline_blocks_old_and_current_actions() {
    let clock = ManualClock::new(1_000);
    let mut drafts = DraftService::new(demo_bank(), clock.clone(), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    clock.set(1_299);
    assert!(drafts
        .current_for_review("user_001", &first.draft_id, 1)
        .is_ok());
    clock.set(1_300);
    assert_eq!(
        drafts.read("user_001", &first.draft_id).unwrap().status,
        DraftStatus::Expired
    );
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 1),
        Err(DraftError::Expired)
    );
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some("试图恢复".into()),
                ..Default::default()
            }
        ),
        Err(DraftError::Expired)
    );
    clock.set(1_200);
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 1),
        Err(DraftError::Expired)
    );
}

#[test]
fn cancellation_is_saved_and_cannot_be_reopened_or_used_for_review() {
    let clock = ManualClock::new(1_000);
    let mut drafts = DraftService::new(demo_bank(), clock.clone(), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    let cancelled = drafts.cancel("user_001", &first.draft_id, 1).unwrap();
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), cancelled);
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 1),
        Err(DraftError::Cancelled)
    );
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount_yuan: Some("300".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
        ),
        Err(DraftError::Cancelled)
    );
    assert_eq!(
        drafts.cancel("user_001", &first.draft_id, 1).unwrap(),
        cancelled
    );
    clock.set(2_000);
    assert_eq!(
        drafts.read("user_001", &first.draft_id).unwrap().status,
        DraftStatus::Cancelled
    );
}

#[test]
fn generates_and_reads_back_a_200_yuan_draft_in_the_existing_task() {
    let clock = ManualClock::new(1_000);
    let mut drafts = DraftService::new(demo_bank(), clock, 300).unwrap();
    let progress = drafts
        .create("user_001", Some("task-existing-001"), input(), "午饭AA")
        .unwrap();
    assert!(matches!(progress.view, DraftView::AwaitingConfirmation(_)));
    let saved = progress.saved.unwrap();
    assert_eq!(saved.task_id, "task-existing-001");
    assert_eq!(saved.plan_version, 1);
    assert!(!saved.draft_id.is_empty());
    assert!(!saved.operation_id.is_empty());
    assert_ne!(saved.draft_id, saved.operation_id);
    assert_eq!(saved.status, DraftStatus::ReadyToConfirm);
    assert_eq!(saved.expires_at, 1_300);
    let details = saved.details.as_ref().unwrap();
    assert_eq!(details.amount, "200.00");
    assert_eq!(details.currency, "CNY");
    assert_eq!(details.remark, "午饭AA");
    assert_eq!(details.account.id, "account_001");
    assert_eq!(details.payee.id, "payee_008");
    assert_eq!(details.payee.last_four, "1024");
    assert_eq!(drafts.read("user_001", &saved.draft_id).unwrap(), saved);
}

fn bank_with_choices() -> MemoryBankGateway {
    let mut bank = demo_bank();
    let mut catalog = bank.catalog("user_001").unwrap();
    catalog.accounts.push(BankAccount {
        account: Account {
            id: "account_002".into(),
            label: "模拟储蓄账户".into(),
            last_four: "0002".into(),
        },
        balance: "800.00".into(),
        currency: "CNY".into(),
    });
    catalog.payees.push(Payee {
        id: "payee_009".into(),
        name: "小李".into(),
        last_four: "5678".into(),
    });
    bank.insert("user_001".into(), catalog);
    bank
}

#[test]
fn ambiguous_payee_cannot_be_saved_until_selected_in_the_same_task() {
    let mut drafts = DraftService::new(bank_with_choices(), ManualClock::new(1_000), 300).unwrap();
    let progress = drafts
        .create(
            "user_001",
            None,
            DraftInput {
                payee: Some(Selection::Name("小李".into())),
                ..input()
            },
            "午饭AA",
        )
        .unwrap();
    let DraftView::NeedPayee(choices) = progress.view else {
        panic!("同名必须选人")
    };
    assert_eq!(choices.len(), 2);
    assert!(progress.saved.is_none());
    let saved = drafts
        .create(
            "user_001",
            Some(&progress.task_id),
            DraftInput {
                payee: Some(Selection::Id("payee_008".into())),
                ..Default::default()
            },
            "午饭AA",
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(saved.task_id, progress.task_id);
    assert_eq!(saved.plan_version, 1);
    assert_eq!(saved.details.unwrap().amount, "200.00");
}

#[test]
fn missing_amount_is_completed_without_losing_account_payee_or_task() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let progress = drafts
        .create(
            "user_001",
            None,
            DraftInput {
                amount_yuan: None,
                ..input()
            },
            "",
        )
        .unwrap();
    assert_eq!(progress.view, DraftView::NeedAmount);
    assert!(progress.saved.is_none());
    let saved = drafts
        .create(
            "user_001",
            Some(&progress.task_id),
            DraftInput {
                amount_yuan: Some("200.00".into()),
                ..Default::default()
            },
            "",
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(saved.task_id, progress.task_id);
    assert_eq!(saved.details.as_ref().unwrap().account.id, "account_001");
    assert_eq!(saved.details.as_ref().unwrap().payee.id, "payee_008");
    assert_eq!(saved.details.as_ref().unwrap().amount, "200.00");
}

#[test]
fn replacing_account_payee_and_remark_versions_the_same_draft() {
    let mut drafts = DraftService::new(bank_with_choices(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "午饭AA")
        .unwrap()
        .saved
        .unwrap();
    let updates = [
        DraftChanges {
            input: DraftInput {
                account: Some(Selection::Id("account_002".into())),
                ..Default::default()
            },
            ..Default::default()
        },
        DraftChanges {
            input: DraftInput {
                payee: Some(Selection::Id("payee_009".into())),
                ..Default::default()
            },
            ..Default::default()
        },
        DraftChanges {
            remark: Some("聚餐AA".into()),
            ..Default::default()
        },
        DraftChanges {
            remark: Some(String::new()),
            ..Default::default()
        },
        DraftChanges {
            currency: Some("CNY".into()),
            ..Default::default()
        },
    ];
    for (index, update) in updates.into_iter().enumerate() {
        let old_version = index as u64 + 1;
        let changed = drafts
            .modify("user_001", &first.draft_id, old_version, update)
            .unwrap()
            .saved
            .unwrap();
        assert_eq!(changed.plan_version, old_version + 1);
        assert_eq!(changed.draft_id, first.draft_id);
        assert_eq!(changed.operation_id, first.operation_id);
        assert_eq!(changed.task_id, first.task_id);
        assert_eq!(changed.expires_at, first.expires_at);
        assert_eq!(
            drafts.current_for_review("user_001", &first.draft_id, old_version),
            Err(DraftError::VersionConflict {
                expected: old_version,
                actual: old_version + 1
            })
        );
        let details = changed.details.unwrap();
        assert_eq!(details.account.id, "account_002");
        assert_eq!(
            details.payee.id,
            if index == 0 { "payee_008" } else { "payee_009" }
        );
        assert_eq!(
            details.remark,
            match index {
                0 | 1 => "午饭AA",
                2 => "聚餐AA",
                _ => "",
            }
        );
        assert_eq!(details.amount, "200.00");
    }
}

#[test]
fn ambiguous_change_invalidates_old_snapshot_and_needs_a_new_selection() {
    let mut drafts = DraftService::new(bank_with_choices(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    let progress = drafts
        .modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    payee: Some(Selection::Name("小李".into())),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    assert!(matches!(progress.view, DraftView::NeedPayee(_)));
    let pending = drafts.read("user_001", &first.draft_id).unwrap();
    assert_eq!(pending.status, DraftStatus::NeedsInformation);
    assert!(pending.details.is_none());
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 1),
        Err(DraftError::VersionConflict {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(
        drafts.current_for_review("user_001", &first.draft_id, 2),
        Err(DraftError::NotReady)
    );
    let corrected = drafts
        .modify(
            "user_001",
            &first.draft_id,
            2,
            DraftChanges {
                input: DraftInput {
                    payee: Some(Selection::Id("payee_009".into())),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(corrected.plan_version, 3);
    assert_eq!(corrected.task_id, first.task_id);
    assert_eq!(corrected.operation_id, first.operation_id);
    assert_eq!(corrected.details.as_ref().unwrap().payee.id, "payee_009");
    assert_eq!(corrected.details.as_ref().unwrap().amount, "200.00");
}

#[test]
fn invalid_changes_do_not_leave_the_old_fields_ready() {
    for input in [
        DraftInput {
            amount_yuan: Some("-1".into()),
            ..Default::default()
        },
        DraftInput {
            amount_yuan: Some("200.001".into()),
            ..Default::default()
        },
        DraftInput {
            payee: Some(Selection::Id("unknown-payee".into())),
            ..Default::default()
        },
        DraftInput {
            account: Some(Selection::Id("unknown-account".into())),
            ..Default::default()
        },
    ] {
        let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
        let first = drafts
            .create("user_001", None, crate::input(), "")
            .unwrap()
            .saved
            .unwrap();
        let saved = drafts
            .modify(
                "user_001",
                &first.draft_id,
                1,
                DraftChanges {
                    input,
                    ..Default::default()
                },
            )
            .unwrap()
            .saved
            .unwrap();
        assert_eq!(saved.plan_version, 2);
        assert_eq!(saved.status, DraftStatus::NeedsInformation);
        assert!(saved.details.is_none());
        assert_eq!(
            drafts.current_for_review("user_001", &first.draft_id, 2),
            Err(DraftError::NotReady)
        );
    }
}

#[test]
fn other_user_cannot_read_modify_cancel_or_reuse_a_task() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(
        drafts.read("user_other", &first.draft_id),
        Err(DraftError::AccessDenied)
    );
    assert_eq!(
        drafts.modify("user_other", &first.draft_id, 1, DraftChanges::default()),
        Err(DraftError::AccessDenied)
    );
    assert_eq!(
        drafts.cancel("user_other", &first.draft_id, 1),
        Err(DraftError::AccessDenied)
    );
    assert_eq!(
        drafts.current_for_review("user_other", &first.draft_id, 1),
        Err(DraftError::AccessDenied)
    );
    assert_eq!(
        drafts.create("user_other", Some(&first.task_id), input(), ""),
        Err(DraftError::AccessDenied)
    );
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), first);
}

#[test]
fn create_cannot_overwrite_a_saved_draft_or_bypass_a_cancelled_task() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(
        drafts.create("user_001", Some(&first.task_id), input(), "覆盖"),
        Err(DraftError::AlreadyCreated)
    );
    let second = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    assert_ne!(first.task_id, second.task_id);
    assert_ne!(first.draft_id, second.draft_id);
    assert_ne!(first.operation_id, second.operation_id);
    drafts.cancel("user_001", &first.draft_id, 1).unwrap();
    assert_eq!(
        drafts.create("user_001", Some(&first.task_id), input(), ""),
        Err(DraftError::AlreadyCreated)
    );
}

#[test]
fn unsupported_currency_and_empty_update_do_not_replace_the_current_draft() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                currency: Some("USD".into()),
                ..Default::default()
            }
        ),
        Err(DraftError::UnsupportedCurrency)
    );
    assert_eq!(
        drafts
            .modify("user_001", &first.draft_id, 1, DraftChanges::default())
            .unwrap()
            .saved
            .unwrap(),
        first
    );
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), first);
}

#[test]
fn stale_cancellation_cannot_cancel_the_new_version() {
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    let changed = drafts
        .modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some("新备注".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(
        drafts.cancel("user_001", &first.draft_id, 1),
        Err(DraftError::VersionConflict {
            expected: 1,
            actual: 2
        })
    );
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), changed);
    assert_eq!(
        drafts
            .cancel("user_001", &first.draft_id, 2)
            .unwrap()
            .status,
        DraftStatus::Cancelled
    );
}

#[test]
fn refuses_invalid_configuration_identity_unknown_drafts_and_unknown_users() {
    assert!(matches!(
        DraftService::new(demo_bank(), ManualClock::new(1_000), 0),
        Err(DraftError::InvalidTtl)
    ));
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(1_000), 300).unwrap();
    assert_eq!(
        drafts.create("", None, input(), ""),
        Err(DraftError::InvalidIdentity)
    );
    assert_eq!(
        drafts.create("user_001", Some(" "), input(), ""),
        Err(DraftError::InvalidTaskId)
    );
    assert_eq!(
        drafts.create("user_unknown", None, input(), ""),
        Err(DraftError::Gateway(GatewayError::UnknownUser))
    );
    assert_eq!(
        drafts.read("user_001", "missing-draft"),
        Err(DraftError::NotFound)
    );
    assert_eq!(
        drafts.modify("user_001", "missing-draft", 1, DraftChanges::default()),
        Err(DraftError::NotFound)
    );
    assert_eq!(
        drafts.cancel("user_001", "missing-draft", 1),
        Err(DraftError::NotFound)
    );
}

#[test]
fn expiry_does_not_extend_after_changes_and_cancel_at_expiry_is_rejected() {
    let clock = ManualClock::new(1_000);
    let mut drafts = DraftService::new(demo_bank(), clock.clone(), 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    clock.set(1_299);
    let changed = drafts
        .modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some("快到期时修改".into()),
                ..Default::default()
            },
        )
        .unwrap()
        .saved
        .unwrap();
    assert_eq!(changed.expires_at, 1_300);
    clock.set(1_300);
    assert_eq!(
        drafts.cancel("user_001", &first.draft_id, 2),
        Err(DraftError::Expired)
    );
}

#[test]
fn pending_information_also_expires_and_invalid_amounts_cannot_create_a_draft() {
    let clock = ManualClock::new(1_000);
    let mut drafts = DraftService::new(demo_bank(), clock.clone(), 300).unwrap();
    let invalid = drafts
        .create(
            "user_001",
            None,
            DraftInput {
                amount_yuan: Some("200.001".into()),
                ..input()
            },
            "",
        )
        .unwrap();
    assert_eq!(invalid.view, DraftView::InvalidAmount);
    assert!(invalid.saved.is_none());
    let first = drafts
        .create(
            "user_001",
            Some(&invalid.task_id),
            DraftInput {
                amount_yuan: Some("200.00".into()),
                ..Default::default()
            },
            "",
        )
        .unwrap()
        .saved
        .unwrap();
    drafts
        .modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount_yuan: Some("-1".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
    clock.set(1_300);
    assert_eq!(
        drafts.read("user_001", &first.draft_id).unwrap().status,
        DraftStatus::Expired
    );
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            2,
            DraftChanges {
                input: DraftInput {
                    amount_yuan: Some("200".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
        ),
        Err(DraftError::Expired)
    );
}

struct ControlledBank {
    clock: ManualClock,
    fail: std::sync::Arc<std::sync::atomic::AtomicBool>,
    slow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl BankGateway for ControlledBank {
    fn catalog(&self, user_id: &str) -> Result<safeflow_transfer::BankCatalog, GatewayError> {
        use std::sync::atomic::Ordering;
        if self.fail.load(Ordering::SeqCst) {
            return Err(GatewayError::Unavailable);
        }
        if self.slow.load(Ordering::SeqCst) {
            self.clock.set(1_300);
        }
        demo_bank().catalog(user_id)
    }
}

#[test]
fn a_draft_that_expires_during_catalog_loading_cannot_be_modified() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let clock = ManualClock::new(1_000);
    let slow = Arc::new(AtomicBool::new(false));
    let bank = ControlledBank {
        clock: clock.clone(),
        fail: Arc::new(AtomicBool::new(false)),
        slow: slow.clone(),
    };
    let mut drafts = DraftService::new(bank, clock, 300).unwrap();
    let first = drafts
        .create("user_001", None, input(), "")
        .unwrap()
        .saved
        .unwrap();
    slow.store(true, Ordering::SeqCst);
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                remark: Some("读取期间已到期".into()),
                ..Default::default()
            }
        ),
        Err(DraftError::Expired)
    );
    let saved = drafts.read("user_001", &first.draft_id).unwrap();
    assert_eq!(saved.status, DraftStatus::Expired);
    assert_eq!(saved.plan_version, 1);
    assert_eq!(saved.details.unwrap().remark, "");
}

#[test]
fn unavailable_catalog_does_not_save_a_new_draft_or_partially_modify_one() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let clock = ManualClock::new(1_000);
    let fail = Arc::new(AtomicBool::new(true));
    let bank = ControlledBank {
        clock: clock.clone(),
        fail: fail.clone(),
        slow: Arc::new(AtomicBool::new(false)),
    };
    let mut drafts = DraftService::new(bank, clock, 300).unwrap();
    assert_eq!(
        drafts.create("user_001", Some("task-keep"), input(), ""),
        Err(DraftError::Gateway(GatewayError::Unavailable))
    );
    fail.store(false, Ordering::SeqCst);
    let first = drafts
        .create("user_001", Some("task-keep"), input(), "")
        .unwrap()
        .saved
        .unwrap();
    fail.store(true, Ordering::SeqCst);
    assert_eq!(
        drafts.modify(
            "user_001",
            &first.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount_yuan: Some("300".into()),
                    ..Default::default()
                },
                remark: Some("不可部分保存".into()),
                ..Default::default()
            }
        ),
        Err(DraftError::Gateway(GatewayError::Unavailable))
    );
    assert_eq!(drafts.read("user_001", &first.draft_id).unwrap(), first);
}

#[test]
fn fixture_balance_is_1288_and_time_overflow_does_not_leave_a_draft() {
    assert_eq!(
        demo_bank().catalog("user_001").unwrap().accounts[0].balance,
        "1288.00"
    );
    let mut drafts = DraftService::new(demo_bank(), ManualClock::new(u64::MAX), 300).unwrap();
    assert_eq!(
        drafts.create("user_001", Some("task-overflow"), input(), ""),
        Err(DraftError::TimeOverflow)
    );
    // 溢出错误不占用任务；可重复得到同一明确错误，而非 AlreadyCreated。
    assert_eq!(
        drafts.create("user_001", Some("task-overflow"), input(), ""),
        Err(DraftError::TimeOverflow)
    );
}
