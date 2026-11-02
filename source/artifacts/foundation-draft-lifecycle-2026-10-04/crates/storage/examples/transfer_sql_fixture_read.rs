//! Read-only new-process probe of the seven received isolated SQL fixtures.
//! Fixed sample principals are test identities, never aliases for API users.
use safeflow_contracts::{OperationId, ResourceId, SessionRef, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{PgRepository, TransferSqlReader};
use serde_json::json;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema =
        std::env::var("SAFEFLOW_STORAGE_SCHEMA").map_err(|_| "isolated schema required")?;
    let suffix = schema
        .strip_prefix("sf_permit_test_")
        .ok_or("isolated fixture schema required")?;
    if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("isolated fixture schema required".into());
    }
    let url = std::env::var("SAFEFLOW_DATABASE_URL").map_err(|_| "dedicated database required")?;
    let repo = PgRepository::connect_existing(&url, &schema).await?;
    let mut cases = Vec::new();
    for name in [
        "success",
        "failure",
        "pending",
        "lost_reply",
        "settled",
        "released",
        "rollback",
    ] {
        let owner = UserId::parse(format!("sql-owner-{name}"))?;
        let recipient = UserId::parse(format!("sql-recipient-{name}"))?;
        let session = SessionRef::parse("isolated-fixture-payer")?;
        let receiving_session = SessionRef::parse("isolated-fixture-recipient")?;
        let mut sessions = StaticSessionResolver::new();
        sessions.insert(session.clone(), owner);
        sessions.insert(receiving_session.clone(), recipient);
        let reader = TransferSqlReader::for_trusted_backend(repo.clone(), Arc::new(sessions));
        let record = reader
            .business(
                &session,
                &OperationId::parse(format!("sql-operation-{name}"))?,
            )
            .await?;
        let payer = reader
            .funds(&session, &ResourceId::parse(format!("sql-payer-{name}"))?)
            .await?;
        let recipient = reader
            .funds(
                &receiving_session,
                &ResourceId::parse(format!("sql-recipient-account-{name}"))?,
            )
            .await?;
        let debit = reader.transactions(&session).await?;
        let credit = reader.transactions(&receiving_session).await?;
        cases.push(json!({"case":name,"record":record,"payer":payer,"recipient":recipient,"debit":debit,"credit":credit}));
    }
    println!(
        "{}",
        serde_json::to_string(
            &json!({"scope":"ISOLATED_TRANSFER_SQL_FIXTURE_READ_ONLY","cases":cases})
        )?
    );
    repo.close().await;
    Ok(())
}
