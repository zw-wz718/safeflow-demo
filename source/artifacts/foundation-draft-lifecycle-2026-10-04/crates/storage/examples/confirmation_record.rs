//! Isolated foundation storage exercise. No business plugin or Runtime Tool executes.
use safeflow_contracts::{
    AgentPlan, ConfirmationRef, NodeId, PlanNode, SessionRef, TaskId, ToolName, UserId,
};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{
    new_operation_id, new_task_id, ConfirmationConfig, ConfirmationService, ConfirmationStatus,
    PersistenceService, PgRepository, StorageError,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map_or(true, |arg| {
        !matches!(arg.as_str(), "write" | "read" | "revoke" | "reject")
    }) || (args[0] == "write" && args.len() != 1)
        || (args[0] != "write" && args.len() != 3)
    {
        return Err(
            "usage: confirmation_record write | read/revoke/reject TASK_ID CONFIRMATION_REF".into(),
        );
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "SAFEFLOW_DATABASE_URL required (never printed)")?;
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let ttl = std::env::var("SAFEFLOW_CONFIRMATION_TTL_SECONDS")
        .map_err(|_| "explicit server test TTL required; no product default")?
        .parse::<u64>()
        .map_err(|_| "invalid test TTL")?;
    let repo = PgRepository::connect(&url, &schema).await?;
    repo.migrate().await?;
    let session = SessionRef::parse("isolated_confirmation_example_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let sessions = Arc::new(sessions);
    let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions.clone());
    let confirmations = ConfirmationService::for_trusted_backend(
        repo.clone(),
        sessions,
        ConfirmationConfig::new(Duration::from_secs(ttl))?,
    );
    if args[0] == "write" {
        let task_id = new_task_id();
        // A stored public-contract fixture, not a registered Tool or transfer draft.
        let plan = AgentPlan::new(
            task_id.clone(),
            1,
            vec![PlanNode::Tool {
                id: NodeId::parse("snapshot")?,
                tool: ToolName::parse("foundation_snapshot_only")?,
                arguments: serde_json::json!({"amount":"200.00","fixture":"foundation_snapshot_only"}),
                depends_on: vec![],
            }],
        )?;
        let task = storage.create_task(&session, plan, 1).await?;
        let operation = storage
            .create_operation(
                &session,
                &task_id,
                task.revision,
                new_operation_id(),
                NodeId::parse("snapshot")?,
                1,
            )
            .await?;
        let prepared = confirmations
            .prepare(&session, &task_id, &operation.operation_id)
            .await?;
        // The isolated harness acts as the explicit-action backend, with the displayed binding.
        // Production must provide its own confirmed UI/action handler; no LLM route exists.
        let confirmed = confirmations
            .record_explicit_confirmation(
                &session,
                &task_id,
                &prepared.confirmation_ref,
                &prepared.binding,
            )
            .await?;
        let replay = confirmations
            .record_explicit_confirmation(
                &session,
                &task_id,
                &prepared.confirmation_ref,
                &prepared.binding,
            )
            .await?;
        if confirmed != replay || confirmed.expires_at != prepared.expires_at {
            return Err("confirmation replay changed record".into());
        }
        println!("{}", serde_json::to_string_pretty(&confirmed)?);
    } else {
        let task_id = TaskId::parse(&args[1])?;
        let reference = ConfirmationRef::parse(&args[2])?;
        match args[0].as_str() {
            "read" => {
                let record = confirmations.read(&session, &task_id, &reference).await?;
                let verified = confirmations
                    .verify(&session, &task_id, &reference, &record.binding)
                    .await?;
                if record != verified || verified.status != ConfirmationStatus::Confirmed {
                    return Err("restored confirmation changed".into());
                }
                println!("{}", serde_json::to_string_pretty(&verified)?);
            }
            "revoke" => {
                let revoked = confirmations.revoke(&session, &task_id, &reference).await?;
                let replay = confirmations.revoke(&session, &task_id, &reference).await?;
                if revoked != replay {
                    return Err("revocation replay changed record".into());
                }
                println!("{}", serde_json::to_string_pretty(&revoked)?);
            }
            "reject" => {
                if confirmations.read(&session, &task_id, &reference).await
                    != Err(StorageError::ConfirmationRevoked)
                {
                    return Err("revoked record was not rejected".into());
                }
                println!("{{\"rejected\":\"CONFIRMATION_REVOKED\",\"business_executed\":false}}");
            }
            _ => unreachable!(),
        }
    }
    repo.close().await;
    Ok(())
}
