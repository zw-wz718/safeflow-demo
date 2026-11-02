//! Actual loopback provider-protocol fixtures, never substitutes for business Tools.
use super::*;
use axum::{extract::State, routing::post};
use safeflow_runtime::OpenAiGatewayConfig;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy)]
enum Proposal {
    Pass,
    Clarify,
    Unauthorized,
    WrongArguments,
    Cycle,
    Slow,
}
#[derive(Clone)]
struct ProviderState {
    reply: Arc<std::sync::Mutex<Proposal>>,
    calls: Arc<AtomicUsize>,
    started: Arc<tokio::sync::Notify>,
}
struct Provider {
    state: ProviderState,
    endpoint: String,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Provider {
    async fn new(reply: Proposal) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let state = ProviderState {
            reply: Arc::new(std::sync::Mutex::new(reply)),
            calls: Arc::new(AtomicUsize::new(0)),
            started: Arc::new(tokio::sync::Notify::new()),
        };
        let app = Router::new()
            .route("/v1/chat/completions", post(provider))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            endpoint,
            handle,
        }
    }
    fn config(&self) -> OpenAiGatewayConfig {
        OpenAiGatewayConfig::new(
            &self.endpoint,
            "local-transport-fixture",
            Zeroizing::new("explicit-fixture-key".into()),
            Duration::from_secs(5),
        )
        .unwrap()
    }
    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }
    fn reply(&self, reply: Proposal) {
        *self.state.reply.lock().unwrap() = reply;
    }
}
async fn provider(State(state): State<ProviderState>, Json(request): Json<Value>) -> Json<Value> {
    state.calls.fetch_add(1, Ordering::SeqCst);
    state.started.notify_one();
    let reply = *state.reply.lock().unwrap();
    if matches!(reply, Proposal::Slow) {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let system = request["messages"][0]["content"].as_str().unwrap();
    let protocol: Value =
        serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    let context = &protocol["planning_context"];
    let mut proposal = json!({"contract_version":1,"understanding":{"intent":"READ_BALANCE","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},
        "plan":{"contract_version":1,"task_id":context["task_id"],"plan_version":context["plan_version"],"nodes":[{"type":"TOOL","id":"read","tool":"get_account_balance","arguments":{"account_id":"account_001"},"depends_on":[]}]},"clarification":null});
    match reply {
        Proposal::Clarify => {
            proposal["understanding"]["decision"] = json!("CLARIFY");
            proposal["plan"] = Value::Null;
            proposal["clarification"] = json!({"question":"请指定要查询的账户"});
        }
        Proposal::Unauthorized => {
            proposal["plan"]["nodes"][0]["tool"] = json!("unregistered_write_tool")
        }
        Proposal::WrongArguments => {
            proposal["plan"]["nodes"][0]["arguments"] =
                json!({"account_id":"account_001","owner":"user_002"})
        }
        Proposal::Cycle => proposal["plan"]["nodes"][0]["depends_on"] = json!(["read"]),
        Proposal::Pass | Proposal::Slow => {}
    }
    Json(
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":proposal.to_string(),"refusal":null}}],"usage":{"test_float":0.5}}),
    )
}
impl Server {
    async fn with_llm(db: &Db, owner: &str, provider: &Provider) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
        config.bind = address;
        config.llm_gateway = Some(provider.config());
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
    async fn model_plan(&self, cookie: &str) -> Reply {
        self.call(
            Method::POST,
            "/api/llm/plans",
            Some(json!({"message":"查询account_001余额"}).to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await
    }
    async fn model_query(&self, cookie: &str, id: &str) -> Reply {
        self.call(
            Method::GET,
            &format!("/api/llm/plans/{id}"),
            None,
            Some(cookie),
            None,
            None,
            false,
        )
        .await
    }
    async fn model_execute(&self, cookie: &str, view: &Value) -> Reply {
        self.call(Method::POST,&format!("/api/llm/plans/{}/execute",view["task"]["task_id"].as_str().unwrap()),Some(json!({"task_revision":view["task"]["revision"],"plan_version":view["task"]["plan_version"],"facts_version":view["task"]["facts_version"]}).to_string()),Some(cookie),None,None,true).await
    }
}
async fn counts(db: &Db) -> (i64, i64, i64) {
    let tasks = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    let operations = sqlx::query_scalar("SELECT count(*) FROM operations")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    let claims = sqlx::query_scalar("SELECT count(*) FROM runtime_read_claims")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    (tasks, operations, claims)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; loopback protocol fixture with public read-only registration"]
async fn llm_extended_registration_start_hint_query_execution_and_legacy_chat_remain_consistent() {
    let db = Db::new().await;
    let provider = Provider::new(Proposal::Pass).await;
    for lifecycle in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
        config.bind = address;
        config.llm_gateway = Some(provider.config());
        if lifecycle {
            config.draft_confirmation = Some(
                DraftConfirmationConfig::new(
                    Duration::from_secs(300),
                    Duration::from_secs(300),
                    600,
                    "legacy-llm-registration-test",
                )
                .unwrap()
                .with_lifecycle_v2()
                .unwrap(),
            );
        }
        let bank = Arc::new(safeflow_mock_bank::MockBankGateway::new());
        // The lifecycle scope already contains find_payees; reuse the isolated delivered-adapter alias.
        let probe = crate::plugin_test_support::probe(bank.clone(), lifecycle);
        let state = ApiState::for_project_simulation_with_read_only_tools(
            db.repo.clone(),
            config,
            bank,
            vec![probe.registration],
        )
        .unwrap();
        assert_eq!(state.0.runtime.definitions().len(), 2);
        assert!(state
            .0
            .runtime
            .contracts()
            .iter()
            .all(|c| c.projection_revision == "bounded-direct-input-v2"));
        let app = router(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let server = Server {
            state,
            address,
            handle,
        };
        let cookie = server.login().await;
        let provider_before = provider.calls();
        let initial = server.model_plan(&cookie).await;
        assert_eq!(initial.status, StatusCode::OK, "{}", initial.json);
        assert_eq!(
            initial.json["can_start"], true,
            "lifecycle={lifecycle}: {}",
            initial.json
        );
        let id = initial.json["task"]["task_id"].as_str().unwrap();
        let queried = server.model_query(&cookie, id).await;
        assert_eq!(queried.status, StatusCode::OK, "{}", queried.json);
        assert_eq!(queried.json, initial.json);
        assert_eq!(server.calls(), 0);
        assert_eq!(provider.calls(), provider_before + 1);
        let new_cookie = server.login().await;
        let rotated = server.model_query(&new_cookie, id).await;
        assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.json);
        assert_eq!(rotated.json["can_start"], false);
        assert!(!server
            .model_execute(&new_cookie, &initial.json)
            .await
            .status
            .is_success());
        assert_eq!(server.calls(), 0);
        let executed = server.model_execute(&cookie, &initial.json).await;
        assert_eq!(executed.status, StatusCode::OK, "{}", executed.json);
        assert_eq!(executed.json["task"]["task_id"], id);
        assert_eq!(executed.json["task"]["task_state"], "COMPLETED");
        assert_eq!(executed.json["can_start"], false);
        assert_eq!(server.calls(), 1);
        assert_eq!(server.model_query(&cookie, id).await.json, executed.json);
        assert_eq!(
            server.model_execute(&cookie, &initial.json).await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(server.calls(), 1);

        let legacy = server
            .call(
                Method::POST,
                "/api/chat",
                Some(json!({"kind":"NEW_READ_ONLY_DEMO"}).to_string()),
                Some(&cookie),
                None,
                None,
                true,
            )
            .await;
        assert_eq!(legacy.status, StatusCode::OK, "{}", legacy.json);
        let mut legacy = legacy.json;
        assert_eq!(legacy["task_state"], "WAITING_USER");
        let legacy_id = legacy["task_id"].clone();
        for (node, expected_state) in [("choose", "WAITING_USER"), ("choose_two", "COMPLETED")] {
            let continued = server.call(Method::POST, "/api/chat", Some(json!({"kind":"CONTINUE","task_id":legacy_id,"waiting_node_id":node,
                "task_revision":legacy["revision"],"plan_version":legacy["plan_version"],"facts_version":legacy["facts_version"]}).to_string()), Some(&cookie), None, None, true).await;
            assert_eq!(continued.status, StatusCode::OK, "{}", continued.json);
            legacy = continued.json;
            assert_eq!(legacy["task_id"], legacy_id);
            assert_eq!(legacy["task_state"], expected_state);
        }
        assert_eq!(server.calls(), 4);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
        assert_eq!(provider.calls(), provider_before + 1);
        server.stop();
    }
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires explicit local PostgreSQL"]
async fn llm_plan_is_real_transport_persisted_only_and_explicit_original_execution_is_once() {
    let db = Db::new().await;
    let provider = Provider::new(Proposal::Pass).await;
    let server = Server::with_llm(&db, "user_001", &provider).await;
    let cookie = server.login().await;
    let configured = server
        .call(
            Method::GET,
            "/api/llm/status",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(configured.status, StatusCode::OK);
    assert_eq!(configured.json["configured"], true);
    assert_eq!(configured.json["connectivity"], "NOT_VERIFIED");
    assert_eq!(provider.calls(), 0);
    let initial = server.model_plan(&cookie).await;
    assert_eq!(initial.status, StatusCode::OK);
    assert_eq!(initial.json.as_object().unwrap().len(), 7);
    assert_eq!(initial.json["planner_source"], "OPENAI_COMPATIBLE");
    assert_eq!(initial.json["kind"], "TASK");
    assert_eq!(initial.json["can_start"], true);
    assert_eq!(initial.json["task"]["revision"], 1);
    assert_eq!(initial.json["task"]["plan_version"], 1);
    assert_eq!(initial.json["task"]["facts_version"], 1);
    assert_eq!(
        initial.json["task"]["recovery"],
        json!({"kind":"REVIEW_NODE","node_id":"read"})
    );
    assert_eq!(
        initial.json["task"]["next_action"],
        json!({"kind":"REVIEW_ORIGINAL_TASK"})
    );
    assert_eq!(counts(&db).await, (1, 0, 0));
    assert_eq!(server.calls(), 0);
    assert_eq!(provider.calls(), 1);
    let id = initial.json["task"]["task_id"].as_str().unwrap();
    let source: String = sqlx::query_scalar(
        "SELECT details->>'planning_gateway_source' FROM audit_events WHERE kind='TASK_CREATED'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(source, "OPENAI_COMPATIBLE");
    for _ in 0..3 {
        let queried = server.model_query(&cookie, id).await;
        assert_eq!(queried.status, StatusCode::OK);
        assert_eq!(queried.json, initial.json);
    }
    assert_eq!(provider.calls(), 1);
    assert_eq!(server.calls(), 0);
    assert_eq!(counts(&db).await, (1, 0, 0));
    let executed = server.model_execute(&cookie, &initial.json).await;
    assert_eq!(executed.status, StatusCode::OK);
    assert_eq!(executed.json["task"]["task_id"], id);
    assert_eq!(executed.json["task"]["task_state"], "COMPLETED");
    assert_eq!(executed.json["can_start"], false);
    assert_eq!(counts(&db).await, (1, 0, 1));
    assert_eq!(server.calls(), 1);
    assert_eq!(provider.calls(), 1);
    assert_eq!(
        server.model_execute(&cookie, &initial.json).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(server.calls(), 1);
    server.stop();
    let restarted = Server::with_llm(&db, "user_001", &provider).await;
    let new_cookie = restarted.login().await;
    let recovered = restarted.model_query(&new_cookie, id).await;
    assert_eq!(recovered.status, StatusCode::OK);
    assert_eq!(recovered.json["task"], executed.json["task"]);
    assert_eq!(recovered.json["can_start"], false);
    assert_eq!(restarted.calls(), 0);
    assert_eq!(provider.calls(), 1);
    let foreign = Server::with_llm(&db, "user_002", &provider).await;
    let foreign_cookie = foreign.login().await;
    assert_eq!(
        foreign.model_query(&foreign_cookie, id).await.status,
        StatusCode::NOT_FOUND
    );
    foreign.stop();
    restarted.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires explicit local PostgreSQL"]
async fn llm_configuration_clarification_rejections_and_client_source_claims_create_no_task() {
    let db = Db::new().await;
    let unconfigured = Server::new(
        &db,
        "user_001",
        Duration::from_secs(30),
        Duration::from_secs(1800),
    )
    .await;
    let cookie = unconfigured.login().await;
    let status = unconfigured
        .call(
            Method::GET,
            "/api/llm/status",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(status.json["configured"], false);
    assert_eq!(status.json["reason_code"], "LLM_NOT_CONFIGURED");
    let failed = unconfigured.model_plan(&cookie).await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.json["error"]["reason_code"], "LLM_NOT_CONFIGURED");
    assert!(failed.json["error"]["task_id"].is_null());
    unconfigured.stop();
    let provider = Provider::new(Proposal::Clarify).await;
    let server = Server::with_llm(&db, "user_001", &provider).await;
    let cookie = server.login().await;
    for raw in [
        r#"{"message":"query","planner_source":"OPENAI_COMPATIBLE"}"#,
        r#"{"message":"query","endpoint":"http://example.com"}"#,
        r#"{"message":"query","task_id":"chosen"}"#,
        r#"{"message":"query","message":"again"}"#,
    ] {
        let rejected = server
            .call(
                Method::POST,
                "/api/llm/plans",
                Some(raw.into()),
                Some(&cookie),
                None,
                None,
                true,
            )
            .await;
        assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(provider.calls(), 0);
    let clarified = server.model_plan(&cookie).await;
    assert_eq!(clarified.status, StatusCode::OK);
    assert_eq!(clarified.json["kind"], "CLARIFY");
    assert!(clarified.json["task"].is_null());
    assert_eq!(clarified.json["can_start"], false);
    assert_eq!(
        clarified.json["clarification"],
        json!({"question":"请指定要查询的账户"})
    );
    for reply in [
        Proposal::Unauthorized,
        Proposal::WrongArguments,
        Proposal::Cycle,
    ] {
        provider.reply(reply);
        let failed = server.model_plan(&cookie).await;
        assert_eq!(failed.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(failed.json["error"]["reason_code"], "LLM_PROPOSAL_REJECTED");
        assert!(failed.json["error"]["task_id"].is_null());
    }
    assert_eq!(provider.calls(), 4);
    assert_eq!(counts(&db).await, (0, 0, 0));
    assert_eq!(server.calls(), 0);
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires explicit local PostgreSQL"]
async fn llm_unexecuted_task_is_readable_after_login_rotation_and_restart_but_cannot_start() {
    let db = Db::new().await;
    let provider = Provider::new(Proposal::Pass).await;
    let server = Server::with_llm(&db, "user_001", &provider).await;
    let first_cookie = server.login().await;
    let first = server.model_plan(&first_cookie).await;
    let id = first.json["task"]["task_id"].as_str().unwrap();
    let rotated_reply = server
        .call(
            Method::POST,
            "/api/sim/session",
            Some(json!({"login_secret":SECRET}).to_string()),
            Some(&first_cookie),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(rotated_reply.status, StatusCode::OK);
    let rotated_cookie =
        Cookie::parse(rotated_reply.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
    let rotated = format!("{}={}", rotated_cookie.name(), rotated_cookie.value());
    let old = server.model_query(&rotated, id).await;
    assert_eq!(old.status, StatusCode::OK);
    assert_eq!(old.json["can_start"], false);
    assert_eq!(
        server.model_execute(&rotated, &first.json).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(server.calls(), 0);
    let second = server.model_plan(&rotated).await;
    let second_id = second.json["task"]["task_id"].as_str().unwrap();
    assert_eq!(second.json["can_start"], true);
    server.stop();
    let restarted = Server::with_llm(&db, "user_001", &provider).await;
    let cookie = restarted.login().await;
    let old = restarted.model_query(&cookie, second_id).await;
    assert_eq!(old.status, StatusCode::OK);
    assert_eq!(old.json["can_start"], false);
    assert_eq!(
        restarted.model_execute(&cookie, &second.json).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(provider.calls(), 2);
    assert_eq!(restarted.calls(), 0);
    assert_eq!(counts(&db).await, (2, 0, 0));
    let mock = restarted.new_task(&cookie).await;
    let mock_id = mock["task_id"].as_str().unwrap();
    let before = restarted.calls();
    assert_eq!(
        restarted.model_query(&cookie, mock_id).await.status,
        StatusCode::NOT_FOUND
    );
    let fake = json!({"task":mock});
    assert_eq!(
        restarted.model_execute(&cookie, &fake).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(restarted.calls(), before);
    assert_eq!(provider.calls(), 2);
    restarted.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires explicit local PostgreSQL"]
async fn llm_session_revocation_during_real_provider_wait_does_not_persist_or_execute() {
    let db = Db::new().await;
    let provider = Provider::new(Proposal::Slow).await;
    let server = Arc::new(Server::with_llm(&db, "user_001", &provider).await);
    let cookie = server.login().await;
    let pending_server = server.clone();
    let pending_cookie = cookie.clone();
    let pending = tokio::spawn(async move { pending_server.model_plan(&pending_cookie).await });
    provider.state.started.notified().await;
    let logout = server
        .call(
            Method::DELETE,
            "/api/sim/session",
            Some("{}".into()),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(logout.status, StatusCode::NO_CONTENT);
    let rejected = pending.await.unwrap();
    assert_eq!(rejected.status, StatusCode::UNAUTHORIZED);
    assert_eq!(counts(&db).await, (0, 0, 0));
    assert_eq!(server.calls(), 0);
    assert_eq!(provider.calls(), 1);
    Arc::try_unwrap(server).ok().unwrap().stop();
    db.finish().await;
}
