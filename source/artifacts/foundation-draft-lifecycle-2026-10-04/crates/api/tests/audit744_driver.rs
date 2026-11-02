//! Explicit isolated integration driver. It never constructs an event or executes business.
use safeflow_api::{serve_with_public_audit, AuditWorkerConfig};
use safeflow_storage::{PgRepository, PublicAuditOutbox, PublicAuditRetryPolicy, PublicAuditSender, PublicAuditSenderConfig};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor};
use std::{io::Write, time::Duration};

fn env(name: &str) -> String { std::env::var(name).unwrap() }
fn save(path: &str, value: &Value) {
    std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap()
        .write_all(&serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
async fn snapshot(url: &str, schema: &str, outbox: &PublicAuditOutbox) -> Value {
    let pool = PgPoolOptions::new().max_connections(1).connect(url).await.unwrap();
    let mut connection = pool.acquire().await.unwrap();
    connection.execute(format!("SET search_path TO {schema}").as_str()).await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename::text FROM pg_tables WHERE schemaname=$1 ORDER BY tablename")
        .bind(schema).fetch_all(&mut *connection).await.unwrap();
    let mut facts = serde_json::Map::new();
    let mut ack = json!([]);
    for table in tables {
        assert!(table.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        let expression = if table == "public_audit_outbox" {
            "to_jsonb(t)-'receiver'-'receipt_ref'-'acknowledged_at'"
        } else { "to_jsonb(t)" };
        let rows: Value = sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(v ORDER BY v::text),'[]'::jsonb) FROM (SELECT {expression} AS v FROM \"{table}\" t) q"))
            .fetch_one(&mut *connection).await.unwrap();
        facts.insert(table.clone(), rows);
        if table == "public_audit_outbox" {
            ack = sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY sequence),'[]'::jsonb) FROM public_audit_outbox t")
                .fetch_one(&mut *connection).await.unwrap();
        }
    }
    let pending: Vec<_> = outbox.pending(1000).await.unwrap().iter().map(|p| json!({
        "event_id":p.event_id,"envelope":p.envelope,"digest":p.digest,
        "historical":p.historical,"enqueued_at":p.enqueued_at
    })).collect();
    drop(connection);
    pool.close().await;
    json!({"facts":facts,"outbox":ack,"pending":pending})
}

#[tokio::test]
#[ignore = "explicit owned audit744 PG and loopback receiver only"]
async fn audit744_public_driver() {
    let url = env("SAFEFLOW_TEST_DATABASE_URL");
    assert!(url.starts_with("postgresql://sf_a744@127.0.0.1:"));
    assert!(url.ends_with("/billing_test_a744"));
    let schema = env("AUD744_SCHEMA");
    assert!((schema.starts_with("sf_http_test_") && schema.len()==45)
        || (schema.starts_with("sf_subscription_test_") && schema.len()==53));
    assert!(schema.bytes().all(|b| b.is_ascii_alphanumeric() || b==b'_'));
    let repo = PgRepository::connect_existing(&url, &schema).await.unwrap();
    let outbox = PublicAuditOutbox::for_trusted_backend(repo.clone());
    let before = snapshot(&url, &schema, &outbox).await;
    let mut results = vec![];
    let mode = env("AUD744_MODE");
    match mode.as_str() {
        "snapshot" => {},
        "send" => {
            let retry = PublicAuditRetryPolicy::new(Duration::from_millis(env("SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS").parse().unwrap()),
                env("SAFEFLOW_PUBLIC_AUDIT_ATTEMPTS").parse().unwrap(), Duration::from_millis(100)).unwrap();
            let sender = PublicAuditSender::for_trusted_backend(outbox.clone(), PublicAuditSenderConfig::from_env(retry).unwrap()).unwrap();
            let sent = sender.send_pending(env("SAFEFLOW_PUBLIC_AUDIT_BATCH").parse().unwrap()).await.unwrap();
            for r in sent { results.push(json!({"event_id":r.event_id,"attempts":r.attempts,"outcome":format!("{:?}",r.outcome)})); }
        },
        "worker" => {
            let stop = env("AUD744_STOP");
            assert!(!std::path::Path::new(&stop).exists());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            save(&env("AUD744_READY"), &json!({"pid":std::process::id(),"address":listener.local_addr().unwrap().to_string()}));
            serve_with_public_audit(listener, axum::Router::new(), outbox.clone(), AuditWorkerConfig::from_env().unwrap(), async move {
                let start=std::time::Instant::now();
                while !std::path::Path::new(&stop).exists() {
                    assert!(start.elapsed() < Duration::from_secs(60), "trusted stop not received");
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await.unwrap();
        },
        _ => panic!("unrecognized explicit mode"),
    }
    let after = snapshot(&url, &schema, &outbox).await;
    assert_eq!(before["facts"], after["facts"], "delivery must never change business/event/link/original outbox facts");
    save(&env("AUD744_OUTPUT"), &json!({"pid":std::process::id(),"schema":schema,"mode":mode,"before":before,"results":results,"after":after}));
    repo.close().await;
}
