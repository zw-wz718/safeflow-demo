//! Real loopback HTTP/PG; original member owner is never rewritten.
use super::*;
use safeflow_contracts::ContractFuture;
use safeflow_subscription_evidence::managed_ledger_mock::LedgerFixture;
struct Source;
impl SubscriptionInitialSource for Source {
    fn load<'a>(&'a self, _: &'a str) -> ContractFuture<'a, Result<LedgerFixture, StorageError>> {
        Box::pin(async {
            let v: Value =
                serde_json::from_str(include_str!(env!("SAFEFLOW_SUBSCRIPTION_LEDGER_FIXTURE")))
                    .unwrap();
            Ok(serde_json::from_value(
                v["cases"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|c| c["case_id"] == "already-active")
                    .unwrap()["ledger"]
                    .clone(),
            )
            .unwrap())
        })
    }
}
async fn post(server: &Server, path: &str, input: Value, cookie: Option<&str>) -> Reply {
    server
        .call(
            Method::POST,
            path,
            Some(input.to_string()),
            cookie,
            None,
            Some(&server.state.0.frontend_origin),
            true,
        )
        .await
}
async fn server(db: &Db) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    if let Ok(origin) = std::env::var("SAFEFLOW_SUBSCRIPTION_BROWSER_ORIGIN") {
        config.frontend_origin = origin;
    }
    let state =
        ApiState::build(db.repo.clone(), config, UserId::parse("user_demo").unwrap()).unwrap();
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
// Isolated read fixture ONLY: a genuine member preparation and its exact
// original request. No confirmation/gate, successful receipt or domain event.
async fn seed_unconfirmed_original(db: &Db, reference: &SessionRef) -> String {
    use safeflow_subscription_evidence::confirmation_mock::*;
    use sqlx::Connection;
    let store = ProposedLedgerStoreMock::new(&db.namespace).unwrap();
    let mut writer = db.scoped.acquire().await.unwrap();
    let mut tx = writer.begin().await.unwrap();
    let mut work = store
        .load_work_on_connection_mock(&mut tx, "user_demo")
        .await
        .unwrap();
    let operation = format!("operation_{}", uuid::Uuid::new_v4());
    let time = chrono::DateTime::<chrono::Utc>::from(std::time::SystemTime::now());
    let preparation = work
        .prepare_subscription_change_mock(
            SubscriptionChangeRequestMock::CreateCancellationIntent {
                subscription_id: "fictional-subscription-1".into(),
            },
            MockOperationContext {
                user_id: "user_demo".into(),
                session_ref: reference.to_string(),
                task_id: format!("task_{}", uuid::Uuid::new_v4()),
                node_id: "save_subscription".into(),
                operation_id: operation.clone(),
                idempotency_key: operation.clone(),
                plan_version: 1,
            },
            &time.to_rfc3339(),
            &(time + chrono::Duration::seconds(60)).to_rfc3339(),
        )
        .await
        .unwrap();
    let original = preparation.original_request_mock().unwrap();
    let binding = match preparation.confirmation_binding_mock() {
        SubscriptionChangeBindingMock::Intent(v) => json!(v),
        _ => panic!("intent"),
    };
    let pending = store.save_on_connection_mock(&mut tx, work).await.unwrap();
    let business_ref = uuid::Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO subscription_public_bindings_v1(business_ref,owner_id,operation_id,idempotency_key,session_digest,binding_version,writer_generation,member_binding,original_request,public_parameters) VALUES($1,'user_demo',$2,$2,'isolated-read-fixture',1,1,$3,$4,'{}')")
        .bind(&business_ref).bind(operation).bind(binding).bind(json!(original)).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let mut reader = db.scoped.acquire().await.unwrap();
    pending
        .read_committed_on_connection_mock(&mut reader)
        .await
        .unwrap();
    business_ref
}
#[tokio::test]
#[ignore]
async fn subscription_actual_http_owner_projection_and_a2_no_mutations() {
    let db = Db::new().await;
    let server = server(&db).await;
    let login = post(
        &server,
        "/api/sim/session",
        json!({"login_secret":SECRET}),
        None,
    )
    .await;
    assert_eq!(login.status, StatusCode::OK);
    let raw = Cookie::parse(login.headers[header::SET_COOKIE].to_str().unwrap()).unwrap();
    let cookie = format!("{}={}", raw.name(), raw.value());
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    let reference = server
        .state
        .billing_feedback_session_for_backend(&headers)
        .unwrap();
    let source = server
        .state
        .subscription_for_backend()
        .with_initial_source(Arc::new(Source));
    source.initialize_for_host(&reference).await.unwrap();
    let original_ref = seed_unconfirmed_original(&db, &reference).await;
    let before: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(
        post(&server, "/api/subscriptions", json!({"kind":"LIST"}), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let replay_cookie = format!("{}={}", COOKIE_NAME, reference);
    assert_eq!(
        post(
            &server,
            "/api/subscriptions",
            json!({"kind":"LIST"}),
            Some(&replay_cookie)
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    let csrf = server
        .call(
            Method::POST,
            "/api/subscriptions",
            Some(json!({"kind":"LIST"}).to_string()),
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(csrf.status, StatusCode::FORBIDDEN);
    for input in [
        json!({"kind":"LIST","owner":"other"}),
        json!({"kind":"LIST","snapshot":{}}),
        json!({"kind":"ORIGINAL","original_ref":"not-an-id"}),
        json!({"kind":"CREATE"}),
    ] {
        let label = input.to_string();
        assert_eq!(
            post(&server, "/api/subscriptions", input, Some(&cookie))
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{label}"
        );
    }
    let view = post(
        &server,
        "/api/subscriptions",
        json!({"kind":"LIST"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(view.status, StatusCode::OK);
    assert_eq!(view.json["status"], "AVAILABLE");
    assert_eq!(view.json["items"].as_array().unwrap().len(), 1);
    assert_eq!(view.json["originals"][0]["original_ref"], original_ref);
    let recovered = post(
        &server,
        "/api/subscriptions",
        json!({"kind":"ORIGINAL","original_ref":original_ref}),
        Some(&cookie),
    )
    .await;
    assert_eq!(recovered.status, StatusCode::OK);
    assert_eq!(recovered.json["status"], "NEEDS_REVIEW");
    assert_eq!(recovered.json.as_object().unwrap().len(), 3);
    let selection = view.json["items"][0]["selection_ref"].as_str().unwrap();
    let detail = post(
        &server,
        "/api/subscriptions",
        json!({"kind":"DETAILS","selection_ref":selection}),
        Some(&cookie),
    )
    .await;
    assert_eq!(detail.status, StatusCode::OK);
    assert_eq!(detail.json["cancellation"]["history"], json!([]));
    assert_eq!(detail.json["reminder"]["notification_sent"], false);
    for input in [
        json!({"kind":"DETAILS","subscription_id":"fictional-subscription-1"}),
        json!({"kind":"DETAILS","selection_ref":selection,"owner":"user_demo"}),
        json!({"kind":"DETAILS","selection_ref":selection,"now":"2026-01-01"}),
        json!({"kind":"ORIGINAL","original_ref":uuid::Uuid::new_v4().to_string(),"original_binding":{}}),
    ] {
        assert_eq!(
            post(&server, "/api/subscriptions", input, Some(&cookie))
                .await
                .status,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        post(
            &server,
            "/api/subscriptions",
            json!({"kind":"DETAILS","selection_ref":uuid::Uuid::new_v4().to_string()}),
            Some(&cookie)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        post(
            &server,
            "/api/subscriptions",
            json!({"kind":"ORIGINAL","original_ref":uuid::Uuid::new_v4().to_string()}),
            Some(&cookie)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let _ = post(
        &server,
        "/api/subscriptions",
        json!({"kind":"LIST"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(
        post(
            &server,
            "/api/subscriptions",
            json!({"kind":"DETAILS","selection_ref":selection}),
            Some(&cookie)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    for private in [
        "user_demo",
        "fictional-subscription-1",
        "session_ref",
        "journal",
        "raw_receipt",
        "original_request",
    ] {
        assert!(!view.json.to_string().contains(private));
    }
    for kind in [
        "RECORD_CANCELLATION",
        "REVOKE_CANCELLATION",
        "REMINDER_SETTINGS",
        "REGISTRATION",
    ] {
        assert_eq!(
            post(
                &server,
                "/api/subscriptions/change",
                json!({"kind":kind}),
                Some(&cookie)
            )
            .await
            .status,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    let after: Value = sqlx::query_scalar(
        "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(before, after);
    for table in [
        "subscription_public_bindings_v1",
        "tasks",
        "operations",
        "execution_permits",
        "audit_events",
        "public_audit_outbox",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&db.scoped)
            .await
            .unwrap();
        assert_eq!(
            count,
            if table == "subscription_public_bindings_v1" {
                1
            } else {
                0
            }
        );
    }
    let other = Server::new(
        &db,
        "another_user",
        Duration::from_secs(30),
        Duration::from_secs(60),
    )
    .await;
    let other_cookie = other.login().await;
    assert_eq!(
        post(
            &other,
            "/api/subscriptions",
            json!({"kind":"LIST"}),
            Some(&other_cookie)
        )
        .await
        .json["status"],
        "NO_LEDGER"
    );
    other.handle.abort();
    if let Ok(root) = std::env::var("SAFEFLOW_SUBSCRIPTION_BROWSER_HOLD") {
        let root = std::path::PathBuf::from(root);
        assert!(root.is_absolute());
        let state = root.join("browser-server.json");
        assert!(!state.exists());
        std::fs::write(state,serde_json::to_vec_pretty(&json!({"api":format!("http://{}",server.address),"origin":server.state.0.frontend_origin,"fixture_owner":"user_demo","actual_http_pg":true})).unwrap()).unwrap();
        let stop = root.join("browser-stop");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        while !stop.exists() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        assert!(stop.exists(), "browser harness deadline reached");
        let after: Value = sqlx::query_scalar(
            "SELECT journal FROM subscription_runtime_records WHERE owner_id='user_demo'",
        )
        .fetch_one(&db.scoped)
        .await
        .unwrap();
        assert_eq!(before, after);
    }
    let out = server
        .call(
            Method::DELETE,
            "/api/sim/session",
            Some("{}".into()),
            Some(&cookie),
            None,
            Some(&server.state.0.frontend_origin),
            true,
        )
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert_eq!(
        post(
            &server,
            "/api/subscriptions",
            json!({"kind":"LIST"}),
            Some(&cookie)
        )
        .await
        .status,
        StatusCode::UNAUTHORIZED
    );
    server.handle.abort();
    drop(source);
    drop(server);
    db.finish().await;
}
