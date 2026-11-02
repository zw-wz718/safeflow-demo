//! Full explicit customer HTTP path over the already tested public composer.
use super::*;
fn request(original: &Value) -> Value {
    let selection = intent(original);
    json!({"input_version":1,"report_id":selection.report_id,"transaction":selection.transaction,"choice":selection.choice})
}
fn action_path(prepared: &Value, action: &str) -> String {
    format!(
        "/api/billing-feedback/tasks/{}/operations/{}/{action}",
        prepared["task_id"].as_str().unwrap(),
        prepared["operation_id"].as_str().unwrap()
    )
}
fn action(prepared: &Value) -> Value {
    json!({"confirmation_ref":prepared["confirmation"]["confirmation_ref"]})
}
fn result_path(prepared: &Value) -> String {
    format!(
        "/api/billing-feedback/operations/{}",
        prepared["operation_id"].as_str().unwrap()
    )
}
async fn get(server: &Server, cookie: &str, path: &str) -> Reply {
    server
        .call(Method::GET, path, None, Some(cookie), None, None, false)
        .await
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; full loopback explicit HTTP path, no shared site wiring"]
async fn billing_feedback_http_original_report_prepare_confirm_execute_get_and_atomic_rejections() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (server, source, cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    let input = request(original);
    let before = snapshot(&db).await;
    for field in [
        "scope",
        "owner",
        "user_id",
        "report",
        "backend_evidence",
        "permit",
        "saved",
    ] {
        let mut forged = input.clone();
        forged[field] = json!({});
        assert_eq!(
            command(&server, &cookie, "/api/billing-feedback/prepare", forged)
                .await
                .status,
            StatusCode::BAD_REQUEST,
            "{field}"
        );
    }
    let mut nested = input.clone();
    nested["transaction"]["owner"] = json!("forged");
    assert_eq!(
        command(&server, &cookie, "/api/billing-feedback/prepare", nested)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/billing-feedback/prepare",
                Some(input.to_string()),
                None,
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/billing-feedback/prepare",
                Some(input.to_string()),
                Some(&cookie),
                None,
                None,
                false
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/billing-feedback/prepare",
                Some(input.to_string()),
                Some(&cookie),
                None,
                Some("http://attacker.example"),
                true
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(snapshot(&db).await, before);
    let prepared = ok(command(
        &server,
        &cookie,
        "/api/billing-feedback/prepare",
        input.clone(),
    )
    .await);
    assert_eq!(prepared["confirmation"]["status"], "PREPARED");
    assert!(
        prepared.get("task").is_none()
            && prepared.get("permit").is_none()
            && prepared.get("owner_id").is_none()
    );
    assert_eq!(
        get(&server, &cookie, &result_path(&prepared)).await.status,
        StatusCode::NOT_FOUND
    );
    let before = snapshot(&db).await;
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&prepared, "execute"),
            action(&prepared)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(
        ok(command(
            &server,
            &cookie,
            &action_path(&prepared, "confirm"),
            action(&prepared)
        )
        .await)["status"],
        "CONFIRMED"
    );
    let mut forged = action(&prepared);
    forged["permit"] = json!("forged");
    assert_eq!(
        command(&server, &cookie, &action_path(&prepared, "execute"), forged)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let fresh_cookie = server.login().await;
    assert_eq!(
        command(
            &server,
            &fresh_cookie,
            &action_path(&prepared, "execute"),
            action(&prepared)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    // Drive ALL three real transactional failure locations through HTTP.
    for table in [
        "billing_feedback_events_v1",
        "public_billing_feedback_results_v1",
        "public_audit_outbox",
    ] {
        let before = snapshot(&db).await;
        let condition = if table == "public_audit_outbox" {
            "WHEN (NEW.envelope->'public_event'->'details'->>'source'='billing-feedback-local-v1')"
        } else {
            ""
        };
        db.scoped.execute("CREATE OR REPLACE FUNCTION feedback_http_fault() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated http fault'; END $$").await.unwrap();
        db.scoped.execute(format!("CREATE TRIGGER feedback_http_fault BEFORE INSERT ON {table} FOR EACH ROW {condition} EXECUTE FUNCTION feedback_http_fault()").as_str()).await.unwrap();
        assert_eq!(
            command(
                &server,
                &cookie,
                &action_path(&prepared, "execute"),
                action(&prepared)
            )
            .await
            .status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(snapshot(&db).await, before, "{table}");
        db.scoped
            .execute(format!("DROP TRIGGER feedback_http_fault ON {table}").as_str())
            .await
            .unwrap();
    }
    let saved = ok(command(
        &server,
        &cookie,
        &action_path(&prepared, "execute"),
        action(&prepared),
    )
    .await);
    assert_eq!(saved["operation_state"], "SUCCEEDED");
    assert_eq!(saved["task_state"], "COMPLETED");
    assert_eq!(saved["lock_authorized"], false);
    assert_eq!(saved["saved"]["persisted"], true);
    let before = snapshot(&db).await;
    let queried = ok(get(&server, &cookie, &result_path(&prepared)).await);
    assert_eq!(queried["saved"], saved["saved"]);
    assert_eq!(queried["task_state"], "COMPLETED");
    let task = ok(get(
        &server,
        &cookie,
        &format!("/api/tasks/{}", prepared["task_id"].as_str().unwrap()),
    )
    .await);
    assert_eq!(task["task_state"], "COMPLETED");
    assert_eq!(task["next_action"]["kind"], "FINISHED");
    assert_eq!(task["recovery"]["kind"], "FINISHED");
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(
        get(
            &server,
            &cookie,
            &format!("{}?owner=user_demo", result_path(&prepared))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    let replayed = ok(command(
        &server,
        &cookie,
        &action_path(&prepared, "execute"),
        action(&prepared),
    )
    .await);
    assert_eq!(replayed["replayed"], true);
    assert_eq!(snapshot(&db).await, before);
    let second = ok(command(&server, &cookie, "/api/billing-feedback/prepare", input).await);
    ok(command(
        &server,
        &cookie,
        &action_path(&second, "confirm"),
        action(&second),
    )
    .await);
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&second, "execute"),
            action(&second)
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(count(&db, "billing_feedback_v1").await, 1);
    let foreign = billing_server_with_feedback(
        &db,
        &provider,
        "other_owner",
        fixture::bank(),
        None,
        false,
        true,
    )
    .await;
    let foreign_cookie = foreign.login().await;
    assert_eq!(
        get(&foreign, &foreign_cookie, &result_path(&prepared))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        command(
            &foreign,
            &foreign_cookie,
            "/api/billing-feedback/prepare",
            request(original)
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let disabled = billing_server(&db, &provider, "user_demo", fixture::bank(), None, false).await;
    assert_eq!(
        get(&disabled, &disabled.login().await, &result_path(&prepared))
            .await
            .status,
        StatusCode::NOT_IMPLEMENTED
    );
    let restarted = billing_server_with_feedback(
        &db,
        &provider,
        "user_demo",
        fixture::bank(),
        None,
        false,
        true,
    )
    .await;
    assert_eq!(
        ok(get(
            &restarted,
            &restarted.login().await,
            &result_path(&prepared)
        )
        .await)["saved"],
        saved["saved"]
    );
    assert_eq!(source.calls.load(Ordering::SeqCst), 1);
    if let Ok(directory) = std::env::var("SAFEFLOW_FEEDBACK_EVIDENCE") {
        std::fs::write(std::path::Path::new(&directory).join("http-explicit-complete.json"),serde_json::to_vec_pretty(&json!({"original_http":complete,"prepared":prepared,"saved":saved,"original_get":queried,"replay":replayed,"three_http_fault_rollbacks":true,"source_calls":1,"production_source_connected":false})).unwrap()).unwrap();
    }
    // A read failure is 503, distinct from original-result absence/inaccessibility404.
    db.repo.close().await;
    assert_eq!(
        get(&server, &cookie, &result_path(&prepared)).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    server.stop();
    foreign.stop();
    disabled.stop();
    restarted.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL; actual HTTP timeout cancels the entire waiting composer"]
async fn billing_feedback_http_timeout_requires_original_get_and_rolls_back_waiting_stage() {
    let db = Db::new().await;
    let provider = BillingProvider::new("mixed").await;
    let (original_server, _, _prior_cookie, complete) = report(&db, &provider).await;
    let original = &complete["results"][0]["data"]["report"];
    original_server.stop();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.request_timeout = Duration::from_millis(300);
    config.billing_feedback = Some(BillingFeedbackConfig::for_reviewed_backend(
        ConfirmationConfig::new(Duration::from_secs(300)).unwrap(),
        PermitConfig::new(Duration::from_secs(300)).unwrap(),
    ));
    let state = ApiState::build_with_bank(
        db.repo.clone(),
        config,
        UserId::parse("user_demo").unwrap(),
        fixture::bank(),
        &[],
    )
    .unwrap();
    let app = router(state.clone());
    let handle = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let server = Server {
        state,
        address,
        handle,
    };
    let cookie = server.login().await;
    let prepared = ok(command(
        &server,
        &cookie,
        "/api/billing-feedback/prepare",
        request(original),
    )
    .await);
    ok(command(
        &server,
        &cookie,
        &action_path(&prepared, "confirm"),
        action(&prepared),
    )
    .await);
    let before = snapshot(&db).await;
    let mut blocker = install_wait(&db).await;
    let path = action_path(&prepared, "execute");
    let mut running = Box::pin(command(&server, &cookie, &path, action(&prepared)));
    tokio::select! { _=&mut running => panic!("HTTP ended before actual member wait"), _=wait_for_stage(&db) => {} }
    let reply = running.await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_eq!(reply.json["error"]["reason_code"], "RECONCILE_REQUIRED");
    sqlx::query("SELECT pg_advisory_unlock(8021021)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    drop(blocker);
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(prepared["task_id"].as_str().unwrap())
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(
        get(&server, &cookie, &result_path(&prepared)).await.status,
        StatusCode::NOT_FOUND
    );
    // No retry/prepare/execute is performed by the HTTP timeout or GET handler.
    assert_eq!(count(&db, "billing_feedback_v1").await, 0);
    server.stop();
    db.finish().await;
}
