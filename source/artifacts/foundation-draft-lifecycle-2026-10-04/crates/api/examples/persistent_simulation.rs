//! Explicit native local host. No sender/worker/real model or public write tool.
//! Configuration and import data must be supplied by the trusted operator.
use safeflow_api::*;
use safeflow_billing_source_reader::query::{Request, Transaction};
use safeflow_contracts::UserId;
use safeflow_storage::*;
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    spec: SimulationSourceSpec,
    bind: String,
    frontend_origin: String,
    request: Option<Request>,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: persistent_simulation <config.json> <import|activate|deactivate|serve|initialize|lookup|prepare> [data.json|index nonce]".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let url = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_DATABASE_URL")?);
    let repo = if args[2] == "provision" {
        PgRepository::connect(&url, &config.schema).await?
    } else {
        PgRepository::connect_existing(&url, &config.schema).await?
    };
    let result = async {
        if args[2] == "provision" {
            repo.migrate().await?;
            println!("isolated schema provisioned; no source data installed");
            return Ok(());
        }
        if args[2] == "import" {
            let path = args.get(3).ok_or("explicit complete data file required")?;
            if std::fs::metadata(path)?.len() > 8 * 1024 * 1024 {
                return Err("bounded import exceeded".into());
            }
            let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)?;
            if raw.as_object().is_none_or(|m| {
                m.len() != 2 || !m.contains_key("transactions") || !m.contains_key("initial")
            }) {
                return Err("expected exact transactions/initial".into());
            }
            let rows: Vec<Transaction> = serde_json::from_value(raw["transactions"].clone())?;
            let initial = if raw["initial"].is_null() {
                None
            } else {
                Some(serde_json::from_value(raw["initial"].clone())?)
            };
            PgSimulationSource::import(repo.clone(), config.spec.clone(), rows, initial).await?;
            println!("version imported; not activated or initialized");
            return Ok(());
        }
        // Activation needs the exact saved spec but does not require an active head.
        // open_imported validates the saved immutable version, never creates a version.
        let source = PgSimulationSource::open_imported(repo.clone(), config.spec.clone()).await?;
        if args[2] == "activate" || args[2] == "deactivate" {
            source.set_active(args[2] == "activate").await?;
            println!("technical source generation changed");
            return Ok(());
        }
        // Historical lookup checks the exact imported initial, even after its
        // source is disabled/replaced. New operations still require active.
        let source = if args[2] == "lookup" {
            source
        } else {
            PgSimulationSource::open(repo.clone(), config.spec.clone()).await?
        };
        let secret = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")?);
        let mut api = ApiConfig::local_simulation(secret.clone());
        api.bind = config.bind.parse()?;
        api.frontend_origin = config.frontend_origin;
        api.request_timeout = Duration::from_secs(15);
        let bind = api.bind;
        let bank = Arc::new(safeflow_mock_bank::MockBankGateway::new());
        let billing = billing_report_registration_for_backend(
            bank.clone(),
            Some(LocalBillingSource::for_persistent_simulation(
                source.clone(),
            )),
        )?;
        let state = ApiState::for_project_simulation_with_owner_and_tools(
            repo.clone(),
            api,
            UserId::parse(&config.spec.owner)?,
            bank,
            vec![billing],
        )?;
        if args[2] == "serve" {
            let listener = tokio::net::TcpListener::bind(bind).await?;
            println!("local simulation http://{bind}; four writes remain disabled");
            axum::serve(listener, router(state))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
            return Ok(());
        }
        let session = state.simulation_session_for_native_host(&secret)?;
        match args[2].as_str() {
            "initialize" => {
                state
                    .initialize_simulation_source_for_backend(&session, &source)
                    .await?;
                println!("explicit initial independently verified")
            }
            "lookup" => println!(
                "original initial observed: {}",
                state
                    .subscription_for_backend()
                    .lookup_simulation_initial_for_backend(&session, &source)
                    .await?
            ),
            "prepare" => {
                let index: usize = args.get(3).ok_or("explicit index")?.parse()?;
                let nonce = args.get(4).ok_or("explicit nonce")?;
                let host = state
                    .subscription_for_backend()
                    .startup_for_host(&session)
                    .await?;
                let request = config.request.as_ref().ok_or("explicit frozen request")?;
                state
                    .prepare_simulation_subscription_for_backend(
                        &session, &host, &source, request, index, nonce,
                    )
                    .await?;
                println!("same-capture technical preparation saved; no confirmed business write")
            }
            _ => return Err("unknown native command".into()),
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;
    repo.close().await;
    result
}
