//! Actual preflight evaluation using the existing bank queries, with no Tool execution.
#[path = "support/decision_runtime.rs"]
mod support;
use safeflow_contracts::*;
use safeflow_mock_bank::MockBankGateway;
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::*;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let write = args.first().is_some_and(|s| s == "write");
    if (write
        && !(args.len() == 1
            || args.len() == 2 && matches!(args[1].as_str(), "balance" | "payees" | "blocked")))
        || (!write
            && !(args.len() == 3 && matches!(args[0].as_str(), "read" | "revoke" | "reject")))
    {
        return Err("usage: security_decision write [balance|payees|blocked] | read/revoke/reject TASK_ID STORED_DECISION_ID".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "server database configuration required (never printed)")?;
    let schema = std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "example schema required")?;
    let suffix = schema
        .strip_prefix("sf_decision_demo_")
        .ok_or("use isolated decision demonstration schema")?;
    if suffix.len() != 32
        || !suffix
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("invalid demonstration schema".into());
    }
    let ttl = std::env::var("SAFEFLOW_DECISION_TTL_SECONDS")
        .map_err(|_| "explicit demonstration TTL required")?
        .parse::<u64>()
        .map_err(|_| "invalid TTL")?;
    let revision = std::env::var("SAFEFLOW_DECISION_DEPLOYMENT_REVISION")
        .map_err(|_| "trusted deployment revision required")?;
    let session = SessionRef::parse("trusted_decision_example_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let sessions = Arc::new(sessions);
    let calls = Arc::new(AtomicUsize::new(0));
    let assembly = support::assemble(
        sessions.clone(),
        Arc::new(MockBankGateway::new()),
        SecurityLevel::Green,
        calls.clone(),
    );
    let repo = PgRepository::connect(&url, &schema).await?;
    repo.migrate().await?;
    let service = DecisionService::for_trusted_backend(
        repo.clone(),
        assembly.runtime(),
        DecisionConfig::new(
            Duration::from_secs(ttl),
            revision,
            support::input_contracts(),
        )?,
    )?;
    let record = if write {
        let (tool, arguments) = match args.get(1).map(String::as_str).unwrap_or("balance") {
            "payees" => ("find_payees", serde_json::json!({"query":"小李"})),
            "blocked" => (
                "get_account_balance",
                serde_json::json!({"account_id":"account_other"}),
            ),
            _ => (
                "get_account_balance",
                serde_json::json!({"account_id":"account_001"}),
            ),
        };
        let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions);
        let task = storage
            .create_task(
                &session,
                AgentPlan::new(
                    new_task_id(),
                    1,
                    vec![PlanNode::Tool {
                        id: NodeId::parse("query")?,
                        tool: ToolName::parse(tool)?,
                        arguments,
                        depends_on: vec![],
                    }],
                )?,
                1,
            )
            .await?;
        let op = storage
            .create_operation(
                &session,
                &task.task_id,
                task.revision,
                new_operation_id(),
                NodeId::parse("query")?,
                1,
            )
            .await?;
        let request = DecisionRequestId::new_server_id();
        let r = service
            .evaluate_and_save(
                &session,
                &task.task_id,
                &op.operation_id,
                &request,
                SecurityPhase::PreTool,
            )
            .await?;
        let replay = service
            .evaluate_and_save(
                &session,
                &task.task_id,
                &op.operation_id,
                &request,
                SecurityPhase::PreTool,
            )
            .await?;
        if replay != r {
            return Err("replay changed or extended record".into());
        }
        let expected = if args.get(1).is_some_and(|s| s == "blocked") {
            SecurityOutcome::Blocked
        } else {
            SecurityOutcome::Allow
        };
        if r.decision.outcome != expected {
            return Err("unexpected actual Kernel/Policy outcome".into());
        }
        Some(r)
    } else {
        let task = TaskId::parse(&args[1])?;
        let id = StoredDecisionId::parse(&args[2])?;
        match args[0].as_str() {
            "read" => {
                let r = service.read(&session, &task, &id).await?;
                if service.verify(&session, &task, &id, &r.binding).await? != r {
                    return Err("restored record failed verification".into());
                }
                Some(r)
            }
            "revoke" => {
                let r = service.revoke(&session, &task, &id).await?;
                if service.revoke(&session, &task, &id).await? != r
                    || r.status != StoredDecisionStatus::Revoked
                {
                    return Err("revocation failed".into());
                }
                Some(r)
            }
            "reject" => {
                if service.read(&session, &task, &id).await.err()
                    != Some(StorageError::DecisionInactive)
                {
                    return Err("revoked record was accepted".into());
                }
                None
            }
            _ => unreachable!(),
        }
    };
    if calls.load(Ordering::SeqCst) != 0 {
        return Err("preflight unexpectedly executed a Tool".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({"record":record,
        "rejected":if record.is_none(){Some("DECISION_INACTIVE")}else{None},
        "tool_execute_calls":calls.load(Ordering::SeqCst),"business_executed":false,
        "formal_permit_enabled":false,"runtime_funds_enabled":false}))?
    );
    repo.close().await;
    Ok(())
}
