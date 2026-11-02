//! Explicit finite CURRENT native session; persistent LOCAL simulation only.
use safeflow_api::{ApiConfig,ApiState};
use safeflow_contracts::UserId;
use safeflow_storage::*;
use serde::Deserialize;
use std::time::Duration;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config{schema:String,owner:String,stop_file:String,reminder:Limits}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Limits{max_messages:usize,max_ledger_entries:usize,scan_timeout_ms:u64,cycles:usize,interval_ms:u64}
#[tokio::main]
async fn main()->Result<(),Box<dyn std::error::Error>>{
    let args:Vec<_>=std::env::args().collect();
    if !(3..=4).contains(&args.len()) || !matches!(args[2].as_str(),"once"|"run"|"query") {return Err("usage: persistent_subscription_reminders <private-config.json> <once|run|query> [original-attempt-ref]".into());}
    if std::fs::metadata(&args[1])?.len()>1024*1024{return Err("bounded configuration required".into());}
    let config:Config=serde_json::from_slice(&std::fs::read(&args[1])?)?;
    if config.stop_file.is_empty() || std::path::Path::new(&config.stop_file).exists(){return Err("explicit unused stop file required".into());}
    let owner=UserId::parse(&config.owner)?;
    let url=zeroize::Zeroizing::new(std::env::var("SAFEFLOW_DATABASE_URL")?);
    let secret=zeroize::Zeroizing::new(std::env::var("SAFEFLOW_SIM_LOGIN_SECRET")?);
    let repo=PgRepository::connect_existing(&url,&config.schema).await?;
    let result=async{
        let identity=PgUnifiedFictionalSource::open(repo.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await?;
        let accounts=identity.complete_accounts(&owner).await?;
        let mut limits=SubscriptionReminderConfig{owner:owner.clone(),accounts:accounts.iter().map(|a|a.account_id.clone()).collect(),max_messages:config.reminder.max_messages,max_ledger_entries:config.reminder.max_ledger_entries,scan_timeout:Duration::from_millis(config.reminder.scan_timeout_ms),cycles:config.reminder.cycles,interval:Duration::from_millis(config.reminder.interval_ms)};
        limits.validate()?;if args[2]=="once"{limits.cycles=1;}
        let mut state=ApiState::for_unified_team_consumption(identity,ApiConfig::local_simulation(secret.clone()),owner).await?;
        let session=state.simulation_session_for_native_host(&secret)?;
        // Bind the existing web/native host generation; this explicit current
        // session never restarts the business Journal or invalidates the API.
        state.reopen_team_subscription_reads_for_backend(&session).await?;
        let stop=SubscriptionReminderStop::default();let signal_stop=stop.clone();
        let signal=tokio::spawn(async move{loop{if std::path::Path::new(&config.stop_file).exists(){signal_stop.stop();break;}
            tokio::select!{_=tokio::signal::ctrl_c()=>{signal_stop.stop();break;},_=tokio::time::sleep(Duration::from_millis(50))=>{}}}});
        let result=async{
            if args[2]=="query"{return state.persistent_subscription_reminder_view_for_backend(&session,args.get(3).map(String::as_str)).await;}
            let host=state.persistent_subscription_reminder_host_for_backend(&session,limits).await?;
            let reports=host.run(&stop).await?;
            Ok(serde_json::json!({"mode":"LOCAL_PERSISTENT_SIMULATION","notification_sent":false,"reports":reports}))
        }.await;
        stop.stop();signal.abort();let _=signal.await;
        let revoke=state.stop_native_simulation_session(&session);
        let value=result?;revoke?;
        println!("{}",serde_json::to_string(&value)?);
        Ok::<(),Box<dyn std::error::Error>>(())
    }.await;
    repo.close().await;result
}
