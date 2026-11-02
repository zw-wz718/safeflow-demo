//! Independent, bounded/continuous native simulation scheduler. No HTTP inputs.
use safeflow_storage::*;
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgConnectOptions;
use std::{io::{BufRead,Write},str::FromStr,sync::mpsc,time::Duration};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    namespace: String,
    run_id: String,
    producer_role: String,
    worker_role: String,
    kind: String,
    interval_ms: u64,
    max_ticks: u32,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials { database_url: String }
fn display(v:serde_json::Value){println!("{v}");let _=std::io::stdout().flush();}
async fn run()->Result<(),Box<dyn std::error::Error>> {
    let args:Vec<String>=std::env::args().collect();
    if args.len()!=3 || !matches!(args[2].as_str(),"run"|"arm"|"disable") {return Err("usage: automatic_simulated_settlement <config> <run|arm|disable>".into());}
    let c:Config=serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let id=uuid::Uuid::parse_str(&c.run_id)?;
    if id.is_nil() || !(250..=30_000).contains(&c.interval_ms) || c.max_ticks>1800 {return Err("invalid bounded configuration".into());}
    let (role,name)=match c.kind.as_str(){"PRODUCER"=>(PaymentBackgroundRole::Producer,&c.producer_role),"WORKER"=>(PaymentBackgroundRole::Worker,&c.worker_role),_=>return Err("unknown role".into())};
    let raw=Zeroizing::new({let mut s=String::new();std::io::stdin().read_line(&mut s)?;s});
    let private:Credentials=serde_json::from_str(&raw)?;
    let url=Zeroizing::new(private.database_url);drop(raw);
    let options=PgConnectOptions::from_str(&url)?;
    if options.get_host()!="127.0.0.1" {return Err("owned loopback PG only".into());}
    if args[2]!="run" {
        let repo=PgRepository::connect_existing(&url,&c.namespace).await?;
        let source=PgUnifiedFictionalSource::open(repo.clone(),UNIFIED_FICTIONAL_SOURCE,UNIFIED_FICTIONAL_REVISION).await?;
        let deployment=PaymentBackgroundDeployment::for_isolated_backend(source);
        if args[2]=="arm" {deployment.enable_automatic_simulation(id,&c.producer_role,&c.worker_role).await?;}
        else {deployment.disable_automatic_simulation(id).await?;}
        repo.close().await;display(json!({"configured":true,"business_executed":false}));return Ok(());
    }
    if options.get_username()!=name {return Err("credential role mismatch".into());}
    let repository=PgRepository::connect_existing(&url,&c.namespace).await?;
    repository.require_draft_balance_revision_for_backend().await?;
    repository.close().await;
    let options=options.options([("statement_timeout","10000"),("lock_timeout","2000")]);
    let actor=PaymentBackgroundActor::authenticate(options,&c.namespace,role).await?;
    drop(url);
    let (sender,receiver)=mpsc::channel();
    std::thread::spawn(move ||{
        for line in std::io::stdin().lock().lines(){
            match line {
                Ok(s) if serde_json::from_str::<serde_json::Value>(&s).ok()==Some(json!({"kind":"STOP"})) => {let _=sender.send(());break;},
                _=>{let _=sender.send(());break;}, // EOF/malformed control stops; never executes commands.
            }
        }
    });
    display(json!({"ready":true,"mode":"PROJECT_SIMULATION","role":c.kind,"interval_ms":c.interval_ms,"max_ticks":c.max_ticks,"cohort":"NEW_EXPLICIT_ACCEPTANCE_ONLY"}));
    let mut cursor=String::new();let mut ticks=0u32;
    loop {
        if !matches!(receiver.try_recv(),Err(mpsc::TryRecvError::Empty)){break;}
        match tokio::time::timeout(Duration::from_secs(15),actor.automatic_simulation_tick(id,&cursor)).await {
            Ok(Ok(v))=>{cursor=v["next_cursor"].as_str().unwrap_or("").into();display(v);},
            _=>{display(json!({"mode":"PROJECT_SIMULATION","unavailable":true,"business_resubmitted":false}));},
        }
        ticks+=1;
        if c.max_ticks!=0 && ticks>=c.max_ticks{break;}
        // Responsive stop between scans; an already-started original transaction
        // drains under its unchanged final gates. Hard disable fences its COMMIT.
        for _ in 0..c.interval_ms.div_ceil(50) {
            if !matches!(receiver.try_recv(),Err(mpsc::TryRecvError::Empty)){actor.close().await;return Ok(());}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    actor.close().await;Ok(())
}
#[tokio::main]
async fn main(){if run().await.is_err(){eprintln!("Simulation scheduler unavailable/configuration rejected; no original resubmitted.");std::process::exit(1);}}
