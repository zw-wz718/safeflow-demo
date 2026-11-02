//! Explicit standalone subscription startup, never the full banking Agent.
#[cfg(not(feature = "postgres-mock-store"))]
fn main() {
    eprintln!("POSTGRES_MOCK_STORE_FEATURE_REQUIRED");
    std::process::exit(1);
}
#[cfg(feature = "postgres-mock-store")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(v) => {
            println!("{v}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}
#[cfg(feature = "postgres-mock-store")]
async fn run() -> Result<serde_json::Value, String> {
    use safeflow_subscription_evidence::{
        confirmation_mock::SubscriptionRuntimeMock, managed_ledger_mock::load_ledger,
    };
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 || !matches!(args[0].as_str(), "init" | "start") {
        return Err("USAGE: init fictional-ledger.json | start fictional-user-id".into());
    }
    let url = std::env::var("SAFEFLOW_SUBSCRIPTION_MOCK_DATABASE_URL")
        .map_err(|_| "DEDICATED_DATABASE_REQUIRED")?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect(&url)
        .await
        .map_err(|_| "DATABASE_UNAVAILABLE")?;
    let (runtime, user) = if args[0] == "init" {
        let raw = std::fs::read_to_string(&args[1]).map_err(|_| "FIXTURE_READ_FAILED")?;
        let fixture = load_ledger(&raw).map_err(|_| "INVALID_FICTIONAL_LEDGER")?;
        let user = fixture.owner_user_id.clone();
        SubscriptionRuntimeMock::initialize_local_schema(&pool)
            .await
            .map_err(|e| format!("{e:?}"))?;
        let runtime = SubscriptionRuntimeMock::create_fixture_mock(pool.clone(), &user, fixture)
            .await
            .map_err(|e| format!("{e:?}"))?;
        (runtime, user)
    } else {
        let runtime = SubscriptionRuntimeMock::start_mock(pool.clone(), &args[1])
            .await
            .map_err(|e| format!("{e:?}"))?;
        (runtime, args[1].clone())
    };
    let result = serde_json::json!({"fictional":true,"subscription_runtime_ready":true,
        "production_registered":false,"full_banking_system_recovered":false,
        "notification_sent":false,"pid":std::process::id(),
        "subscriptions":runtime.ledger_mock().list_mock(&user).map_err(|_|"LEDGER_NOT_READY")?.len(),
        "registration_events":runtime.ledger_mock().events_mock(&user).map_err(|_|"LEDGER_NOT_READY")?.len(),
        "intent_events":runtime.ledger_mock().intent_events_mock(&user).map_err(|_|"LEDGER_NOT_READY")?.len()});
    pool.close().await;
    Ok(result)
}
