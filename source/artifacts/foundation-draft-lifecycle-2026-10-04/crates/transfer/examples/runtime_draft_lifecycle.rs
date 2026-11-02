#[path = "support/mock_draft_write.rs"]
mod support;

use safeflow_contracts::{MockDraftAction, NodeId, SessionRef};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::{MockDraftWriteRequest, RuntimeOutcome, SessionResolver};
use safeflow_transfer::fixtures::ManualClock;
use safeflow_transfer_integration::DraftStatus;
use serde_json::{json, Value};
use std::sync::Arc;
use support::{assemble, context, App, DemoClock};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    futures::executor::block_on(run())
}

async fn confirmed(
    app: &App,
    action: MockDraftAction,
    version: u32,
    arguments: Value,
) -> Result<(MockDraftWriteRequest, RuntimeOutcome), Box<dyn std::error::Error>> {
    let request = app.authority.request_for_action(
        action,
        SessionRef::parse("mock_session_001")?,
        context("lifecycle_demo").task_id,
        NodeId::parse(format!("lifecycle_{version}"))?,
        version,
        arguments,
        60,
    )?;
    println!(
        "模拟用户显式确认 {}，仅操作内存草案，不授权资金",
        action.action()
    );
    let confirmation = app.authority.record_explicit_confirmation(&request)?;
    let permit = app.authority.issue(&confirmation)?;
    let outcome = app
        .assembly
        .runtime()
        .execute_mock_draft(request.clone(), &permit)
        .await?;
    let repeated = app
        .assembly
        .runtime()
        .execute_mock_draft(request.clone(), &permit)
        .await?;
    if repeated != outcome {
        return Err("重复请求结果不一致".into());
    }
    if let RuntimeOutcome::Completed { results, .. } = &outcome {
        println!("{}", serde_json::to_string_pretty(&results[0].data)?);
    } else {
        return Err("未完成草案操作".into());
    }
    println!("同请求重复返回相同回执，仅执行一次");
    Ok((request, outcome))
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let same_name = args.iter().any(|arg| arg == "--same-name");
    let selected = args
        .iter()
        .position(|arg| arg == "--select-payee")
        .map(|index| args.get(index + 1).ok_or("--select-payee 缺少 ID"))
        .transpose()?;
    let bank = Arc::new(if same_name {
        MockBankGateway::with_same_name_payees()
    } else {
        MockBankGateway::new()
    });
    let app = assemble(bank, DemoClock(ManualClock::new(1_791_065_600)))?;
    let ctx = context("lifecycle_demo");
    // Shared session resolver is used by both Runtime and the trusted authority.
    if app
        .sessions
        .resolve(&SessionRef::parse("mock_session_001")?)
        .is_err()
    {
        return Err("演示会话无效".into());
    }
    let candidates = app.store.find_candidates(&ctx, "小李").await?;
    let payee = match selected {
        Some(id) => candidates
            .candidates
            .iter()
            .find(|p| p.payee_id.as_str() == id)
            .ok_or("所选收款人不在候选中")?,
        None if candidates.candidates.len() == 1 => &candidates.candidates[0],
        _ => {
            println!(
                "WAITING_SELECTION：候选 {:?}；没有确认或保存草案",
                candidates.candidates
            );
            if app.store.read_created_for_task(&ctx).await?.is_some() {
                return Err("不应保存".into());
            }
            return Ok(());
        }
    };
    confirmed(
        &app,
        MockDraftAction::Create,
        1,
        json!({"payer_account_id":"account_001", "payee_id":payee.payee_id,
            "amount":"200.00", "currency":"CNY", "remark":"模拟草案"}),
    )
    .await?;
    let initial = app
        .store
        .read_created_for_task(&ctx)
        .await?
        .ok_or("创建读回失败")?;
    confirmed(
        &app,
        MockDraftAction::Modify,
        2,
        json!({"draft_id":initial.draft_id, "expected_version":1,
            "changes":{"amount":"300.00", "remark":""}}),
    )
    .await?;
    let updated = app.store.read(&ctx, &initial.draft_id).await?;
    if updated.draft_id != initial.draft_id
        || updated.operation_id != initial.operation_id
        || updated.plan_version != 2
        || updated.expires_at != initial.expires_at
        || updated
            .details
            .as_ref()
            .ok_or("无详细字段")?
            .amount
            .as_str()
            != "300.00"
    {
        return Err("修改读回失败".into());
    }
    println!("读回：草案与业务操作 ID 不变，业务版本 1 -> 2，有效期不延长，备注已清空");
    let stale = app.authority.request_for_action(
        MockDraftAction::Modify,
        SessionRef::parse("mock_session_001")?,
        ctx.task_id.clone(),
        NodeId::parse("stale_modify")?,
        3,
        json!({"draft_id":initial.draft_id, "expected_version":1, "changes":{"amount":"400.00"}}),
        60,
    )?;
    let confirmation = app.authority.record_explicit_confirmation(&stale)?;
    let permit = app.authority.issue(&confirmation)?;
    let rejected = app
        .assembly
        .runtime()
        .execute_mock_draft(stale, &permit)
        .await;
    if rejected.is_ok() {
        return Err("旧业务版本不应成功".into());
    }
    println!("旧业务版本拒绝：{}", rejected.unwrap_err());
    confirmed(
        &app,
        MockDraftAction::Cancel,
        4,
        json!({"draft_id":initial.draft_id, "expected_version":2}),
    )
    .await?;
    let cancelled = app.store.read(&ctx, &initial.draft_id).await?;
    if cancelled.status != DraftStatus::Cancelled
        || cancelled.plan_version != 2
        || cancelled.expires_at != initial.expires_at
    {
        return Err("取消读回失败".into());
    }
    println!(
        "读回：CANCELLED，业务版本仍为 2；可信回执 {} 条，审计 {} 条",
        app.store.saved_receipt_count()?,
        app.audit.events()?.len()
    );
    println!("仅单进程 Mock；无真实银行、扣款、生产认证或草案数据库化");
    Ok(())
}
