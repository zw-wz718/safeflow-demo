use super::*;

async fn same_name_server(db: &Db, provider: &Provider) -> Server {
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
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "agent-test",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    let state = ApiState::for_project_simulation_with_bank(
        db.repo.clone(),
        config,
        Arc::new(safeflow_mock_bank::MockBankGateway::with_same_name_payees()),
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
#[ignore = "isolated PostgreSQL, protocol fixture and actual restored business candidate cache"]
async fn restart_adapter_payee_change_requeries_candidates_after_business_restore() {
    let db = Db::new().await;
    let provider = Provider::new().await;
    let first = same_name_server(&db, &provider).await;
    let cookie = first.login().await;
    let proposed = ok(command(
        &first,
        &cookie,
        "/api/agent/messages",
        message("create", None),
    )
    .await);
    let (_, saved) = run(&first, &cookie, &proposed["draft"]).await;
    first.stop();
    let second = same_name_server(&db, &provider).await;
    let cookie = second.login().await;
    let mut input = message("modify payee", Some(&saved));
    input["selection"]["payee_id"] = json!("payee_009");
    let proposed = ok(command(&second, &cookie, "/api/agent/messages", input).await);
    let (_, modified) = run(&second, &cookie, &proposed["draft"]).await;
    assert_eq!(modified["receipt"]["draft_version"], 2);
    let sql: Value = sqlx::query_scalar("SELECT details FROM transfer_drafts_v1")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(sql["payee"]["payee_id"], "payee_009");
    assert_eq!(second.calls(), 1);
    second.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual fixed-plan Agent, no LLM network"]
async fn sql_bridge_fixed_agent_actual_actions_save_same_business_draft() {
    let db = Db::new().await;
    let server = server_mode(&db, None, true, true).await;
    let cookie = server.login().await;
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    let identity = server
        .state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("API session required"));
    let reader = server.state.transfer_sql_reader_for_backend();
    let mut previous = None;
    let mut draft_id = None;
    for (sample, version, amount, cancelled) in [
        ("CREATE_200", 1, "200.00", false),
        ("MODIFY_300", 2, "300.00", false),
        ("CANCEL_DRAFT", 2, "300.00", true),
    ] {
        let proposed = ok(command(
            &server,
            &cookie,
            "/api/agent/samples",
            sample_input(sample, previous.as_ref()),
        )
        .await);
        let (confirmation, saved) = run(&server, &cookie, &proposed["draft"]).await;
        let id = serde_json::from_value(saved["receipt"]["draft_id"].clone()).unwrap();
        let sql = reader.draft(&identity.reference, &id).await.unwrap();
        assert_eq!(sql.plan_version, version);
        assert_eq!(sql.details.as_ref().unwrap().amount.as_str(), amount);
        assert_eq!(
            sql.status == safeflow_transfer_integration::DraftStatus::Cancelled,
            cancelled
        );
        assert_eq!(json!(sql.task_id), saved["task_id"]);
        assert_eq!(sql.user_id, identity.binding.user_id);
        if let Some(original) = &draft_id {
            assert_eq!(original, &id);
        } else {
            draft_id = Some(id);
        }
        let replay = ok(command(
            &server,
            &cookie,
            &path(&confirmation, "execute"),
            execute_input(&confirmation),
        )
        .await);
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["receipt"], saved["receipt"]);
        previous = Some(saved);
    }
    assert_eq!(server.calls(), 3);
    let funds: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_accounts_v1")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(funds, 0, "draft bridge never provisions accounts or funds");
    assert!(server.state.0.llm_gateway.is_none());
    server.stop();
    db.finish().await;
}
