use super::*;
use crate::plugin_test_support::{probe, Probe};
use safeflow_bank_gateway::*;
use safeflow_contracts::{ContractFuture, ResourceId};
use safeflow_mock_bank::MockBankGateway;

async fn plugin_server(db: &Db, provider: &Provider, lifecycle: bool, probe: &Probe) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.llm_gateway = Some(
        safeflow_runtime::OpenAiGatewayConfig::new(
            &provider.endpoint,
            "local-protocol-only",
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
                "plugin-test",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        );
    }
    let state = ApiState::for_project_simulation_with_read_only_tools(
        db.repo.clone(),
        config,
        Arc::new(MockBankGateway::new()),
        vec![probe.registration.clone()],
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

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; local protocol fixture, no model service"]
async fn real_http_plugin_entry_plan_execute_wait_restart_continue_history_and_changed_source() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    for lifecycle in [false, true] {
        // Lifecycle already owns find_payees: a test-only alias exercises generic propagation.
        let probe = probe(Arc::new(MockBankGateway::new()), lifecycle);
        let server = plugin_server(&db, &provider, lifecycle, &probe).await;
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
        let text = if lifecycle {
            "plugin alias wait"
        } else {
            "plugin wait"
        };
        let initial = ok(command(
            &server,
            &cookie,
            "/api/agent/messages",
            json!({"message":text}),
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
        assert_eq!(names.len(), if lifecycle { 7 } else { 2 });
        let tool = probe.registration.input.tool_name.clone();
        assert_eq!(
            server.state.0.runtime.execution(&tool),
            Some(assembly::ExecutionPath::ReadOnly)
        );
        if lifecycle {
            assert_eq!(
                server
                    .state
                    .0
                    .draft
                    .as_ref()
                    .unwrap()
                    .lifecycle_runtime
                    .as_ref()
                    .unwrap()
                    .execution(&tool),
                Some(assembly::ExecutionPath::ReadOnly)
            );
        }
        assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
        let id = initial["task"]["task_id"].as_str().unwrap();
        let pending = ok(command(
            &server,
            &cookie,
            "/api/agent/messages",
            json!({"message":text}),
        )
        .await);
        let pending_id = pending["task"]["task_id"].as_str().unwrap();
        let waiting = ok(command(
            &server,
            &cookie,
            &format!("/api/agent/tasks/{id}/execute"),
            version(&initial),
        )
        .await);
        assert_eq!(waiting["task"]["task_state"], "WAITING_USER");
        assert_eq!(
            waiting["results"][0]["data"]["candidates"][0]["payee_id"],
            "payee_008"
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        assert_eq!(probe.users.lock().unwrap()[0].as_str(), "user_001");
        let legacy = ok(command(
            &server,
            &cookie,
            "/api/chat",
            json!({"kind":"NEW_READ_ONLY_DEMO"}),
        )
        .await);
        assert_eq!(legacy["task_state"], "WAITING_USER");
        let legacy = ok(command(&server, &cookie, "/api/chat", json!({"kind":"CONTINUE","task_id":legacy["task_id"],"waiting_node_id":"choose",
            "task_revision":legacy["revision"],"plan_version":legacy["plan_version"],"facts_version":legacy["facts_version"]})).await);
        assert_eq!(legacy["task_state"], "WAITING_USER");
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        server.stop();
        let restarted = plugin_server(&db, &provider, lifecycle, &probe).await;
        let cookie = restarted.login().await;
        let recovered = ok(query(&restarted, &cookie, id).await);
        assert_eq!(recovered["results"], waiting["results"]);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
        let mut input = version(&recovered);
        input["waiting_node_id"] = json!("choice");
        let complete = ok(command(
            &restarted,
            &cookie,
            &format!("/api/agent/tasks/{id}/continue"),
            input.clone(),
        )
        .await);
        assert_eq!(complete["task"]["task_state"], "COMPLETED");
        assert_eq!(complete["results"].as_array().unwrap().len(), 2);
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        ok(command(
            &restarted,
            &cookie,
            &format!("/api/agent/tasks/{id}/continue"),
            input,
        )
        .await);
        assert_eq!(
            ok(query(&restarted, &cookie, id).await)["results"],
            complete["results"]
        );
        assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
        // Original balance selection gate remains active with extended registration.
        assert_eq!(
            ok(command(
                &restarted,
                &cookie,
                "/api/agent/messages",
                json!({"message":"read"})
            )
            .await)["kind"],
            "CLARIFY"
        );
        assert!(!command(
            &restarted,
            &cookie,
            "/api/agent/messages",
            json!({"message":"read","selection":{"account_id":"account_002"}})
        )
        .await
        .status
        .is_success());
        if !lifecycle {
            assert!(!command(
                &restarted,
                &cookie,
                "/api/agent/messages",
                json!({"message":"plugin missing"})
            )
            .await
            .status
            .is_success());
        }
        let wait_again = ok(command(
            &restarted,
            &cookie,
            "/api/agent/messages",
            json!({"message":text}),
        )
        .await);
        let wait_id = wait_again["task"]["task_id"].as_str().unwrap();
        let wait_again = ok(command(
            &restarted,
            &cookie,
            &format!("/api/agent/tasks/{wait_id}/execute"),
            version(&wait_again),
        )
        .await);
        restarted.stop();
        let mut changed = probe;
        changed
            .registration
            .implementation_revision
            .push_str("-changed");
        let changed_server = plugin_server(&db, &provider, lifecycle, &changed).await;
        let cookie = changed_server.login().await;
        let before = changed.calls.load(Ordering::SeqCst);
        assert!(!command(
            &changed_server,
            &cookie,
            &format!("/api/agent/tasks/{pending_id}/execute"),
            version(&pending)
        )
        .await
        .status
        .is_success());
        let mut input = version(&wait_again);
        input["waiting_node_id"] = json!("choice");
        assert!(!command(
            &changed_server,
            &cookie,
            &format!("/api/agent/tasks/{wait_id}/continue"),
            input
        )
        .await
        .status
        .is_success());
        assert_eq!(changed.calls.load(Ordering::SeqCst), before);
        let legacy_input = json!({"kind":"CONTINUE","task_id":wait_id,"waiting_node_id":"choice",
            "task_revision":wait_again["task"]["revision"],"plan_version":wait_again["task"]["plan_version"],"facts_version":wait_again["task"]["facts_version"]});
        assert!(
            !command(&changed_server, &cookie, "/api/chat", legacy_input)
                .await
                .status
                .is_success()
        );
        assert_eq!(changed.calls.load(Ordering::SeqCst), before);
        changed_server.stop();
    }
    db.finish().await;
}

struct DeniedBank;
impl BankGateway for DeniedBank {
    fn get_account_balance<'a>(
        &'a self,
        _: &'a UserId,
        _: &'a ResourceId,
    ) -> ContractFuture<'a, Result<GetAccountBalanceOutput, BankError>> {
        Box::pin(async { Err(BankError::ResourceNotOwned) })
    }
    fn find_payees<'a>(
        &'a self,
        user: &'a UserId,
        _: &'a str,
    ) -> ContractFuture<'a, Result<FindPayeesOutput, BankError>> {
        assert_eq!(user.as_str(), "user_001");
        Box::pin(async { Err(BankError::ResourceNotOwned) })
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; real FindPayeesPolicy denial"]
async fn real_http_plugin_entry_policy_and_duplicate_startup_reject_without_tool_calls() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let probe = probe(Arc::new(DeniedBank), false);
    let server = plugin_server(&db, &provider, false, &probe).await;
    let cookie = server.login().await;
    let initial = ok(command(
        &server,
        &cookie,
        "/api/agent/messages",
        json!({"message":"payees"}),
    )
    .await);
    let id = initial["task"]["task_id"].as_str().unwrap();
    let denied = command(
        &server,
        &cookie,
        &format!("/api/agent/tasks/{id}/execute"),
        version(&initial),
    )
    .await;
    assert!(!denied.status.is_success());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    server.stop();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "duplicate-test",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    assert!(ApiState::for_project_simulation_with_read_only_tools(
        db.repo.clone(),
        config,
        Arc::new(MockBankGateway::new()),
        vec![probe.registration]
    )
    .is_err());
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    db.finish().await;
}
