//! Existing-database native maintenance. Credentials on stdin only.
use safeflow_storage::{PgRepository,PgUnifiedFictionalSource,UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION};
use serde::Deserialize;
use std::io::{self,BufRead};
use zeroize::Zeroizing;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {schema:String,api_role:String}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials {database_url:String}
#[tokio::main]
async fn main(){if run().await.is_err(){eprintln!("RECIPIENT_PROFILE_MAINTENANCE_FAILED; no credential or SQL details emitted");std::process::exit(1)}}
async fn run()->Result<(),Box<dyn std::error::Error>>{
 let args:Vec<String>=std::env::args().collect();
 if args.len()!=3 || !matches!(args[2].as_str(),"adopt"|"inspect"){return Err("usage: recipient_profile <nonsecret-config.json> <adopt|inspect>; database_url on stdin".into());}
 let c:Config=serde_json::from_slice(&std::fs::read(&args[1])?)?;
 let raw=Zeroizing::new(io::stdin().lock().lines().next().ok_or("credentials required")??);
 let credentials:Credentials=serde_json::from_str(&raw)?;drop(raw);
 let url=Zeroizing::new(credentials.database_url);
 let repo=PgRepository::connect_existing(&url,&c.schema).await?;
 let source=PgUnifiedFictionalSource::open(repo.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await?;
 let result=if args[2]=="adopt" {source.validate_recipient_profile_maintenance(&c.api_role).await?;repo.migrate().await?;source.adopt_recipient_profile_for_native_backend(&c.api_role).await?}
 else {source.inspect_recipient_profile_for_native_backend().await?};
 println!("{}",serde_json::to_string(&result)?);repo.close().await;Ok(())
}
