use super::*;
use std::process::{Child, Command, Stdio};

const CHILD_TEST: &str =
    "http::tests::lifecycle_tests::process_tests::lifecycle_process_server_child";

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires PostgreSQL, real process HTTP and PowerShell demonstration"]
async fn real_http_lifecycle_powershell_example_uses_actual_three_actions() {
    let db = Db::new().await;
    let process = ProcessServer::start(&db).await;
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/lifecycle-http.ps1");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-File"])
        .arg(example)
        .args([
            "-Url",
            &format!("http://{}", process.address),
            "-ConfirmDemonstrationActions",
        ])
        .env("SAFEFLOW_SIM_LOGIN_SECRET", SECRET)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "actual HTTP demonstration failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 3);
    let cancelled: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks WHERE state='CANCELLED'")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(cancelled, 1);
    process.kill();
    db.finish().await;
}

pub(super) struct ProcessServer {
    child: Child,
    pub(super) address: SocketAddr,
}
impl ProcessServer {
    pub(super) async fn start(db: &Db) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
            .env("SAFEFLOW_LIFECYCLE_CHILD_SCHEMA", &db.namespace)
            .env("SAFEFLOW_LIFECYCLE_CHILD_ADDR", address.to_string())
            .env("SAFEFLOW_RESTART_AGENT_ENABLED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self { child, address };
        for _ in 0..200 {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "child exited before readiness"
            );
            if tokio::net::TcpStream::connect(address).await.is_ok() {
                return server;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("real child server did not become ready");
    }
    pub(super) fn kill(mut self) {
        let pid = self.child.id();
        self.child.kill().unwrap();
        let status = self.child.wait().unwrap();
        assert!(!status.success());
        eprintln!("terminated real lifecycle server process {pid}");
    }
    pub(super) async fn call(
        &self,
        method: Method,
        path: &str,
        input: Option<Value>,
        cookie: Option<&str>,
    ) -> Reply {
        let post = method == Method::POST;
        Server::call_at(
            self.address,
            method,
            path,
            input.map(|v| v.to_string()),
            cookie,
            None,
            None,
            post,
        )
        .await
    }
    pub(super) async fn login(&self) -> String {
        let reply = self
            .call(
                Method::POST,
                "/api/sim/session",
                Some(json!({"login_secret":SECRET})),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        let cookie = Cookie::parse(reply.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
        format!("{}={}", cookie.name(), cookie.value())
    }
    pub(super) async fn confirmed(&self, cookie: &str) -> Value {
        let first = ok(self
            .call(
                Method::POST,
                "/api/sim/draft-lifecycles",
                Some(scope()),
                Some(cookie),
            )
            .await);
        let prepared = ok(self
            .call(
                Method::POST,
                &format!("{}/prepare", operation_path(&first)),
                Some(scope()),
                Some(cookie),
            )
            .await);
        ok(self
            .call(
                Method::POST,
                &format!("{}/confirm", operation_path(&first)),
                Some(confirmation(&prepared, "CONFIRM")),
                Some(cookie),
            )
            .await)
    }
}
impl Drop for ProcessServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and killing an active settlement process"]
async fn real_http_lifecycle_kill_during_settlement_preserves_committed_claim() {
    let db = Db::new().await;
    let process = ProcessServer::start(&db).await;
    let cookie = process.login().await;
    let confirmed = process.confirmed(&cookie).await;
    let key = 100_000
        + u32::from_le_bytes(*uuid::Uuid::new_v4().as_bytes().first_chunk::<4>().unwrap()) as u64;
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key as i64)
        .execute(&mut *held)
        .await
        .unwrap();
    db.scoped.execute(format!("CREATE FUNCTION wait_lifecycle_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='SUCCEEDED' THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$; CREATE TRIGGER wait_lifecycle_settlement BEFORE UPDATE ON mock_draft_lifecycle_claims FOR EACH ROW EXECUTE FUNCTION wait_lifecycle_settlement();").as_str()).await.unwrap();
    let address = process.address;
    let request = execution(&confirmed).to_string();
    let path = format!("{}/execute", operation_path(&confirmed));
    let request_path = path.clone();
    let request_cookie = cookie.clone();
    let request = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &request_path,
            Some(request),
            Some(&request_cookie),
            None,
            None,
            true,
        )
        .await
    });
    let mut waiting = false;
    for _ in 0..200 {
        waiting=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid::bigint=$1 AND objid::bigint=$2 AND NOT granted)")
            .bind((key>>32) as i64).bind((key&0xffff_ffff) as i64).fetch_one(&db.admin).await.unwrap();
        if waiting {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        waiting,
        "actual Tool completed and settlement reached blocking trigger"
    );
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    request.abort();
    process.kill();
    held.rollback().await.unwrap();
    let process = ProcessServer::start(&db).await;
    let fresh = process.login().await;
    let original = ok(process
        .call(Method::GET, &operation_path(&confirmed), None, Some(&fresh))
        .await);
    assert_eq!(original["execution_status"], "RESULT_UNKNOWN");
    assert!(original["receipt"].is_null());
    assert_eq!(original["operation_state"], "DRAFT");
    assert_eq!(
        process
            .call(
                Method::GET,
                &format!("{}/current-draft", operation_path(&confirmed)),
                None,
                Some(&fresh)
            )
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        ok(process
            .call(
                Method::POST,
                &path,
                Some(execution(&confirmed)),
                Some(&fresh)
            )
            .await)["execution_status"],
        "RESULT_UNKNOWN"
    );
    process.kill();
    db.finish().await;
}

