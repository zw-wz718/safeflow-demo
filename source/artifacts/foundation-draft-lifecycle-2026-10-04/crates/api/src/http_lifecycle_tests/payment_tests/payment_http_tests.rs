//! Actual loopback HTTP + PG. Catalog is isolated mechanism data only.
use super::*;

async fn http_server(
    db: &Db,
    source: Option<CardCatalogSourceBinding>,
    behavior: MockBehavior,
    level: SecurityLevel,
    owner: &str,
    draft_enabled: bool,
    payment_enabled: bool,
    timeout: Duration,
    short_confirmation: bool,
) -> Server {
    let browser = std::env::var("SAFEFLOW_PAYMENT_BROWSER").as_deref() == Ok("1");
    let listener = tokio::net::TcpListener::bind(if browser {
        "127.0.0.1:8787"
    } else {
        "127.0.0.1:0"
    })
    .await
    .unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    if browser {
        config.frontend_origin = "http://127.0.0.1:5185".into();
    }
    config.request_timeout = timeout;
    config.agent_fixed_plan_enabled = true;
    if draft_enabled {
        config.draft_confirmation = Some(
            DraftConfirmationConfig::new(
                if short_confirmation {
                    Duration::from_millis(800)
                } else {
                    Duration::from_secs(300)
                },
                Duration::from_secs(300),
                600,
                "http-payment-isolated",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        );
    }
    if payment_enabled {
        config.payment_execution = Some(
            PaymentExecutionConfig::for_reviewed_project_simulation(
                source,
                payment_profile(level),
                PermitConfig::new(Duration::from_secs(30)).unwrap(),
                behavior,
            )
            .unwrap(),
        );
    }
    let state = ApiState::build_with_bank(
        db.repo.clone(),
        config,
        UserId::parse(owner).unwrap(),
        Arc::new(safeflow_mock_bank::MockBankGateway::new()),
        &[],
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
#[ignore = "manual isolated browser harness; never shared source adoption"]
async fn payment_browser_isolated_harness() {
    assert_eq!(
        std::env::var("SAFEFLOW_PAYMENT_BROWSER").as_deref(),
        Ok("1")
    );
    let db = Db::new().await;
    let provider = catalog();
    let binding = source(provider);
    let server = http_server(
        &db,
        Some(binding.clone()),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
        "user_001",
        true,
        true,
        Duration::from_secs(30),
        false,
    )
    .await;
    provision_payer(&db).await;
    let mut tx = db.scoped.begin().await.unwrap();
    FundsStore::provision_account(
        &mut tx,
        &UserId::parse("composition_recipient_owner").unwrap(),
        &ResourceId::parse("composition_recipient").unwrap(),
        &safeflow_bank_gateway::DecimalAmount::parse("0.00").unwrap(),
    )
    .await
    .unwrap();
    FundsStore::provision_payee(
        &mut tx,
        &UserId::parse("user_001").unwrap(),
        &ResourceId::parse("payee_008").unwrap(),
        &ResourceId::parse("composition_recipient").unwrap(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let reply = server
        .call(
            Method::POST,
            "/api/sim/session",
            Some(json!({"login_secret":SECRET}).to_string()),
            None,
            None,
            Some("http://127.0.0.1:5185"),
            true,
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let cookie = reply.headers[header::SET_COOKIE].to_str().unwrap();
    CardCatalogService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
        Some(binding),
    )
    .initialize(&session(cookie), &ResourceId::parse("account_001").unwrap())
    .await
    .unwrap();
    println!("ISOLATED browser harness ready at http://127.0.0.1:8787; NOT shared demo adoption; schema {}",db.namespace);
    let stop = std::path::Path::new(
        "E:/银行agent/artifacts/payment-http-web-connection-2026-10-20/browser.stop",
    );
    let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
    while !stop.exists() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("Isolated final execution counts: {:?}", counts(&db).await);
    server.stop();
    db.finish().await;
}
fn catalog() -> Arc<Catalog> {
    Arc::new(Catalog {
        pause_at: AtomicU64::new(0),
        revision_at: AtomicU64::new(0),
        revoke_at: AtomicU64::new(0),
        fail: AtomicBool::new(false),
        calls: AtomicU64::new(0),
        fail_at: AtomicU64::new(0),
        pause: AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}
fn command_body(p: &Value) -> Value {
    json!({"contract_version":1,"scope":"PAYMENT_EXECUTION_ONLY","confirmation_ref":p["confirmation_ref"],"display_ticket":p["display_ticket"]})
}
fn result_path(p: &Value) -> String {
    format!("{}/funds-result", path(p))
}
fn execute_path(p: &Value) -> String {
    format!("{}/execute", path(p))
}

#[tokio::test]
#[ignore = "isolated PG and loopback HTTP"]
async fn payment_http_explicit_100_concurrent_replay_safe_result_and_disabled_restart_history() {
    let db = Db::new().await;
    let provider = catalog();
    let server = http_server(
        &db,
        Some(source(provider)),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
        "user_001",
        true,
        true,
        Duration::from_secs(30),
        false,
    )
    .await;
    let cookie = server.login().await;
    let (p, _, _) = setup_amount(&db, &server, &cookie, "100.00").await;
    let before = counts(&db).await;
    assert_eq!(before, (0, 0, 0, 0, 0));
    let status = ok(query(&server, &cookie, "/api/agent/status").await);
    assert_eq!(status["funds_execution_enabled"], true);
    let view = ok(query(&server, &cookie, &path(&p)).await);
    assert_eq!(view["execution"]["enabled"], true);
    assert_eq!(view["funds_execution_enabled"], true);
    assert_eq!(view["execution_conditions"], json!([]));
    let snapshot = payment_execution_tests::persistent_state(&db).await;
    let original = ok(query(&server, &cookie, &result_path(&p)).await);
    assert_eq!(original["state"], "UNSUBMITTED");
    assert_eq!(
        snapshot,
        payment_execution_tests::persistent_state(&db).await
    );
    // Reconfirm/evaluate are observations, never funds commands.
    let evaluated = ok(command(
        &server,
        &cookie,
        &format!("{}/evaluate", path(&p)),
        decision(&p, "CONFIRM"),
    )
    .await);
    assert_eq!(evaluated["confirmation_status"], "CONFIRMED");
    assert_eq!(counts(&db).await, before);
    let endpoint = execute_path(&p);
    let (a, b) = tokio::join!(
        command(&server, &cookie, &endpoint, command_body(&p)),
        command(&server, &cookie, &endpoint, command_body(&p))
    );
    let a = ok(a);
    let b = ok(b);
    assert_eq!(a, b);
    assert_eq!(a["state"], "SUCCEEDED");
    assert_eq!(a["transfer"]["amount"], "100.00");
    assert_eq!(a["transfer"], p["snapshot"]["transfer"]);
    assert_eq!(a.as_object().unwrap().len(), 10);
    for forbidden in [
        "idempotency_key",
        "fingerprint",
        "permit",
        "authority",
        "business",
        "source",
        "binding",
        "operation",
    ] {
        assert!(a.get(forbidden).is_none());
    }
    assert_eq!(counts(&db).await, (1, 1, 1, 2, 1));
    assert_eq!(
        ok(command(&server, &cookie, &endpoint, command_body(&p)).await),
        a
    );
    let fixed = payment_execution_tests::persistent_state(&db).await;
    assert_eq!(ok(query(&server, &cookie, &result_path(&p)).await), a);
    assert_eq!(fixed, payment_execution_tests::persistent_state(&db).await);
    server.stop();
    // Service rebuild: no source, no final config, and NO draft runtime.
    let rebuilt = http_server(
        &db,
        None,
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
        "user_001",
        false,
        false,
        Duration::from_secs(30),
        false,
    )
    .await;
    let cookie = rebuilt.login().await;
    assert_eq!(ok(query(&rebuilt, &cookie, &result_path(&p)).await), a);
    assert_eq!(
        ok(query(&rebuilt, &cookie, &format!("{}/original-result", path(&p))).await)["state"],
        "SUCCEEDED"
    );
    assert_eq!(fixed, payment_execution_tests::persistent_state(&db).await);
    rebuilt.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PG and loopback HTTP"]
async fn payment_http_strict_body_perimeter_identity_and_denials_have_zero_writes() {
    for mode in [
        "strict",
        "missing",
        "stale_source",
        "red",
        "revoked",
        "expired",
        "foreign",
        "changed",
        "unconfirmed",
    ] {
        let db = Db::new().await;
        let provider = catalog();
        let server = http_server(
            &db,
            if mode == "missing" {
                None
            } else {
                Some(source(provider.clone()))
            },
            MockBehavior::Immediate,
            if mode == "red" {
                SecurityLevel::Red
            } else {
                SecurityLevel::Yellow
            },
            "user_001",
            true,
            true,
            Duration::from_secs(30),
            false,
        )
        .await;
        let cookie = server.login().await;
        let (mut p, _, _) = setup_amount(&db, &server, &cookie, "100.00").await;
        if mode == "unconfirmed" {
            let d = draft(&server, &cookie).await;
            p = prepared(&server, &cookie, &d).await;
        }
        if mode == "stale_source" {
            provider.fail.store(true, Ordering::SeqCst);
        }
        if mode == "revoked" {
            server.state.0.sessions.revoke(&session(&cookie)).unwrap();
        }
        if mode == "expired" {
            // Expire only this payment confirmation after normal setup. A short
            // draft TTL expires the earlier draft before this test reaches the
            // payment execution rejection that it is meant to check.
            let confirmation = p["confirmation_ref"].as_str().unwrap();
            let changed = sqlx::query("WITH test_clock AS (SELECT clock_timestamp() AS now) UPDATE server_confirmations SET prepared_at=test_clock.now-interval '10 seconds',confirmed_at=test_clock.now-interval '9 seconds',expires_at=test_clock.now-interval '1 second' FROM test_clock WHERE confirmation_ref=$1")
                .bind(confirmation)
                .execute(&db.scoped)
                .await
                .unwrap();
            assert_eq!(changed.rows_affected(), 1);
            let expired: bool = sqlx::query_scalar("SELECT status='CONFIRMED' AND prepared_at <= confirmed_at AND confirmed_at < expires_at AND expires_at < clock_timestamp() FROM server_confirmations WHERE confirmation_ref=$1")
                .bind(confirmation)
                .fetch_one(&db.scoped)
                .await
                .unwrap();
            assert!(expired, "the original isolated payment confirmation is expired");
        }

        let mut body = command_body(&p);
        if mode == "changed" {
            body["display_ticket"] = json!("display_wrong");
        }
        let before = payment_execution_tests::persistent_state(&db).await;
        let endpoint = execute_path(&p);
        if mode == "strict" {
            for raw in [format!("{{\"contract_version\":1,\"contract_version\":1,\"scope\":\"PAYMENT_EXECUTION_ONLY\",\"confirmation_ref\":{},\"display_ticket\":{}}}",p["confirmation_ref"],p["display_ticket"]),
        {let mut v=body.clone();v["amount"]=json!("1.00");v.to_string()},
        {let mut v=body.clone();v["scope"]=json!("PAYMENT_INTENT_ONLY");v.to_string()},
        {let mut v=body.clone();v["contract_version"]=json!(2);v.to_string()},"x".repeat(9000)] {
        assert!(!server.call(Method::POST,&endpoint,Some(raw),Some(&cookie),None,None,true).await.status.is_success());
      }
            assert_eq!(
                server
                    .call(
                        Method::POST,
                        &endpoint,
                        Some(body.to_string()),
                        Some(&cookie),
                        None,
                        Some("http://attacker.example"),
                        true
                    )
                    .await
                    .status,
                StatusCode::FORBIDDEN
            );
            assert_eq!(
                server
                    .call(
                        Method::POST,
                        &endpoint,
                        Some(body.to_string()),
                        Some(&cookie),
                        None,
                        None,
                        false
                    )
                    .await
                    .status,
                StatusCode::FORBIDDEN
            );
            assert!(!server
                .call(
                    Method::GET,
                    &endpoint,
                    None,
                    Some(&cookie),
                    None,
                    None,
                    false
                )
                .await
                .status
                .is_success());
            assert!(!server
                .call(
                    Method::POST,
                    &format!("{endpoint}?amount=1"),
                    Some(body.to_string()),
                    Some(&cookie),
                    None,
                    None,
                    true
                )
                .await
                .status
                .is_success());
        } else if mode == "foreign" {
            let other = http_server(
                &db,
                None,
                MockBehavior::Immediate,
                SecurityLevel::Yellow,
                "foreign_owner",
                true,
                true,
                Duration::from_secs(30),
                false,
            )
            .await;
            let foreign = other.login().await;
            assert!(!command(&other, &foreign, &endpoint, body)
                .await
                .status
                .is_success());
            assert!(!query(&other, &foreign, &result_path(&p))
                .await
                .status
                .is_success());
            other.stop();
        } else {
            assert!(
                !command(&server, &cookie, &endpoint, body)
                    .await
                    .status
                    .is_success(),
                "{mode}"
            );
        }
        assert_eq!(
            before,
            payment_execution_tests::persistent_state(&db).await,
            "{mode}"
        );
        assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PG and loopback HTTP"]
async fn payment_http_pending_decline_lost_reply_and_deadline_recovery() {
    for behavior in [
        MockBehavior::Pending,
        MockBehavior::Decline,
        MockBehavior::LostReplyAfterCommit,
    ] {
        let db = Db::new().await;
        let provider = catalog();
        let server = http_server(
            &db,
            Some(source(provider)),
            behavior,
            SecurityLevel::Yellow,
            "user_001",
            true,
            true,
            Duration::from_secs(30),
            false,
        )
        .await;
        let cookie = server.login().await;
        let (p, _, _) = setup_amount(&db, &server, &cookie, "100.00").await;
        let result = ok(command(&server, &cookie, &execute_path(&p), command_body(&p)).await);
        let expected = match behavior {
            MockBehavior::Pending => "PROCESSING",
            MockBehavior::Decline => "FAILED",
            _ => "SUCCEEDED",
        };
        assert_eq!(result["state"], expected);
        assert_eq!(ok(query(&server, &cookie, &result_path(&p)).await), result);
        assert_eq!(
            ok(command(&server, &cookie, &execute_path(&p), command_body(&p)).await),
            result
        );
        assert_eq!(counts(&db).await.0, 1);
        server.stop();
        db.finish().await;
    }
    let db = Db::new().await;
    let provider = catalog();
    let server = http_server(
        &db,
        Some(source(provider.clone())),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
        "user_001",
        true,
        true,
        Duration::from_secs(1),
        false,
    )
    .await;
    let cookie = server.login().await;
    let (p, _, _) = setup_amount(&db, &server, &cookie, "100.00").await;
    provider.pause.store(true, Ordering::SeqCst);
    let before = payment_execution_tests::persistent_state(&db).await;
    let response = command(&server, &cookie, &execute_path(&p), command_body(&p)).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        ok(query(&server, &cookie, &result_path(&p)).await)["state"],
        "UNSUBMITTED"
    );
    assert_eq!(before, payment_execution_tests::persistent_state(&db).await);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PG; enabled Agent continuation must remain read-only"]
async fn payment_http_enabled_agent_confirm_evaluate_continue_get_never_execute_funds() {
    let db = Db::new().await;
    let server = http_server(
        &db,
        Some(source(catalog())),
        MockBehavior::Immediate,
        SecurityLevel::Yellow,
        "user_001",
        true,
        true,
        Duration::from_secs(30),
        false,
    )
    .await;
    let cookie = server.login().await;
    setup_amount(&db, &server, &cookie, "100.00").await;
    let initial=ok(command(&server,&cookie,"/api/agent/samples",json!({"sample":"CREATE_200","selection":{"account_id":"account_001","payee_query":"小李","payee_id":"payee_008"}})).await);
    let d = run(&server, &cookie, &initial["draft"]).await.1;
    let view=ok(command(&server,&cookie,"/api/agent/samples",json!({"sample":"EXECUTE_TRANSFER","context":{"task_id":d["task_id"],"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"],"selection_ticket":d["selection_ticket"]}})).await);
    let p = decided(&server, &cookie, &view["payment"], "CONFIRM").await;
    ok(command(
        &server,
        &cookie,
        &format!("{}/evaluate", path(&p)),
        decision(&p, "CONFIRM"),
    )
    .await);
    let endpoint = format!("/api/agent/tasks/{}", p["task_id"].as_str().unwrap());
    let before = payment_execution_tests::persistent_state(&db).await;
    let mut body = json!({"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"]});
    let waiting = ok(command(
        &server,
        &cookie,
        &format!("{endpoint}/execute"),
        body.clone(),
    )
    .await);
    assert_eq!(
        waiting["payment_flow"]["status"],
        "WAITING_EXPLICIT_PAYMENT"
    );
    body["waiting_node_id"] = waiting["payment_flow"]["waiting_node_id"].clone();
    assert_eq!(
        ok(command(&server, &cookie, &format!("{endpoint}/continue"), body).await)["payment_flow"]
            ["status"],
        "WAITING_EXPLICIT_PAYMENT"
    );
    ok(query(&server, &cookie, &endpoint).await);
    assert_eq!(before, payment_execution_tests::persistent_state(&db).await);
    assert_eq!(counts(&db).await, (0, 0, 0, 0, 0));
    let paid = ok(command(&server, &cookie, &execute_path(&p), command_body(&p)).await);
    let recovered = ok(query(&server, &cookie, &endpoint).await);
    assert_eq!(recovered["payment_flow"]["status"], "QUERY_ORIGINAL");
    assert_eq!(recovered["payment_flow"]["funds_result"], paid);
    assert_eq!(paid["state"], "SUCCEEDED");
    server.stop();
    db.finish().await;
}
