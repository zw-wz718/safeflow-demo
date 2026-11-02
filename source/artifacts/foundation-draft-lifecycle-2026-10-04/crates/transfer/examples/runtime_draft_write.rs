#[path = "support/mock_draft_write.rs"]
mod mock_draft_write;
#[path = "support/read_references.rs"]
mod read_references;

use mock_draft_write::{assemble, context, DemoClock};
use safeflow_contracts::{NodeId, ResourceId, SessionRef};
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::RuntimeOutcome;
use safeflow_transfer::fixtures::ManualClock;
use serde_json::json;
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    futures::executor::block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    let same_name = arguments.iter().any(|a| a == "--same-name");
    let selected = arguments
        .iter()
        .position(|a| a == "--select-payee")
        .map(|index| {
            arguments
                .get(index + 1)
                .cloned()
                .ok_or("--select-payee requires an ID")
        })
        .transpose()?;
    let bank = Arc::new(if same_name {
        MockBankGateway::with_same_name_payees()
    } else {
        MockBankGateway::new()
    });
    let app = assemble(bank.clone(), DemoClock(ManualClock::new(1_791_065_600)))?;
    let context = context("mock_write_demo_task");
    let session = SessionRef::parse("mock_session_001")?;
    let references = read_references::query_bank(
        bank,
        app.sessions.clone(),
        session.clone(),
        context.task_id.clone(),
        ResourceId::parse("account_001")?,
        "小李",
    )
    .await?;
    println!(
        "模拟查询已通过公共 Runtime；余额 {} CNY，查询审计 {} 条，查询操作 {:?}",
        references.account.available_balance.as_str(),
        references.audit_event_count,
        references.query_operations
    );
    let candidates = app
        .store
        .find_candidates(&references.context, "小李")
        .await?;
    if candidates != references.payees {
        return Err("候选资料已变化，需重新展示后确认".into());
    }
    let payee = match selected {
        Some(id) => candidates
            .candidates
            .iter()
            .find(|p| p.payee_id.as_str() == id)
            .ok_or("所选 ID 不在本人候选中")?,
        None if candidates.candidates.len() == 1 => &candidates.candidates[0],
        _ => {
            println!(
                "WAITING_SELECTION：同名候选 {:?}；尚未确认或保存草案",
                candidates.candidates
            );
            println!(
                "实际草案：{:?}",
                app.store.read_created_for_task(&context).await?
            );
            return Ok(());
        }
    };
    let request = app.authority.request(session, context.task_id.clone(), NodeId::parse("save_mock_draft")?, 1,
        json!({"payer_account_id": "account_001", "payee_id": payee.payee_id, "amount": "200.00", "currency": "CNY", "remark": "", "replaces": null}), 60)?;
    println!("模拟用户显式确认：只保存上述200.00 CNY草案，不授权扣款");
    // This is the mock server/UI boundary, never an LLM or DomainPolicy action.
    let confirmation = app.authority.record_explicit_confirmation(&request)?;
    let permit = app.authority.issue(&confirmation)?;
    let outcome = app
        .assembly
        .runtime()
        .execute_mock_draft(request.clone(), &permit)
        .await?;
    let RuntimeOutcome::Completed { results, .. } = &outcome else {
        return Err("草案未完成".into());
    };
    println!(
        "公共 Runtime 写入回执：{}",
        serde_json::to_string_pretty(&results[0].data)?
    );
    let readback = app
        .store
        .read_created_for_task(&context)
        .await?
        .ok_or("读回失败")?;
    println!(
        "读回核验：{}，版本{}，状态{:?}",
        readback.draft_id, readback.plan_version, readback.status
    );
    let repeated = app
        .assembly
        .runtime()
        .execute_mock_draft(request, &permit)
        .await?;
    if repeated != outcome || app.store.saved_receipt_count()? != 1 {
        return Err("重复执行检查失败".into());
    }
    println!(
        "同一请求重试返回同一结果，草案保存1次；写入审计{}条",
        app.audit.events()?.len()
    );
    println!("单进程Mock内存草案；没有资金提交、真实认证或生产Permit v1");
    Ok(())
}
