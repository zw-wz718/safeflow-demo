//! Local user-safe read against the disposable shared-proposal test DB ONLY.
//! CLI owner/namespace are fictional fixtures, never production authentication.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}
#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use safeflow_subscription_evidence::confirmation_mock::shared_query_error_view_mock;
    let (view, ok) = match run().await {
        Ok(v) => (v, true),
        Err(e) => (shared_query_error_view_mock(e), false),
    };
    println!("{view}");
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
#[cfg(feature = "postgres-mock-store")]
async fn run() -> Result<
    serde_json::Value,
    safeflow_subscription_evidence::confirmation_mock::ProposedLedgerWriteIssue,
> {
    use safeflow_subscription_evidence::confirmation_mock::{
        ProposedLedgerStoreMock, ProposedLedgerWriteIssue as Issue, SharedSubscriptionReadQueryMock,
    };
    use sqlx::{Connection, Executor};
    use std::str::FromStr;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 4
        || args[0] != "--run-fictional"
        || !args[1].starts_with("sub_write_check_")
        || args[2].trim().is_empty()
    {
        return Err(Issue::Conflict);
    }
    let raw = std::fs::read_to_string(&args[3]).map_err(|_| Issue::Conflict)?;
    let query: SharedSubscriptionReadQueryMock =
        serde_json::from_str(&raw).map_err(|_| Issue::Conflict)?;
    let options = sqlx::postgres::PgConnectOptions::from_str(
        &std::env::var("SAFEFLOW_SUBSCRIPTION_SHARED_QUERY_URL").map_err(|_| Issue::Unavailable)?,
    )
    .map_err(|_| Issue::Unavailable)?;
    if options.get_host() != "127.0.0.1"
        || options.get_database() != Some("safeflow_subscription_write_test")
    {
        return Err(Issue::VerificationDatabaseMismatch);
    }
    let store = ProposedLedgerStoreMock::new(&args[1]).map_err(|_| Issue::Unavailable)?;
    let mut c = sqlx::PgConnection::connect_with(&options)
        .await
        .map_err(|_| Issue::Unavailable)?;
    c.execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|_| Issue::Unavailable)?;
    let view = store
        .query_on_connection_mock(&mut c, &args[2], &query)
        .await?;
    c.execute("ROLLBACK")
        .await
        .map_err(|_| Issue::Unavailable)?;
    c.close().await.map_err(|_| Issue::Unavailable)?;
    Ok(view)
}
