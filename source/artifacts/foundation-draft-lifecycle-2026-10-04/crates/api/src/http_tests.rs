use super::*;
use axum::body::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use safeflow_runtime::AuditEventKind;
use serde_json::{json, Value};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};

const SECRET: &str = "isolated-http-test-secret-123456";
#[path = "http_account_source_tests.rs"]
mod account_source_tests;
#[path = "http_agent_tests.rs"]
mod agent_tests;
#[path = "http_bank_commandcode_tests.rs"]
mod bank_commandcode_tests;
#[path = "http_site_repair_performance_tests.rs"]
mod site_repair_performance_tests;
#[path = "http_card_read_tests.rs"]
mod card_read_tests;
#[path = "http_card_final_tests.rs"]
mod card_final_tests;
#[path = "http_integration_batch_tests.rs"]
mod integration_batch;
#[path = "http_lifecycle_tests.rs"]
mod lifecycle_tests;
#[path = "http_llm_tests.rs"]
mod llm_tests;
#[path = "http_subscription_tests.rs"]
mod subscription_tests;
#[path = "http_transfer_sql_tests.rs"]
mod transfer_sql_tests;
#[test]
fn startup_configuration_rejects_remote_origin_address_and_short_secret() {
    let valid = || ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    valid().validate().unwrap();
    let mut config = valid();
    config.bind = "0.0.0.0:8787".parse().unwrap();
    assert!(config.validate().is_err());
    for origin in [
        "http://attacker.example:5174",
        "http://127.0.0.1:5174/path",
        "http://127.0.0.1:5174/",
        "http://127.0.0.1:5174?secret=redacted",
    ] {
        let mut config = valid();
        config.frontend_origin = origin.into();
        assert!(config.validate().is_err());
    }
    let config = ApiConfig::local_simulation(Zeroizing::new("short-secret".into()));
    let error = config.validate().unwrap_err();
    assert!(!error.to_string().contains("short-secret"));
    assert!(!format!("{error:?}").contains("short-secret"));
}
#[test]
fn strict_request_schema_rejects_trusted_fields_unknown_intents_and_duplicates() {
    for raw in [
        r#"{"kind":"NEW_READ_ONLY_DEMO","owner":"user_002"}"#,
        r#"{"kind":"NEW_READ_ONLY_DEMO","user_id":"user_002"}"#,
        r#"{"kind":"NEW_READ_ONLY_DEMO","arguments":{}}"#,
        r#"{"kind":"NEW_READ_ONLY_DEMO","message":"transfer money"}"#,
        r#"{"kind":"TRANSFER"}"#,
        r#"{"kind":"NEW_READ_ONLY_DEMO","kind":"CONTINUE"}"#,
        r#"{"kind":"CONTINUE","task_id":"task","waiting_node_id":"choose","task_revision":1.5,"plan_version":2,"facts_version":7}"#,
        r#"{"kind":"CONTINUE","task_id":"task","waiting_node_id":"choose","task_revision":1,"plan_version":2,"facts_version":7,"completed_nodes":[]}"#,
    ] {
        assert!(
            serde_json::from_str::<ChatRequest>(raw).is_err(),
            "schema accepted unsupported fields"
        );
    }
    for raw in [
        r#"{"login_secret":"x","profile":"admin"}"#,
        r#"{"login_secret":"x","owner":"user_002"}"#,
        r#"{"login_secret":"a","login_secret":"b"}"#,
    ] {
        assert!(serde_json::from_str::<Login>(raw).is_err());
    }
}
fn progress() -> TaskProgressView {
    TaskProgressView {
        contract_version: 1,
        task_id: TaskId::parse("task_fixture").unwrap(),
        task_state: TaskState::WaitingUser,
        revision: 2,
        plan_version: 2,
        facts_version: 7,
        operations: vec![],
        recovery: TaskRecoveryHint::AwaitUser {
            node_id: NodeId::parse("choose").unwrap(),
        },
        security: TaskQuerySecurityScope::CurrentDecisionNotProvided,
    }
}
#[test]
fn continuation_hint_requires_verified_source_and_public_response_is_restricted() {
    let unavailable = serde_json::to_value(TaskResponse::project(progress(), None, false)).unwrap();
    assert_eq!(
        unavailable["next_action"],
        json!({"kind":"REVIEW_ORIGINAL_TASK"})
    );
    let response = serde_json::to_value(TaskResponse::project(
        progress(),
        Some(RuntimeTaskExecutionStatus::WaitingUser),
        false,
    ))
    .unwrap();
    assert_eq!(
        response["next_action"],
        json!({"kind":"CONTINUE","node_id":"choose"})
    );
    assert!(response["security_outcome"].is_null() && response["operation_state"].is_null());
    assert_eq!(response.as_object().unwrap().len(), 14);
    for forbidden in [
        "owner",
        "user_id",
        "plan",
        "arguments",
        "session",
        "permit",
        "audit",
        "result_references",
        "completed_nodes",
    ] {
        assert!(response.get(forbidden).is_none());
    }
}
#[test]
fn unsafe_or_zero_public_versions_are_rejected_without_rounding() {
    for revision in [0, MAX_SAFE_INTEGER + 1, u64::MAX] {
        let mut view = progress();
        view.revision = revision;
        assert!(checked_view_versions(&view).is_err());
    }
    for facts in [0, u32::MAX as u64 + 1] {
        let mut view = progress();
        view.facts_version = facts;
        assert!(checked_view_versions(&view).is_err());
    }
    let mut view = progress();
    view.revision = MAX_SAFE_INTEGER;
    checked_view_versions(&view).unwrap_or_else(|_| panic!("safe integer rejected"));
}