#[tokio::test]
#[ignore = "child process entry; parent owns isolated schema and process termination"]
async fn lifecycle_process_server_child() {
    let Ok(namespace) = std::env::var("SAFEFLOW_LIFECYCLE_CHILD_SCHEMA") else {
        return;
    };
    assert!(namespace.starts_with("sf_http_test_"));
    let address = std::env::var("SAFEFLOW_LIFECYCLE_CHILD_ADDR").unwrap();
    let repo = PgRepository::connect_existing(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &namespace,
    )
    .await
    .unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address.parse().unwrap();
    config.request_timeout = Duration::from_secs(60);
    config.agent_fixed_plan_enabled =
        std::env::var("SAFEFLOW_RESTART_AGENT_ENABLED").as_deref() == Ok("1");
    config.session_ttl = Duration::from_secs(600);
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "isolated-http-lifecycle-v2",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    let state = ApiState::build(repo, config, UserId::parse("user_001").unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    axum::serve(listener, router(state)).await.unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual child process kill/restart"]
async fn real_http_lifecycle_real_process_restart_history_and_unknown_are_permanent() {
    let db = Db::new().await;
    let process = ProcessServer::start(&db).await;
    let cookie = process.login().await;
    let success = process.confirmed(&cookie).await;
    let saved = ok(process
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(&success)),
            Some(execution(&success)),
            Some(&cookie),
        )
        .await);
    assert_eq!(saved["execution_status"], "SUCCEEDED");
    let unknown = process.confirmed(&cookie).await;
    db.scoped.execute("CREATE FUNCTION reject_process_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='SUCCEEDED' THEN RAISE EXCEPTION 'isolated process settlement failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_process_settlement BEFORE UPDATE ON mock_draft_lifecycle_claims FOR EACH ROW EXECUTE FUNCTION reject_process_settlement();").await.unwrap();
    assert_eq!(
        process
            .call(
                Method::POST,
                &format!("{}/execute", operation_path(&unknown)),
                Some(execution(&unknown)),
                Some(&cookie)
            )
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let actual = ok(process
        .call(
            Method::GET,
            &format!("{}/current-draft", operation_path(&unknown)),
            None,
            Some(&cookie),
        )
        .await);
    assert_eq!(
        actual["draft"]["draft_version"], 1,
        "real business save preceded failed settlement"
    );
    let before = count(&db, "audit_events").await;
    process.kill();
    let process = ProcessServer::start(&db).await;
    let fresh = process.login().await;
    let history = ok(process
        .call(Method::GET, &operation_path(&success), None, Some(&fresh))
        .await);
    assert_eq!(history["receipt"], saved["receipt"]);
    assert_eq!(history["task_state"], "WAITING_USER");
    assert_eq!(
        process
            .call(
                Method::GET,
                &format!("{}/current-draft", operation_path(&success)),
                None,
                Some(&fresh)
            )
            .await
            .status,
        StatusCode::OK
    );
    let unresolved = ok(process
        .call(Method::GET, &operation_path(&unknown), None, Some(&fresh))
        .await);
    assert_eq!(unresolved["execution_status"], "RESULT_UNKNOWN");
    assert_eq!(unresolved["unknown_operation_id"], unknown["operation_id"]);
    assert_eq!(
        process
            .call(
                Method::GET,
                &format!("{}/current-draft", operation_path(&unknown)),
                None,
                Some(&fresh)
            )
            .await
            .status,
        StatusCode::CONFLICT
    );
    let replay = ok(process
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(&unknown)),
            Some(execution(&unknown)),
            Some(&fresh),
        )
        .await);
    assert_eq!(replay["execution_status"], "RESULT_UNKNOWN");
    for action in [
        "CREATE_MOCK_DRAFT",
        "MODIFY_MOCK_DRAFT",
        "CANCEL_MOCK_DRAFT",
    ] {
        assert_ne!(
            process
                .call(
                    Method::POST,
                    &format!("{}/actions", task_path(&unknown)),
                    Some(selection(
                        &unresolved,
                        action,
                        unknown["parameters"].clone()
                    )),
                    Some(&fresh)
                )
                .await
                .status,
            StatusCode::OK
        );
    }
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 2);
    assert_eq!(count(&db, "audit_events").await, before);
    process.kill();
    db.finish().await;
}
