#[path = "support/read_references.rs"]
mod read_references;
#[path = "../../mock-bank/examples/support/mod.rs"]
mod support;

use safeflow_contracts::{ResourceId, SessionRef, TaskId, UserId};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::StaticSessionResolver;
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{
    DraftChanges, DraftInput, DraftView, PayeeSelection, TransferDrafts,
};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    support::block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let same_name = args.iter().any(|arg| arg == "--same-name");
    let selected = args
        .iter()
        .position(|arg| arg == "--select-payee")
        .map(|index| args.get(index + 1).ok_or("--select-payee requires an ID"))
        .transpose()?
        .map(|value| ResourceId::parse(value.clone()))
        .transpose()?;
    let bank = Arc::new(if same_name {
        MockBankGateway::with_same_name_payees()
    } else {
        MockBankGateway::new()
    });
    let mut sessions = StaticSessionResolver::new();
    let session = SessionRef::parse("server_joint_demo_session")?;
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let references = read_references::query_bank(
        bank.clone(),
        Arc::new(sessions),
        session,
        TaskId::parse("server_joint_demo_task")?,
        ResourceId::parse("account_001")?,
        "小李",
    )
    .await?;
    println!("阶段1：查询真实经过MockLlm、Runtime、安全门、只读工具与模拟银行。");
    println!(
        "付款账户：{} {}，查询快照余额{} CNY",
        references.account.account.display_name,
        references.account.account.masked_account_no,
        references.account.available_balance.as_str()
    );
    println!("本人收款人候选：{:?}", references.payees.candidates);
    println!(
        "查询审计事件{}条；查询操作编号{:?}",
        references.audit_event_count, references.query_operations
    );

    println!("阶段2：独立内存草案实验，不是Runtime写工具；没有用户确认、扣款或草案审计。");
    if selected.as_ref().is_some_and(|id| {
        !references
            .payees
            .candidates
            .iter()
            .any(|payee| &payee.payee_id == id)
    }) {
        return Err("选择的收款人不在本人查询结果中".into());
    }
    let mut drafts = TransferDrafts::new(bank, ManualClock::new(1000), 300)?;
    let progress = drafts
        .create(
            &references.context,
            DraftInput {
                payer_account_id: Some(references.account.account.account_id),
                payee_query: Some("小李".into()),
                payee: Some(
                    selected
                        .map(PayeeSelection::Id)
                        .unwrap_or_else(|| PayeeSelection::Name("小李".into())),
                ),
                amount: Some("200.00".into()),
            },
            "午饭AA",
        )
        .await?;
    if let DraftView::NeedPayee(choices) = &progress.view {
        println!("同名尚未选择：{choices:?}。保持等待，没有生成可用草案。");
        println!("选择演示：在命令末尾加 --same-name --select-payee payee_009；固定演示选择不是默认代选。");
        return Ok(());
    }
    let draft = progress.saved.ok_or("草案信息未齐，不能继续")?;
    if references.query_operations.contains(&draft.operation_id) {
        return Err("草案操作编号不能复用查询编号".into());
    }
    println!(
        "同一任务的200元草案已保存并读回：{:?}",
        drafts.read(&references.context, &draft.draft_id)?
    );
    let changed = drafts
        .modify(
            &references.context,
            &draft.draft_id,
            1,
            DraftChanges {
                input: DraftInput {
                    amount: Some("300.00".into()),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await?
        .saved
        .ok_or("修改后需要补充信息")?;
    println!(
        "修改后版本{}；旧版本核对被拒绝：{:?}",
        changed.plan_version,
        drafts
            .current_for_review(&references.context, &draft.draft_id, 1)
            .unwrap_err()
    );
    println!(
        "阶段3：取消未提交草案：{:?}",
        drafts
            .cancel(&references.context, &draft.draft_id, changed.plan_version)?
            .status
    );
    println!("本示例不是完整Agent转账链，查询完成不等于转账成功。");
    Ok(())
}
