use super::*;
use safeflow_bank_gateway::{BankGateway, DecimalAmount};
use safeflow_contracts::ResourceId;
use safeflow_transfer_integration::funds_store::FundsStore;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual loopback HTTP"]
async fn batch_sql_balance_gateway_is_used_by_actual_http_and_missing_sql_fails_closed() {
    let db = Db::new().await;
    let bank = Arc::new(TransferSqlBankGateway::for_trusted_backend(
        db.repo.clone(),
        Arc::new(safeflow_mock_bank::MockBankGateway::new()),
    ));
    // No account initialization from a display tail number or a fixture owner.
    assert!(bank
        .get_account_balance(
            &UserId::parse("user_001").unwrap(),
            &ResourceId::parse("account_001").unwrap()
        )
        .await
        .is_err());
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &ResourceId::parse("account_001").unwrap(),
        &DecimalAmount::parse("333.00").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.agent_fixed_plan_enabled = true;
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "batch-sql-bank-test",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    let state =
        ApiState::for_project_simulation_with_bank(db.repo.clone(), config, bank.clone()).unwrap();
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
    let view = server.new_task(&cookie).await;
    assert_eq!(view["task_state"], "WAITING_USER");
    let evidence = server
        .state
        .public_audit_outbox_for_backend()
        .pending(100)
        .await
        .unwrap();
    assert!(!evidence.is_empty());
    assert!(evidence
        .iter()
        .all(|event| event.envelope["owner_id"] == "user_001"));
    // Runtime's actual Tool result is stored, not the catalog fixture's balance.
    let initial = server
        .call(
            Method::POST,
            "/api/agent/samples",
            Some(
                json!({"sample":"READ_WAIT","selection":{"account_id":"account_001"}}).to_string(),
            ),
            Some(&cookie),
            None,
            None,
            true,
        )
        .await;
    assert_eq!(initial.status, StatusCode::OK);
    let path = format!(
        "/api/agent/tasks/{}/execute",
        initial.json["task"]["task_id"].as_str().unwrap()
    );
    let result=server.call(Method::POST,&path,Some(json!({"task_revision":initial.json["task"]["revision"],"plan_version":initial.json["task"]["plan_version"],"facts_version":initial.json["task"]["facts_version"]}).to_string()),Some(&cookie),None,None,true).await;
    assert_eq!(result.status, StatusCode::OK);
    assert_eq!(
        result.json["results"][0]["data"]["available_balance"],
        "333.00"
    );
    assert!(bank
        .get_account_balance(
            &UserId::parse("user_002").unwrap(),
            &ResourceId::parse("account_001").unwrap()
        )
        .await
        .is_err());
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn batch_actual_lifecycle_outbox_atomic_failure_and_replay() {
    let db = Db::new().await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(300),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "batch-outbox-test",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    // Reuse the already delivered actual HTTP grammar and execution helpers.
    let initial = super::lifecycle_tests::create(&server, &cookie).await;
    let prepared = super::lifecycle_tests::prepare(&server, &cookie, &initial).await;
    let confirmed = super::lifecycle_tests::confirm(&server, &cookie, &prepared).await;
    db.scoped.execute("CREATE FUNCTION batch_outbox_reject() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'outbox failure'; END $$").await.unwrap();
    // Reject result settlement only, after the durable one-call claim.
    db.scoped.execute("CREATE TRIGGER batch_outbox_failure BEFORE INSERT ON public_audit_outbox FOR EACH ROW WHEN (NEW.envelope->'public_event'->'details' ? 'lifecycle_settled') EXECUTE FUNCTION batch_outbox_reject()").await.unwrap();
    let path = format!(
        "{}/execute",
        super::lifecycle_tests::operation_path(&confirmed)
    );
    let input = super::lifecycle_tests::execution(&confirmed);
    let result = super::lifecycle_tests::command(&server, &cookie, &path, input.clone()).await;
    assert_ne!(result.status, StatusCode::OK);
    let drafts: i64 = sqlx::query_scalar("SELECT count(*) FROM transfer_drafts_v1")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(drafts, 0);
    let calls = server.calls();
    assert_eq!(calls, 1);
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM public_audit_outbox WHERE envelope->'public_event'->'details' ? 'lifecycle_settled'").fetch_one(&db.scoped).await.unwrap();
    assert_eq!(count, 0);
    db.scoped
        .execute("DROP TRIGGER batch_outbox_failure ON public_audit_outbox")
        .await
        .unwrap();
    let retry = super::lifecycle_tests::command(&server, &cookie, &path, input).await;
    assert_ne!(retry.json["execution_status"], "SUCCEEDED");
    assert_eq!(server.calls(), calls);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn batch_actual_success_enqueues_once_and_original_replay_does_not_resend_business() {
    let db = Db::new().await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(300),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "batch-outbox-success",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    let initial = super::lifecycle_tests::create(&server, &cookie).await;
    let prepared = super::lifecycle_tests::prepare(&server, &cookie, &initial).await;
    let confirmed = super::lifecycle_tests::confirm(&server, &cookie, &prepared).await;
    let path = format!(
        "{}/execute",
        super::lifecycle_tests::operation_path(&confirmed)
    );
    let input = super::lifecycle_tests::execution(&confirmed);
    let first = super::lifecycle_tests::command(&server, &cookie, &path, input.clone()).await;
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.json["execution_status"], "SUCCEEDED");
    let pending = server
        .state
        .public_audit_outbox_for_backend()
        .pending(100)
        .await
        .unwrap();
    let settled: Vec<_> = pending
        .iter()
        .filter(|e| {
            e.envelope["public_event"]["details"]
                .get("lifecycle_settled")
                .is_some()
        })
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled[0].envelope["owner_id"], "user_001");
    let replay = super::lifecycle_tests::command(&server, &cookie, &path, input).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json["receipt"], first.json["receipt"]);
    assert_eq!(server.calls(), 1);
    assert_eq!(
        server
            .state
            .public_audit_outbox_for_backend()
            .pending(100)
            .await
            .unwrap(),
        pending
    );
    server.stop();
    db.finish().await;
}
