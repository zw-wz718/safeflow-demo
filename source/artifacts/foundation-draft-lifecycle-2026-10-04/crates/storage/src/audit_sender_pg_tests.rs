//! Sender mechanics against real isolated PG and a local protocol fixture.
//! This is NOT the member receiver and cannot prove two-end integration.
use super::*;
use crate::{
    AuditSendFailure, PublicAuditOutbox, PublicAuditRetryPolicy, PublicAuditSender,
    PublicAuditSenderConfig,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

#[derive(Clone)]
enum Reply {
    Good,
    Status(u16),
    DropAck,
    Slow,
    Partial,
    Malformed,
    Oversize,
    Wrong(&'static str),
    PositionalArray,
    NonObject(Value),
    DuplicateField,
    UnknownField,
    MissingField(&'static str),
}
#[derive(Default)]
struct ReceiverState {
    replies: VecDeque<Reply>,
    requests: Vec<Value>,
    receipts: HashMap<String, (Value, String)>,
    duplicates: Vec<(String, String)>,
    request_times: Vec<std::time::Instant>,
    response_bodies: Vec<String>,
}
struct Receiver {
    endpoint: String,
    state: Arc<Mutex<ReceiverState>>,
    job: tokio::task::JoinHandle<()>,
}
impl Receiver {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/api/audit/outbox/v1",
            listener.local_addr().unwrap()
        );
        let state = Arc::new(Mutex::new(ReceiverState::default()));
        let shared = state.clone();
        let job = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let shared = shared.clone();
                // Short-lived local fixture, no real credentials or external network.
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let end = loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(at) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
                            break at + 4;
                        }
                        assert!(bytes.len() < 8192);
                    };
                    let headers = String::from_utf8(bytes[..end].to_vec())
                        .unwrap()
                        .to_lowercase();
                    assert!(headers.starts_with("post /api/audit/outbox/v1 http/1.1\r\n"));
                    assert!(
                        headers.contains("authorization: bearer isolated-sender-test-token\r\n")
                    );
                    assert!(headers.contains("content-type: application/json\r\n"));
                    let length: usize = headers
                        .lines()
                        .find_map(|s| s.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert!(length <= 1048576);
                    while bytes.len() < end + length {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let body: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
                    let (mode, mut status, mut ack) = {
                        let mut state = shared.lock().unwrap();
                        state.requests.push(body.clone());
                        state.request_times.push(std::time::Instant::now());
                        let mode = state.replies.pop_front().unwrap_or(Reply::Good);
                        let event = body["event_id"].as_str().unwrap().to_owned();
                        let mut status = 201;
                        let mut duplicate = false;
                        let audit_id = if let Some((old, id)) = state.receipts.get(&event).cloned()
                        {
                            if old == body {
                                status = 200;
                                duplicate = true;
                                state.duplicates.push((event.clone(), id.clone()));
                            } else {
                                status = 409;
                            }
                            id
                        } else {
                            let id = uuid::Uuid::new_v4().to_string();
                            if matches!(mode, Reply::Good | Reply::DropAck) {
                                state
                                    .receipts
                                    .insert(event.clone(), (body.clone(), id.clone()));
                            }
                            id
                        };
                        let ack = json!({"ack_version":"audit-receiver-candidate-v1","accepted":true,"duplicate":duplicate,"event_id":event,"audit_id":audit_id,"task_id":body["envelope"]["public_event"]["task_id"],"operation_id":body["envelope"]["public_event"]["operation_id"],"digest_echo":body["digest"],"recorded_at":"2026-10-21T00:00:00Z"});
                        (mode, status, ack)
                    };
                    let mut raw = match mode {
                        Reply::DropAck => return,
                        Reply::Slow => {
                            tokio::time::sleep(Duration::from_millis(600)).await;
                            serde_json::to_vec(&ack).unwrap()
                        }
                        Reply::Status(code) => {
                            status = code;
                            b"{}".to_vec()
                        }
                        Reply::Partial => {
                            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 400\r\nConnection: close\r\n\r\n{\"accepted\":").await;
                            return;
                        }
                        Reply::Malformed => b"{broken".to_vec(),
                        Reply::Oversize => vec![b' '; 16385],
                        Reply::Wrong(key) => {
                            ack[key] = if key == "audit_id" {
                                json!("")
                            } else {
                                json!("mismatch")
                            };
                            serde_json::to_vec(&ack).unwrap()
                        }
                        Reply::PositionalArray => serde_json::to_vec(&json!([
                            ack["ack_version"],
                            ack["accepted"],
                            ack["duplicate"],
                            ack["event_id"],
                            ack["audit_id"],
                            ack["task_id"],
                            ack["operation_id"],
                            ack["digest_echo"],
                            ack["recorded_at"]
                        ]))
                        .unwrap(),
                        Reply::NonObject(value) => serde_json::to_vec(&value).unwrap(),
                        Reply::DuplicateField => serde_json::to_string(&ack)
                            .unwrap()
                            .replacen('{', "{\"accepted\":true,", 1)
                            .into_bytes(),
                        Reply::UnknownField => {
                            ack["unexpected"] = json!(true);
                            serde_json::to_vec(&ack).unwrap()
                        }
                        Reply::MissingField(key) => {
                            ack.as_object_mut().unwrap().remove(key);
                            serde_json::to_vec(&ack).unwrap()
                        }
                        Reply::Good => serde_json::to_vec(&ack).unwrap(),
                    };
                    shared
                        .lock()
                        .unwrap()
                        .response_bodies
                        .push(String::from_utf8(raw.clone()).unwrap());
                    let location = if status == 302 {
                        "Location: /api/audit/outbox/v1\r\n"
                    } else {
                        ""
                    };
                    let head = format!("HTTP/1.1 {status} Test\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", raw.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&raw).await;
                    raw.clear();
                });
            }
        });
        Self {
            endpoint,
            state,
            job,
        }
    }
    fn queue(&self, modes: impl IntoIterator<Item = Reply>) {
        self.state.lock().unwrap().replies.extend(modes);
    }
    fn count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.job.abort();
    }
}
fn sender(outbox: PublicAuditOutbox, endpoint: &str, attempts: u8) -> PublicAuditSender {
    let policy = PublicAuditRetryPolicy::new(
        Duration::from_millis(200),
        attempts,
        Duration::from_millis(100),
    )
    .unwrap();
    let config = PublicAuditSenderConfig::new(
        endpoint,
        Zeroizing::new("isolated-sender-test-token".into()),
        "isolated-protocol-receiver",
        policy,
    )
    .unwrap();
    PublicAuditSender::for_trusted_backend(outbox, config).unwrap()
}
fn wire(p: &crate::PendingPublicAudit) -> Value {
    json!({"event_id":p.event_id,"envelope":p.envelope,"digest":p.digest,"historical":p.historical,"enqueued_at":p.enqueued_at})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated PostgreSQL 16; real HTTP ACK shape regression"]
async fn audit_sender_real_pg_ack_requires_named_object() {
    let db = Db::new().await;
    let _ = db.fixture().await;
    let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
    let original = outbox.pending(100).await.unwrap();
    assert!(!original.is_empty());
    let receiver = Receiver::start().await;
    let mut rejected = Vec::new();
    for (name, reply) in [
        ("equivalent_positional_array", Reply::PositionalArray),
        ("empty_array", Reply::NonObject(json!([]))),
        ("null", Reply::NonObject(json!(null))),
        ("boolean", Reply::NonObject(json!(true))),
        ("number", Reply::NonObject(json!(17))),
        ("string", Reply::NonObject(json!("not an object"))),
        ("duplicate_accepted", Reply::DuplicateField),
        ("unknown_field", Reply::UnknownField),
        ("missing_event_id", Reply::MissingField("event_id")),
        (
            "missing_nullable_operation_id",
            Reply::MissingField("operation_id"),
        ),
    ] {
        let count = receiver.count();
        receiver.queue([reply]);
        let result = sender(outbox.clone(), &receiver.endpoint, 2)
            .send_pending(1)
            .await
            .unwrap();
        let after = outbox.pending(100).await.unwrap();
        let columns: Value = sqlx::query_scalar("SELECT jsonb_build_object('receiver',receiver,'receipt_ref',receipt_ref,'acknowledged_at',acknowledged_at) FROM public_audit_outbox WHERE event_id=$1")
            .bind(original[0].event_id.as_str()).fetch_one(&db.sql).await.unwrap();
        let ack_count: i64 = sqlx::query_scalar("SELECT count(*) FROM public_audit_outbox WHERE receiver IS NOT NULL OR receipt_ref IS NOT NULL OR acknowledged_at IS NOT NULL")
            .fetch_one(&db.sql).await.unwrap();
        let state = receiver.state.lock().unwrap();
        let evidence = json!({"name":name,"outcome":format!("{:?}",result[0].outcome),"attempts":result[0].attempts,"requests":state.requests.len()-count,"raw_ack":state.response_bodies.last().unwrap(),"request_body":state.requests.last().unwrap(),"original_pending":original.iter().map(wire).collect::<Vec<_>>(),"pending_after":after.iter().map(wire).collect::<Vec<_>>(),"acknowledgement_columns":columns,"any_acknowledgement_count":ack_count});
        drop(state);
        // Preserve actual evidence and clean the random schema even if a
        // regression returns success and has erroneously removed the pending.
        if result[0].outcome != Err(AuditSendFailure::AckProtocol)
            || after != original
            || ack_count != 0
        {
            db.finish().await;
            panic!("ACK shape regression: {evidence}");
        }
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attempts, 1);
        assert_eq!(receiver.count() - count, 1);
        assert_eq!(
            columns,
            json!({"receiver":null,"receipt_ref":null,"acknowledged_at":null})
        );
        assert_eq!(evidence["request_body"], wire(&original[0]));
        rejected.push(evidence);
    }
    // Same rejected original, then remaining real originals, accept only named
    // object responses, covering both null and string operation_id values.
    let mut accepted = Vec::new();
    for pending in &original {
        assert_eq!(outbox.pending(1).await.unwrap()[0], *pending);
        let count = receiver.count();
        let result = sender(outbox.clone(), &receiver.endpoint, 2)
            .send_pending(1)
            .await
            .unwrap();
        assert_eq!(result[0].outcome, Ok(()));
        assert_eq!(result[0].attempts, 1);
        assert_eq!(receiver.count() - count, 1);
        let columns: Value = sqlx::query_scalar("SELECT jsonb_build_object('receiver',receiver,'receipt_ref',receipt_ref,'acknowledged_at',acknowledged_at) FROM public_audit_outbox WHERE event_id=$1")
            .bind(pending.event_id.as_str()).fetch_one(&db.sql).await.unwrap();
        assert_eq!(columns["receiver"], "isolated-protocol-receiver");
        assert!(columns["receipt_ref"].is_string());
        assert!(columns["acknowledged_at"].is_string());
        let state = receiver.state.lock().unwrap();
        let raw = state.response_bodies.last().unwrap();
        let ack: Value = serde_json::from_str(raw).unwrap();
        assert!(ack.is_object());
        assert_eq!(
            ack["operation_id"],
            pending.envelope["public_event"]["operation_id"]
        );
        assert_eq!(ack["audit_id"], columns["receipt_ref"]);
        assert_eq!(state.requests.last().unwrap(), &wire(pending));
        accepted.push(json!({"original":wire(pending),"ack":ack,"outcome":"Ok(())","attempts":1,"requests":1,"acknowledgement_columns":columns}));
    }
    assert!(outbox.pending(100).await.unwrap().is_empty());
    assert!(accepted.iter().any(|v| v["ack"]["operation_id"].is_null()));
    assert!(accepted
        .iter()
        .any(|v| v["ack"]["operation_id"].is_string()));
    let mut business = Vec::new();
    for table in [
        "transfer_ledger_v1",
        "public_execution_requests",
        "execution_permits",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(n, 0);
        business.push(json!({"table":table,"count":n}));
    }
    let evidence = json!({"rejected":rejected,"accepted":accepted,"business_counts":business,"schema":db.schema,"member_receiver_used":false,"shared_pending_used":false});
    db.finish().await;
    if let Ok(dir) = std::env::var("SAFEFLOW_AUDIT_SENDER_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&dir).join("ack-shape-results.json"),
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();
    }
    println!(
        "AUDIT_SENDER_ACK_SHAPE_PG_OK rejected={} accepted={}",
        rejected.len(),
        accepted.len()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated PostgreSQL 16; local protocol fixture, not member integration"]
async fn audit_sender_real_pending_failures_retry_ack_and_process_restart() {
    let db = Db::new().await;
    let _ = db.fixture().await;
    let outbox = PublicAuditOutbox::for_trusted_backend(db.repo.clone());
    let original = outbox.pending(100).await.unwrap();
    assert!(original.len() >= 2);
    let item = &original[0];
    assert_eq!(item.digest, crate::models::hash(&item.envelope).unwrap());
    // Contains only generated isolated fixture identities, no shared user pending.
    if let Ok(dir) = std::env::var("SAFEFLOW_AUDIT_SENDER_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&dir).join("pending-isolated-actual.json"),
            serde_json::to_vec_pretty(&wire(item)).unwrap(),
        )
        .unwrap();
    }
    let receiver = Receiver::start().await;
    let mut checks = Vec::new();
    for (reply, expected, attempts) in [
        (Reply::Status(400), AuditSendFailure::Http(400), 1),
        (Reply::Status(401), AuditSendFailure::Http(401), 1),
        (Reply::Status(409), AuditSendFailure::Http(409), 1),
        (Reply::Status(503), AuditSendFailure::Http(503), 2),
        (Reply::Status(302), AuditSendFailure::Http(302), 1),
        (Reply::Slow, AuditSendFailure::Transport, 2),
        (Reply::Partial, AuditSendFailure::Transport, 2),
        (Reply::Malformed, AuditSendFailure::AckProtocol, 1),
        (Reply::Oversize, AuditSendFailure::AckProtocol, 1),
    ] {
        let before = receiver.count();
        receiver.queue(std::iter::repeat_n(reply, attempts));
        let result = sender(outbox.clone(), &receiver.endpoint, 2)
            .send_pending(1)
            .await
            .unwrap();
        assert_eq!(result[0].outcome, Err(expected));
        assert_eq!(result[0].attempts, attempts as u8);
        assert_eq!(receiver.count() - before, attempts);
        if attempts == 2 {
            let state = receiver.state.lock().unwrap();
            assert!(
                state.request_times[before + 1].duration_since(state.request_times[before])
                    >= Duration::from_millis(100)
            );
        }
        assert_eq!(outbox.pending(100).await.unwrap(), original);
        checks.push(format!(
            "{expected:?}: pending unchanged; attempts={attempts}"
        ));
    }
    for field in [
        "ack_version",
        "accepted",
        "event_id",
        "task_id",
        "operation_id",
        "digest_echo",
        "audit_id",
        "recorded_at",
        "duplicate",
    ] {
        receiver.queue([Reply::Wrong(field)]);
        let result = sender(outbox.clone(), &receiver.endpoint, 2)
            .send_pending(1)
            .await
            .unwrap();
        assert_eq!(
            result[0].outcome,
            Err(AuditSendFailure::AckProtocol),
            "{field}"
        );
        assert_eq!(result[0].attempts, 1);
        assert_eq!(outbox.pending(100).await.unwrap(), original);
    }
    checks.push("all ACK field mismatches leave original pending".into());

    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!(
        "http://{}/api/audit/outbox/v1",
        closed.local_addr().unwrap()
    );
    drop(closed);
    let unavailable = sender(outbox.clone(), &endpoint, 2)
        .send_pending(1)
        .await
        .unwrap();
    assert_eq!(unavailable[0].outcome, Err(AuditSendFailure::Transport));
    assert_eq!(unavailable[0].attempts, 2);
    assert_eq!(outbox.pending(100).await.unwrap(), original);
    checks.push("connection refused preserves pending".into());

    let mut conflicting = wire(item);
    conflicting["envelope"]["public_event"]["details"] =
        json!({"different":"existing receiver fixture content"});
    receiver.state.lock().unwrap().receipts.insert(
        item.event_id.as_str().into(),
        (conflicting.clone(), "existing-conflict-receipt".into()),
    );
    let conflict = sender(outbox.clone(), &receiver.endpoint, 2)
        .send_pending(1)
        .await
        .unwrap();
    assert_eq!(conflict[0].outcome, Err(AuditSendFailure::Http(409)));
    assert_eq!(conflict[0].attempts, 1);
    assert_eq!(outbox.pending(100).await.unwrap(), original);
    assert_eq!(
        receiver.state.lock().unwrap().receipts[item.event_id.as_str()].0,
        conflicting
    );
    receiver.state.lock().unwrap().receipts.clear();
    checks.push("actual same-ID different fixture contents return 409 unchanged".into());

    // A matching valid ACK cannot mark delivered if the local transaction fails.
    db.sql.execute("CREATE FUNCTION reject_sender_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated ack failure'; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER reject_sender_ack BEFORE UPDATE ON public_audit_outbox FOR EACH ROW EXECUTE FUNCTION reject_sender_ack()").await.unwrap();
    let failed = sender(outbox.clone(), &receiver.endpoint, 1)
        .send_pending(1)
        .await
        .unwrap();
    assert_eq!(failed[0].outcome, Err(AuditSendFailure::Acknowledge));
    assert_eq!(outbox.pending(100).await.unwrap(), original);
    db.sql
        .execute("DROP TRIGGER reject_sender_ack ON public_audit_outbox")
        .await
        .unwrap();
    checks.push("real acknowledge transaction failure preserves pending".into());

    receiver.queue([Reply::DropAck]);
    let dropped = sender(outbox.clone(), &receiver.endpoint, 1)
        .send_pending(1)
        .await
        .unwrap();
    assert_eq!(dropped[0].outcome, Err(AuditSendFailure::Transport));
    assert_eq!(outbox.pending(100).await.unwrap(), original);
    let receipt = receiver.state.lock().unwrap().receipts[item.event_id.as_str()]
        .1
        .clone();
    let exe = std::env::current_exe().unwrap();
    let schema = db.schema.clone();
    let endpoint = receiver.endpoint.clone();
    let expected = serde_json::to_string(&wire(item)).unwrap();
    let child = tokio::task::spawn_blocking(move || {
        std::process::Command::new(exe)
            .args([
                "--exact",
                "permit::tests::audit_sender_pg::audit_sender_process_child",
                "--ignored",
                "--nocapture",
            ])
            .env("SAFEFLOW_AUDIT_SENDER_CHILD_SCHEMA", schema)
            .env("SAFEFLOW_AUDIT_SENDER_CHILD_ENDPOINT", endpoint)
            .env("SAFEFLOW_AUDIT_SENDER_CHILD_PENDING", expected)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        child.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&child.stdout)
    );
    assert!(String::from_utf8_lossy(&child.stdout).contains("AUDIT_SENDER_CHILD_CONFIRMED"));
    assert_eq!(outbox.pending(100).await.unwrap(), original[1..]);
    let ack: (String, String) =
        sqlx::query_as("SELECT receiver,receipt_ref FROM public_audit_outbox WHERE event_id=$1")
            .bind(item.event_id.as_str())
            .fetch_one(&db.sql)
            .await
            .unwrap();
    assert_eq!(ack, ("isolated-protocol-receiver".into(), receipt.clone()));
    assert!(receiver
        .state
        .lock()
        .unwrap()
        .duplicates
        .iter()
        .all(|(_, id)| id == &receipt));
    checks
        .push("lost ACK, new process original reread and duplicate same receipt confirmed".into());

    receiver.queue([Reply::Status(503), Reply::Good]);
    let recovered = sender(outbox.clone(), &receiver.endpoint, 2)
        .send_pending(1)
        .await
        .unwrap();
    assert_eq!(recovered[0].outcome, Ok(()));
    assert_eq!(recovered[0].attempts, 2);
    let remaining = sender(outbox.clone(), &receiver.endpoint, 1)
        .send_pending(100)
        .await
        .unwrap();
    assert!(remaining.iter().all(|r| r.outcome.is_ok()));
    assert!(outbox.pending(100).await.unwrap().is_empty());
    // Compare every sent field with actual pending snapshots, not attachment examples.
    let requests = receiver.state.lock().unwrap().requests.clone();
    for sent in &requests {
        let p = original
            .iter()
            .find(|p| Some(p.event_id.as_str()) == sent["event_id"].as_str())
            .unwrap();
        assert_eq!(sent, &wire(p));
    }
    assert!(requests
        .iter()
        .any(|p| p["envelope"]["public_event"]["operation_id"].is_string()));
    let mut business = Vec::new();
    for table in [
        "transfer_ledger_v1",
        "public_execution_requests",
        "execution_permits",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(n, 0);
        business.push((table, n));
    }
    checks.push(
        "first 201, explicit null/string operations, full body preserved; no business writes"
            .into(),
    );
    let result = json!({"checks":checks,"pending_source":"actual isolated PersistenceService fixture + migration trigger + pending()","original_count":original.len(),"request_count":requests.len(),"business_counts":business,"member_receiver_used":false,"shared_pending_used":false,"schema_cleanup_expected":db.schema});
    db.finish().await;
    if let Ok(dir) = std::env::var("SAFEFLOW_AUDIT_SENDER_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&dir).join("sender-pg-results.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
    }
    println!("AUDIT_SENDER_REAL_PG_MECHANICS_OK {result}");
}

#[tokio::test]
#[ignore = "only the parent supplies a random isolated schema and local endpoint"]
async fn audit_sender_process_child() {
    let schema = std::env::var("SAFEFLOW_AUDIT_SENDER_CHILD_SCHEMA").unwrap();
    let suffix = schema.strip_prefix("sf_permit_test_").unwrap();
    assert_eq!(suffix.len(), 32);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    let repo = PgRepository::connect_existing(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &schema,
    )
    .await
    .unwrap();
    let outbox = PublicAuditOutbox::for_trusted_backend(repo.clone());
    let original: Value =
        serde_json::from_str(&std::env::var("SAFEFLOW_AUDIT_SENDER_CHILD_PENDING").unwrap())
            .unwrap();
    assert_eq!(wire(&outbox.pending(1).await.unwrap()[0]), original);
    let endpoint = std::env::var("SAFEFLOW_AUDIT_SENDER_CHILD_ENDPOINT").unwrap();
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let result = sender(outbox, &endpoint, 1).send_pending(1).await.unwrap();
    assert_eq!(result[0].outcome, Ok(()));
    repo.close().await;
    println!("AUDIT_SENDER_CHILD_CONFIRMED");
}
