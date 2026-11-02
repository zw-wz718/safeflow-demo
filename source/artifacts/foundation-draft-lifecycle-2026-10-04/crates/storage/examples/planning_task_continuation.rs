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
    let mode = args.first().map(String::as_str).unwrap_or("prepare");
    if !matches!(
        (mode, args.len()),
        ("prepare", 0 | 1) | ("continue", 6) | ("recover", 2)
    ) {
        return Err("usage: prepare | continue TASK_ID NODE_ID TASK_REVISION PLAN_VERSION FACTS_VERSION | recover TASK_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "backend database configuration required")?;
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let suffix = schema
        .strip_prefix("sf_runtime_continue_demo_")
        .ok_or("use sf_runtime_continue_demo_<32 hex> namespace")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid isolated demo namespace".into());
    }
    let repo = if mode == "prepare" {
        let repo = PgRepository::connect(&url, &schema).await?;
        repo.migrate().await?;
        repo
    } else {
        PgRepository::connect_existing(&url, &schema).await?
    };
    let session = SessionRef::parse("backend_runtime_continuation_demo_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let (assembly, observed, audit) = support::assemble(Arc::new(sessions), None);
    let runtime = assembly.runtime();
    let continuation = RuntimeTaskContinuationService::for_trusted_backend(
        repo.clone(),
        runtime,
        support::contracts(),
    )?;
    let view = match mode {
        "prepare" => {
            let id = new_task_id();
            let mut proposal = support::proposal(&id, 2);
            proposal.plan.as_mut().unwrap().nodes.extend([
                PlanNode::Tool {
                    id: NodeId::parse("read_two")?,
                    tool: ToolName::parse("get_account_balance")?,
                    arguments: json!({"account_id":"account_001"}),
                    depends_on: vec![NodeId::parse("choose")?],
                },
                PlanNode::UserDecision {
                    id: NodeId::parse("choose_two")?,
                    depends_on: vec![NodeId::parse("read_two")?],
                },
                PlanNode::Tool {
                    id: NodeId::parse("read_three")?,
                    tool: ToolName::parse("get_account_balance")?,
                    arguments: json!({"account_id":"account_001"}),
                    depends_on: vec![NodeId::parse("choose_two")?],
                },
            ]);
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
            let first = RuntimeTaskExecutionService::for_trusted_backend(
                repo.clone(),
                runtime,
                support::contracts(),
            )?
            .schedule(&session, &id, RuntimeTaskVersion::of(&initial))
            .await?;
            assert_eq!(first.task.state, TaskState::WaitingUser);
            assert_eq!(observed.tool.load(Ordering::SeqCst), 1);
            continuation
                .recover(&session, &id, RuntimeTaskVersion::of(&first.task))
                .await?
        }
        "continue" => {
            let id = TaskId::parse(&args[1])?;
            let node = NodeId::parse(&args[2])?;
            continuation
                .continue_task(
                    &session,
                    &id,
                    &node,
                    RuntimeTaskVersion {
                        task_revision: args[3].parse()?,
                        plan_version: args[4].parse()?,
                        facts_version: args[5].parse()?,
                    },
                )
                .await?
        }
        "recover" => {
            let id = TaskId::parse(&args[1])?;
            let progress = TaskQueryService::for_trusted_backend(repo.clone(), runtime)
                .query(&session, &id)
                .await?;
            let view = continuation
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
        }
        _ => unreachable!(),
    };
    let progress = TaskQueryService::for_trusted_backend(repo.clone(), runtime)
        .query(&session, &view.task.task_id)
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode":mode,"task_id":view.task.task_id,"execution":view,"progress":progress,
            "tool_calls":observed.tool.load(Ordering::SeqCst),"policy_calls":observed.policy.load(Ordering::SeqCst),
            "bank_calls":observed.bank.load(Ordering::SeqCst),"runtime_audit_events":audit.events()?.len(),
            "local_mock_bank_only":true,"automatic_claim_retry":false,"funds_execution_enabled":false
        }))?
    );
    repo.close().await;
    Ok(())
}