struct Db {
    namespace: String,
    url: String,
    admin: PgPool,
    scoped: PgPool,
    repo: PgRepository,
}
impl Db {
    async fn current44() -> Self {
        let db=Self::new().await;
        current_maintenance_fixture(&db.scoped,&db.namespace).await;
        db
    }
    async fn new() -> Self {
        let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL")
            .expect("explicit local PostgreSQL configuration required");
        let admin = PgPoolOptions::new()
            .max_connections(3)
            .connect(&url)
            .await
            .unwrap();
        let namespace = format!("sf_http_test_{}", uuid::Uuid::new_v4().simple());
        let repo = PgRepository::connect(&url, &namespace).await.unwrap();
        repo.migrate().await.unwrap();
        let path = namespace.clone();
        let scoped = PgPoolOptions::new()
            .max_connections(3)
            .after_connect(move |connection, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT set_config('search_path',$1,false)")
                        .bind(path)
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        Self {
            namespace,
            url,
            admin,
            scoped,
            repo,
        }
    }
    async fn finish(self) {
        self.repo.close().await;
        self.scoped.close().await;
        assert!(self.namespace.starts_with("sf_http_test_") && self.namespace.len() == 45);
        self.admin
            .execute(format!("DROP SCHEMA {} CASCADE", self.namespace).as_str())
            .await
            .unwrap();
        self.admin.close().await;
    }
}
struct Server {
    state: ApiState,
    address: SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new(db: &Db, owner: &str, timeout: Duration, ttl: Duration) -> Self {
        Self::with_draft(db, owner, timeout, ttl, None).await
    }
    async fn with_draft(
        db: &Db,
        owner: &str,
        timeout: Duration,
        ttl: Duration,
        draft: Option<DraftConfirmationConfig>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
        config.bind = address;
        config.request_timeout = timeout;
        config.session_ttl = ttl;
        config.draft_confirmation = draft;
        let state =
            ApiState::build(db.repo.clone(), config, UserId::parse(owner).unwrap()).unwrap();
        let app = router(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            address,
            handle,
        }
    }
    fn calls(&self) -> usize {
        self.state
            .0
            .audit
            .events()
            .unwrap()
            .iter()
            .filter(|event| event.kind == AuditEventKind::ToolStarted)
            .count()
    }
    async fn call(
        &self,
        method: Method,
        path: &str,
        raw: Option<String>,
        cookie: Option<&str>,
        host: Option<&str>,
        origin: Option<&str>,
        command_header: bool,
    ) -> Reply {
        Self::call_at(
            self.address,
            method,
            path,
            raw,
            cookie,
            host,
            origin,
            command_header,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn call_at(
        address: SocketAddr,
        method: Method,
        path: &str,
        raw: Option<String>,
        cookie: Option<&str>,
        host: Option<&str>,
        origin: Option<&str>,
        command_header: bool,
    ) -> Reply {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        let transport = tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host.unwrap_or(&address.to_string()));
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(origin) = origin {
            let browser_origin = std::env::var("SAFEFLOW_FEEDBACK_BROWSER_ORIGIN").ok();
            request = request.header(
                header::ORIGIN,
                if origin == "http://127.0.0.1:5174" {
                    browser_origin.as_deref().unwrap_or(origin)
                } else {
                    origin
                },
            );
        }
        if command_header {
            request = request.header("x-safeflow-request", "1");
        }
        if raw.is_some() {
            request = request.header(header::CONTENT_TYPE, "application/json");
        }
        let response = sender
            .send_request(
                request
                    .body(Full::new(Bytes::from(raw.unwrap_or_default())))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        drop(sender);
        transport.abort();
        Reply {
            status,
            headers,
            json: if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).expect("every API response must be JSON")
            },
        }
    }
    async fn login(&self) -> String {
        let reply = self
            .call(
                Method::POST,
                "/api/sim/session",
                Some(json!({"login_secret":SECRET}).to_string()),
                None,
                None,
                Some("http://127.0.0.1:5174"),
                true,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        let cookie = Cookie::parse(reply.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
        assert_eq!(cookie.http_only(), Some(true));
        assert_eq!(cookie.same_site(), Some(cookie::SameSite::Strict));
        format!("{}={}", cookie.name(), cookie.value())
    }
    async fn new_task(&self, cookie: &str) -> Value {
        let reply = self
            .call(
                Method::POST,
                "/api/chat",
                Some(json!({"kind":"NEW_READ_ONLY_DEMO"}).to_string()),
                Some(cookie),
                None,
                None,
                true,
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
        reply.json
    }
    async fn continue_task(&self, cookie: &str, view: &Value) -> Reply {
        self.call(Method::POST,"/api/chat",Some(json!({"kind":"CONTINUE","task_id":view["task_id"],"waiting_node_id":view["next_action"]["node_id"],
            "task_revision":view["revision"],"plan_version":view["plan_version"],"facts_version":view["facts_version"]}).to_string()),Some(cookie),None,None,true).await
    }
    fn stop(self) {
        self.handle.abort();
    }
}
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    json: Value,
}

#[path = "http_draft_tests.rs"]
mod draft;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_mock_flow_multiple_waits_replay_and_new_process_login_recovery() {
    let db = Db::new().await;
    let server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let health = server
        .call(Method::GET, "/api/health", None, None, None, None, false)
        .await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json["ready"], true);
    assert_eq!(health.json["funds_execution_enabled"], false);
    let cookie = server.login().await;
    let first = server.new_task(&cookie).await;
    assert_eq!(server.calls(), 1);
    let path = format!("/api/tasks/{}", first["task_id"].as_str().unwrap());
    let query = server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await;
    assert_eq!(query.json, first);
    assert_eq!(server.calls(), 1);
    let second = server.continue_task(&cookie, &first).await;
    assert_eq!(second.status, StatusCode::OK);
    assert_eq!(second.json["revision"], 4);
    assert_eq!(server.calls(), 2);
    let final_reply = server.continue_task(&cookie, &second.json).await;
    assert_eq!(final_reply.json["task_state"], "COMPLETED");
    assert_eq!(final_reply.json["next_action"], json!({"kind":"FINISHED"}));
    assert_eq!(server.calls(), 3);
    let replay = server.continue_task(&cookie, &first).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json["replayed"], true);
    assert_eq!(replay.json["revision"], 6);
    assert_eq!(replay.json["task_state"], "COMPLETED");
    assert_eq!(server.calls(), 3);
    let other = Server::new(
        &db,
        "user_002",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let other_cookie = other.login().await;
    let denied = other
        .call(
            Method::GET,
            &path,
            None,
            Some(&other_cookie),
            None,
            None,
            false,
        )
        .await;
    let unknown = other
        .call(
            Method::GET,
            "/api/tasks/nonexistent",
            None,
            Some(&other_cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(denied.status, StatusCode::NOT_FOUND);
    assert_eq!(
        denied.json["error"]["reason_code"],
        unknown.json["error"]["reason_code"]
    );
    assert_eq!(other.calls(), 0);
    other.stop();
    server.stop();
    let restarted_repo = PgRepository::connect_existing(&db.url, &db.namespace)
        .await
        .unwrap();
    let restarted_db = Db {
        namespace: db.namespace.clone(),
        url: db.url.clone(),
        admin: db.admin.clone(),
        scoped: db.scoped.clone(),
        repo: restarted_repo.clone(),
    };
    let restarted = Server::new(
        &restarted_db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    assert_eq!(
        restarted
            .call(Method::GET, &path, None, Some(&cookie), None, None, false)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = restarted.login().await;
    let recovered = restarted
        .call(Method::GET, &path, None, Some(&fresh), None, None, false)
        .await;
    assert_eq!(recovered.json["revision"], 6);
    assert_eq!(restarted.calls(), 0);
    restarted.stop();
    restarted_repo.close().await;
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_host_origin_csrf_body_schema_versions_and_disabled_routes_fail_closed() {
    let db = Db::new().await;
    let server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let login = json!({"login_secret":SECRET}).to_string();
    for (host, origin, header) in [
        (Some("attacker.example"), None, true),
        (None, Some("http://attacker.example"), true),
        (None, None, false),
    ] {
        assert_eq!(
            server
                .call(
                    Method::POST,
                    "/api/sim/session",
                    Some(login.clone()),
                    None,
                    host,
                    origin,
                    header
                )
                .await
                .status,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/sim/session",
                Some(json!({"login_secret":"wrong"}).to_string()),
                None,
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let cookie = server.login().await;
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/chat",
                None,
                Some(&cookie),
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    for raw in [
        json!({"kind":"TRANSFER"}).to_string(),
        json!({"kind":"NEW_READ_ONLY_DEMO","user_id":"user_002"}).to_string(),
        "{".into(),
        " ".repeat(MAX_BODY + 1),
    ] {
        let reply = server
            .call(
                Method::POST,
                "/api/chat",
                Some(raw),
                Some(&cookie),
                None,
                None,
                true,
            )
            .await;
        assert!(
            reply.status == StatusCode::BAD_REQUEST
                || reply.status == StatusCode::PAYLOAD_TOO_LARGE
        );
    }
    assert_eq!(server.calls(), 0);
    for raw in [
        json!({"kind":"CONTINUE","task_id":"task_unknown","waiting_node_id":"choose","task_revision":0,"plan_version":2,"facts_version":7}),
        json!({"kind":"CONTINUE","task_id":"task_unknown","waiting_node_id":"choose","task_revision":MAX_SAFE_INTEGER+1,"plan_version":2,"facts_version":7}),
        json!({"kind":"CONTINUE","task_id":"task_unknown","waiting_node_id":"choose","task_revision":2,"plan_version":2,"facts_version":4294967296u64}),
    ] {
        assert_eq!(
            server
                .call(
                    Method::POST,
                    "/api/chat",
                    Some(raw.to_string()),
                    Some(&cookie),
                    None,
                    None,
                    true
                )
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    for (method, path) in [
        (Method::POST, "/api/operations/operation_x/confirm"),
        (Method::POST, "/api/auth/challenges/challenge_x/verify"),
        (Method::GET, "/api/audit/operations/operation_x"),
        (Method::GET, "/api/audit/audit_x"),
    ] {
        let raw = (method == Method::POST).then(|| "{}".into());
        let reply = server
            .call(method, path, raw, Some(&cookie), None, None, true)
            .await;
        assert_eq!(reply.status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(reply.json["error"]["reason_code"], "DEPENDENCY_UNAVAILABLE");
    }
    assert_eq!(
        server
            .call(
                Method::GET,
                "/api/chat",
                None,
                Some(&cookie),
                None,
                None,
                false
            )
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(server.calls(), 0);
    let first = server.new_task(&cookie).await;
    let mut wrong = first.clone();
    wrong["next_action"]["node_id"] = json!("read_two");
    assert_eq!(
        server.continue_task(&cookie, &wrong).await.status,
        StatusCode::CONFLICT
    );
    wrong = first.clone();
    wrong["facts_version"] = json!(8);
    assert_eq!(
        server.continue_task(&cookie, &wrong).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(server.calls(), 1);
    let rotation = server
        .call(
            Method::POST,
            "/api/sim/session",
            Some(json!({"login_secret":SECRET}).to_string()),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(rotation.status, StatusCode::OK);
    let rotated_cookie =
        Cookie::parse(rotation.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
    let rotated = format!("{}={}", rotated_cookie.name(), rotated_cookie.value());
    assert_eq!(
        server
            .call(
                Method::GET,
                "/api/sim/session",
                None,
                Some(&cookie),
                None,
                None,
                false
            )
            .await
            .json["authenticated"],
        false
    );
    let logout = server
        .call(
            Method::DELETE,
            "/api/sim/session",
            Some("{}".into()),
            Some(&rotated),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    assert_eq!(
        server
            .call(
                Method::GET,
                "/api/sim/session",
                None,
                Some(&rotated),
                None,
                None,
                false
            )
            .await
            .json["authenticated"],
        false
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_logout_during_database_lock_refuses_claim_and_timeout_keeps_original_id() {
    let db = Db::new().await;
    let server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let cookie = server.login().await;
    let first = server.new_task(&cookie).await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(first["task_id"].as_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    let (reply,())=tokio::time::timeout(Duration::from_secs(10),async {
        tokio::join!(server.continue_task(&cookie,&first),async {
            loop { let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE%'").fetch_one(&db.admin).await.unwrap();
                if waiting>0 {break;} tokio::time::sleep(Duration::from_millis(5)).await; }
            assert_eq!(server.call(Method::DELETE,"/api/sim/session",Some("{}".into()),Some(&cookie),None,None,true).await.status,StatusCode::NO_CONTENT);
            tx.commit().await.unwrap();
        })
    }).await.unwrap();
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert_eq!(reply.json["error"]["task_id"], first["task_id"]);
    assert_eq!(server.calls(), 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM runtime_read_continuations")
            .fetch_one(&db.scoped)
            .await
            .unwrap(),
        0
    );
    server.stop();
    let slow = Server::new(
        &db,
        "user_001",
        Duration::from_millis(40),
        Duration::from_secs(30),
    )
    .await;
    let fresh = slow.login().await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(first["task_id"].as_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    let timed = slow.continue_task(&fresh, &first).await;
    assert_eq!(timed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(timed.json["error"]["task_id"], first["task_id"]);
    assert_eq!(timed.json["error"]["next_action"], "QUERY_ORIGINAL_TASK");
    tx.commit().await.unwrap();
    assert_eq!(slow.calls(), 0);
    slow.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_settlement_audit_failure_and_unsafe_stored_revision_do_not_expose_continue_or_retry(
) {
    let db = Db::new().await;
    let server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await;
    let cookie = server.login().await;
    let first = server.new_task(&cookie).await;
    db.scoped.execute("CREATE FUNCTION reject_http_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'runtime_read_continuation_execution' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER reject_http_settlement BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_http_settlement()").await.unwrap();
    let failed = server.continue_task(&cookie, &first).await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.json["error"]["task_id"], first["task_id"]);
    let path = format!("/api/tasks/{}", first["task_id"].as_str().unwrap());
    let pending = server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await;
    assert_eq!(pending.json["task_state"], "RUNNING");
    assert_eq!(
        pending.json["next_action"],
        json!({"kind":"REVIEW_ORIGINAL_TASK"})
    );
    assert_eq!(
        server.continue_task(&cookie, &first).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(server.calls(), 2);
    sqlx::query("UPDATE tasks SET revision=$1 WHERE task_id=$2")
        .bind((MAX_SAFE_INTEGER + 1) as i64)
        .bind(first["task_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
    let unsafe_view = server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await;
    assert_eq!(unsafe_view.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(unsafe_view.json.get("revision").is_none());
    assert_eq!(server.calls(), 2);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_legacy_wait_expired_session_invalid_path_and_database_health_fail_closed() {
    let db = Db::new().await;
    let server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(10),
        Duration::from_secs(2),
    )
    .await;
    let cookie = server.login().await;
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_str(&cookie).unwrap());
    let identity = server
        .state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("test session unavailable"));
    let id = TaskId::parse("task_legacy_http_wait").unwrap();
    RuntimeRecoveryService::for_trusted_backend(db.repo.clone(), server.state.0.runtime.runtime())
        .execute_until_wait(identity.reference, assembly::proposal(&id).plan.unwrap(), 7)
        .await
        .unwrap();
    assert_eq!(server.calls(), 1);
    let path = format!("/api/tasks/{id}");
    let legacy = server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await;
    assert_eq!(legacy.status, StatusCode::OK);
    assert_eq!(legacy.json["task_state"], "WAITING_USER");
    assert_eq!(
        legacy.json["next_action"],
        json!({"kind":"REVIEW_ORIGINAL_TASK"})
    );
    let invalid = server
        .call(
            Method::GET,
            "/api/tasks/%FF",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid.json["error"]["reason_code"], "VALIDATION_ERROR");
    assert_eq!(server.calls(), 1);
    tokio::time::sleep(Duration::from_millis(2100)).await;
    let expired = server
        .call(
            Method::GET,
            "/api/sim/session",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(expired.json["authenticated"], false);
    let denied = server
        .call(Method::GET, &path, None, Some(&cookie), None, None, false)
        .await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    assert_eq!(server.calls(), 1);
    db.repo.close().await;
    let unavailable = server
        .call(Method::GET, "/api/health", None, None, None, None, false)
        .await;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavailable.json["ready"], false);
    server.stop();
    db.finish().await;
}

#[path = "http_bank_conversation_tests.rs"]
mod bank_conversation_tests;
#[path = "http_bank_independent_read_tests.rs"]
mod bank_independent_read_tests;

// Test-only current maintenance schema, using the REAL reviewed 43/44 SQL.
// No zero-return stub, fabricated continuation or background permission. This
// generic isolated fixture has no deployed cohort; native deployment/CAS is
// exercised separately by the owned 42 -> 43 -> 44 -> 45 integration harness.
async fn current_maintenance_fixture(pool:&sqlx::PgPool,schema:&str) {
    assert!(schema.starts_with("sf_") && schema.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'));
    let mut tx=pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('search_path',$1,true)").bind(schema).execute(&mut *tx).await.unwrap();
    for (version,name,raw) in [
        (43_i64,"noon new authority",include_str!("../../storage/maintenance/0043_noon_new_authority.sql")),
        (44_i64,"simulation balance and draft intent",include_str!("../../storage/maintenance/0044_simulation_balance.sql")),
    ] {
        let migration=sqlx::migrate::Migration::new(version,name.into(),sqlx::migrate::MigrationType::Simple,raw.replace("__SCHEMA__",schema).replace("\r\n","\n").into(),false);
        sqlx::raw_sql(&migration.sql).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES($1,$2,true,$3,0)")
            .bind(version).bind(name).bind(migration.checksum.as_ref()).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
}
