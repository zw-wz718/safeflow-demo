//! Explicit trusted backend for a provisioned Outbox. No migration or business assembly.
//! Credentials arrive on private stdin, never command arguments or output.
use safeflow_api::{serve_with_public_audit, AuditWorkerConfig};
use safeflow_storage::{PgRepository, PublicAuditOutbox, PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig};
use serde::Deserialize;
use serde_json::json;
use std::{io::{BufRead, Write}, time::Duration};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credentials { database_url: String, namespace: String }
fn emit(v: serde_json::Value) {
    println!("{v}");
    std::io::stdout().flush().expect("backend output");
}
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut input = std::io::stdin().lock().lines();
    let raw = Zeroizing::new(input.next().ok_or("private configuration required")??);
    let c: Credentials = serde_json::from_str(&raw)?;
    drop(raw);
    let url = Zeroizing::new(c.database_url);
    let repository = PgRepository::connect_existing(&url, &c.namespace).await?;
    let outbox = PublicAuditOutbox::for_trusted_backend(repository.clone());
    emit(json!({"ready":true,"pid":std::process::id(),"existing_only":true}));
    while let Some(line) = input.next() {
        let command = line?;
        match command.as_str() {
            "PENDING" => {
                let pending: Vec<_> = outbox.pending(1000).await?.iter().map(|p| json!({
                    "event_id":p.event_id,"envelope":p.envelope,"digest":p.digest,
                    "historical":p.historical,"enqueued_at":p.enqueued_at
                })).collect();
                emit(json!({"pending":pending}));
            }
            "SEND" | "SEND_ONE" => {
                let timeout = std::env::var("SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS")?.parse()?;
                let retry = PublicAuditRetryPolicy::new(Duration::from_millis(timeout), 1, Duration::from_millis(100))
                    .map_err(|_| "invalid retry configuration")?;
                let config = PublicAuditSenderConfig::from_env(retry).map_err(|_| "invalid sender configuration")?;
                let sender = PublicAuditSender::for_trusted_backend(outbox.clone(), config).map_err(|_| "invalid sender")?;
                let limit = if command == "SEND_ONE" { 1 } else { 1000 };
                let result: Vec<_> = sender.send_pending(limit).await?.iter().map(|r| json!({
                    "event_id":r.event_id,"attempts":r.attempts,"outcome":format!("{:?}",r.outcome)
                })).collect();
                emit(json!({"results":result}));
            }
            "WORKER" => {
                let config = AuditWorkerConfig::from_env()?.ok_or("worker must be explicitly enabled")?;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
                let (stop, stopped) = tokio::sync::oneshot::channel();
                let cloned = outbox.clone();
                let task = tokio::spawn(serve_with_public_audit(listener, axum::Router::new(), cloned,
                    Some(config), async { let _ = stopped.await; }));
                emit(json!({"worker_started":true}));
                if input.next().ok_or("trusted stop required")?? != "STOP_WORKER" {
                    return Err("invalid trusted stop".into());
                }
                let _ = stop.send(());
                task.await??;
                emit(json!({"worker_stopped":true}));
            }
            "STOP" => break,
            _ => return Err("invalid trusted command".into()),
        }
    }
    repository.close().await;
    Ok(())
}
#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("existing public audit backend unavailable");
        std::process::exit(1);
    }
}
