//! Actual HTTP/Runtime uses async PG source; original member rows unchanged.
use super::*;
use safeflow_billing_source_reader::query::{Request as BillingRequest, Transaction};
use safeflow_storage::{PgSimulationSource, SimulationSourceSpec};
fn input() -> (SimulationSourceSpec, BillingRequest, Vec<Transaction>) {
    let raw: Value = serde_json::from_str(include_str!(
        "../../../members/jonathan/fixtures/billing/consumption_pages.v1.json"
    ))
    .unwrap();
    let sample = raw["samples"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "complete")
        .unwrap();
    let request: BillingRequest = serde_json::from_value(sample["request"].clone()).unwrap();
    let rows = sample["steps"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["page"]["items"].as_array().unwrap())
        .map(|r| serde_json::from_value(r.clone()).unwrap())
        .collect();
    (
        SimulationSourceSpec {
            source_id: raw["source_id"].as_str().unwrap().into(),
            owner: "user_demo".into(),
            revision: "original-complete-v1".into(),
            accounts: request.account_ids.clone(),
            coverage: request.window.clone(),
            coverage_evidence: "original complete case; isolated contract verification".into(),
            complete: true,
            page_size: 2,
        },
        request,
        rows,
    )
}
fn ledger(case: &str) -> safeflow_subscription_evidence::managed_ledger_mock::LedgerFixture {
    let raw: Value =
        serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_LEDGER_FIXTURE"))).unwrap();
    serde_json::from_value(
        raw["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["case_id"] == case)
            .unwrap()["ledger"]
            .clone(),
    )
    .unwrap()
}
#[tokio::test]
#[ignore]
async fn simulation_source_actual_http_billing_list_native_assembly_and_default_denial() {
    let db = Db::new().await;
    let (spec, r, rows) = input();
    let source = PgSimulationSource::import(
        db.repo.clone(),
        spec.clone(),
        rows,
        Some(ledger("complete-empty")),
    )
    .await
    .unwrap();
    source.set_active(true).await.unwrap();
    let provider = BillingProvider::new("mixed").await;
    *provider.arguments.lock().unwrap() = json!({"input_version":1,"account_ids_json":serde_json::to_string(&r.account_ids).unwrap(),"start_inclusive":r.window.start_inclusive,"end_exclusive":r.window.end_exclusive,"timezone":r.window.timezone});
    let bank = fixture::bank();
    let registration = crate::billing_report_registration_for_backend(
        bank.clone(),
        Some(crate::LocalBillingSource::for_persistent_simulation(
            source.clone(),
        )),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.llm_gateway = Some(
        safeflow_runtime::OpenAiGatewayConfig::new(
            &provider.endpoint,
            "local-billing-protocol-only",
            Zeroizing::new("isolated-protocol-key".into()),
            Duration::from_secs(5),
        )
        .unwrap(),
    );
    let state = ApiState::for_project_simulation_with_owner_and_tools(
        db.repo.clone(),
        config,
        UserId::parse("user_demo").unwrap(),
        bank,
        vec![registration],
    )
    .unwrap();
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
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    let session = server
        .state
        .billing_feedback_session_for_backend(&headers)
        .unwrap();
    server
        .state
        .initialize_simulation_source_for_backend(&session, &source)
        .await
        .unwrap();
    let host = server
        .state
        .subscription_for_backend()
        .startup_for_host(&session)
        .await
        .unwrap();
    // Real same-capture -> original selection -> original technical preparation.
    server
        .state
        .prepare_simulation_subscription_for_backend(
            &session,
            &host,
            &source,
            &r,
            0,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM subscription_runtime_records t")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    let prepared = initial(&server, &cookie, "report").await;
    let response = ok(execute(&server, &cookie, &prepared).await);
    let data = &response["results"][0]["data"];
    assert_eq!(data["source_mode"], "LOCAL_PG_CAPTURE_V1");
    assert_eq!(data["report"]["read"]["is_complete"], true);
    let list = command(
        &server,
        &cookie,
        "/api/subscriptions",
        json!({"kind":"LIST"}),
    )
    .await;
    assert_eq!(list.status, StatusCode::OK);
    assert_eq!(list.json["status"], "AVAILABLE");
    for kind in [
        "REGISTRATION",
        "REMINDER_SETTINGS",
        "RECORD_CANCELLATION",
        "REVOKE_CANCELLATION",
    ] {
        let reply = command(
            &server,
            &cookie,
            "/api/subscriptions/change",
            json!({"kind":kind}),
        )
        .await;
        assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    }
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(t)) FROM subscription_runtime_records t")
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(before, after);
    let task = response["task"]["task_id"].as_str().unwrap();
    source.set_active(false).await.unwrap();
    assert_eq!(
        ok(query(&server, &cookie, task).await)["results"],
        response["results"]
    );
    let pending = initial(&server, &cookie, "report").await;
    let blocked = execute(&server, &cookie, &pending).await;
    assert!(!blocked.status.is_success() || blocked.json["task"]["task_state"] != "COMPLETED");
    assert!(PgSimulationSource::open(db.repo.clone(), spec)
        .await
        .is_err());
    assert!(crate::billing_report_registration_for_backend(fixture::bank(), None).is_err());
    std::fs::write(
        std::path::Path::new(&std::env::var("SAFEFLOW_SIMULATION_EVIDENCE").unwrap())
            .join("http-source-response.json"),
        serde_json::to_vec_pretty(&response).unwrap(),
    )
    .unwrap();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore]
async fn reminder_native_api_uses_existing_issuer_bank_source_and_logout() {
    use safeflow_subscription_evidence::{confirmation_mock::*, reminder_mock::*};
    struct Gate;
    impl MockScheduleGate for Gate {
        fn verify_and_consume_mock(&mut self, _: &ScheduleBinding) -> MockGateOutcome {
            MockGateOutcome::Confirmed
        }
    }
    let db = Db::new().await;
    let (spec, _, rows) = input();
    let source = PgSimulationSource::import(
        db.repo.clone(),
        spec.clone(),
        rows,
        Some(ledger("already-active")),
    )
    .await
    .unwrap();
    source.set_active(true).await.unwrap();
    let state = ApiState::for_project_simulation_with_owner_and_tools(
        db.repo.clone(),
        ApiConfig::local_simulation(Zeroizing::new(SECRET.into())),
        UserId::parse("user_demo").unwrap(),
        fixture::bank(),
        vec![],
    )
    .unwrap();
    let session = state.simulation_session_for_native_host(SECRET).unwrap();
    state
        .initialize_simulation_source_for_backend(&session, &source)
        .await
        .unwrap();
    // Exact existing member daily schedule; actual DB clock performs rollover.
    let store = ProposedLedgerStoreMock::new(&db.namespace).unwrap();
    let mut c = db.scoped.acquire().await.unwrap();
    let mut work = store
        .load_work_on_connection_mock(&mut c, "user_demo")
        .await
        .unwrap();
    let context = MockOperationContext {
        user_id: "user_demo".into(),
        session_ref: "fictional-session".into(),
        task_id: "fictional-task".into(),
        node_id: "fictional-node".into(),
        operation_id: "daily-reminder".into(),
        idempotency_key: "fictional-key-daily-reminder".into(),
        plan_version: 1,
    };
    let schedule = Schedule {
        cycle: safeflow_subscription_evidence::recognition::Cycle::Day,
        anchor_date: "2026-10-13".parse().unwrap(),
        period_index: 1,
        expected_date: "2026-10-14".parse().unwrap(),
        enabled: true,
        days_before: 0,
    };
    let binding = work
        .prepare_settings_mock(
            context,
            "fictional-subscription-1",
            schedule,
            "2026-10-14T12:00:00+08:00",
            "2026-10-14T12:05:00+08:00",
        )
        .await
        .unwrap();
    work.confirm_settings_mock(&binding, "2026-10-14T12:00:00+08:00", &mut Gate)
        .await
        .unwrap();
    use sqlx::Connection;
    let mut tx = c.begin().await.unwrap();
    let pending = store.save_on_connection_mock(&mut tx, work).await.unwrap();
    tx.commit().await.unwrap();
    let mut other = db.scoped.acquire().await.unwrap();
    pending
        .read_committed_on_connection_mock(&mut other)
        .await
        .unwrap();
    drop(other);
    drop(c);
    let host = state
        .subscription_for_backend()
        .startup_for_host(&session)
        .await
        .unwrap();
    let config = safeflow_storage::SubscriptionReminderConfig {
        owner: UserId::parse("user_demo").unwrap(),
        accounts: vec![safeflow_contracts::ResourceId::parse("account_001").unwrap()],
        max_messages: 8,
        max_ledger_entries: 32,
        scan_timeout: Duration::from_secs(10),
        cycles: 2,
        interval: Duration::from_secs(1),
    };
    let runner = state
        .subscription_reminder_host_for_backend(&session, &host, config)
        .await
        .unwrap();
    let reports = runner
        .run(&safeflow_storage::SubscriptionReminderStop::default())
        .await
        .unwrap();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].simulated_count, 1);
    assert!(reports[0].result_saved);
    assert_eq!(reports[1].simulated_count, 0);
    assert!(!reports[0].notification_sent);
    let before: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    state.stop_native_simulation_session(&session).unwrap();
    assert_eq!(
        runner
            .scan_once(&safeflow_storage::SubscriptionReminderStop::default())
            .await
            .err(),
        Some(safeflow_storage::StorageError::Session)
    );
    let after: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(before, after);
    std::fs::write(
        std::path::Path::new(&std::env::var("SAFEFLOW_SIMULATION_EVIDENCE").unwrap())
            .join("native-api-reminders.json"),
        serde_json::to_vec_pretty(
            &json!({"reports":reports,"logout_blocks_old_runner":true,"journal":after}),
        )
        .unwrap(),
    )
    .unwrap();
    drop(runner);
    db.finish().await;
}
