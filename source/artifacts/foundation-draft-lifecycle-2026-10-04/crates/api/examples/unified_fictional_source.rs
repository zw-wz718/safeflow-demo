//! Explicit local host for the adopted fictional v1, no payment or workers.
use safeflow_api::{router, ApiConfig, ApiState};
use safeflow_contracts::{ResourceId, SessionRef, UserId};
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    source_file: String,
    source_id: String,
    revision: String,
    owner: String,
    bind: String,
    frontend_origin: String,
    stop_file: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        return Err("usage: unified_fictional_source <trusted-config.json> <initialize|inspect|catalog-initialize|verify-session-boundaries|serve|deactivate>".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    if !matches!(
        args[2].as_str(),
        "initialize"
            | "inspect"
            | "catalog-initialize"
            | "verify-session-boundaries"
            | "serve"
            | "deactivate"
    ) {
        return Err("unknown native command".into());
    }
    if config.source_id != UNIFIED_FICTIONAL_SOURCE || config.revision != UNIFIED_FICTIONAL_REVISION
    {
        return Err("explicit fixed source/revision mismatch".into());
    }
    let secret = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")?);
    let mut api = ApiConfig::local_simulation(secret.clone());
    api.bind = config.bind.parse()?;
    api.frontend_origin = config.frontend_origin;
    api.validate()?;
    let owner = UserId::parse(&config.owner)?;
    let url = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_DATABASE_URL")?);
    let source = if args[2] == "initialize" {
        // Load before connecting; bounded original bytes and typed validation.
        if std::fs::metadata(&config.source_file)?.len() > 16384 {
            return Err("source file limit".into());
        }
        let doc = UnifiedFictionalDocument::read_adopted_v1(&std::fs::read(&config.source_file)?)?;
        doc.complete_accounts(&owner)?;
        PgUnifiedFictionalSource::provision_new_database(&url, &config.schema, doc).await?
    } else {
        let repo = PgRepository::connect_existing(&url, &config.schema).await?;
        let result =
            PgUnifiedFictionalSource::open(repo.clone(), &config.source_id, &config.revision).await;
        if result.is_err() {
            repo.close().await;
        }
        result?
    };
    let repo = source.repository_for_isolated_backend();
    let result=async {
        if args[2]=="deactivate" {
            source.deactivate_for_isolated_backend().await?;
            println!("{}",json!({"withdrawn":true,"reactivation_supported":false}));return Ok(());
        }
        let bind=api.bind;
        let state=ApiState::for_unified_fictional_source(source.clone(),api,owner).await?;
        if args[2]=="serve" {
            if std::path::Path::new(&config.stop_file).exists(){return Err("stop file must be absent".into());}
            let listener=tokio::net::TcpListener::bind(bind).await?;
            println!("{}",json!({"pid":std::process::id(),"bind":bind,"mode":"PROJECT_SIMULATION","owner_selected_by_native_host":config.owner,"payments_enabled":false,"workers_enabled":false,"consumption":"DEPENDENCY_UNAVAILABLE","initial":"DEPENDENCY_UNAVAILABLE"}));
            axum::serve(listener,router(state)).with_graceful_shutdown(async move {
                loop {
                    if std::path::Path::new(&config.stop_file).exists(){break;}
                    tokio::select!{_ = tokio::signal::ctrl_c()=>break,_ = tokio::time::sleep(Duration::from_millis(100))=>{}}
                }
            }).await?;
            return Ok(());
        }
        let session=state.simulation_session_for_native_host(&secret)?;
        let accounts=state.unified_accounts_for_backend(&session).await?;
        let mut rows=Vec::new();
        for a in accounts {
            if args[2]=="catalog-initialize" {
                state.initialize_unified_catalog_for_backend(&session,&a.account_id).await?;
            }
            let balance=state.unified_balance_for_backend(&session,&a.account_id).await?;
            rows.push(json!({"account":a,"balance":balance}));
        }
        if args[2]=="verify-session-boundaries" {
            let fake=SessionRef::parse("missing-session").unwrap();
            assert!(state.unified_accounts_for_backend(&fake).await.is_err());
            assert!(state.simulation_session_for_native_host("wrong-secret").is_err());
            for foreign in ["account_001","account_002","demo_account_li_v1"] {
                if !rows.iter().any(|r|r["account"]["account_id"]==foreign) {
                    let id=ResourceId::parse(foreign)?;
                    assert!(state.unified_balance_for_backend(&session,&id).await.is_err());
                    assert!(state.initialize_unified_catalog_for_backend(&session,&id).await.is_err());
                }
            }
            state.stop_native_simulation_session(&session)?;
            assert!(state.unified_accounts_for_backend(&session).await.is_err());
        }
        assert_eq!(source.consumption_source(),Err(StorageError::DependencyUnavailable));
        assert_eq!(source.initial_subscription_ledger(),Err(StorageError::DependencyUnavailable));
        println!("{}",json!({"pid":std::process::id(),"source":config.source_id,"revision":config.revision,"owner":config.owner,"command":args[2],"accounts":rows,"consumption":"DEPENDENCY_UNAVAILABLE","initial":"DEPENDENCY_UNAVAILABLE","payments_enabled":false,"workers_enabled":false}));
        Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    repo.close().await;
    result
}
