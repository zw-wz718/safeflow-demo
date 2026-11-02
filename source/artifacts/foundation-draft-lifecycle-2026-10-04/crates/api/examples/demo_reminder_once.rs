//! One native CURRENT session, one bounded persistent in-app simulation scan.
//! No environment/argv credentials, provisioning, web listener or model call.
use safeflow_api::{ApiConfig,ApiState};
use safeflow_contracts::UserId;
use safeflow_storage::*;
use serde::Deserialize;
use sqlx::Connection;
use std::{io::{self,Read},time::Duration};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials{database_url:String,login_secret:String}
async fn run()->Result<serde_json::Value,Box<dyn std::error::Error>>{
 if std::env::args().count()!=1{return Err("no arguments accepted".into());}
 let mut raw=Zeroizing::new(String::new());io::stdin().take(65537).read_to_string(&mut raw)?;if raw.len()>65536{return Err("bounded private input required".into());}
 let private:Credentials=serde_json::from_str(&raw)?;drop(raw);let url=Zeroizing::new(private.database_url);let secret=Zeroizing::new(private.login_secret);
 let options=url.parse::<sqlx::postgres::PgConnectOptions>()?;
 if options.get_database()!=Some("billing_test_noon_acceptance_20261029") || options.get_username()!="sf_noon_reminder_20261101" || !matches!(options.get_host(),"127.0.0.1"|"/var/run/postgresql"){return Err("fixed reminder scope required".into());}
 let mut gate=sqlx::PgConnection::connect_with(&sqlx::ConnectOptions::disable_statement_logging(options)).await?;
 sqlx::raw_sql("SET search_path TO sf_unified_noon_acceptance,pg_catalog;SET statement_timeout='5s';SET lock_timeout='2s'").execute(&mut gate).await?;
 sqlx::query("SELECT demo_reminder_authority47_v1()").execute(&mut gate).await?;
 let remaining:f64=sqlx::query_scalar("SELECT extract(epoch FROM ('2026-11-29T00:00:00+08:00'::timestamptz-clock_timestamp()))::float8").fetch_one(&mut gate).await?;gate.close().await?;
 if remaining<2.0{return Err("reminder cutoff reached".into());}
 let repository=PgRepository::connect_existing(&url,"sf_unified_noon_acceptance").await?;drop(url);
 let result=async{
  repository.require_draft_balance_revision_for_backend().await?;
  let identity=PgUnifiedFictionalSource::open(repository.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await?;
  let owner=UserId::parse("user_001")?;let accounts=identity.complete_accounts(&owner).await?;
  let mut config=ApiConfig::local_simulation(secret.clone());config.session_ttl=Duration::from_secs((remaining.floor() as u64).min(30));
  let mut state=ApiState::for_unified_team_consumption(identity,config,owner.clone()).await?;
  let session=state.simulation_session_for_native_host(&secret)?;drop(secret);
  let stop=SubscriptionReminderStop::default();let stop_signal=stop.clone();
  let signal=tokio::spawn(async move{tokio::select!{_=tokio::signal::ctrl_c()=>{},_=tokio::time::sleep(Duration::from_secs_f64(remaining.min(30.0)))=>{}}stop_signal.stop();});
  let scan=async{
   state.reopen_team_subscription_reads_for_backend(&session).await?;
   let host=state.persistent_subscription_reminder_host_for_backend(&session,SubscriptionReminderConfig{owner,accounts:accounts.into_iter().map(|a|a.account_id).collect(),max_messages:128,max_ledger_entries:256,scan_timeout:Duration::from_secs(5),cycles:1,interval:Duration::from_secs(1)}).await?;
   host.run(&stop).await
  }.await;
  stop.stop();signal.abort();let _=signal.await;let revoked=state.stop_native_simulation_session(&session);
  let reports=scan?;revoked?;
  Ok::<_,Box<dyn std::error::Error>>(serde_json::json!({"mode":"LOCAL_PERSISTENT_SIMULATION","cycles":1,"notification_sent":false,"session_ended":true,"reports":reports}))
 }.await;repository.close().await;result
}
#[tokio::main]
async fn main(){let result=match tokio::time::timeout(Duration::from_secs(25),run()).await{Ok(v)=>v,Err(_)=>Err("finite native invocation timeout".into())};match result{Ok(v)=>println!("{v}"),Err(e)=>{let code=if let Some(s)=e.downcast_ref::<StorageError>(){format!("{s:?}")}else if let Some(sqlx::Error::Database(d))=e.downcast_ref::<sqlx::Error>(){d.code().map(|s|s.into_owned()).unwrap_or_default()}else{"SCOPE_OR_DEPENDENCY".into()};eprintln!("DEMO_REMINDER_ONCE_REJECTED [{code}]; original attempts preserved; no credentials/SQL emitted");std::process::exit(1);}}}
