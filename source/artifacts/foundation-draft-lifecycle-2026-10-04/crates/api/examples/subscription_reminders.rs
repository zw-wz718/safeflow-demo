//! Explicit local/current-session mock host. No HTTP route, transport or daemon.
use safeflow_api::*;
use safeflow_bank_gateway::BankGateway;
use safeflow_contracts::{ResourceId, UserId};
use safeflow_storage::*;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    source: SimulationSourceSpec,
    reminder: Limits,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits {
    max_messages: usize,
    max_ledger_entries: usize,
    scan_timeout_ms: u64,
    cycles: usize,
    interval_ms: u64,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !matches!(args[2].as_str(), "once" | "run") {
        return Err("usage: subscription_reminders <explicit-config.json> <once|run>".into());
    }
    if std::fs::metadata(&args[1])?.len() > 1024 * 1024 {
        return Err("bounded configuration required".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let owner = UserId::parse(&config.source.owner)?;
    let mut limits = SubscriptionReminderConfig {
        owner: owner.clone(),
        accounts: config
            .source
            .accounts
            .iter()
            .map(ResourceId::parse)
            .collect::<Result<_, _>>()?,
        max_messages: config.reminder.max_messages,
        max_ledger_entries: config.reminder.max_ledger_entries,
        scan_timeout: Duration::from_millis(config.reminder.scan_timeout_ms),
        cycles: config.reminder.cycles,
        interval: Duration::from_millis(config.reminder.interval_ms),
    };
    limits.validate()?;
    if args[2] == "once" {
        limits.cycles = 1;
    }
    let url = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_DATABASE_URL")?);
    let secret = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")?);
    let repository = PgRepository::connect_existing(&url, &config.schema).await?;
    let result = async {
        let source = PgSimulationSource::open(repository.clone(), config.source).await?;
        let bank = Arc::new(safeflow_mock_bank::MockBankGateway::new());
        let state = ApiState::for_project_simulation_with_owner_and_tools(
            repository.clone(),
            ApiConfig::local_simulation(secret.clone()),
            owner.clone(),
            bank.clone(),
            vec![],
        )?;
        let session = state.simulation_session_for_native_host(&secret)?;
        let stop = SubscriptionReminderStop::default();
        let signal_stop = stop.clone();
        let signal = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                signal_stop.stop();
            }
        });
        let run = async {
            let startup_timeout = limits.scan_timeout;
            let boot = async {
                // Validate actual Gateway ownership BEFORE explicit startup writes.
                for account in &limits.accounts {
                    bank.get_account_balance(&owner, account)
                        .await?
                        .validate_for(account)?;
                }
                if !state
                    .subscription_for_backend()
                    .lookup_simulation_initial_for_backend(&session, &source)
                    .await?
                {
                    return Err("original adopted initial not observed".into());
                }
                let host = state
                    .subscription_for_backend()
                    .startup_for_host(&session)
                    .await?;
                state
                    .subscription_reminder_host_for_backend(&session, &host, limits)
                    .await
                    .map_err(|e| Box::<dyn std::error::Error>::from(e))
            };
            let runner = tokio::time::timeout(startup_timeout, boot).await??;
            let reports = runner.run(&stop).await?;
            println!("{}", serde_json::to_string(&reports)?);
            Ok::<(), Box<dyn std::error::Error>>(())
        };
        let result = tokio::select! {biased;
            _=stop.cancelled()=>Err("native host stopped; inspect original saved facts".into()),
            result=run=>result,
        };
        stop.stop();
        signal.abort();
        let revoked = state.stop_native_simulation_session(&session);
        result?;
        revoked?;
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    repository.close().await;
    result
}
