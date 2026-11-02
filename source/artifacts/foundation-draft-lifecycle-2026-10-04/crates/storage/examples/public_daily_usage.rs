//! Read public-bound facts in an explicitly isolated local schema; no issuer.
use safeflow_contracts::{SessionRef, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{ExecutionRequestStore, PgRepository};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = std::env::var("SAFEFLOW_STORAGE_SCHEMA")?;
    let suffix = schema
        .strip_prefix("sf_permit_test_")
        .ok_or("isolated test schema required")?;
    if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid isolated schema".into());
    }
    // Explicit trusted test owner, never a client identity/login endpoint.
    let owner = UserId::parse(std::env::args().nth(1).ok_or("test owner required")?)?;
    let repository =
        PgRepository::connect_existing(&std::env::var("SAFEFLOW_DATABASE_URL")?, &schema).await?;
    let session = SessionRef::parse("isolated-daily-example")?;
    let mut sessions = StaticSessionResolver::new();
    sessions.insert(session.clone(), owner);
    let store = ExecutionRequestStore::for_trusted_backend(repository.clone(), Arc::new(sessions));
    let snapshot = store.daily_usage(&session).await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scope":"ISOLATED_PUBLIC_SQL_BINDING_NOT_FINAL_FUNDS_PERMISSION",
            "snapshot":snapshot,
        "final_permit_issuance":"UNAVAILABLE_MISSING_ADOPTED_FUNDS_PROFILE_CARD_COMMIT_LOCK_AND_REQUIRED_AUTH_FACTS"
        }))?
    );
    repository.close().await;
    Ok(())
}
