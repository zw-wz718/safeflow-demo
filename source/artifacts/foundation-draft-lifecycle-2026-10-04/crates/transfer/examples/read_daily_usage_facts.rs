//! Read-only isolated business facts; NOT public identity, time binding or permission.
#[cfg(feature = "postgres-store")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use safeflow_contracts::UserId;
    use safeflow_transfer_integration::funds_store::FundsStore;
    use sqlx::{Connection, PgConnection};

    let owners: Vec<_> = std::env::args().skip(1).collect();
    if owners.is_empty() {
        return Err("expected USER [USER ...]; isolated database only".into());
    }
    let options: sqlx::postgres::PgConnectOptions =
        std::env::var("TRANSFER_STORE_TEST_URL")?.parse()?;
    if options.get_host() != "127.0.0.1"
        || !options
            .get_database()
            .unwrap_or_default()
            .starts_with("transfer_isolated_")
    {
        return Err("refusing a shared or unmarked database".into());
    }
    // The example opens a TEST connection. The business method itself borrows
    // the foundation connection and never opens/commits a second connection.
    let mut conn = PgConnection::connect_with(&options).await?;
    let actual: String = sqlx::query_scalar("SELECT token FROM transfer_isolated_test_guard")
        .fetch_one(&mut conn)
        .await?;
    if actual != std::env::var("TRANSFER_STORE_TEST_GUARD")? {
        return Err("isolated database marker mismatch".into());
    }
    let mut facts_by_owner = serde_json::Map::new();
    for owner in owners {
        let facts = FundsStore::daily_usage_facts(&mut conn, &UserId::parse(&owner)?).await?;
        facts_by_owner.insert(owner, serde_json::to_value(facts)?);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "scope": "ISOLATED_TRANSFER_SQL_FACTS_NOT_PUBLIC_DAILY_USAGE_OR_PERMIT",
            "public_submission_time_bound": false,
            "facts_by_owner": facts_by_owner
        }))?
    );
    Ok(())
}

#[cfg(not(feature = "postgres-store"))]
fn main() {
    eprintln!("enable --features postgres-store and use the isolated test runner");
    std::process::exit(2);
}
