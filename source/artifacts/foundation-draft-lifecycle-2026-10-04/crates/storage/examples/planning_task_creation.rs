#[path = "support/planning_runtime.rs"]
mod support;
use safeflow_contracts::*;
use safeflow_runtime::*;
use safeflow_storage::{
    new_task_id, PgRepository, PlanningTaskCreationOutcome, PlanningTaskCreationService,
    TaskQueryService,
};
use serde_json::json;
use std::sync::{atomic::Ordering, Arc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("typed");
    if !matches!(mode, "typed" | "raw" | "clarify" | "reject" | "query")
        || (mode == "query" && args.len() != 2)
        || (mode != "query" && args.len() > 1)
    {
        return Err("usage: typed | raw | clarify | reject | query TASK_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "backend database configuration required")?;
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let suffix = schema
        .strip_prefix("sf_planning_create_demo_")
        .ok_or("use sf_planning_create_demo_<32 hex> namespace")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid demo namespace".into());
    }
    let repo = if mode == "query" {
        PgRepository::connect_existing(&url, &schema).await?
    } else {
        let repo = PgRepository::connect(&url, &schema).await?;
        repo.migrate().await?;
        repo
    };
    let session = SessionRef::parse("backend_planning_create_demo_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let (assembly, observed, audit) = support::assemble(Arc::new(sessions), None);
    let id = if mode == "query" {
        TaskId::parse(&args[1])?
    } else {
        new_task_id()
    };
    let (status, view, gateway_calls) = if mode == "query" {
        let view = TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
            .query(&session, &id)
            .await?;
        ("TASK_QUERIED", Some(view), 0)
    } else {
        let mut proposal = support::proposal(&id, 2);
        if mode == "clarify" {
            proposal.understanding.decision = UnderstandingDecision::Clarify;
            proposal.understanding.missing_fields = vec!["account_id".into()];
            proposal.plan = None;
            proposal.clarification = Some(Clarification {
                question: "查询哪一个账户？".into(),
            });
        }
        if mode == "reject" {
            if let PlanNode::Tool { tool, .. } = &mut proposal.plan.as_mut().unwrap().nodes[0] {
                *tool = ToolName::parse("unregistered_tool")?;
            }
        }
        // Frozen PlanningInput has no task context. MockLlm is prebound for this demonstration.
        let mock = Arc::new(MockLlm::new(proposal.clone()));
        let gateway = if mode == "raw" {
            None
        } else {
            Some(mock.clone() as Arc<dyn LlmGateway>)
        };
        let service = PlanningTaskCreationService::for_trusted_backend(
            repo.clone(),
            assembly.runtime(),
            gateway,
            support::contracts(),
        )?;
        let result = if mode == "raw" {
            service
                .accept_json_and_create(&session, &id, 2, 7, &serde_json::to_vec(&proposal)?)
                .await
        } else {
            service
                .plan_and_create(&session, &id, 2, 7, "查询我的账户余额")
                .await
        };
        match result {
            Ok(PlanningTaskCreationOutcome::TaskCreated(saved)) => {
                assert!(matches!(mode, "typed" | "raw"));
                assert_eq!(saved.state, TaskState::Running);
                assert_eq!(saved.revision, 1);
                assert!(saved.completed_nodes.is_empty());
                let view = TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
                    .query(&session, &id)
                    .await?;
                ("INITIAL_TASK_CREATED", Some(view), mock.call_count())
            }
            Ok(PlanningTaskCreationOutcome::Clarification(_)) => {
                assert_eq!(mode, "clarify");
                assert_eq!(
                    TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
                        .query(&session, &id)
                        .await
                        .err(),
                    Some(safeflow_storage::StorageError::NotFound)
                );
                ("CLARIFICATION_NO_TASK", None, mock.call_count())
            }
            Err(error) => {
                if mode != "reject" {
                    return Err(error.into());
                }
                assert_eq!(
                    error,
                    safeflow_storage::PlanningTaskCreationError::Intake(
                        PlanningIntakeError::Registration
                    )
                );
                assert_eq!(
                    TaskQueryService::for_trusted_backend(repo.clone(), assembly.runtime())
                        .query(&session, &id)
                        .await
                        .err(),
                    Some(safeflow_storage::StorageError::NotFound)
                );
                ("REJECTED_NO_TASK", None, mock.call_count())
            }
        }
    };
    observed.assert_intake_only();
    assert!(audit.events()?.is_empty());
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode":mode, "status":status, "task_id":id, "view":view,
            "gateway_calls":gateway_calls, "tool_calls":observed.tool.load(Ordering::SeqCst),
            "policy_calls":observed.policy.load(Ordering::SeqCst), "bank_calls":observed.bank.load(Ordering::SeqCst),
            "runtime_audit_events":audit.events()?.len(), "execution_authority_provided":false,
            "automatic_execution":false, "local_mock_only":true, "initializer_migrates_schema":mode!="query"
        }))?
    );
    repo.close().await;
    Ok(())
}
