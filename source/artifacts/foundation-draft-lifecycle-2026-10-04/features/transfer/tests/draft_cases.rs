use safeflow_transfer::{Account, DraftInput, DraftSession, DraftView, Payee, Selection};

fn session() -> DraftSession {
    DraftSession::new(
        "task-demo-001".into(),
        vec![
            Account {
                id: "salary".into(),
                label: "工资卡".into(),
                last_four: "1234".into(),
            },
            Account {
                id: "savings".into(),
                label: "储蓄卡".into(),
                last_four: "5678".into(),
            },
        ],
        vec![
            Payee {
                id: "zhang".into(),
                name: "张三".into(),
                last_four: "8888".into(),
            },
            Payee {
                id: "li-a".into(),
                name: "小李".into(),
                last_four: "1111".into(),
            },
            Payee {
                id: "li-b".into(),
                name: "小李".into(),
                last_four: "2222".into(),
            },
        ],
    )
}

fn complete_input() -> DraftInput {
    DraftInput {
        account: Some(Selection::Name("工资卡".into())),
        payee: Some(Selection::Name("张三".into())),
        amount_yuan: Some("200".into()),
    }
}

#[test]
fn case_01_complete_information_produces_only_a_pending_draft() {
    let mut flow = session();
    let DraftView::AwaitingConfirmation(draft) = flow.apply(complete_input()) else {
        panic!("信息齐全时应生成待确认草案");
    };
    assert_eq!(draft.task_id, "task-demo-001");
    assert_eq!(draft.account.id, "salary");
    assert_eq!(draft.account.last_four, "1234");
    assert_eq!(draft.payee.id, "zhang");
    assert_eq!(draft.payee.last_four, "8888");
    assert_eq!(draft.amount_fen, 20_000);
}

#[test]
fn case_02_missing_amount_requests_amount() {
    let mut flow = session();
    let mut input = complete_input();
    input.amount_yuan = None;
    assert_eq!(flow.apply(input), DraftView::NeedAmount);
}

#[test]
fn case_03_same_name_payees_require_a_choice() {
    let mut flow = session();
    let mut input = complete_input();
    input.payee = Some(Selection::Name("小李".into()));
    let DraftView::NeedPayee(choices) = flow.apply(input) else {
        panic!("不能代选同名收款人");
    };
    assert_eq!(
        choices.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec!["li-a", "li-b"]
    );
    assert_eq!(
        choices
            .iter()
            .map(|p| p.last_four.as_str())
            .collect::<Vec<_>>(),
        vec!["1111", "2222"]
    );
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        payee: Some(Selection::Id("li-b".into())),
        ..Default::default()
    }) else {
        panic!("选择后应保留账户与金额并生成草案");
    };
    assert_eq!(draft.payee.last_four, "2222");
    assert_eq!(draft.amount_fen, 20_000);
}

#[test]
fn case_04_missing_account_does_not_use_a_default() {
    let mut flow = session();
    let mut input = complete_input();
    input.account = None;
    let DraftView::NeedAccount(choices) = flow.apply(input) else {
        panic!("未指定付款卡时必须询问");
    };
    assert_eq!(choices.len(), 2);
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        account: Some(Selection::Id("savings".into())),
        ..Default::default()
    }) else {
        panic!("选择付款卡后应生成草案");
    };
    assert_eq!(draft.account.last_four, "5678");
}

#[test]
fn case_05_nonpositive_amounts_are_rejected() {
    for amount in ["0", "0.00", "-1", "-200"] {
        let mut flow = session();
        let mut input = complete_input();
        input.amount_yuan = Some(amount.into());
        assert_eq!(flow.apply(input), DraftView::InvalidAmount, "{amount}");
    }
}

#[test]
fn case_06_followup_preserves_task_account_and_payee() {
    let mut flow = session();
    let mut input = complete_input();
    input.amount_yuan = None;
    assert_eq!(flow.apply(input), DraftView::NeedAmount);
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        amount_yuan: Some("200".into()),
        ..Default::default()
    }) else {
        panic!("补金额后应继续原任务");
    };
    assert_eq!(draft.task_id, "task-demo-001");
    assert_eq!(draft.account.id, "salary");
    assert_eq!(draft.payee.id, "zhang");
    assert_eq!(draft.amount_fen, 20_000);
}

