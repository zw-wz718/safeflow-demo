use safeflow_contracts::{
    AgentPlan, NodeId, PlanNode, SessionRef, TaskEvent, TaskId, ToolName, UserId,
};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{
    new_operation_id, new_task_id, CheckpointChange, PersistenceService, PgRepository,
    RecoveryPoint,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if !matches!(
        arguments.first().map(String::as_str),
        Some("write" | "read")
    ) || (arguments[0] == "read" && arguments.len() != 2)
        || (arguments[0] == "write" && arguments.len() != 1)
    {
        return Err("usage: recovery_checkpoint write | read TASK_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "SAFEFLOW_DATABASE_URL is required (never printed)")?;
    let namespace = std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| {
        "SAFEFLOW_STORAGE_SCHEMA is required; use an isolated sf_step4a_demo_* schema"
    })?;
    let repo = PgRepository::connect(&url, &namespace).await?;
    repo.migrate().await?;
    let session = SessionRef::parse("mock_backend_session_001")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let service = PersistenceService::for_trusted_backend(repo.clone(), Arc::new(sessions));
    let task_id = if arguments[0] == "write" {
        let task_id = new_task_id();
        let plan = AgentPlan::new(
            task_id.clone(),
            1,
            vec![
                PlanNode::UserDecision {
                    id: NodeId::parse("choose_payee")?,
                    depends_on: vec![],
                },
                PlanNode::Tool {
                    id: NodeId::parse("save_draft")?,
                    tool: ToolName::parse("create_mock_transfer_draft")?,
                    arguments: serde_json::json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":""}),
                    depends_on: vec![NodeId::parse("choose_payee")?],
                },
            ],
        )?;
        service.create_task(&session, plan, 1).await?;
        service
            .create_operation(
                &session,
                &task_id,
                1,
                new_operation_id(),
                NodeId::parse("save_draft")?,
                1,
            )
            .await?;
        service
            .save_checkpoint(
                &session,
                &task_id,
                CheckpointChange {
                    expected_revision: 2,
                    event: Some(TaskEvent::AwaitUser),
                    checkpoint: RecoveryPoint::AtNode {
                        node_id: NodeId::parse("choose_payee")?,
                    },
                    completed_nodes: vec![],
                    facts_version: 2,
                },
            )
            .await?;
        task_id
    } else {
        TaskId::parse(&arguments[1])?
    };
    // A fresh process reads the same IDs, progress and audit; it never executes a Tool.
    println!(
        "{}",
        serde_json::to_string_pretty(&service.recover(&session, &task_id).await?)?
    );
    repo.close().await;
    Ok(())
}
