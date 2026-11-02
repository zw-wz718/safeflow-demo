//! Read existing persisted draft evidence in an explicitly isolated simulation schema.
//! No Runtime, draft Store, migration, sender or business execution is assembled.
use safeflow_contracts::{SessionRef, TaskId, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{LifecycleAuditSourceReader, PgRepository};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        return Err("usage: lifecycle_audit_sources TASK_ID (isolated simulation only)".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "SAFEFLOW_DATABASE_URL required (never printed)")?;
    let schema = std::env::var("SAFEFLOW_STORAGE_SCHEMA")
        .map_err(|_| "SAFEFLOW_STORAGE_SCHEMA required; use your isolated simulation schema")?;
    let repository = PgRepository::connect(&url, &schema).await?;
    let session = SessionRef::parse("isolated_audit_source_example_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let reader =
        LifecycleAuditSourceReader::for_trusted_backend(repository.clone(), Arc::new(sessions));
    let view = reader
        .read_task(&session, &TaskId::parse(&args[0])?)
        .await?;
    // Explicit allowlist: no receipt body, user, session, confirmation or source material.
    let summaries: Vec<_> = view
        .records
        .iter()
        .map(|record| {
            let receipt = record.receipt_for_backend();
            serde_json::json!({
                "event_id":record.event_id,"task_id":record.task_id,
                "runtime_operation_id":record.operation_id,"node_id":record.node_id,
                "tool_name":record.tool_name,"action":receipt.action,
                "runtime_plan_version":receipt.plan_version,
                "business_draft_version":receipt.draft_version,
                "draft_id":receipt.draft_id,"business_operation_id":receipt.business_operation_id,
                "scope":record.scope,"identity_source":record.identity_source,
                "result_captured_at":record.result_captured_at,
                "source_recorded_at":record.source_recorded_at,
                "business_occurred_at":record.business_occurred_at,
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "records":summaries,"unresolved_operations":view.unresolved_operations,
            "operations_without_source":view.operations_without_source,
        }))?
    );
    repository.close().await;
    Ok(())
}
