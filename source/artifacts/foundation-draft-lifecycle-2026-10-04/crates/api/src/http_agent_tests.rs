//! Wire-protocol provider responses with actual delivered Tools, Policy, Store and PG.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
#[path = "http_billing_report_tests.rs"]
mod billing_report_tests;
#[path = "http_plugin_entry_tests.rs"]
mod plugin_entry_tests;
#[path = "http_database_card_tests.rs"]
mod database_card_tests;
#[path = "http_agent_sql_bridge_tests.rs"]
mod sql_bridge_tests;

struct Provider {
    endpoint: String,
    calls: Arc<AtomicUsize>,
    definitions: Arc<std::sync::Mutex<Value>>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Provider {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let definitions = Arc::new(std::sync::Mutex::new(Value::Null));
        let app = Router::new()
            .route("/v1/chat/completions", post(provider))
            .with_state((calls.clone(), definitions.clone()));
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            endpoint,
            calls,
            definitions,
            handle,
        }
    }
}
async fn provider(
    State((calls, definitions)): State<(Arc<AtomicUsize>, Arc<std::sync::Mutex<Value>>)>,
    Json(request): Json<Value>,
) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    let system = request["messages"][0]["content"].as_str().unwrap();
    let protocol: Value =
        serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    *definitions.lock().unwrap() = protocol["approved_tool_definitions"].clone();
    assert!(!system.contains("user_001") && !system.contains("session_"));
    let context = &protocol["planning_context"];
    let data = &protocol["controlled_task_data"];
    let message = request["messages"][1]["content"].as_str().unwrap();
    let read = |id: &str, dependencies: Value| json!({"type":"TOOL","id":id,"tool":"get_account_balance","arguments":{"account_id":"account_001"},"depends_on":dependencies});
    let mut nodes = json!([read("balance", json!([]))]);
    if message == "read and continue" {
        nodes = json!([read("balance",json!([])), {"type":"USER_DECISION","id":"choice","depends_on":["balance"]}, read("second",json!(["choice"]))]);
    }
    if [
        "create",
        "invent",
        "modify",
        "modify payee",
        "cancel",
        "mixed",
    ]
    .contains(&message)
    {
        let (tool, args) = match message {
            "modify payee" => (
                "modify_mock_transfer_draft",
                json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"],"changes":{"payee_id":data["selection"]["payee_id"]}}),
            ),
            "modify" => (
                "modify_mock_transfer_draft",
                json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"],"changes":{"amount":"300.00"}}),
            ),
            "cancel" => (
                "cancel_mock_transfer_draft",
                json!({"draft_id":data["current_draft"]["draft_id"],"expected_version":data["current_draft"]["expected_version"]}),
            ),
            _ => (
                "create_mock_transfer_draft",
                json!({"payer_account_id":"account_001","payee_id":if message=="invent" {"payee_009"} else {"payee_008"},"amount":"200.00","currency":"CNY","remark":"","replaces":null}),
            ),
        };
        assert!(
            protocol["approved_tool_definitions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["name"] == tool)
                || message == "create"
        );
        nodes =
            json!([{"type":"TOOL","id":"proposal","tool":tool,"arguments":args,"depends_on":[]}]);
        if message == "mixed" {
            nodes.as_array_mut().unwrap().push(read("extra", json!([])));
        }
    }
    if message == "unauthorized" {
        nodes[0]["tool"] = json!("submit_transfer");
    }
    if [
        "payment",
        "payment renamed",
        "payment wrong draft",
        "payment stale",
        "payment forged amount",
        "payment mixed",
    ]
    .contains(&message)
    {
        let mut arguments = json!({"draft_id":data["current_draft"]["draft_id"],"draft_version":data["current_draft"]["expected_version"]});
        if message == "payment stale" {
            arguments["draft_version"] = json!(99);
        }
        if message == "payment forged amount" {
            arguments["amount"] = json!("0.01");
        }
        if message == "payment wrong draft" {
            arguments["draft_id"] = json!("another_draft");
        }
        let node_id = if message == "payment renamed" {
            "model_replanned_payment"
        } else {
            "payment"
        };
        nodes = json!([{"type":"TOOL","id":node_id,"tool":"execute_transfer","arguments":arguments,"depends_on":[]}]);
        if message == "payment mixed" {
            nodes.as_array_mut().unwrap().push(read("extra", json!([])));
        }
    }
    if message == "payees" {
        nodes[0]["tool"] = json!("find_payees");
        nodes[0]["arguments"] = json!({"query":"小李"});
    }
    if message == "plugin wait" || message == "plugin alias wait" || message == "plugin missing" {
        let tool = if message == "plugin alias wait" {
            "payees_registration_probe"
        } else {
            "find_payees"
        };
        let read = |id: &str, dependencies: Value| json!({"type":"TOOL","id":id,"tool":tool,"arguments":{"query":"小李"},"depends_on":dependencies});
        nodes = json!([read("payees",json!([])), {"type":"USER_DECISION","id":"choice","depends_on":["payees"]}, read("second",json!(["choice"]))]);
        if message == "plugin missing" {
            nodes[0]["arguments"] = json!({});
        }
    }
    if message=="database cards" {
        nodes=json!([{"type":"TOOL","id":"cards","tool":"get_account_card_facts","arguments":{"account_id":"account_001"},"depends_on":[]},
            {"type":"USER_DECISION","id":"choice","depends_on":["cards"]}]);
    }
    let mut result = json!({"contract_version":1,"understanding":{"intent":"AGENT_REQUEST","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},
        "plan":{"contract_version":1,"task_id":context["task_id"],"plan_version":context["plan_version"],"nodes":nodes},"clarification":null});
    if message == "clarify" {
        result["understanding"]["decision"] = json!("CLARIFY");
        result["plan"] = Value::Null;
        result["clarification"] = json!({"question":"请补充账户"});
    }
    Json(
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]}),
    )
}
async fn server(db: &Db, provider: Option<&Provider>, draft: bool) -> Server {
    server_mode(db, provider, draft, false).await
}
async fn server_mode(db: &Db, provider: Option<&Provider>, draft: bool, fixed: bool) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.agent_fixed_plan_enabled = fixed;
    config.bind = address;
    if let Some(provider) = provider {
        config.llm_gateway = Some(
            safeflow_runtime::OpenAiGatewayConfig::new(
                &provider.endpoint,
                "wire-protocol-only",
                Zeroizing::new("isolated-protocol-key".into()),
                Duration::from_secs(5),
            )
            .unwrap(),
        );
    }
    if draft {
        config.draft_confirmation = Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "agent-main-flow-test",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        );
    }
    let state =
        ApiState::build(db.repo.clone(), config, UserId::parse("user_001").unwrap()).unwrap();
    let app = router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server {
        state,
        address,
        handle,
    }
}

