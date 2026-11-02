use safeflow_transfer::fixtures::{demo_bank, ManualClock};
use safeflow_transfer::{
    BankGateway, DraftChanges, DraftError, DraftInput, DraftService, DraftStatus, Selection,
};

fn main() -> Result<(), DraftError> {
    println!("SafeFlow：200元草案内存保存演示（固定模拟资料、模拟时钟，无扣款）");
    let bank = demo_bank();
    let account = bank
        .catalog("user_001")
        .map_err(DraftError::Gateway)?
        .accounts
        .remove(0);
    println!("模拟账户余额：{} {}", account.balance, account.currency);
    let clock = ManualClock::new(1_000);
    // 300秒仅是演示配置，实际有效期由公共规则提供。
    let mut drafts = DraftService::new(bank, clock.clone(), 300)?;
    let input = DraftInput {
        account: Some(Selection::Id("account_001".into())),
        payee: Some(Selection::Id("payee_008".into())),
        amount_yuan: Some("200.00".into()),
    };
    let first = drafts
        .create("user_001", Some("task-from-runtime"), input, "午饭AA")?
        .saved
        .expect("完整输入应生成草案");
    let read_back = drafts.read("user_001", &first.draft_id)?;
    assert_eq!(read_back, first);
    let details = read_back.details.as_ref().expect("待确认草案字段完整");
    println!(
        "已生成并读回：{}，任务 {}，操作 {}，第{}版",
        first.draft_id, first.task_id, first.operation_id, first.plan_version
    );
    println!(
        "收款人：{}（尾号{}）；金额 {} {}；备注 {}",
        details.payee.name,
        details.payee.last_four,
        details.amount,
        details.currency,
        details.remark
    );

    let second = drafts
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
        )?
        .saved
        .expect("修改后有保存记录");
    println!(
        "修改金额：第{}版，{} CNY；任务与操作编号不变",
        second.plan_version,
        second.details.as_ref().unwrap().amount
    );
    let old = drafts.current_for_review("user_001", &first.draft_id, 1);
    assert_eq!(
        old,
        Err(DraftError::VersionConflict {
            expected: 1,
            actual: 2
        })
    );
    println!("旧版本被拒绝：{:?}", old.unwrap_err());
    let cancelled = drafts.cancel("user_001", &first.draft_id, 2)?;
    assert_eq!(cancelled.status, DraftStatus::Cancelled);
    let cancelled_check = drafts.current_for_review("user_001", &first.draft_id, 2);
    assert_eq!(cancelled_check, Err(DraftError::Cancelled));
    println!(
        "已取消并保存，继续核对被拒绝：{:?}",
        cancelled_check.unwrap_err()
    );

    // 另建一份草案只用于演示到期；不是提交第二笔转账。
    let expiry = drafts
        .create(
            "user_001",
            None,
            DraftInput {
                account: Some(Selection::Id("account_001".into())),
                payee: Some(Selection::Id("payee_008".into())),
                amount_yuan: Some("200.00".into()),
            },
            "到期演示",
        )?
        .saved
        .unwrap();
    clock.set(expiry.expires_at);
    let expired = drafts.current_for_review("user_001", &expiry.draft_id, 1);
    assert_eq!(expired, Err(DraftError::Expired));
    println!("恰好到期时被拒绝：{:?}", expired.unwrap_err());
    println!("演示结束：未执行转账。内存记录在程序退出后消失。");
    Ok(())
}
