use safeflow_contracts::{
    ExecutionContext, NodeId, OperationId, ResourceId, SessionRef, TaskId, UserId,
};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{SessionResolver, StaticSessionResolver};
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::{
    DraftChanges, DraftInput, DraftView, PayeeSelection, TransferDrafts,
};
use std::sync::Arc;

// Example executor reused from the delivered bank example; no new runtime.
#[path = "../../mock-bank/examples/support/mod.rs"]
mod support;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let same_name = std::env::args().any(|arg| arg == "--same-name");
    // Simulates an ALREADY authenticated server session; never authenticates a
    // client-supplied user_id. Real login is owned by the foundation.
    let mut sessions = StaticSessionResolver::new();
    let session = SessionRef::parse("server_demo_session")?;
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let context = ExecutionContext {
        authenticated_user_id: sessions.resolve(&session)?,
        task_id: TaskId::parse("server_existing_task")?,
        node_id: NodeId::parse("server_query_node")?,
        operation_id: OperationId::parse("server_query_operation")?,
        plan_version: 1,
        idempotency_key: None,
    };
    let bank = if same_name {
        MockBankGateway::with_same_name_payees()
    } else {
        MockBankGateway::new()
    };
    let clock = ManualClock::new(1000);
    let mut drafts = TransferDrafts::new(Arc::new(bank), clock.clone(), 300)?;
    let mut result = support::block_on(drafts.create(
        &context,
        DraftInput {
            // Explicit demo selection from the provider fixture. Not a service
            // default, not a substitute for the missing account-list interface.
            payer_account_id: Some(ResourceId::parse("account_001")?),
            payee: Some(PayeeSelection::Name("小李".into())),
            amount: Some("200".into()),
            ..Default::default()
        },
        "午饭AA",
    ))?;
    println!("模拟资料、内存草案；无确认、无扣款、未接 Runtime 写工具。");
    if let DraftView::NeedPayee(choices) = &result.view {
        println!("同名候选（没有默认选择）：{choices:?}");
        result = support::block_on(drafts.create(
            &context,
            DraftInput {
                // Scripted user choice only in this fixed demo.
                payee: Some(PayeeSelection::Id(ResourceId::parse("payee_009")?)),
                ..Default::default()
            },
            "午饭AA",
        ))?;
    }
    let first = result.saved.ok_or("demo still needs information")?;
    println!("生成并读回：{:?}", drafts.read(&context, &first.draft_id)?);
    let second = support::block_on(drafts.modify(
        &context,
        &first.draft_id,
        1,
        DraftChanges {
            input: DraftInput {
                amount: Some("300.00".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    ))?
    .saved
    .ok_or("modified draft needs information")?;
    println!(
        "修改：版本 {}，金额 {}，操作编号保持 {}",
        second.plan_version,
        second.details.as_ref().unwrap().amount.as_str(),
        second.operation_id
    );
    println!(
        "旧版本核对被拒绝：{:?}",
        drafts
            .current_for_review(&context, &first.draft_id, 1)
            .unwrap_err()
    );
    println!(
        "取消：{:?}",
        drafts.cancel(&context, &first.draft_id, 2)?.status
    );
    println!(
        "取消后核对被拒绝：{:?}",
        drafts
            .current_for_review(&context, &first.draft_id, 2)
            .unwrap_err()
    );
    let mut expiry_context = context.clone();
    expiry_context.task_id = TaskId::parse("another_server_task")?;
    let expiry = support::block_on(drafts.create(
        &expiry_context,
        DraftInput {
            payer_account_id: Some(ResourceId::parse("account_001")?),
            payee_query: Some("小李".into()),
            payee: Some(PayeeSelection::Id(ResourceId::parse("payee_008")?)),
            amount: Some("200.00".into()),
        },
        "",
    ))?
    .saved
    .ok_or("expiry demo needs information")?;
    clock.set(expiry.expires_at);
    println!(
        "到期被拒绝：{:?}",
        drafts
            .current_for_review(&expiry_context, &expiry.draft_id, 1)
            .unwrap_err()
    );
    Ok(())
}