fn sample_input(sample: &str, context: Option<&Value>) -> Value {
    let mut input = message("unused", context);
    input.as_object_mut().unwrap().remove("message");
    input["sample"] = json!(sample);
    input
}

#[test]
fn fixed_planning_is_opt_in_and_cannot_be_combined_with_a_real_gateway() {
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    assert!(!config.agent_fixed_plan_enabled);
    config.agent_fixed_plan_enabled = true;
    assert!(config.validate().is_ok());
    config.llm_gateway = Some(
        safeflow_runtime::OpenAiGatewayConfig::new(
            "http://127.0.0.1:12345/v1/chat/completions",
            "configuration-only-test",
            Zeroizing::new("isolated-test-key".into()),
            Duration::from_secs(5),
        )
        .unwrap(),
    );
    assert!(config.validate().is_err());
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; no model server or key"]
async fn real_http_agent_legacy_web_response_shapes_omit_payment_fields() {
    let db = Db::new().await;
    let server = server_mode(&db, None, true, true).await;
    let cookie = server.login().await;
    let status = ok(server
        .call(
            Method::GET,
            "/api/agent/status",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await);
    assert_eq!(status["payment_planning_enabled"], true);
    assert_eq!(status["funds_execution_enabled"], false);
    let clarify = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        json!({"sample":"CREATE_200"}),
    )
    .await);
    let read = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("READ_WAIT", None),
    )
    .await);
    let waiting = ok(command(
        &server,
        &cookie,
        &format!(
            "/api/agent/tasks/{}/execute",
            read["task"]["task_id"].as_str().unwrap()
        ),
        version(&read),
    )
    .await);
    let draft = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CREATE_200", None),
    )
    .await);
    let prepared = ok(command(
        &server,
        &cookie,
        &format!(
            "/api/agent/tasks/{}/prepare",
            draft["draft"]["task_id"].as_str().unwrap()
        ),
        json!({"operation_id":draft["draft"]["operation_id"]}),
    )
    .await);
    let views = [clarify, read, waiting, draft, prepared];
    assert_eq!(
        views
            .iter()
            .map(|v| v["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["CLARIFY", "READ", "READ", "DRAFT", "DRAFT"]
    );
    for view in &views {
        let mut keys: Vec<_> = view
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "can_start",
                "clarification",
                "contract_version",
                "draft",
                "kind",
                "mode",
                "planning_source",
                "results",
                "task"
            ]
        );
    }
    // Export only public responses for the frontend contract regression, never cookies.
    if let Some(path) = std::env::var_os("SAFEFLOW_AGENT_COMPAT_RESPONSES") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&json!({"status":status,"responses":views})).unwrap(),
        )
        .unwrap();
    }
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; no model server or key"]
async fn real_http_agent_fixed_samples_no_key_same_runtime_confirmation_and_provenance() {
    let db = Db::new().await;
    let server = server_mode(&db, None, true, true).await;
    let cookie = server.login().await;
    assert!(server.state.0.llm_gateway.is_none());
    let status = ok(server
        .call(
            Method::GET,
            "/api/agent/status",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await);
    assert_eq!(status["planning_mode"], "FIXED_PLAN");
    assert_eq!(status["configured"], false);
    let denied = command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("arbitrary natural language", None),
    )
    .await;
    assert!(!denied.status.is_success());
    for patch in [
        json!({"sample":"SUBMIT_TRANSFER"}),
        json!({"sample":"READ_WAIT","plan":{}}),
        json!({"sample":"CREATE_200","confirmed":true}),
    ] {
        assert!(!command(&server, &cookie, "/api/agent/samples", patch)
            .await
            .status
            .is_success());
    }
    let missing = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        json!({"sample":"CREATE_200"}),
    )
    .await);
    assert_eq!(missing["kind"], "CLARIFY");
    assert_eq!(missing["planning_source"], "MOCK_LLM");
    assert_eq!(server.calls(), 0);
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("READ_WAIT", None),
    )
    .await);
    assert_eq!(initial["planning_source"], "MOCK_LLM");
    let id = initial["task"]["task_id"].as_str().unwrap();
    let waiting = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/execute"),
        version(&initial),
    )
    .await);
    assert_eq!(waiting["task"]["task_state"], "WAITING_USER");
    assert_eq!(
        waiting["results"][0]["data"]["available_balance"],
        "1288.00"
    );
    let mut input = version(&waiting);
    input["waiting_node_id"] = json!("choice");
    let complete = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/continue"),
        input,
    )
    .await);
    assert_eq!(complete["task"]["task_state"], "COMPLETED");
    assert_eq!(complete["results"].as_array().unwrap().len(), 2);
    let created = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CREATE_200", None),
    )
    .await)["draft"]
        .clone();
    assert_eq!(server.calls(), 2);
    let prepared = prepare(&server, &cookie, &created).await;
    assert!(!command(
        &server,
        &cookie,
        &path(&prepared, "execute"),
        execute_input(&prepared)
    )
    .await
    .status
    .is_success());
    assert_eq!(server.calls(), 2);
    let (old_confirmation, saved) = run(&server, &cookie, &created).await;
    let modified_view = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("MODIFY_300", Some(&saved)),
    )
    .await);
    assert_eq!(modified_view["planning_source"], "MOCK_LLM");
    let modified = &modified_view["draft"];
    assert_eq!(modified["task_id"], saved["task_id"]);
    assert_eq!(modified["parameters"]["changes"]["amount"], "300.00");
    assert_ne!(modified["operation_id"], saved["operation_id"]);
    assert!(!command(
        &server,
        &cookie,
        &path(modified, "execute"),
        execute_input(&old_confirmation)
    )
    .await
    .status
    .is_success());
    assert_eq!(server.calls(), 3);
    let (_, changed) = run(&server, &cookie, modified).await;
    let cancelled = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CANCEL_DRAFT", Some(&changed)),
    )
    .await)["draft"]
        .clone();
    assert_eq!(server.calls(), 4);
    let (_, done) = run(&server, &cookie, &cancelled).await;
    assert_eq!(done["task_state"], "CANCELLED");
    assert_eq!(server.calls(), 5);
    let original = ok(query(&server, &cookie, done["task_id"].as_str().unwrap()).await);
    assert_eq!(original["planning_source"], "MOCK_LLM");
    assert_eq!(server.calls(), 5);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; actual Store recreated, no model"]
