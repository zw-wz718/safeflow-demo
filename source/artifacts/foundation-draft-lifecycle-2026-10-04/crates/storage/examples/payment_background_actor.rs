//! Dedicated native control/scheduler. Private stdin credentials, no HTTP success.
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgConnectOptions;
use std::{
    io::{BufRead, Write},
    str::FromStr,
};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    namespace: String,
    kind: String,
    role_name: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {
    database_url: String,
    new_role_password: Option<String>,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
enum Command {
    Settle { grant_id: String, result_id: String },
    Decline { grant_id: String, result_id: String },
    Poll { grant_id: String },
    Complete { grant_id: String },
    ReadCompleted { grant_id: String },
    Stop {},
}
fn display(v: serde_json::Value) {
    println!("{v}");
    std::io::stdout().flush().expect("native display");
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() != 3 || !matches!(a[2].as_str(), "run" | "register" | "revoke" | "enable-final") {
        return Err("usage: payment_background_actor <nonsecret-config.json> <run|register|revoke|enable-final>; private credentials on stdin".into());
    }
    let c: Config = serde_json::from_slice(&std::fs::read(&a[1])?)?;
    let role = match c.kind.as_str() {
        "PRODUCER" => PaymentBackgroundRole::Producer,
        "WORKER" => PaymentBackgroundRole::Worker,
        _ => return Err("unknown role".into()),
    };
    let mut lines = std::io::stdin().lock().lines();
    let raw = Zeroizing::new(lines.next().ok_or("private credentials required")??);
    let secret: Credentials = serde_json::from_str(&raw)?;
    let url = Zeroizing::new(secret.database_url);
    drop(raw);
    let options = PgConnectOptions::from_str(&url)?;
    if options.get_host() != "127.0.0.1" {
        return Err("owned loopback PG only".into());
    }
    if a[2] != "run" {
        let repo = PgRepository::connect_existing(&url, &c.namespace).await?;
        let source = PgUnifiedFictionalSource::open(
            repo.clone(),
            UNIFIED_FICTIONAL_SOURCE,
            UNIFIED_FICTIONAL_REVISION,
        )
        .await?;
        let deployment = PaymentBackgroundDeployment::for_isolated_backend(source);
        let result = if a[2] == "register" {
            deployment
                .register_new_login(
                    &c.role_name,
                    role,
                    Zeroizing::new(
                        secret
                            .new_role_password
                            .ok_or("new random credential required")?,
                    ),
                )
                .await
        } else if a[2] == "enable-final" {
            deployment.enable_final_completion_for_login(&c.role_name).await
        } else {
            deployment.revoke_login(&c.role_name).await
        };
        display(json!({"applied":result.is_ok(),"business_executed":false}));
        repo.close().await;
        return result.map_err(Into::into);
    }
    if options.get_username() != c.role_name {
        return Err("credential role mismatch".into());
    }
    let repository=PgRepository::connect_existing(&url,&c.namespace).await?;
    repository.require_draft_balance_revision_for_backend().await?;
    repository.close().await;
    let actor = PaymentBackgroundActor::authenticate(options, &c.namespace, role).await?;
    display(json!({"authenticated":true,"role":c.kind,"mode":"PROJECT_SIMULATION","writer_requires_explicit_enable":true}));
    for line in lines {
        let raw = Zeroizing::new(line?);
        let command = serde_json::from_str::<Command>(&raw);
        drop(raw);
        let completion = matches!(&command, Ok(Command::Complete{..}|Command::ReadCompleted{..}));
        let result = match command {
            Ok(Command::Stop {}) => break,
            Ok(Command::Settle {
                grant_id,
                result_id,
            }) => {
                actor
                    .record_explicit_decision(
                        &grant_id,
                        &result_id,
                        PaymentSimulationDecision::SettleLocalSimulation,
                    )
                    .await
            }
            Ok(Command::Decline {
                grant_id,
                result_id,
            }) => {
                actor
                    .record_explicit_decision(
                        &grant_id,
                        &result_id,
                        PaymentSimulationDecision::DefiniteSimulatedDecline,
                    )
                    .await
            }
            Ok(Command::Poll { grant_id }) => actor.inspect_and_claim_original(&grant_id).await,
            Ok(Command::Complete { grant_id }) => actor.complete_original(&grant_id).await,
            Ok(Command::ReadCompleted { grant_id }) => actor.read_completed_original(&grant_id).await.map(|v|json!({"original_completion":v})),
            Err(_) => {
                display(json!({"error":"INVALID_NATIVE_COMMAND"}));
                continue;
            }
        };
        match result {
            Ok(v) => display(v),
            Err(_) if completion => display(json!({"error":"COMPLETION_UNAVAILABLE_OR_UNAUTHORIZED","completion_state":"UNKNOWN_OR_REJECTED","query_original_required":true})),
            Err(_) => display(
                json!({"error":"ORIGINAL_UNAVAILABLE_OR_UNAUTHORIZED","business_executed":false}),
            ),
        }
    }
    actor.close().await;
    Ok(())
}
