//! Isolated native console. Credentials arrive via private stdin, never argv,
//! environment, config files or HTTP. Stdout is a trusted ONE-TIME display.
use safeflow_api::{router, ApiConfig, ApiState};
use safeflow_contracts::{OperationId, UserId};
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use std::io::{BufRead, Write};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    owner: String,
    bind: String,
    frontend_origin: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    login_secret: String,
    controller_secret: String,
    database_url: String,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Command {
    Challenge {
        cookie: String,
        operation_id: OperationId,
        controller_secret: String,
    },
    Stop {},
}
fn display(value: serde_json::Value) {
    println!("{value}");
    std::io::stdout().flush().expect("native display");
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 || !matches!(args[2].as_str(), "provision" | "serve") {
        return Err("usage: card_safety <nonsecret-config.json> <provision|serve>; credentials on private stdin".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let mut lines = std::io::stdin().lock().lines();
    let raw = Zeroizing::new(lines.next().ok_or("credentials required")??);
    let c: Credentials = serde_json::from_str(&raw)?;
    let login = Zeroizing::new(c.login_secret);
    let controller = Zeroizing::new(c.controller_secret);
    let url = Zeroizing::new(c.database_url);
    drop(raw);
    let owner = UserId::parse(&config.owner)?;
    let source = if args[2] == "provision" {
        PgUnifiedFictionalSource::provision_new_database(
            &url,
            &config.schema,
            UnifiedFictionalDocument::read_adopted_v1(include_bytes!(
                "../../../../../fixtures/transfer/unified_demo_source_v1.json"
            ))?,
        )
        .await?
    } else {
        PgUnifiedFictionalSource::open(
            PgRepository::connect_existing(&url, &config.schema).await?,
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_REVISION,
        )
        .await?
    };
    let repo = source.repository_for_isolated_backend();
    let mut api = ApiConfig::local_simulation(login.clone());
    api.bind = config.bind.parse()?;
    api.frontend_origin = config.frontend_origin;
    let bind = api.bind;
    let state = ApiState::for_unified_card_safety(source, api, owner, controller).await?;
    if args[2] == "provision" {
        let s = state.simulation_session_for_native_host(&login)?;
        for account in state.unified_accounts_for_backend(&s).await? {
            state
                .initialize_unified_catalog_for_backend(&s, &account.account_id)
                .await?;
        }
        state.stop_native_card_session(&s).await?;
        display(json!({"provisioned":true,"business_executed":false}));
        repo.close().await;
        return Ok(());
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let app = state.clone();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(app))
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
    });
    display(
        json!({"ready":true,"bind":bind,"mode":"ISOLATED_SIMULATION","card_writer_enabled":false}),
    );
    // Blocking stdin stays on this native task; HTTP runs on Tokio workers.
    for line in lines {
        let raw = Zeroizing::new(line?);
        let command = serde_json::from_str::<Command>(&raw);
        drop(raw);
        match command {
            Ok(Command::Stop {}) => break,
            Ok(Command::Challenge {
                cookie,
                operation_id,
                controller_secret,
            }) => {
                let secret = Zeroizing::new(controller_secret);
                let result = async {
                    let mut headers = axum::http::HeaderMap::new();
                    headers.insert(
                        axum::http::header::COOKIE,
                        cookie.parse().map_err(|_| StorageError::Session)?,
                    );
                    let s = state.billing_feedback_session_for_backend(&headers)?;
                    state
                        .card_challenge_for_native_controller(&secret, &s, &operation_id)
                        .await
                }
                .await;
                match result {
                    Ok(mut delivery) => {
                        let code = delivery.take_secret();
                        display(
                            json!({"challenge_id":delivery.challenge_id,"code":code.as_ref().map(|c|c.expose_once_for_native_display()),"one_time_native_display":true}),
                        );
                    }
                    Err(_) => display(json!({"error":"CHALLENGE_UNAVAILABLE"})),
                }
            }
            Err(_) => display(json!({"error":"INVALID_NATIVE_COMMAND"})),
        }
    }
    let _ = tx.send(());
    server.await??;
    repo.close().await;
    Ok(())
}