#[test]
fn decimal_amounts_are_exact_without_floating_point() {
    for (text, expected) in [("0.01", 1), ("200.10", 20_010), ("200.1", 20_010)] {
        let mut input = complete_input();
        input.amount_yuan = Some(text.into());
        let DraftView::AwaitingConfirmation(draft) = session().apply(input) else {
            panic!("合法两位以内小数应通过");
        };
        assert_eq!(draft.amount_fen, expected);
    }
}

#[test]
fn malformed_precision_and_overflow_are_rejected() {
    for text in [
        "",
        " ",
        "200元",
        "1e2",
        "NaN",
        "1.001",
        ".5",
        "1.",
        "+1",
        "1,000",
        "18446744073709551616",
        "184467440737095516.16",
    ] {
        let mut input = complete_input();
        input.amount_yuan = Some(text.into());
        assert_eq!(session().apply(input), DraftView::InvalidAmount, "{text}");
    }
}

#[test]
fn changing_to_an_unknown_payee_does_not_keep_the_old_payee() {
    let mut flow = session();
    flow.apply(complete_input());
    assert_eq!(
        flow.apply(DraftInput {
            payee: Some(Selection::Id("unknown".into())),
            ..Default::default()
        }),
        DraftView::NeedPayee(vec![])
    );
}

#[test]
fn invalid_amount_replaces_previous_amount_until_corrected() {
    let mut flow = session();
    flow.apply(complete_input());
    assert_eq!(
        flow.apply(DraftInput {
            amount_yuan: Some("-1".into()),
            ..Default::default()
        }),
        DraftView::InvalidAmount
    );
    assert_eq!(flow.view(), DraftView::InvalidAmount);
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        amount_yuan: Some("300".into()),
        ..Default::default()
    }) else {
        panic!("纠正金额后恢复草案");
    };
    assert_eq!(draft.amount_fen, 30_000);
}

#[test]
fn unknown_account_is_not_replaced_with_another_account() {
    let mut input = complete_input();
    input.account = Some(Selection::Id("foreign-or-unknown".into()));
    assert_eq!(session().apply(input), DraftView::NeedAccount(vec![]));
}

#[test]
fn changing_payee_updates_the_draft_and_preserves_task_account_and_amount() {
    let mut flow = session();
    flow.apply(complete_input());
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        payee: Some(Selection::Id("li-b".into())),
        ..Default::default()
    }) else {
        panic!("明确改收款人后应展示新收款人");
    };
    assert_eq!(draft.task_id, "task-demo-001");
    assert_eq!(draft.payee.id, "li-b");
    assert_eq!(draft.payee.last_four, "2222");
    assert_eq!(draft.account.id, "salary");
    assert_eq!(draft.amount_fen, 20_000);
    assert_eq!(flow.view(), DraftView::AwaitingConfirmation(draft));
}

#[test]
fn changing_account_updates_the_draft_and_preserves_payee_and_amount() {
    let mut flow = session();
    flow.apply(complete_input());
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        account: Some(Selection::Id("savings".into())),
        ..Default::default()
    }) else {
        panic!("明确改付款卡后应展示新卡");
    };
    assert_eq!(draft.task_id, "task-demo-001");
    assert_eq!(draft.account.id, "savings");
    assert_eq!(draft.account.last_four, "5678");
    assert_eq!(draft.payee.id, "zhang");
    assert_eq!(draft.amount_fen, 20_000);
    assert_eq!(flow.view(), DraftView::AwaitingConfirmation(draft));
}

#[test]
fn changing_to_ambiguous_name_leaves_ready_state_until_user_selects() {
    let mut flow = session();
    flow.apply(complete_input());
    let DraftView::NeedPayee(choices) = flow.apply(DraftInput {
        payee: Some(Selection::Name("小李".into())),
        ..Default::default()
    }) else {
        panic!("改为同名收款人后必须退出待确认并重新选择");
    };
    assert_eq!(
        choices.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(),
        vec!["li-a", "li-b"]
    );
    assert_eq!(flow.view(), DraftView::NeedPayee(choices));
    let DraftView::AwaitingConfirmation(draft) = flow.apply(DraftInput {
        payee: Some(Selection::Id("li-a".into())),
        ..Default::default()
    }) else {
        panic!("用户选择后才重新展示草案");
    };
    assert_eq!(draft.payee.id, "li-a");
    assert_eq!(draft.account.id, "salary");
    assert_eq!(draft.amount_fen, 20_000);
}
