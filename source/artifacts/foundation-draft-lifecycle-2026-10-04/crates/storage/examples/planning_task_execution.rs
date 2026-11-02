#[path = "support/planning_runtime.rs"]
mod support;
use safeflow_contracts::*;
use safeflow_runtime::*;
use safeflow_storage::*;
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("complete");
    if !matches!(mode, "complete" | "wait" | "recover")
        || (mode == "recover" && args.len() != 2)
        || (mode != "recover" && args.len() > 1)
    {
        return Err("usage: complete | wait | recover TASK_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "backend database configuration required")?;
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let suffix = schema
        .strip_prefix("sf_runtime_exec_demo_")
        .ok_or("use sf_runtime_exec_demo_<32 hex> namespace")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid isolated demo namespace".into());
    }
    let repo = if mode == "recover" {
        PgRepository::connect_existing(&url, &schema).await?
    } else {
        let repo = PgRepository::connect(&url, &schema).await?;
        repo.migrate().await?;
        repo
    };
    let session = SessionRef::parse("backend_runtime_execution_demo_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let (assembly, observed, audit) = support::assemble(Arc::new(sessions), None);
    let runtime = assembly.runtime();
    let service = RuntimeTaskExecutionService::for_trusted_backend(
        repo.clone(),
        runtime,
        support::contracts(),
    )?;
    let view = if mode == "recover" {
        let id = TaskId::parse(&args[1])?;
        let progress = TaskQueryService::for_trusted_backend(repo.clone(), runtime)
            .query(&session, &id)
            .await?;
        let view = service
            .recover(
                &session,
                &id,
                RuntimeTaskVersion {
                    task_revision: progress.revision,
                    plan_version: progress.plan_version,
                    facts_version: progress.facts_version,
                },
            )
            .await?;
        assert_eq!(observed.tool.load(Ordering::SeqCst), 0);
        view
    } else {
        let id = new_task_id();
        let mut proposal = support::proposal(&id, 2);
        if mode == "complete" {
            proposal.plan.as_mut().unwrap().nodes.pop();
        }
        let gateway = Arc::new(MockLlm::new(proposal));
        let intake = PlanningTaskCreationService::for_trusted_backend(
            repo.clone(),
            runtime,
            Some(gateway.clone()),
            support::contracts(),
        )?;
        let initial = match intake
            .plan_and_create(&session, &id, 2, 7, "查询我的账户余额")
            .await?
        {
            PlanningTaskCreationOutcome::TaskCreated(task) => task,
            _ => return Err("unexpected clarification".into()),
        };
        observed.assert_intake_only();
        assert_eq!(gateway.call_count(), 1);
        let view = service
            .schedule(&session, &id, RuntimeTaskVersion::of(&initial))
            .await?;
        assert_eq!(view.task.task_id, id);
        assert_eq!(observed.tool.load(Ordering::SeqCst), 1);
        assert_eq!(
            view.task.state,
            if mode == "wait" {
                TaskState::WaitingUser
            } else {
                TaskState::Completed
            }
        );
        assert!(service
            .schedule(&session, &id, RuntimeTaskVersion::of(&view.task))
            .await
            .is_err());
        view
    };
    let progress = TaskQueryService::for_trusted_backend(repo.clone(), runtime)
        .query(&session, &view.task.task_id)
        .await?;
    assert_eq!(progress.task_state, view.task.state);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode":mode, "task_id":view.task.task_id, "execution":view, "progress":progress,
            "tool_calls": observed.tool.load(Ordering::SeqCst), "policy_calls":observed.policy.load(Ordering::SeqCst),
            "bank_calls": observed.bank.load(Ordering::SeqCst), "runtime_audit_events":audit.events()?.len(),
            "local_mock_bank_only":true, "automatic_claim_retry":false, "funds_execution_enabled":false
        }))?
    );
    repo.close().await;
    Ok(())
}
