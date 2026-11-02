//! Actual Agent/Runtime/PG path with Jonathan's unchanged fictional report cases.
use super::*;
use crate::billing::duplicate_tests::{assert_candidates, legacy_v1, ActionsSource};
use crate::billing::{tests as fixture, BILLING_REPORT_TOOL};
use safeflow_bank_gateway::BankGateway;
use std::sync::Mutex;
#[path = "billing/feedback_composition_tests.rs"]
mod feedback_composition_tests;
#[path = "simulation_source_http_tests.rs"]
mod simulation_source_http_tests;

struct BillingProvider {
    endpoint: String,
    arguments: Arc<Mutex<Value>>,
    definitions: Arc<Mutex<Value>>,
    handle: tokio::task::JoinHandle<()>,
}
impl Drop for BillingProvider {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl BillingProvider {
    async fn new(case: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let arguments = Arc::new(Mutex::new(fixture::arguments(case)));
        let definitions = Arc::new(Mutex::new(Value::Null));
        let app = Router::new()
            .route("/v1/chat/completions", post(billing_plan))
            .with_state((arguments.clone(), definitions.clone()));
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            endpoint,
            arguments,
            definitions,
            handle,
        }
    }
}
async fn billing_plan(
    State((arguments, definitions)): State<(Arc<Mutex<Value>>, Arc<Mutex<Value>>)>,
    Json(request): Json<Value>,
) -> Json<Value> {
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(!system.contains("user_demo") && !system.contains("session_"));
    let protocol: Value =
        serde_json::from_str(system.split("Protocol data: ").nth(1).unwrap()).unwrap();
    *definitions.lock().unwrap() = protocol["approved_tool_definitions"].clone();
    let input = arguments.lock().unwrap().clone();
    let read = |id: &str, depends: Value| json!({"type":"TOOL","id":id,"tool":BILLING_REPORT_TOOL,"arguments":input,"depends_on":depends});
    let nodes = if request["messages"][1]["content"] == "wait" {
        json!([read("report",json!([])),{"type":"USER_DECISION","id":"choice","depends_on":["report"]},read("second",json!(["choice"]))])
    } else {
        json!([read("report", json!([]))])
    };
    let result = json!({"contract_version":1,"understanding":{"intent":"LOCAL_BILLING_REPORT","entities":{},"missing_fields":[],"context_refs":[],"decision":"PASS"},
        "plan":{"contract_version":1,"task_id":protocol["planning_context"]["task_id"],"plan_version":protocol["planning_context"]["plan_version"],"nodes":nodes},"clarification":null});
    Json(
        json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":result.to_string()}}]}),
    )
}
async fn billing_server(
    db: &Db,
    provider: &BillingProvider,
    owner: &str,
    bank: Arc<dyn BankGateway>,
    registration: Option<crate::ReadOnlyToolRegistration>,
    lifecycle: bool,
) -> Server {
    billing_server_with_feedback(db, provider, owner, bank, registration, lifecycle, false).await
}
async fn billing_server_with_feedback(
    db: &Db,
    provider: &BillingProvider,
    owner: &str,
    bank: Arc<dyn BankGateway>,
    registration: Option<crate::ReadOnlyToolRegistration>,
    lifecycle: bool,
    feedback: bool,
) -> Server {
    billing_server_with_feedback_and_cards(
        db,
        provider,
        owner,
        bank,
        registration,
        lifecycle,
        feedback,
        None,
    )
    .await
}
async fn billing_server_with_feedback_and_cards(
    db: &Db,
    provider: &BillingProvider,
    owner: &str,
    bank: Arc<dyn BankGateway>,
    registration: Option<crate::ReadOnlyToolRegistration>,
    lifecycle: bool,
    feedback: bool,
    card_source: Option<safeflow_storage::CardCatalogSourceBinding>,
) -> Server {
    billing_server_with_recovery(
        db,
        provider,
        owner,
        bank,
        registration,
        lifecycle,
        feedback,
        card_source,
        None,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn billing_server_with_recovery(
    db: &Db,
    provider: &BillingProvider,
    owner: &str,
    bank: Arc<dyn BankGateway>,
    registration: Option<crate::ReadOnlyToolRegistration>,
    lifecycle: bool,
    feedback: bool,
    card_source: Option<safeflow_storage::CardCatalogSourceBinding>,
    recovery: Option<safeflow_storage::BillingRecoveryConfig>,
) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.card_read_source = card_source;
    if let Ok(origin) = std::env::var("SAFEFLOW_FEEDBACK_BROWSER_ORIGIN") {
        config.frontend_origin = origin;
    }
    if feedback {
        config.billing_feedback = Some(BillingFeedbackConfig::for_reviewed_backend(
            ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
            PermitConfig::new(Duration::from_secs(300)).unwrap(),
        ));
    }
    if let Some(recovery) = recovery {
        config.billing_feedback = Some(
            config
                .billing_feedback
                .take()
                .unwrap()
                .with_recovery_v2(recovery),
        );
    }
    config.llm_gateway = Some(
        safeflow_runtime::OpenAiGatewayConfig::new(
            &provider.endpoint,
            "local-billing-protocol-only",
            Zeroizing::new("isolated-protocol-key".into()),
            Duration::from_secs(5),
        )
        .unwrap(),
    );
    if lifecycle {
        config.draft_confirmation = Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "billing-test",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        );
    }
    // Existing private test constructor supports the member's original user_demo.
    // The public PROJECT_SIMULATION default remains user_001.
    let state = ApiState::build_with_bank(
        db.repo.clone(),
        config,
        UserId::parse(owner).unwrap(),
        bank,
        &registration.into_iter().collect::<Vec<_>>(),
    )
    .unwrap();
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
async fn initial(server: &Server, cookie: &str, text: &str) -> Value {
    ok(command(
        server,
        cookie,
        "/api/agent/messages",
        json!({"message":text}),
    )
    .await)
}
async fn execute(server: &Server, cookie: &str, view: &Value) -> Reply {
    command(
        server,
        cookie,
        &format!(
            "/api/agent/tasks/{}/execute",
            view["task"]["task_id"].as_str().unwrap()
        ),
        version(view),
    )
    .await
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; original fictional JSON, no production source or model"]
async fn real_http_billing_four_original_reports_execute_persist_restart_and_query_without_rerun() {
    let db = Db::new().await;
    for (index, case) in ["mixed", "empty", "page-failed", "unknown-refund"]
        .into_iter()
        .enumerate()
    {
        let provider = BillingProvider::new(case).await;
        let source = fixture::replay(case);
        let bank = fixture::bank();
        let registration =
            fixture::approved(bank.clone(), source.clone(), &format!("7007ce2:{case}"));
        let server = billing_server(
            &db,
            &provider,
            "user_demo",
            bank.clone(),
            Some(registration.clone()),
            index % 2 == 0,
        )
        .await;
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
        assert!(status["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool == BILLING_REPORT_TOOL));
        let prepared = initial(&server, &cookie, "report").await;
        assert_eq!(source.calls.load(Ordering::SeqCst), 0);
        assert!(provider
            .definitions
            .lock()
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["name"] == BILLING_REPORT_TOOL));
        let complete = ok(execute(&server, &cookie, &prepared).await);
        assert_eq!(complete["task"]["task_state"], "COMPLETED");
        let data = &complete["results"][0]["data"];
        fixture::assert_report(case, &data["report"]);
        if let Ok(directory) = std::env::var("SAFEFLOW_WEB_RESPONSE_EVIDENCE") {
            std::fs::write(
                std::path::Path::new(&directory).join(format!("billing-{case}.json")),
                serde_json::to_vec_pretty(&complete).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(data["source_revision"], format!("7007ce2:{case}"));
        let calls = source.calls.load(Ordering::SeqCst);
        assert_eq!(calls, if case == "page-failed" { 2 } else { 1 });
        let id = complete["task"]["task_id"].as_str().unwrap();
        assert_eq!(
            ok(query(&server, &cookie, id).await)["results"],
            complete["results"]
        );
        assert_eq!(
            execute(&server, &cookie, &prepared).await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), calls);
        server.stop();
        let restarted = billing_server(
            &db,
            &provider,
            "user_demo",
            bank.clone(),
            Some(registration),
            index % 2 == 0,
        )
        .await;
        let new_cookie = restarted.login().await;
        assert_eq!(
            query(&restarted, &cookie, id).await.status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ok(query(&restarted, &new_cookie, id).await)["results"],
            complete["results"]
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), calls);
        assert!(source
            .owners
            .lock()
            .unwrap()
            .iter()
            .all(|id| id == "user_demo"));
        restarted.stop();
    }
    let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(permits, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; member's original actions JSON, no production source"]
async fn real_http_billing_duplicate_candidates_persist_restart_and_query_same_snapshot() {
    let db = Db::new().await;
    for case in [
        "candidate",
        "missing-merchant",
        "source-unavailable",
        "pair-limit",
    ] {
        let provider = BillingProvider::new("mixed").await;
        let source = ActionsSource::new(case);
        let bank = fixture::bank();
        let registration = fixture::approved(bank.clone(), source.clone(), "d822709:actions");
        let server = billing_server(
            &db,
            &provider,
            "user_demo",
            bank.clone(),
            Some(registration),
            true,
        )
        .await;
        let cookie = server.login().await;
        let pending = initial(&server, &cookie, "report").await;
        let completed = ok(execute(&server, &cookie, &pending).await);
        assert_eq!(completed["task"]["task_state"], "COMPLETED");
        if case != "pair-limit" {
            assert_candidates(case, &completed["results"][0]["data"]);
        } else {
            assert_eq!(
                completed["results"][0]["data"]["duplicate_candidates"]["Ok"]["state"],
                "PAIR_LIMIT_EXCEEDED"
            );
        }
        if let Ok(directory) = std::env::var("SAFEFLOW_WEB_RESPONSE_EVIDENCE") {
            std::fs::write(
                std::path::Path::new(&directory).join(format!("billing-duplicate-{case}.json")),
                serde_json::to_vec_pretty(&completed).unwrap(),
            )
            .unwrap();
        }
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            execute(&server, &cookie, &pending).await.status,
            StatusCode::CONFLICT
        );
        let id = completed["task"]["task_id"].as_str().unwrap();
        server.stop();
        // History survives even when the source/tool is no longer configured.
        let restarted = billing_server(&db, &provider, "user_demo", bank, None, true).await;
        let new_cookie = restarted.login().await;
        assert_eq!(
            ok(query(&restarted, &new_cookie, id).await)["results"],
            completed["results"]
        );
        assert_eq!(
            query(&restarted, &cookie, id).await.status,
            StatusCode::UNAUTHORIZED
        );
        if case == "candidate" {
            let audit_count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE task_id=$1")
                    .bind(id)
                    .fetch_one(&db.scoped)
                    .await
                    .unwrap();
            assert_eq!(
                ok(query(&restarted, &new_cookie, id).await)["results"],
                completed["results"]
            );
            let after: i64 =
                sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE task_id=$1")
                    .bind(id)
                    .fetch_one(&db.scoped)
                    .await
                    .unwrap();
            assert_eq!(audit_count, after);
            // History still verifies persisted hashes and checkpoint/audit agreement.
            sqlx::query("UPDATE runtime_read_claims SET receipt=jsonb_set(receipt,'{results,0,data,duplicate_candidates,Ok,report_id}', '\"tampered\"'::jsonb) WHERE task_id=$1").bind(id).execute(&db.scoped).await.unwrap();
            assert!(!query(&restarted, &new_cookie, id).await.status.is_success());
        }
        assert_eq!(source.calls.load(Ordering::SeqCst), 1);
        restarted.stop();
    }
    let permits: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_permits")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(permits, 0);
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; old serializer compatibility, no re-created member evidence"]
async fn real_http_billing_v1_history_remains_readable_but_old_plans_cannot_run_v2() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let current = fixture::approved(bank.clone(), source.clone(), "7007ce2:unchanged-source");
    let legacy = legacy_v1(current.clone());
    assert_ne!(
        legacy.implementation_revision,
        current.implementation_revision
    );
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(legacy),
        true,
    )
    .await;
    let cookie = server.login().await;
    let pending = initial(&server, &cookie, "report").await;
    let waiting = initial(&server, &cookie, "wait").await;
    let waiting = ok(execute(&server, &cookie, &waiting).await);
    assert_eq!(
        waiting["results"][0]["data"]["adapter_contract"],
        "LOCAL_BILLING_REPORT_ADAPTER_V1"
    );
    assert!(waiting["results"][0]["data"]
        .get("duplicate_candidates")
        .is_none());
    let id = waiting["task"]["task_id"].as_str().unwrap();
    let selectable: i64 =
        sqlx::query_scalar("SELECT count(*) FROM public_billing_reports_v1 WHERE task_id=$1")
            .bind(id)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(
        selectable, 0,
        "V1 display history is not trusted feedback evidence"
    );
    let calls = source.calls.load(Ordering::SeqCst);
    server.stop();
    let server = billing_server(&db, &provider, "user_demo", bank, Some(current), true).await;
    let cookie = server.login().await;
    assert_eq!(
        ok(query(&server, &cookie, id).await)["results"],
        waiting["results"]
    );
    assert!(!execute(&server, &cookie, &pending)
        .await
        .status
        .is_success());
    let mut input = version(&waiting);
    input["waiting_node_id"] = json!("choice");
    assert!(!command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/continue"),
        input
    )
    .await
    .status
    .is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), calls);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; strict input and missing source"]
async fn real_http_billing_missing_source_and_forged_trusted_fields_do_not_execute() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let bank = fixture::bank();
    let missing = billing_server(&db, &provider, "user_demo", bank.clone(), None, false).await;
    let cookie = missing.login().await;
    let status = ok(missing
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
    assert!(!status["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool == BILLING_REPORT_TOOL));
    assert!(!command(
        &missing,
        &cookie,
        "/api/agent/messages",
        json!({"message":"report"})
    )
    .await
    .status
    .is_success());
    missing.stop();
    let source = fixture::replay("mixed");
    let server = billing_server(
        &db,
        &provider,
        "user_demo",
        bank.clone(),
        Some(fixture::approved(bank, source.clone(), "7007ce2:mixed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    for field in [
        "trusted_scope",
        "owner_id",
        "as_of",
        "report_id",
        "calculated_at",
        "page",
        "is_complete",
        "source_revision",
    ] {
        let mut input = fixture::arguments("mixed");
        input[field] = json!("forged");
        *provider.arguments.lock().unwrap() = input;
        assert!(
            !command(
                &server,
                &cookie,
                "/api/agent/messages",
                json!({"message":"report"})
            )
            .await
            .status
            .is_success(),
            "{field}"
        );
    }
    *provider.arguments.lock().unwrap() = fixture::arguments("mixed");
    assert!(!command(
        &server,
        &cookie,
        "/api/agent/messages",
        json!({"message":"report","owner_id":"user_demo"})
    )
    .await
    .status
    .is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; real existing bank ownership and original fixture identity"]
async fn real_http_billing_foreign_account_owner_and_fixture_identity_are_never_aliased() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let registration = fixture::approved(bank.clone(), source.clone(), "7007ce2:mixed");
    let server = billing_server(&db, &provider, "user_demo", bank, Some(registration), false).await;
    let cookie = server.login().await;
    let mut input = fixture::arguments("mixed");
    input["account_ids_json"] = json!(r#"["account_001","account_002"]"#);
    *provider.arguments.lock().unwrap() = input;
    let prepared = initial(&server, &cookie, "report").await;
    assert!(!execute(&server, &cookie, &prepared)
        .await
        .status
        .is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), 0);
    server.stop();
    *provider.arguments.lock().unwrap() = fixture::arguments("mixed");
    let bank: Arc<dyn BankGateway> = Arc::new(safeflow_mock_bank::MockBankGateway::new());
    let server = billing_server(
        &db,
        &provider,
        "user_001",
        bank.clone(),
        Some(fixture::approved(bank, source.clone(), "7007ce2:mixed")),
        false,
    )
    .await;
    let cookie = server.login().await;
    let prepared = initial(&server, &cookie, "report").await;
    let report =
        ok(execute(&server, &cookie, &prepared).await)["results"][0]["data"]["report"].clone();
    assert_eq!(report["read"]["state"], "RESOURCE_UNAVAILABLE");
    assert_eq!(report["calculation_status"], "READ_BLOCKED");
    assert!(report["summary"].is_null());
    assert_eq!(source.owners.lock().unwrap().as_slice(), ["user_001"]);
    let id = prepared["task"]["task_id"].as_str().unwrap();
    server.stop();
    let stranger = billing_server(
        &db,
        &provider,
        "user_recipient",
        fixture::bank(),
        None,
        false,
    )
    .await;
    let cookie = stranger.login().await;
    assert!(!query(&stranger, &cookie, id).await.status.is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    stranger.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; registration source version binding"]
async fn real_http_billing_changed_source_revision_rejects_old_execute_and_continue() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::replay("mixed");
    let bank = fixture::bank();
    let first = fixture::approved(bank.clone(), source.clone(), "7007ce2:source-v1");
    let server = billing_server(&db, &provider, "user_demo", bank.clone(), Some(first), true).await;
    let cookie = server.login().await;
    let pending = initial(&server, &cookie, "report").await;
    let waiting = initial(&server, &cookie, "wait").await;
    let waiting = ok(execute(&server, &cookie, &waiting).await);
    assert_eq!(waiting["task"]["task_state"], "WAITING_USER");
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    server.stop();
    let changed = fixture::approved(bank.clone(), source.clone(), "7007ce2:source-v2");
    let server = billing_server(&db, &provider, "user_demo", bank, Some(changed), true).await;
    let cookie = server.login().await;
    assert!(!execute(&server, &cookie, &pending)
        .await
        .status
        .is_success());
    let mut input = version(&waiting);
    input["waiting_node_id"] = json!("choice");
    assert!(!command(
        &server,
        &cookie,
        &format!(
            "/api/agent/tasks/{}/continue",
            waiting["task"]["task_id"].as_str().unwrap()
        ),
        input
    )
    .await
    .status
    .is_success());
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "isolated PostgreSQL; deterministic session revoke while original source is held"]
async fn real_http_billing_session_revocation_during_read_discards_report_without_retry() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let source = fixture::GatedReplay::new();
    let bank = fixture::bank();
    let registration = fixture::approved(bank.clone(), source.clone(), "7007ce2:gated");
    let server = billing_server(&db, &provider, "user_demo", bank, Some(registration), false).await;
    let cookie = server.login().await;
    let prepared = initial(&server, &cookie, "report").await;
    let address = server.address;
    let execute_cookie = cookie.clone();
    let version = version(&prepared);
    let path = format!(
        "/api/agent/tasks/{}/execute",
        prepared["task"]["task_id"].as_str().unwrap()
    );
    let running = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &path,
            Some(version.to_string()),
            Some(&execute_cookie),
            None,
            None,
            true,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(3), source.entered.notified())
        .await
        .unwrap();
    let response = server
        .call(
            Method::GET,
            "/api/agent/status",
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let reference = server.state.0.sessions.resolve_binding(
        &crate::session::token_reference(
            cookie.split(';').next().unwrap().split_once('=').unwrap().1,
        )
        .unwrap(),
    );
    assert!(reference.is_ok());
    let token = cookie.split(';').next().unwrap().split_once('=').unwrap().1;
    server
        .state
        .0
        .sessions
        .revoke(&crate::session::token_reference(token).unwrap())
        .unwrap();
    source.release();
    assert!(!tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .status
        .is_success());
    let new_cookie = server.login().await;
    assert!(!execute(&server, &new_cookie, &prepared)
        .await
        .status
        .is_success());
    assert_eq!(source.inner.calls.load(Ordering::SeqCst), 1);
    let persisted: String =
        sqlx::query_scalar("SELECT COALESCE(jsonb_agg(checkpoint)::text,'[]') FROM tasks")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert!(!persisted.contains("gross_consumption"));
    server.stop();
    db.finish().await;
}