async fn real_http_agent_fixed_restart_history_missing_store_unknown_and_mode_isolation() {
    let db = Db::new().await;
    let server = server_mode(&db, None, true, true).await;
    let cookie = server.login().await;
    let read = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("READ_WAIT", None),
    )
    .await);
    let read_id = read["task"]["task_id"].as_str().unwrap();
    let waiting = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{read_id}/execute"),
        version(&read),
    )
    .await);
    let created = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CREATE_200", None),
    )
    .await)["draft"]
        .clone();
    let (_, saved) = run(&server, &cookie, &created).await;
    let id = saved["task_id"].as_str().unwrap();
    let pending = ok(command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CREATE_200", None),
    )
    .await)["draft"]
        .clone();
    let prepared = prepare(&server, &cookie, &pending).await;
    let confirmed = ok(command(
        &server,
        &cookie,
        &path(&prepared, "confirm"),
        confirm_input(&prepared),
    )
    .await);
    // A real execution with an audit outage settles as unknown; no replacement action is allowed.
    db.scoped.execute("CREATE FUNCTION reject_fixed_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.details ? 'lifecycle_settled' THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER reject_fixed_settlement BEFORE INSERT ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_fixed_settlement()").await.unwrap();
    let unknown = command(
        &server,
        &cookie,
        &path(&confirmed, "execute"),
        execute_input(&confirmed),
    )
    .await;
    assert!(!unknown.status.is_success());
    db.scoped
        .execute("DROP TRIGGER reject_fixed_settlement ON audit_events")
        .await
        .unwrap();
    let pending_id = pending["task_id"].as_str().unwrap();
    let unknown = ok(query(&server, &cookie, pending_id).await);
    assert_eq!(unknown["draft"]["execution_status"], "RESULT_UNKNOWN");
    let before = server.calls();
    assert!(!command(
        &server,
        &cookie,
        "/api/agent/samples",
        sample_input("CANCEL_DRAFT", Some(&unknown["draft"]))
    )
    .await
    .status
    .is_success());
    assert_eq!(server.calls(), before);
    server.stop();
    let restarted = server_mode(&db, None, true, false).await;
    let new_cookie = restarted.login().await;
    assert_eq!(
        ok(query(&restarted, &new_cookie, read_id).await)["results"],
        waiting["results"]
    );
    let history = ok(query(&restarted, &new_cookie, id).await);
    assert_eq!(history["draft"]["receipt"], saved["receipt"]);
    assert_eq!(history["planning_source"], "MOCK_LLM");
    assert!(!command(
        &restarted,
        &new_cookie,
        "/api/agent/samples",
        sample_input("MODIFY_300", Some(&saved))
    )
    .await
    .status
    .is_success());
    assert_eq!(
        ok(query(&restarted, &new_cookie, pending_id).await)["draft"]["execution_status"],
        "RESULT_UNKNOWN"
    );
    assert_eq!(restarted.calls(), 0);
    restarted.stop();
    let fixed_again = server_mode(&db, None, true, true).await;
    let session = fixed_again.login().await;
    let missing = command(
        &fixed_again,
        &session,
        "/api/agent/samples",
        sample_input("MODIFY_300", Some(&saved)),
    )
    .await;
    assert_eq!(missing.status, StatusCode::OK);
    assert_eq!(missing.json["draft"]["action"], "MODIFY_MOCK_DRAFT");
    assert_eq!(fixed_again.calls(), 0);
    fixed_again.stop();
    db.finish().await;
}
async fn command(server: &Server, cookie: &str, path: &str, input: Value) -> Reply {
    server
        .call(
            Method::POST,
            path,
            Some(input.to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await
}
async fn query(server: &Server, cookie: &str, id: &str) -> Reply {
    server
        .call(
            Method::GET,
            &format!("/api/agent/tasks/{id}"),
            None,
            Some(cookie),
            None,
            None,
            false,
        )
        .await
}
fn ok(reply: Reply) -> Value {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    reply.json
}
fn selection() -> Value {
    json!({"account_id":"account_001","payee_query":"小李","payee_id":"payee_008"})
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; model wire-protocol fixture is not a real LLM"]
async fn payment_agent_http_reviewed_model_locator_rejects_stale_extra_and_mixed_plans() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("create", None),
    )
    .await);
    let (_, saved) = run(&server, &cookie, &initial["draft"]).await;
    for (text, status) in [
        ("payment stale", StatusCode::CONFLICT),
        ("payment forged amount", StatusCode::UNPROCESSABLE_ENTITY),
        ("payment mixed", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        assert_eq!(
            command(
                &server,
                &cookie,
                "/api/agent/messages",
                message(text, Some(&saved))
            )
            .await
            .status,
            status
        );
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM payment_preparations")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let response = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("payment", Some(&saved)),
    )
    .await);
    assert_eq!(response["kind"], "PAYMENT");
    assert_eq!(
        response["payment_flow"]["status"],
        "WAITING_PAYMENT_CONFIRMATION"
    );
    assert_eq!(
        response["payment"]["agent_binding"]["planning_source"],
        "OPENAI_COMPATIBLE"
    );
    assert_eq!(
        response["payment"]["snapshot"]["transfer"]["amount"],
        "200.00"
    );
    let definitions = provider.definitions.lock().unwrap().clone();
    let tool = definitions
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "execute_transfer")
        .unwrap();
    assert_eq!(tool["has_side_effect"], true);
    assert!(tool["description"]
        .as_str()
        .unwrap()
        .contains("funds execution is disabled"));
    assert_eq!(
        server.calls(),
        1,
        "no funds or repeated draft Tool invocation"
    );
    server.stop();
    db.finish().await;
}
#[tokio::test]
#[ignore = "requires isolated PostgreSQL; model wire-protocol fixture is not a real LLM"]
async fn payment_agent_http_model_node_rename_replays_first_binding_without_writes() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("create", None),
    )
    .await);
    let (_, saved) = run(&server, &cookie, &initial["draft"]).await;
    let first = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("payment", Some(&saved)),
    )
    .await);
    assert_eq!(
        first["payment"]["agent_binding"]["reviewed_plan"]["nodes"][0]["id"],
        "payment"
    );
    let before: Value = sqlx::query_scalar("SELECT jsonb_build_array((SELECT count(*) FROM operations),(SELECT count(*) FROM payment_preparations),(SELECT count(*) FROM server_confirmations),(SELECT count(*) FROM payment_agent_bindings),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox))")
        .fetch_one(&db.scoped).await.unwrap();
    let replay = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("payment renamed", Some(&saved)),
    )
    .await);
    assert_eq!(first["payment"]["replayed"], false);
    assert_eq!(replay["payment"]["replayed"], true);
    let mut expected = first["payment"].clone();
    expected["replayed"] = json!(true);
    assert_eq!(replay["payment"], expected);
    assert_eq!(replay["payment_flow"], first["payment_flow"]);
    for (text, status) in [
        ("payment stale", StatusCode::CONFLICT),
        ("payment wrong draft", StatusCode::CONFLICT),
        ("payment forged amount", StatusCode::UNPROCESSABLE_ENTITY),
        ("payment mixed", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let reply = command(
            &server,
            &cookie,
            "/api/agent/messages",
            message(text, Some(&saved)),
        )
        .await;
        assert_eq!(reply.status, status, "{text}: {}", reply.json);
    }
    let after: Value = sqlx::query_scalar("SELECT jsonb_build_array((SELECT count(*) FROM operations),(SELECT count(*) FROM payment_preparations),(SELECT count(*) FROM server_confirmations),(SELECT count(*) FROM payment_agent_bindings),(SELECT count(*) FROM audit_events),(SELECT count(*) FROM public_audit_outbox))")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(
        after, before,
        "replanning must not append payment or authority records"
    );
    let funds: (i64, i64, i64, i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM public_execution_requests),(SELECT count(*) FROM execution_permits),(SELECT count(*) FROM transfer_business_orders_v1),(SELECT count(*) FROM transfer_fund_orders_v1),(SELECT count(*) FROM transfer_ledger_v1)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(funds, (0, 0, 0, 0, 0));
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

fn message(text: &str, context: Option<&Value>) -> Value {
    json!({"message":text,"selection":selection(),"context":context.map(|d| json!({"task_id":d["task_id"],"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"],"selection_ticket":d["selection_ticket"]}))})
}
fn version(view: &Value) -> Value {
    json!({"task_revision":view["task"]["revision"],"plan_version":view["task"]["plan_version"],"facts_version":view["task"]["facts_version"]})
}
fn path(draft: &Value, action: &str) -> String {
    format!(
        "/api/sim/draft-lifecycles/{}/operations/{}/{action}",
        draft["task_id"].as_str().unwrap(),
        draft["operation_id"].as_str().unwrap()
    )
}
fn confirm_input(d: &Value) -> Value {
    json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":d["confirmation"]["confirmation_ref"],"display_ticket":d["display_ticket"],"action":"CONFIRM"})
}
fn execute_input(d: &Value) -> Value {
    json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":d["confirmation"]["confirmation_ref"],"task_revision":d["task_revision"],"operation_revision":d["operation_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"]})
}
async fn prepare(server: &Server, cookie: &str, d: &Value) -> Value {
    ok(command(
        server,
        cookie,
        &format!(
            "/api/agent/tasks/{}/prepare",
            d["task_id"].as_str().unwrap()
        ),
        json!({"operation_id":d["operation_id"]}),
    )
    .await)["draft"]
        .clone()
}
async fn run(server: &Server, cookie: &str, d: &Value) -> (Value, Value) {
    let prepared = prepare(server, cookie, d).await;
    assert_eq!(prepared["next_action"]["kind"], "CONFIRM");
    let confirmed = ok(command(
        server,
        cookie,
        &path(&prepared, "confirm"),
        confirm_input(&prepared),
    )
    .await);
    let executed = ok(command(
        server,
        cookie,
        &path(&confirmed, "execute"),
        execute_input(&confirmed),
    )
    .await);
    (confirmed, executed)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; provider is protocol fixture only"]
async fn real_http_agent_reads_actual_results_continues_and_recovers_without_reexecution() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("read and continue", None),
    )
    .await);
    assert_eq!(server.calls(), 0);
    assert_eq!(initial["can_start"], true);
    let id = initial["task"]["task_id"].as_str().unwrap();
    let waiting = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/execute"),
        version(&initial),
    )
    .await);
    assert_eq!(waiting["task"]["task_state"], "WAITING_USER");
    assert_eq!(
        waiting["results"][0]["data"]["available_balance"],
        "1288.00"
    );
    assert_eq!(server.calls(), 1);
    let mut input = version(&waiting);
    input["waiting_node_id"] = json!("choice");
    let complete = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/continue"),
        input.clone(),
    )
    .await);
    assert_eq!(complete["task"]["task_state"], "COMPLETED");
    assert_eq!(complete["results"].as_array().unwrap().len(), 2);
    assert_eq!(server.calls(), 2);
    let duplicate = command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/continue"),
        input,
    )
    .await;
    assert!(duplicate.status.is_success() || duplicate.status == StatusCode::CONFLICT);
    assert_eq!(server.calls(), 2);
    assert_eq!(
        ok(query(&server, &cookie, id).await)["results"],
        complete["results"]
    );
    let original: Value =
        sqlx::query_scalar("SELECT receipt FROM runtime_read_continuations WHERE task_id=$1")
            .bind(id)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    let mut corrupt = original.clone();
    corrupt["results"][1]["data"]["available_balance"] = json!("9999.00");
    sqlx::query("UPDATE runtime_read_continuations SET receipt=$1 WHERE task_id=$2")
        .bind(sqlx::types::Json(&corrupt))
        .bind(id)
        .execute(&db.scoped)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET details=jsonb_set(details,'{runtime_read_continuation_execution}',$1) WHERE task_id=$2 AND details ? 'runtime_read_continuation_execution'").bind(sqlx::types::Json(&corrupt)).bind(id).execute(&db.scoped).await.unwrap();
    assert_eq!(
        query(&server, &cookie, id).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(server.calls(), 2);
    sqlx::query("UPDATE runtime_read_continuations SET receipt=$1 WHERE task_id=$2")
        .bind(sqlx::types::Json(&original))
        .bind(id)
        .execute(&db.scoped)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET details=jsonb_set(details,'{runtime_read_continuation_execution}',$1) WHERE task_id=$2 AND details ? 'runtime_read_continuation_execution'").bind(sqlx::types::Json(&original)).bind(id).execute(&db.scoped).await.unwrap();
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    let reloaded = super::agent_tests::server(&db, None, true).await;
    let new_cookie = reloaded.login().await;
    assert_eq!(
        ok(query(&reloaded, &new_cookie, id).await)["results"],
        complete["results"]
    );
    assert_eq!(reloaded.calls(), 0);
    server.stop();
    reloaded.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn real_http_agent_legacy_receipt_has_no_fabricated_result_body() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("read", None),
    )
    .await);
    let id = initial["task"]["task_id"].as_str().unwrap();
    let complete = ok(command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/execute"),
        version(&initial),
    )
    .await);
    assert_eq!(complete["results"].as_array().unwrap().len(), 1);
    sqlx::query("UPDATE runtime_read_claims SET receipt=receipt-'results' WHERE task_id=$1")
        .bind(id)
        .execute(&db.scoped)
        .await
        .unwrap();
    sqlx::query("UPDATE audit_events SET details=jsonb_set(details,'{runtime_read_execution}',(details->'runtime_read_execution')-'results') WHERE task_id=$1 AND details ? 'runtime_read_execution'").bind(id).execute(&db.scoped).await.unwrap();
    let legacy = ok(query(&server, &cookie, id).await);
    assert_eq!(legacy["task"]["task_state"], "COMPLETED");
    assert!(legacy["results"].is_null());
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual delivered draft Tools"]
async fn real_http_agent_creates_modifies_cancels_same_task_only_after_fresh_confirmation() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    let created = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("create", None),
    )
    .await)["draft"]
        .clone();
    assert_eq!(server.calls(), 0);
    assert!(created["receipt"].is_null());
    let missing = command(&server,&cookie,&path(&created,"execute"),json!({"contract_version":2,"scope":"MOCK_DRAFT_LIFECYCLE_ONLY","confirmation_ref":"missing","task_revision":created["task_revision"],"operation_revision":created["operation_revision"],"plan_version":created["plan_version"],"facts_version":created["facts_version"]})).await;
    assert!(!missing.status.is_success());
    assert_eq!(server.calls(), 0);
    let (old_confirmed, saved) = run(&server, &cookie, &created).await;
    assert_eq!(saved["task_state"], "WAITING_USER");
    assert_eq!(server.calls(), 1);
    let replay = ok(command(
        &server,
        &cookie,
        &path(&old_confirmed, "execute"),
        execute_input(&old_confirmed),
    )
    .await);
    assert_eq!(replay["receipt"], saved["receipt"]);
    assert_eq!(server.calls(), 1);
    let modified = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("modify", Some(&saved)),
    )
    .await)["draft"]
        .clone();
    assert_eq!(modified["task_id"], saved["task_id"]);
    assert_ne!(modified["operation_id"], saved["operation_id"]);
    assert_eq!(server.calls(), 1);
    let stale = command(
        &server,
        &cookie,
        &path(&modified, "execute"),
        execute_input(&old_confirmed),
    )
    .await;
    assert!(!stale.status.is_success());
    assert_eq!(server.calls(), 1);
    let (_, changed) = run(&server, &cookie, &modified).await;
    assert_eq!(changed["receipt"]["draft_version"], 2);
    assert_eq!(server.calls(), 2);
    let cancelled = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("cancel", Some(&changed)),
    )
    .await)["draft"]
        .clone();
    assert_eq!(server.calls(), 2);
    let (_, done) = run(&server, &cookie, &cancelled).await;
    assert_eq!(done["task_state"], "CANCELLED");
    assert_eq!(done["receipt"]["draft_id"], saved["receipt"]["draft_id"]);
    assert_eq!(server.calls(), 3);
    let rejected = command(
        &server,
        &cookie,
        "/api/agent/messages",
        message("modify", Some(&done)),
    )
    .await;
    assert!(!rejected.status.is_success());
    assert_eq!(server.calls(), 3);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL"]
