//! Local-only host of the fixed identity/history/initial. No writes or workers.
use safeflow_api::{router, ApiConfig, ApiState};
use safeflow_contracts::UserId;
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    schema: String,
    owner: String,
    bind: String,
    frontend_origin: String,
    stop_file: String,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() < 3 {
        return Err("usage: team_consumption <native-config.json> <provision|import|activate|deactivate|initialize|lookup|inspect|preview|prepare|serve|verify-session> [index nonce]".into());
    }
    if !matches!(
        args[2].as_str(),
        "provision"
            | "import"
            | "activate"
            | "deactivate"
            | "initialize"
            | "catalog-initialize"
            | "lookup"
            | "inspect"
            | "preview"
            | "prepare"
            | "serve"
            | "verify-session"
    ) {
        return Err("unknown native command".into());
    }
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let owner = UserId::parse(&config.owner)?;
    let secret = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")?);
    let mut api = ApiConfig::local_simulation(secret.clone());
    api.bind = config.bind.parse()?;
    api.frontend_origin = config.frontend_origin;
    api.validate()?;
    let url = zeroize::Zeroizing::new(std::env::var("SAFEFLOW_DATABASE_URL")?);
    // Fixed input validation precedes every connection, including provisioning.
    let doc = TeamConsumptionDocument::fixed_adopted()?;
    let identity = if args[2] == "provision" {
        let identity_doc = UnifiedFictionalDocument::read_adopted_v1(include_bytes!(
            "../../../../../fixtures/transfer/unified_demo_source_v1.json"
        ))?;
        identity_doc.complete_accounts(&owner)?;
        PgUnifiedFictionalSource::provision_new_database(&url, &config.schema, identity_doc).await?
    } else {
        let repo = PgRepository::connect_existing(&url, &config.schema).await?;
        PgUnifiedFictionalSource::open(repo, UNIFIED_FICTIONAL_SOURCE, UNIFIED_FICTIONAL_REVISION)
            .await?
    };
    let repo = identity.repository_for_isolated_backend();
    let result=async {
        if args[2]=="provision"{println!("{}",json!({"identity_provisioned":true,"consumption_imported":false}));return Ok(())}
        if args[2]=="import"{
            let source=TeamConsumptionSource::import(&identity,&owner,&doc).await?;
            println!("{}",json!({"owner":owner,"imported":true,"initialized":false,"active_changed":false,"request":source.request(),"adoption":doc.provenance()}));return Ok(())
        }
        let source=TeamConsumptionSource::open(&identity,&owner).await?;
        if args[2]=="activate"||args[2]=="deactivate"{source.persistent_for_backend().set_active(args[2]=="activate").await?;println!("{}",json!({"active":args[2]=="activate"}));return Ok(())}
        let bind=api.bind;
        let mut state=ApiState::for_unified_team_consumption(identity.clone(),api,owner.clone()).await?;
        if args[2]=="serve"{
            let session=state.simulation_session_for_native_host(&secret)?;
            state.enable_team_subscription_writes_for_backend(&session,std::env::var("SAFEFLOW_SUBSCRIPTION_INSTALL").as_deref()==Ok("1")).await?;
            if std::path::Path::new(&config.stop_file).exists(){return Err("stop file exists".into())}
            let listener=tokio::net::TcpListener::bind(bind).await?;
            println!("{}",json!({"pid":std::process::id(),"bind":bind,"owner":owner,"as_of":TEAM_AS_OF,"writes_enabled":true,"workers_enabled":false,"real_llm":false}));
            axum::serve(listener,router(state)).with_graceful_shutdown(async move{loop{if std::path::Path::new(&config.stop_file).exists(){break}tokio::select!{_=tokio::signal::ctrl_c()=>break,_=tokio::time::sleep(Duration::from_millis(100))=>{}}}}).await?;
            return Ok(())
        }
        let session=state.simulation_session_for_native_host(&secret)?;
        match args[2].as_str(){
            "catalog-initialize"=>{for account in state.unified_accounts_for_backend(&session).await?{state.initialize_unified_catalog_for_backend(&session,&account.account_id).await?;}println!("{}",json!({"catalog_initialized":true,"card_actions_executed":false}));}
            "initialize"=>{state.initialize_team_initial_for_backend(&session).await?;println!("{}",json!({"initialized_and_independently_read":true}));}
            "lookup"=>println!("{}",json!({"original_initial_observed":state.lookup_team_initial_for_backend(&session).await?})),
            "preview"=>println!("{}",state.team_subscription_preview_for_backend(&session).await?),
            "prepare"=>{
                let index=args.get(3).ok_or("index required")?.parse()?;let nonce=args.get(4).ok_or("nonce required")?;
                let host=state.subscription_for_backend().startup_for_host(&session).await?;
                state.prepare_team_subscription_for_backend(&session,&host,index,nonce).await?;
                println!("{}",json!({"technical_preparation_saved":true,"business_executed":false}));
            }
            "verify-session"=>{
                state.stop_native_simulation_session(&session)?;
                assert!(state.initialize_team_initial_for_backend(&session).await.is_err());
                assert!(state.team_subscription_preview_for_backend(&session).await.is_err());
                assert!(state.lookup_team_initial_for_backend(&session).await.is_err());
                println!("{}",json!({"revoked_session_rejected":true}));
            }
            "inspect"=>{
                let current=state.subscription_for_backend().read(&session).await?;let mut balances=vec![];
                for a in state.unified_accounts_for_backend(&session).await?{balances.push(state.unified_balance_for_backend(&session,&a.account_id).await?);}
                println!("{}",json!({"owner":owner,"current":current,"balances":balances,"request":source.request(),"calendar":source.calendar_for_backend(),"writes_enabled":false}));
            }
            _=>return Err("unsupported command".into())
        }
        Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    repo.close().await;
    result
}
