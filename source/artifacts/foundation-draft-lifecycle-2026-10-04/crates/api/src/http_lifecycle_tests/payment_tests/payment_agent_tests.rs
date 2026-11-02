//! Actual fixed-plan Agent -> existing persistent payment services. No funds authority.
use super::*;
use payment_execution_tests::{persistent_state, submit_isolated};
use safeflow_transfer_integration::mock_execution::MockBehavior;

async fn agent_server(db: &Db, ttl: u64) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.bind = address;
    config.agent_fixed_plan_enabled = true;
    config.request_timeout = Duration::from_secs(60);
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(ttl),
            Duration::from_secs(300),
            600,
            "payment-agent-dispatch-test",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    let state = ApiState::build_with_bank(
        db.repo.clone(),
        config,
        UserId::parse("user_001").unwrap(),
        Arc::new(safeflow_mock_bank::MockBankGateway::with_same_name_payees()),
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
fn sample(d: Option<&Value>) -> Value {
    match d {
        Some(d) => json!({"sample":"EXECUTE_TRANSFER","context":{
            "task_id":d["task_id"],"task_revision":d["task_revision"],"plan_version":d["plan_version"],
            "facts_version":d["facts_version"],"selection_ticket":d["selection_ticket"]}}),
        None => {
            json!({"sample":"CREATE_200","selection":{"account_id":"account_001","payee_query":"小李","payee_id":"payee_008"}})
        }
    }
}
async fn agent_draft(s: &Server, c: &str) -> Value {
    let initial = ok(command(s, c, "/api/agent/samples", sample(None)).await);
    run(s, c, &initial["draft"]).await.1
}
async fn intent(s: &Server, c: &str, d: &Value) -> Value {
    ok(command(s, c, "/api/agent/samples", sample(Some(d))).await)
}
fn agent_path(d: &Value) -> String {
    format!("/api/agent/tasks/{}", d["task_id"].as_str().unwrap())
}
fn continuation(d: &Value) -> Value {
    json!({"task_revision":d["task_revision"],"plan_version":d["plan_version"],"facts_version":d["facts_version"],"waiting_node_id":"payment"})
}
async fn all_state(db: &Db) -> Vec<Value> {
    let mut state = persistent_state(db).await;
    for table in [
        "payment_agent_bindings",
        "payment_business_observations",
        "execution_permits",
    ] {
        state.push(sqlx::query_scalar(&format!("SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text),'[]') FROM {table} t")).fetch_one(&db.scoped).await.unwrap());
    }
    state
}
async fn no_execution(db: &Db) {
    let counts: (i64,i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM public_execution_requests),(SELECT count(*) FROM execution_permits),(SELECT count(*) FROM transfer_business_orders_v1),(SELECT count(*) FROM transfer_fund_orders_v1),(SELECT count(*) FROM transfer_ledger_v1)")
        .fetch_one(&db.scoped).await.unwrap();
    assert_eq!(counts, (0, 0, 0, 0, 0));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual Agent/payment HTTP"]
async fn payment_agent_http_dispatch_full_snapshot_independent_confirmation_replay_and_wait() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let s = agent_server(&db, 300).await;
    let c = s.login().await;
    let d = agent_draft(&s, &c).await;
    let (a, b) = tokio::join!(
        command(&s, &c, "/api/agent/samples", sample(Some(&d))),
        command(&s, &c, "/api/agent/samples", sample(Some(&d)))
    );
    let a = ok(a);
    let b = ok(b);
    let p = &a["payment"];
    assert_eq!(a["kind"], "PAYMENT");
    assert_eq!(a["payment_flow"]["status"], "WAITING_PAYMENT_CONFIRMATION");
    assert_eq!(p["operation"], b["payment"]["operation"]);
    assert_eq!(p["snapshot"]["transfer"]["amount"], "200.00");
    assert_eq!(p["snapshot"]["transfer"]["payee_id"], "payee_008");
    assert_eq!(
        p["snapshot"]["transfer"]["operation_id"],
        d["receipt"]["business_operation_id"]
    );
    let old_confirmation: String = sqlx::query_scalar(
        "SELECT confirmation_ref FROM server_confirmations WHERE operation_id=$1",
    )
    .bind(d["operation_id"].as_str().unwrap())
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_ne!(p["confirmation_ref"], old_confirmation);
    assert_eq!(
        p["agent_binding"]["locator"],
        json!({"draft_id":d["receipt"]["draft_id"],"draft_version":d["receipt"]["draft_version"]})
    );
    assert_eq!(
        p["agent_binding"]["reviewed_plan"]["plan_version"]
            .as_u64()
            .unwrap(),
        d["plan_version"].as_u64().unwrap() + 1
    );
    assert_eq!(p["agent_binding"]["snapshot_digest"], p["snapshot_digest"]);
    assert_eq!(prepared(&s, &c, &d).await["operation"], p["operation"]);
    let mut forged = decision(p, "CONFIRM");
    forged["confirmation_ref"] = json!(old_confirmation);
    assert_eq!(
        command(&s, &c, &format!("{}/confirm", path(p)), forged)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let confirmed = decided(&s, &c, p, "CONFIRM").await;
    assert_eq!(confirmed["security_decisions"].as_array().unwrap().len(), 1);
    assert_eq!(
        confirmed["business_preflight"]["observations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(confirmed["funds_execution_enabled"], false);
    let before = all_state(&db).await;
    if let Ok(directory) = std::env::var("SAFEFLOW_WEB_RESPONSE_EVIDENCE") {
        let waiting = ok(query(&s, &c, &agent_path(&d)).await);
        let original = ok(query(&s, &c, p["original_result_path"].as_str().unwrap()).await);
        std::fs::write(
            std::path::Path::new(&directory).join("payment.json"),
            serde_json::to_vec_pretty(&json!({"prepared":a,"waiting":waiting,"original":original}))
                .unwrap(),
        )
        .unwrap();
    }
    for _ in 0..5 {
        let waiting = ok(command(
            &s,
            &c,
            &format!("{}/continue", agent_path(&d)),
            continuation(&d),
        )
        .await);
        assert_eq!(
            waiting["payment_flow"]["status"],
            "WAITING_EXECUTION_CONDITIONS"
        );
        assert_eq!(waiting["payment_flow"]["automatic_retry"], false);
        assert_eq!(
            waiting["payment"]["confirmation_ref"],
            p["confirmation_ref"]
        );
        assert_eq!(
            ok(query(&s, &c, &agent_path(&d)).await)["payment"],
            waiting["payment"]
        );
    }
    assert_eq!(all_state(&db).await, before);
    assert_eq!(payment_count(&db).await, 1);
    no_execution(&db).await;
    eprintln!("Agent dispatch -> independent payment confirmation -> actual minimum/preflight -> safe wait: {a}");
    s.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual member lifecycle changes"]
async fn payment_agent_http_amount_payee_versions_and_cancel_invalidate_old_bindings() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let s = agent_server(&db, 300).await;
    let c = s.login().await;
    let d = agent_draft(&s, &c).await;
    let first = intent(&s, &c, &d).await;
    decided(&s, &c, &first["payment"], "CONFIRM").await;
    let selected=select(&s,&c,&d,"MODIFY_MOCK_DRAFT",json!({"draft_id":d["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00","remark":"  payment note  "}})).await;
    let changed = run(&s, &c, &selected).await.1;
    assert_ne!(
        command(&s, &c, "/api/agent/samples", sample(Some(&d)))
            .await
            .status,
        StatusCode::OK
    );
    let second = intent(&s, &c, &changed).await;
    assert_eq!(
        second["payment"]["snapshot"]["transfer"]["amount"],
        "300.00"
    );
    assert_ne!(
        second["payment"]["confirmation_ref"],
        first["payment"]["confirmation_ref"]
    );
    assert_eq!(
        ok(query(&s, &c, &path(&first["payment"])).await)["invalid_reason"],
        "SOURCE_CHANGED"
    );
    let selected=select(&s,&c,&changed,"MODIFY_MOCK_DRAFT",json!({"draft_id":changed["receipt"]["draft_id"],"expected_version":2,"changes":{"payee_id":"payee_009"}})).await;
    let changed = run(&s, &c, &selected).await.1;
    let third = intent(&s, &c, &changed).await;
    assert_eq!(
        third["payment"]["snapshot"]["transfer"]["remark"],
        "  payment note  "
    );
    assert_eq!(
        third["payment"]["snapshot"]["transfer"]["payee_id"],
        "payee_009"
    );
    assert_eq!(
        third["payment"]["snapshot"]["transfer"]["operation_id"],
        first["payment"]["snapshot"]["transfer"]["operation_id"]
    );
    let cancelled = select(
        &s,
        &c,
        &changed,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":changed["receipt"]["draft_id"],"expected_version":3}),
    )
    .await;
    run(&s, &c, &cancelled).await;
    let recovered = ok(query(&s, &c, &agent_path(&changed)).await);
    assert_eq!(recovered["payment_flow"]["status"], "ENDED_INVALIDATED");
    assert_eq!(recovered["payment"]["invalid_reason"], "SOURCE_CHANGED");
    assert_eq!(payment_count(&db).await, 3);
    no_execution(&db).await;
    s.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and transactional failure injection"]
async fn payment_agent_http_binding_audit_failure_rolls_back_all_preparation_records() {
    let db = Db::new().await;
    let s = agent_server(&db, 300).await;
    let c = s.login().await;
    let d = agent_draft(&s, &c).await;
    let before = all_state(&db).await;
    db.scoped.execute("CREATE FUNCTION fail_agent_binding() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected'; END $$; CREATE TRIGGER fail_agent_binding BEFORE INSERT ON audit_events FOR EACH ROW WHEN (NEW.details ? 'payment_agent_binding') EXECUTE FUNCTION fail_agent_binding()").await.unwrap();
    assert_eq!(
        command(&s, &c, "/api/agent/samples", sample(Some(&d)))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(all_state(&db).await, before);
    assert_eq!(payment_count(&db).await, 0);
    db.scoped
        .execute("DROP TRIGGER fail_agent_binding ON audit_events")
        .await
        .unwrap();
    let p = intent(&s, &c, &d).await;
    assert!(p["payment"]["agent_binding"].is_object());
    let before = all_state(&db).await;
    assert!(db
        .scoped
        .execute("UPDATE payment_agent_bindings SET binding=binding-'locator'")
        .await
        .is_err());
    assert_eq!(all_state(&db).await, before);
    no_execution(&db).await;
    s.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual expiry/rejection/session changes"]
async fn payment_agent_http_expiry_rejection_and_session_revoke_never_restore_confirmation() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let s = agent_server(&db, 2).await;
    let c = s.login().await;
    let d = agent_draft(&s, &c).await;
    let p = intent(&s, &c, &d).await;
    let p = &p["payment"];
    tokio::time::timeout(Duration::from_secs(5),async { loop {
        let expired:bool=sqlx::query_scalar("SELECT clock_timestamp()>=expires_at FROM server_confirmations WHERE confirmation_ref=$1").bind(p["confirmation_ref"].as_str().unwrap()).fetch_one(&db.scoped).await.unwrap();
        if expired {break;} tokio::time::sleep(Duration::from_millis(10)).await;
    }}).await.unwrap();
    let before = all_state(&db).await;
    assert_eq!(
        ok(query(&s, &c, &agent_path(&d)).await)["payment"]["invalid_reason"],
        "EXPIRED"
    );
    assert_eq!(all_state(&db).await, before);
    assert_eq!(
        command(
            &s,
            &c,
            &format!("{}/confirm", path(p)),
            decision(p, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let saved:(String,String)=sqlx::query_as("SELECT c.status,p.invalid_reason FROM payment_preparations p JOIN server_confirmations c USING(confirmation_ref)").fetch_one(&db.scoped).await.unwrap();
    assert_eq!(saved, ("REVOKED".into(), "EXPIRED".into()));
    let d = agent_draft(&s, &c).await;
    let p = intent(&s, &c, &d).await;
    decided(&s, &c, &p["payment"], "REJECT").await;
    assert_eq!(intent(&s, &c, &d).await["payment"]["state"], "REJECTED");
    let d = agent_draft(&s, &c).await;
    let p = intent(&s, &c, &d).await;
    assert_eq!(
        s.call(
            Method::DELETE,
            "/api/sim/session",
            Some("{}".into()),
            Some(&c),
            None,
            None,
            true
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        query(&s, &c, &agent_path(&d)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = s.login().await;
    assert_eq!(
        ok(query(&s, &fresh, &agent_path(&d)).await)["payment"]["invalid_reason"],
        "SESSION_CHANGED"
    );
    assert_eq!(
        command(
            &s,
            &fresh,
            &format!("{}/confirm", path(&p["payment"])),
            decision(&p["payment"], "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    no_execution(&db).await;
    s.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL; original is an existing member SQL fixture"]
async fn payment_agent_http_processing_original_queries_same_order_without_new_intent() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let s = agent_server(&db, 300).await;
    let c = s.login().await;
    let d = agent_draft(&s, &c).await;
    let a = intent(&s, &c, &d).await;
    let p = &a["payment"];
    submit_isolated(&db, p, MockBehavior::Pending).await;
    let before = all_state(&db).await;
    let recovered = ok(query(&s, &c, &agent_path(&d)).await);
    assert_eq!(recovered["payment_flow"]["status"], "QUERY_ORIGINAL");
    if let Ok(directory) = std::env::var("SAFEFLOW_WEB_RESPONSE_EVIDENCE") {
        std::fs::write(
            std::path::Path::new(&directory).join("payment-processing.json"),
            serde_json::to_vec_pretty(&recovered).unwrap(),
        )
        .unwrap();
    }
    assert_eq!(
        recovered["payment_flow"]["original"]["state"],
        "RECONCILE_REQUIRED"
    );
    assert_eq!(
        recovered["payment_flow"]["original"]["verified_business_result"]["latest_receipt"]
            ["status"],
        "PROCESSING"
    );
    assert_eq!(
        intent(&s, &c, &d).await["payment_flow"],
        recovered["payment_flow"]
    );
    assert_eq!(
        ok(command(
            &s,
            &c,
            &format!("{}/continue", agent_path(&d)),
            continuation(&d)
        )
        .await)["payment_flow"],
        recovered["payment_flow"]
    );
    assert_eq!(all_state(&db).await, before);
    assert_eq!(payment_count(&db).await, 1);
    s.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and killing/restarting an actual HTTP process"]
async fn payment_agent_http_real_process_restart_recovers_binding_without_new_authority() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let process = ProcessServer::start(&db).await;
    let c = process.login().await;
    let first = process.confirmed(&c).await;
    let d = ok(process
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(&first)),
            Some(execution(&first)),
            Some(&c),
        )
        .await);
    let a = ok(process
        .call(
            Method::POST,
            "/api/agent/samples",
            Some(sample(Some(&d))),
            Some(&c),
        )
        .await);
    let p = &a["payment"];
    ok(process
        .call(
            Method::POST,
            &format!("{}/confirm", path(p)),
            Some(decision(p, "CONFIRM")),
            Some(&c),
        )
        .await);
    process.kill();
    let process = ProcessServer::start(&db).await;
    assert_eq!(
        process
            .call(Method::GET, &agent_path(&d), None, Some(&c))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let fresh = process.login().await;
    let before = all_state(&db).await;
    let recovered = ok(process
        .call(Method::GET, &agent_path(&d), None, Some(&fresh))
        .await);
    assert_eq!(recovered["payment"]["agent_binding"], p["agent_binding"]);
    assert_eq!(recovered["payment"]["invalid_reason"], "SESSION_CHANGED");
    assert_eq!(recovered["payment_flow"]["status"], "ENDED_INVALIDATED");
    assert_eq!(all_state(&db).await, before);
    no_execution(&db).await;
    process.kill();
    db.finish().await;
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "requires isolated PostgreSQL and public PowerShell HTTP demonstration"]
async fn payment_agent_http_public_powershell_example_prepares_confirms_and_waits() {
    let db = Db::new().await;
    provision_payer(&db).await;
    let process = ProcessServer::start(&db).await;
    let example =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/payment-agent-http.ps1");
    let output = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-File"])
        .arg(example)
        .args([
            "-Url",
            &format!("http://{}", process.address),
            "-ConfirmDemonstrationActions",
        ])
        .env("SAFEFLOW_SIM_LOGIN_SECRET", SECRET)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!(
        "Public Agent HTTP PowerShell demonstration: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(payment_count(&db).await, 1);
    no_execution(&db).await;
    process.kill();
    db.finish().await;
}