async fn real_http_agent_clarifies_and_rejects_unavailable_or_invented_capabilities_without_writes()
{
    let db = Db::new().await;
    let provider = Provider::new().await;
    let server = server(&db, Some(&provider), true).await;
    let cookie = server.login().await;
    for text in ["clarify", "invent"] {
        let result =
            ok(command(&server, &cookie, "/api/agent/messages", message(text, None)).await);
        assert_eq!(result["kind"], "CLARIFY");
    }
    for text in ["unauthorized", "mixed"] {
        let reply = command(&server, &cookie, "/api/agent/messages", message(text, None)).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    }
    let mut unknown_account = message("create", None);
    unknown_account["selection"]["account_id"] = json!("account_unknown");
    assert_eq!(
        command(&server, &cookie, "/api/agent/messages", unknown_account)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let task_count: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(task_count, 0);
    assert_eq!(server.calls(), 0);
    let disabled = super::agent_tests::server(&db, Some(&provider), false).await;
    let disabled_cookie = disabled.login().await;
    let input = json!({"message":"create","selection":{"account_id":"account_001","payee_query":null,"payee_id":null},"context":null});
    assert_eq!(
        command(&disabled, &disabled_cookie, "/api/agent/messages", input)
            .await
            .status,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let unconfigured = super::agent_tests::server(&db, None, false).await;
    let unconfigured_cookie = unconfigured.login().await;
    let missing = command(
        &unconfigured,
        &unconfigured_cookie,
        "/api/agent/messages",
        json!({"message":"read"}),
    )
    .await;
    assert_eq!(missing.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(missing.json["error"]["reason_code"], "LLM_NOT_CONFIGURED");
    assert_eq!(unconfigured.calls(), 0);
    assert_eq!(disabled.calls(), 0);
    server.stop();
    disabled.stop();
    unconfigured.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; loopback protocol fixture and delivered Tools"]
async fn real_http_agent_registration_status_model_and_dispatch_match() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    for enabled in [false, true] {
        let server = server(&db, Some(&provider), enabled).await;
        let cookie = server.login().await;
        let status = ok(server
            .call(
                Method::GET,
                "/api/agent/status",
                None,
                Some(&cookie),
                None,
                None,
                false,
            )
            .await);
        let text = if enabled { "payees" } else { "read" };
        let initial = ok(command(
            &server,
            &cookie,
            "/api/agent/messages",
            json!({"message":text,"selection":{"account_id":"account_001"}}),
        )
        .await);
        let definitions = provider.definitions.lock().unwrap().clone();
        let names: Vec<_> = definitions
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].clone())
            .collect();
        assert_eq!(status["tools"], json!(names));
        assert_eq!(names.len(), if enabled { 6 } else { 1 });
        assert_eq!(status["payment_planning_enabled"], enabled);
        assert_eq!(status["funds_execution_enabled"], false);
        let checked = if enabled {
            server
                .state
                .0
                .draft
                .as_ref()
                .unwrap()
                .lifecycle_runtime
                .as_ref()
                .unwrap()
        } else {
            &server.state.0.runtime
        };
        for name in &names {
            assert!(checked
                .execution(&safeflow_contracts::ToolName::parse(name.as_str().unwrap()).unwrap())
                .is_some());
        }
        let id = initial["task"]["task_id"].as_str().unwrap();
        let executed = ok(command(
            &server,
            &cookie,
            &format!("/api/agent/tasks/{id}/execute"),
            version(&initial),
        )
        .await);
        assert_eq!(executed["task"]["task_state"], "COMPLETED");
        assert_eq!(
            executed["results"][0]["tool_name"],
            if enabled {
                "find_payees"
            } else {
                "get_account_balance"
            }
        );
        assert_eq!(server.calls(), 1);
        // A model-selected account still requires an explicit, matching trusted user selection.
        for selection in [Value::Null, json!({"account_id":"account_002"})] {
            let reply = command(
                &server,
                &cookie,
                "/api/agent/messages",
                json!({"message":"read","selection":selection}),
            )
            .await;
            if selection.is_null() {
                assert_eq!(ok(reply)["kind"], "CLARIFY");
            } else {
                assert_eq!(reply.status, StatusCode::FORBIDDEN);
            }
        }
        assert_eq!(server.calls(), 1);
        if !enabled {
            let denied = command(
                &server,
                &cookie,
                "/api/agent/messages",
                json!({"message":"payees"}),
            )
            .await;
            assert_eq!(denied.status, StatusCode::UNPROCESSABLE_ENTITY);
        } else {
            assert_eq!(
                executed["results"][0]["data"]["candidates"][0]["payee_id"],
                "payee_008"
            );
        }
        server.stop();
    }
    db.finish().await;
}
