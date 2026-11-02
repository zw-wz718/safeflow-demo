//! Isolated server-secret exercise; no business Tool, real identity verification or funds.
use safeflow_contracts::{AgentPlan, NodeId, PlanNode, SessionRef, TaskId, ToolName, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::*;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use zeroize::Zeroizing;

fn setting(name: &str) -> Result<u64, Box<dyn std::error::Error>> {
    std::env::var(name)
        .map_err(|_| "explicit backend test setting required; no product default")?
        .parse()
        .map_err(|_| "invalid backend test setting".into())
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map_or(true, |a| {
        !matches!(a.as_str(), "write" | "read" | "revoke" | "reject")
    }) || (args[0] == "write" && args.len() != 1)
        || (args[0] != "write" && args.len() != 3)
    {
        return Err(
            "usage: simulation_auth write | read/revoke/reject TASK_ID SIMULATION_GRANT_ID".into(),
        );
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL")
        .map_err(|_| "database configuration required (never printed)")?;
    let schema = std::env::var("SAFEFLOW_STORAGE_SCHEMA")
        .map_err(|_| "isolated simulation schema required")?;
    let config = SimulationAuthConfig::new(
        Duration::from_secs(setting("SAFEFLOW_SIM_AUTH_CHALLENGE_TTL_SECONDS")?),
        Duration::from_secs(setting("SAFEFLOW_SIM_AUTH_GRANT_TTL_SECONDS")?),
        u32::try_from(setting("SAFEFLOW_SIM_AUTH_MAX_FAILURES")?)?,
        usize::try_from(setting("SAFEFLOW_SIM_AUTH_SECRET_BYTES")?)?,
    )?;
    let repo = PgRepository::connect(&url, &schema).await?;
    repo.migrate().await?;
    let session = SessionRef::parse("isolated_auth_example_backend_session")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), UserId::parse("user_001")?);
    let sessions = Arc::new(sessions);
    let auth = SimulationAuthService::for_isolated_backend(
        repo.clone(),
        sessions.clone(),
        config,
        SimulationAuthMode::FoundationIsolatedV1,
    )?;
    let grant = if args[0] == "write" {
        let storage = PersistenceService::for_trusted_backend(repo.clone(), sessions.clone());
        let confirmation = ConfirmationService::for_trusted_backend(
            repo.clone(),
            sessions.clone(),
            ConfirmationConfig::new(Duration::from_secs(setting(
                "SAFEFLOW_CONFIRMATION_TTL_SECONDS",
            )?))?,
        );
        // Only a foundation-owned stored snapshot. Nothing registers or executes this tool name.
        let task = storage
            .create_task(
                &session,
                AgentPlan::new(
                    new_task_id(),
                    1,
                    vec![PlanNode::Tool {
                        id: NodeId::parse("snapshot")?,
                        tool: ToolName::parse("foundation_snapshot_only")?,
                        arguments: json!({"amount":"200.00", "fixture":"foundation_snapshot_only"}),
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
                NodeId::parse("snapshot")?,
                1,
            )
            .await?;
        let prepared = confirmation
            .prepare(&session, &task.task_id, &op.operation_id)
            .await?;
        let confirmed = confirmation
            .record_explicit_confirmation(
                &session,
                &task.task_id,
                &prepared.confirmation_ref,
                &prepared.binding,
            )
            .await?;
        let mut created = auth
            .create_challenge(
                &session,
                &task.task_id,
                &op.operation_id,
                &confirmed.confirmation_ref,
            )
            .await?;
        let secret = created
            .take_secret()
            .ok_or("first creation did not export backend material")?;
        let proof = SimulationProof::from_material(secret.expose_for_isolated_backend())?;
        let mut incorrect = Zeroizing::new(secret.expose_for_isolated_backend().to_vec());
        incorrect[0] ^= 1;
        let incorrect = SimulationProof::from_material(&incorrect)?;
        drop(secret);
        let failed_id = SimulationSubmissionId::new_server_id();
        let failed = auth
            .submit(
                &session,
                &task.task_id,
                &created.record().challenge_id,
                &failed_id,
                &incorrect,
            )
            .await?;
        if failed.result != SimulationVerificationResult::NotVerified
            || failed.failures != 1
            || auth
                .submit(
                    &session,
                    &task.task_id,
                    &created.record().challenge_id,
                    &failed_id,
                    &incorrect,
                )
                .await?
                != failed
        {
            return Err("incorrect material/retry changed attempt count".into());
        }
        let submission = SimulationSubmissionId::new_server_id();
        let result = auth
            .submit(
                &session,
                &task.task_id,
                &created.record().challenge_id,
                &submission,
                &proof,
            )
            .await?;
        if auth
            .submit(
                &session,
                &task.task_id,
                &created.record().challenge_id,
                &submission,
                &proof,
            )
            .await?
            != result
        {
            return Err("successful replay differed".into());
        }
        let after = storage.recover(&session, &task.task_id).await?;
        if after.operations != vec![op]
            || after.task.revision != confirmed.binding.task_revision
            || after.task.facts_version != confirmed.binding.facts_version
        {
            return Err("authentication changed task/operation snapshot".into());
        }
        result.grant.ok_or("no simulation grant issued")?
    } else {
        let task = TaskId::parse(&args[1])?;
        let id = SimulationGrantId::parse(&args[2])?;
        match args[0].as_str() {
            "read" => {
                let read = auth.read_grant(&session, &task, &id).await?;
                let verified = auth
                    .verify_grant(&session, &task, &id, &read.binding)
                    .await?;
                if read != verified {
                    return Err("restored grant changed".into());
                }
                verified
            }
            "revoke" => {
                let revoked = auth.revoke_grant(&session, &task, &id).await?;
                if revoked != auth.revoke_grant(&session, &task, &id).await? {
                    return Err("revocation replay changed".into());
                }
                revoked
            }
            "reject" => {
                if auth.read_grant(&session, &task, &id).await
                    != Err(StorageError::AuthGrantRevoked)
                {
                    return Err("revoked grant accepted".into());
                }
                println!(
                    "{}",
                    json!({"rejected":"AUTH_GRANT_REVOKED","business_executed":false,"runtime_funds_enabled":false})
                );
                repo.close().await;
                return Ok(());
            }
            _ => unreachable!(),
        }
    };
    if auth
        .request_formal_auth(&session, &grant.binding.task_id)
        .await
        != Err(StorageError::FormalAuthUnavailable)
    {
        return Err("formal authentication unexpectedly enabled".into());
    }
    let permits = PermitStore::for_trusted_backend(
        repo.clone(),
        sessions,
        PermitConfig::new(Duration::from_secs(setting(
            "SAFEFLOW_SIM_AUTH_GRANT_TTL_SECONDS",
        )?))?,
    );
    if permits
        .request_issuance(&session, &grant.binding.task_id)
        .await
        .err()
        != Some(StorageError::PermitIssuanceUnavailable)
    {
        return Err("formal Permit unexpectedly enabled".into());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"grant":grant,"business_executed":false,
        "runtime_funds_enabled":false,"formal_auth_enabled":false,"formal_permit_enabled":false}))?
    );
    repo.close().await;
    Ok(())
}
