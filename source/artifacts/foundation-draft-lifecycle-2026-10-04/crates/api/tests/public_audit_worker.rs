//! Isolated real PostgreSQL plus protocol fixture; never the member receiver.
use safeflow_api::{serve_with_public_audit, AuditWorkerConfig};
use safeflow_contracts::{AgentPlan, NodeId, PlanNode, SessionRef, UserId};
use safeflow_runtime::StaticSessionResolver;
use safeflow_storage::{new_task_id, PersistenceService, PgRepository, PublicAuditOutbox};
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone, Copy)]
enum Mode {
    Good,
    Status(u16),
    DropAck,
    Wrong,
    Hold,
}
struct State {
    mode: Mode,
    requests: Vec<(Value, Instant)>,
    receipts: HashMap<String, (Value, String)>,
    responses: Vec<Value>,
}
struct Receiver {
    endpoint: String,
    state: Arc<Mutex<State>>,
    job: tokio::task::JoinHandle<()>,
}
impl Receiver {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/api/audit/outbox/v1",
            listener.local_addr().unwrap()
        );
        let state = Arc::new(Mutex::new(State {
            mode: Mode::Good,
            requests: vec![],
            receipts: HashMap::new(),
            responses: vec![],
        }));
        let shared = state.clone();
        let job = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let state = shared.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let end = loop {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                        if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                            break i + 4;
                        }
                        assert!(bytes.len() < 8192);
                    };
                    let headers = String::from_utf8(bytes[..end].to_vec())
                        .unwrap()
                        .to_lowercase();
                    assert!(headers.starts_with("post /api/audit/outbox/v1 http/1.1\r\n"));
                    assert!(headers.contains("authorization: bearer isolated-worker-token\r\n"));
                    let length: usize = headers
                        .lines()
                        .find_map(|v| v.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    assert!(length < 1048576);
                    while bytes.len() < end + length {
                        let mut buf = [0; 4096];
                        let n = socket.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buf[..n]);
                    }
                    let body: Value = serde_json::from_slice(&bytes[end..end + length]).unwrap();
                    let (mode, status, ack) = {
                        let mut s = state.lock().unwrap();
                        s.requests.push((body.clone(), Instant::now()));
                        let mode = s.mode;
                        let event = body["event_id"].as_str().unwrap().to_string();
                        let (mut status, duplicate, id) = match s.receipts.get(&event) {
                            Some((old, id)) => {
                                (if old == &body { 200 } else { 409 }, true, id.clone())
                            }
                            None => (201, false, uuid::Uuid::new_v4().to_string()),
                        };
                        if matches!(mode, Mode::Good | Mode::DropAck) && status != 409 {
                            s.receipts.insert(event.clone(), (body.clone(), id.clone()));
                        }
                        if let Mode::Status(code) = mode {
                            status = code;
                        }
                        let mut ack = json!({"ack_version":"audit-receiver-candidate-v1","accepted":true,"duplicate":duplicate,"event_id":event,"audit_id":id,"task_id":body["envelope"]["public_event"]["task_id"],"operation_id":body["envelope"]["public_event"]["operation_id"],"digest_echo":body["digest"],"recorded_at":"2026-10-21T00:00:00Z"});
                        if matches!(mode, Mode::Wrong) {
                            ack["digest_echo"] = json!("wrong");
                        }
                        s.responses.push(json!({"status":status,"ack":ack}));
                        (mode, status, ack)
                    };
                    if matches!(mode, Mode::DropAck) {
                        return;
                    }
                    if matches!(mode, Mode::Hold) {
                        let mut byte = [0];
                        let _ = socket.read(&mut byte).await;
                        return;
                    }
                    let raw = serde_json::to_vec(&ack).unwrap();
                    let header=format!("HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",raw.len());
                    let _ = socket.write_all(header.as_bytes()).await;
                    let _ = socket.write_all(&raw).await;
                });
            }
        });
        Self {
            endpoint,
            state,
            job,
        }
    }
    fn mode(&self, mode: Mode) {
        self.state.lock().unwrap().mode = mode;
    }
    fn count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }
    async fn wait_count(&self, n: usize) {
        let until = Instant::now() + Duration::from_secs(8);
        while self.count() < n {
            assert!(Instant::now() < until, "receiver timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.job.abort();
    }
}
struct Db {
    url: String,
    schema: String,
    admin: PgPool,
    sql: PgPool,
    repo: PgRepository,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("explicit isolated PostgreSQL required");
        let schema = format!("sf_http_sim_{}", uuid::Uuid::new_v4().simple());
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .expect("isolated DB connection");
        admin
            .execute(format!("CREATE SCHEMA {schema}").as_str())
            .await
            .unwrap();
        let path = schema.clone();
        let sql = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |c, _| {
                let p = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(p)
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let repo = PgRepository::connect(&url, &schema).await.unwrap();
        repo.migrate().await.unwrap();
        Self {
            url,
            schema,
            admin,
            sql,
            repo,
        }
    }
    async fn seed(&self) -> Value {
        let session = SessionRef::parse("isolated_audit_worker_session").unwrap();
        let mut sessions = StaticSessionResolver::new();
        sessions.insert(
            session.clone(),
            UserId::parse("isolated_audit_worker_owner").unwrap(),
        );
        let plan = AgentPlan::new(
            new_task_id(),
            1,
            vec![PlanNode::UserDecision {
                id: NodeId::parse("isolated_choice").unwrap(),
                depends_on: vec![],
            }],
        )
        .unwrap();
        PersistenceService::for_trusted_backend(self.repo.clone(), Arc::new(sessions))
            .create_task(&session, plan, 1)
            .await
            .unwrap();
        let items = PublicAuditOutbox::for_trusted_backend(self.repo.clone())
            .pending(100)
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        let p = &items[0];
        json!({"event_id":p.event_id,"envelope":p.envelope,"digest":p.digest,"historical":p.historical,"enqueued_at":p.enqueued_at})
    }
    async fn ack(&self) -> Value {
        sqlx::query_scalar("SELECT jsonb_build_object('receiver',receiver,'receipt_ref',receipt_ref,'acknowledged_at',acknowledged_at) FROM public_audit_outbox ORDER BY sequence LIMIT 1").fetch_one(&self.sql).await.unwrap()
    }
    async fn wait_ack(&self) {
        let until = Instant::now() + Duration::from_secs(8);
        while self.ack().await["acknowledged_at"].is_null() {
            assert!(Instant::now() < until, "local ACK timed out");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    async fn finish(self) {
        self.repo.close().await;
        self.sql.close().await;
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.schema).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn root() -> PathBuf {
    PathBuf::from(
        std::env::var("SAFEFLOW_WORKER_EVIDENCE_DIR").expect("explicit evidence directory"),
    )
}
fn save(name: &str, value: &Value) {
    std::fs::write(root().join(name), serde_json::to_vec_pretty(value).unwrap()).unwrap();
}
fn env_values(endpoint: &str) -> Vec<(&'static str, String)> {
    vec![
        ("SAFEFLOW_PUBLIC_AUDIT_ENABLED", "1".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_ENDPOINT", endpoint.into()),
        (
            "SAFEFLOW_PUBLIC_AUDIT_TOKEN",
            "isolated-worker-token".into(),
        ),
        (
            "SAFEFLOW_PUBLIC_AUDIT_RECEIVER",
            "isolated-worker-receiver".into(),
        ),
        ("SAFEFLOW_PUBLIC_AUDIT_POLL_MS", "300".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_BATCH", "1".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS", "5000".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_ATTEMPTS", "1".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_BACKOFF_MS", "100".into()),
        ("SAFEFLOW_PUBLIC_AUDIT_STOP_GRACE_MS", "500".into()),
    ]
}
struct Main {
    launcher: Child,
    base: PathBuf,
    pid: u32,
}
impl Main {
    async fn start(
        db: &Db,
        name: &str,
        endpoint: Option<&str>,
        overrides: &[(&str, &str)],
        port: u16,
    ) -> Self {
        let base = root().join(name);
        assert!(!base.with_extension("pid").exists());
        let mut c = Command::new("powershell.exe");
        c.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(root().join("launch-main.ps1"))
        .arg("-Executable")
        .arg(env!("CARGO_BIN_EXE_safeflow-api"))
        .arg("-OutputBase")
        .arg(&base);
        for (k, _) in
            std::env::vars_os().filter(|(k, _)| k.to_string_lossy().starts_with("SAFEFLOW_"))
        {
            c.env_remove(k);
        }
        c.env("SAFEFLOW_DATABASE_URL", &db.url)
            .env("SAFEFLOW_STORAGE_SCHEMA", &db.schema)
            .env("SAFEFLOW_SIM_LOGIN_SECRET", "isolated-worker-login-secret")
            .env("SAFEFLOW_API_ADDR", format!("127.0.0.1:{port}"));
        if let Some(e) = endpoint {
            c.envs(env_values(e));
        }
        c.envs(overrides.iter().copied());
        let launcher = c.spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(8);
        while !base.with_extension("pid").exists() {
            assert!(Instant::now() < until, "main launcher timeout");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let pid = std::fs::read_to_string(base.with_extension("pid"))
            .unwrap()
            .parse()
            .unwrap();
        Self {
            launcher,
            base,
            pid,
        }
    }
    async fn stop(&mut self) {
        let status = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(root().join("signal-main.ps1"))
            .arg("-TargetProcessId")
            .arg(self.pid.to_string())
            .arg("-Executable")
            .arg(env!("CARGO_BIN_EXE_safeflow-api"))
            .status()
            .unwrap();
        assert!(status.success(), "real Ctrl+C helper failed");
        assert!(
            self.launcher.wait().unwrap().success(),
            "main exited nonzero"
        );
        assert_eq!(
            std::fs::read_to_string(self.base.with_extension("exit")).unwrap(),
            "0"
        );
    }
    async fn failed(&mut self) {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(s) = self.launcher.try_wait().unwrap() {
                assert!(!s.success());
                return;
            }
            assert!(Instant::now() < until);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}
impl Drop for Main {
    fn drop(&mut self) {
        if self.launcher.try_wait().ok().flatten().is_none() {
            // Failure cleanup only; never evidence of graceful exit.
            let _ = Command::new("taskkill.exe")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .output();
            let _ = self.launcher.kill();
            let _ = self.launcher.wait();
        }
    }
}
fn port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
async fn ready(port: u16) {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        if let Ok(mut s) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            s.write_all(
                format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
            let mut b = vec![];
            s.read_to_end(&mut b).await.unwrap();
            assert!(b.starts_with(b"HTTP/1.1 200"));
            return;
        }
        assert!(Instant::now() < until, "main not listening");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires isolated PG16 and Windows dedicated hidden console"]
async fn public_audit_worker_real_main_pg_lifecycle() {
    assert!(cfg!(windows), "Windows real Ctrl+C test");
    let receiver = Receiver::new().await;
    // Off, bad/residual config and bind failure cannot consume an original item.
    let db = Db::new().await;
    let pending = db.seed().await;
    save("real-pending.json", &pending);
    save("real-pending-before-sql.json", &db.ack().await);
    let p = port();
    let mut off = Main::start(&db, "main-default-off", None, &[], p).await;
    ready(p).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(receiver.count(), 0);
    assert!(db.ack().await["receiver"].is_null());
    off.stop().await;
    for (name, opts) in [
        (
            "main-residual",
            vec![("SAFEFLOW_PUBLIC_AUDIT_TOKEN", "isolated-worker-token")],
        ),
        (
            "main-invalid",
            vec![("SAFEFLOW_PUBLIC_AUDIT_ENABLED", "yes")],
        ),
        ("main-partial", vec![("SAFEFLOW_PUBLIC_AUDIT_ENABLED", "1")]),
    ] {
        let mut main = Main::start(&db, name, None, &opts, port()).await;
        main.failed().await;
        assert_eq!(receiver.count(), 0);
    }
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut failed = Main::start(
        &db,
        "main-bind-failure",
        Some(&receiver.endpoint),
        &[],
        occupied.local_addr().unwrap().port(),
    )
    .await;
    failed.failed().await;
    assert_eq!(receiver.count(), 0);
    drop(occupied);
    let p = port();
    let mut good = Main::start(&db, "main-first-201", Some(&receiver.endpoint), &[], p).await;
    ready(p).await;
    db.wait_ack().await;
    good.stop().await;
    let ack = db.ack().await;
    let s = receiver.state.lock().unwrap();
    assert_eq!(s.requests[0].0, pending);
    assert_eq!(s.responses[0]["status"], 201);
    assert_eq!(ack["receipt_ref"], s.responses[0]["ack"]["audit_id"]);
    drop(s);
    assert_eq!(ack["receiver"], "isolated-worker-receiver");
    save("real-pending-after-sql.json", &ack);
    db.finish().await;
    // Each failure leaves identical bytes/IDs; stop cancels an in-flight send.
    let mut matrix = vec![];
    for (name, mode) in [
        ("503", Mode::Status(503)),
        ("401", Mode::Status(401)),
        ("wrong-ack", Mode::Wrong),
        ("lost-ack", Mode::DropAck),
        ("inflight-stop", Mode::Hold),
        ("timeout", Mode::Hold),
    ] {
        let db = Db::new().await;
        let original = db.seed().await;
        receiver.mode(mode);
        let before = receiver.count();
        let p = port();
        let mut main = Main::start(
            &db,
            &format!("main-{name}"),
            Some(&receiver.endpoint),
            if name == "timeout" {
                &[("SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS", "200")]
            } else if name == "503" {
                &[("SAFEFLOW_PUBLIC_AUDIT_ATTEMPTS", "3")]
            } else {
                &[]
            },
            p,
        )
        .await;
        ready(p).await;
        receiver.wait_count(before + 1).await;
        if name != "inflight-stop" {
            receiver
                .wait_count(before + if name == "503" { 4 } else { 3 })
                .await;
            let s = receiver.state.lock().unwrap();
            for (i, pair) in s.requests[before..].windows(2).enumerate() {
                let floor = if name == "503" {
                    [90, 190, 280][i % 3]
                } else {
                    280
                };
                assert!(
                    pair[1].1.duration_since(pair[0].1) >= Duration::from_millis(floor),
                    "busy polling"
                );
            }
        }
        let stopped = Instant::now();
        main.stop().await;
        let stop_ms = stopped.elapsed().as_millis();
        assert!(stop_ms < 8000);
        assert!(db.ack().await["acknowledged_at"].is_null());
        let stable = receiver.count();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            receiver.count(),
            stable,
            "background request after shutdown"
        );
        let item = PublicAuditOutbox::for_trusted_backend(db.repo.clone())
            .pending(1)
            .await
            .unwrap()
            .remove(0);
        assert_eq!(
            json!({"event_id":item.event_id,"envelope":item.envelope,"digest":item.digest,"historical":item.historical,"enqueued_at":item.enqueued_at}),
            original
        );
        let stored = receiver
            .state
            .lock()
            .unwrap()
            .receipts
            .get(original["event_id"].as_str().unwrap())
            .map(|(_, id)| id.clone());
        receiver.mode(Mode::Good);
        let p = port();
        let mut next = Main::start(
            &db,
            &format!("main-{name}-restart"),
            Some(&receiver.endpoint),
            &[],
            p,
        )
        .await;
        ready(p).await;
        db.wait_ack().await;
        next.stop().await;
        let ack = db.ack().await;
        if let Some(id) = stored {
            assert_eq!(ack["receipt_ref"], id);
            let s = receiver.state.lock().unwrap();
            assert_eq!(s.responses.last().unwrap()["status"], 200);
        }
        let tasks: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        let operations: i64 = sqlx::query_scalar("SELECT count(*) FROM operations")
            .fetch_one(&db.sql)
            .await
            .unwrap();
        assert_eq!(tasks, 1);
        assert_eq!(operations, 0);
        let s = receiver.state.lock().unwrap();
        assert!(s.requests[before..].iter().all(|(v, _)| v == &original));
        let responses = s.responses[before..].to_vec();
        let count = s.requests.len() - before;
        drop(s);
        matrix.push(json!({"case":name,"requests":count,"original":original,"responses":responses,"ack_after_restart":ack,"stop_ms":stop_ms,"tasks":tasks,"business_operations":operations}));
        db.finish().await;
    }
    // ACK transaction error: remote stored, local refused, original survives.
    let db = Db::new().await;
    let original = db.seed().await;
    db.sql.execute("CREATE FUNCTION reject_worker_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated ack rejection'; END $$").await.unwrap();
    db.sql.execute("CREATE TRIGGER reject_worker_ack BEFORE UPDATE ON public_audit_outbox FOR EACH ROW EXECUTE FUNCTION reject_worker_ack()").await.unwrap();
    receiver.mode(Mode::Good);
    let before = receiver.count();
    let p = port();
    let mut main = Main::start(&db, "main-db-ack-failure", Some(&receiver.endpoint), &[], p).await;
    ready(p).await;
    receiver.wait_count(before + 2).await;
    main.stop().await;
    assert!(db.ack().await["receiver"].is_null());
    let id = receiver.state.lock().unwrap().receipts[original["event_id"].as_str().unwrap()]
        .1
        .clone();
    db.sql
        .execute("DROP TRIGGER reject_worker_ack ON public_audit_outbox")
        .await
        .unwrap();
    let p = port();
    let mut main = Main::start(&db, "main-db-ack-restart", Some(&receiver.endpoint), &[], p).await;
    ready(p).await;
    db.wait_ack().await;
    main.stop().await;
    assert_eq!(db.ack().await["receipt_ref"], id);
    matrix.push(json!({"case":"local-ack-transaction-failure","original":original,"recovered_ack":db.ack().await}));
    db.finish().await;
    // Connection refused remains pending, resumes in another process.
    let db = Db::new().await;
    let original = db.seed().await;
    let p = port();
    let dead = format!("http://127.0.0.1:{}/api/audit/outbox/v1", port());
    let mut main = Main::start(
        &db,
        "main-network-down",
        Some(&dead),
        &[("SAFEFLOW_PUBLIC_AUDIT_TIMEOUT_MS", "200")],
        p,
    )
    .await;
    ready(p).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    main.stop().await;
    assert!(db.ack().await["receiver"].is_null());
    let p = port();
    let mut main = Main::start(
        &db,
        "main-network-restart",
        Some(&receiver.endpoint),
        &[],
        p,
    )
    .await;
    ready(p).await;
    db.wait_ack().await;
    main.stop().await;
    matrix
        .push(json!({"case":"network-refused","original":original,"recovered_ack":db.ack().await}));
    db.finish().await;
    save(
        "main-lifecycle-matrix.json",
        &json!({"real_main":true,"real_ctrl_c":true,"fixture_only":true,"matrix":matrix}),
    );
    // Scan only production stdout/stderr: fixture evidence intentionally has public payload.
    for entry in std::fs::read_dir(root()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with("main-") && name.ends_with(".log") {
            let log = std::fs::read_to_string(&path).unwrap();
            for secret in [
                "isolated-worker-token",
                "isolated-worker-receiver",
                "isolated_audit_worker_owner",
                &db_url_marker(),
                &receiver.endpoint,
            ] {
                assert!(!log.contains(secret), "production log leak");
            }
        }
    }
}
fn db_url_marker() -> String {
    std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated PG16; graceful library shutdown and HTTP serve cancellation"]
async fn public_audit_worker_library_shutdown() {
    let db = Db::new().await;
    let _original = db.seed().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    // Default-off assembly also exits normally and the repository is closable.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(serve_with_public_audit(
        listener,
        axum::Router::new(),
        PublicAuditOutbox::for_trusted_backend(db.repo.clone()),
        None,
        async {
            let _ = rx.await;
        },
    ));
    tx.send(()).unwrap();
    serving.await.unwrap().unwrap();
    assert!(db.ack().await["receiver"].is_null());
    // This integration binary runs serially. Restore every process env value;
    // production configuration stays on the same from_env path as main.
    let receiver = Receiver::new().await;
    receiver.mode(Mode::Hold);
    let config = || {
        let values = env_values(&receiver.endpoint);
        let previous: Vec<_> = values
            .iter()
            .map(|(key, _)| (*key, std::env::var_os(key)))
            .collect();
        for (key, value) in &values {
            std::env::set_var(key, value);
        }
        let config = AuditWorkerConfig::from_env();
        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }
        config.unwrap()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(serve_with_public_audit(
        listener,
        axum::Router::new(),
        PublicAuditOutbox::for_trusted_backend(db.repo.clone()),
        config(),
        async {
            let _ = rx.await;
        },
    ));
    receiver.wait_count(1).await;
    tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), serving)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(db.ack().await["receiver"].is_null());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let serving = tokio::spawn(serve_with_public_audit(
        listener,
        axum::Router::new(),
        PublicAuditOutbox::for_trusted_backend(db.repo.clone()),
        config(),
        std::future::pending(),
    ));
    receiver.wait_count(2).await;
    serving.abort();
    assert!(serving.await.unwrap_err().is_cancelled());
    let count = receiver.count();
    tokio::time::sleep(Duration::from_millis(650)).await;
    assert_eq!(receiver.count(), count);
    assert!(db.ack().await["receiver"].is_null());
    save(
        "library-lifecycle.json",
        &json!({"default_off_graceful":true,"enabled_inflight_graceful":true,"cancelled_http_future_aborts_worker":true,"local_ack":db.ack().await}),
    );
    db.finish().await;
}
