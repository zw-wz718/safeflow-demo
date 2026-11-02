use super::*;
use safeflow_contracts::{MockDraftAction, PolicyVersion, SecurityLevel};
use safeflow_runtime::{
    DomainPolicyRegistry, MockDraftAuthority, MockDraftClock, RuntimeBootstrap, ToolRegistry,
    ToolSecurityCatalog, ToolSecurityProfile,
};
use safeflow_transfer_integration::runtime_tools::*;

fn current_path(view: &Value) -> String {
    format!("{}/current-draft", receipt_path(view))
}

async fn saved(server: &Server, cookie: &str) -> (Value, Value) {
    let view = confirmed(server, cookie).await;
    let result = command(server, cookie, &execute_path(&view), input(&view)).await;
    assert_eq!(result.status, StatusCode::OK, "{}", result.json);
    (view, result.json)
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_current_draft_reads_actual_store_without_new_execution_or_database_writes() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (view, history) = saved(&server, &cookie).await;
    let before: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('task', (SELECT jsonb_agg(to_jsonb(t)) FROM tasks t),
        'operation', (SELECT jsonb_agg(to_jsonb(o)) FROM operations o),
        'confirmation', (SELECT jsonb_agg(to_jsonb(c)) FROM server_confirmations c),
        'decision', (SELECT jsonb_agg(to_jsonb(d)) FROM security_decisions d),
        'fence', (SELECT jsonb_agg(to_jsonb(f)) FROM security_decision_time_fence f),
        'claim', (SELECT jsonb_agg(to_jsonb(e)) FROM mock_draft_execution_claims e))",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    let audits = count(&db, "audit_events").await;
    let current = query(&server, &cookie, &current_path(&view)).await;
    assert_eq!(current.status, StatusCode::OK, "{}", current.json);
    assert_eq!(current.json["scope"], "MOCK_DRAFT_CURRENT_STATE_ONLY");
    assert_eq!(current.json["current_draft_verified"], true);
    assert_eq!(current.json["operation_id"], view["operation_id"]);
    assert_eq!(
        current.json["draft"]["draft_id"],
        history["receipt"]["draft_id"]
    );
    assert_eq!(
        current.json["draft"]["business_operation_id"],
        history["receipt"]["business_operation_id"]
    );
    assert_eq!(current.json["draft"]["draft_version"], 1);
    assert_eq!(current.json["draft"]["status"], "READY_TO_CONFIRM");
    assert_eq!(current.json["lifecycle_execution_enabled"], false);
    assert_eq!(
        current.json["lifecycle_unavailable_reason"],
        "DEPENDENCY_UNAVAILABLE"
    );
    assert_eq!(current.json.as_object().unwrap().len(), 9);
    assert_eq!(current.json["draft"].as_object().unwrap().len(), 5);
    assert_eq!(current.headers["cache-control"], "no-store");
    assert_eq!(
        query(&server, &cookie, &current_path(&view)).await.json,
        current.json
    );
    let after: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('task', (SELECT jsonb_agg(to_jsonb(t)) FROM tasks t),
        'operation', (SELECT jsonb_agg(to_jsonb(o)) FROM operations o),
        'confirmation', (SELECT jsonb_agg(to_jsonb(c)) FROM server_confirmations c),
        'decision', (SELECT jsonb_agg(to_jsonb(d)) FROM security_decisions d),
        'fence', (SELECT jsonb_agg(to_jsonb(f)) FROM security_decision_time_fence f),
        'claim', (SELECT jsonb_agg(to_jsonb(e)) FROM mock_draft_execution_claims e))",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(before, after);
    assert_eq!(count(&db, "audit_events").await, audits);
    assert_eq!(count(&db, "execution_permits").await, 0);
    assert_eq!(server.calls(), 1);
    let original = query(&server, &cookie, &receipt_path(&view)).await;
    assert_eq!(original.json["current_draft_verified"], false);
    assert_eq!(original.json["receipt"], history["receipt"]);
    assert_eq!(
        server
            .call(
                Method::GET,
                &current_path(&view),
                None,
                None,
                None,
                None,
                false
            )
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        command(&server, &cookie, &current_path(&view), json!({}))
            .await
            .status,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        query(
            &server,
            &cookie,
            &format!("{}?expected_version=1", current_path(&view))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_current_draft_restart_absence_wrong_owner_and_wrong_operation_are_rejected() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (view, history) = saved(&server, &cookie).await;
    let second = create(&server, &cookie).await;
    let crossed = format!(
        "/api/sim/draft-executions/{}/operations/{}/current-draft",
        view["task_id"].as_str().unwrap(),
        second["operation_id"].as_str().unwrap()
    );
    assert_eq!(
        query(&server, &cookie, &crossed).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        query(&server, &cookie, &current_path(&second)).await.status,
        StatusCode::NOT_FOUND
    );
    let restarted = super::super::server(&db).await;
    let fresh = restarted.login().await;
    let missing = query(&restarted, &fresh, &current_path(&view)).await;
    assert_eq!(missing.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        missing.json["error"]["reason_code"],
        "DEPENDENCY_UNAVAILABLE"
    );
    assert!(missing.json.get("draft").is_none());
    let original = query(&restarted, &fresh, &receipt_path(&view)).await;
    assert_eq!(original.status, StatusCode::OK);
    assert_eq!(original.json["receipt"], history["receipt"]);
    assert_eq!(original.json["current_draft_verified"], false);
    let other = Server::with_draft(
        &db,
        "user_002",
        Duration::from_secs(15),
        Duration::from_secs(120),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(60),
                Duration::from_secs(60),
                300,
                "current-draft-owner-test-v1",
            )
            .unwrap(),
        ),
    )
    .await;
    assert_eq!(
        query(&other, &other.login().await, &current_path(&view))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(restarted.calls(), 0);
    assert_eq!(other.calls(), 0);
    assert_eq!(claim_count(&db).await, 1);
    other.stop();
    restarted.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_current_draft_expiry_is_observed_without_recreating_or_extending_it() {
    let db = Db::new().await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(15),
        Duration::from_secs(120),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(60),
                Duration::from_secs(60),
                2,
                "current-draft-expiry-test-v1",
            )
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    let (view, history) = saved(&server, &cookie).await;
    let initial = query(&server, &cookie, &current_path(&view)).await;
    assert_eq!(initial.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let expired = query(&server, &cookie, &current_path(&view)).await;
    assert_eq!(expired.status, StatusCode::OK, "{}", expired.json);
    assert_eq!(expired.json["draft"]["status"], "EXPIRED");
    assert_eq!(
        expired.json["draft"]["expires_at"],
        initial.json["draft"]["expires_at"]
    );
    assert_eq!(expired.json["draft"]["draft_version"], 1);
    assert_eq!(expired.json["lifecycle_execution_enabled"], false);
    assert_eq!(
        query(&server, &cookie, &receipt_path(&view)).await.json["receipt"],
        history["receipt"]
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(claim_count(&db).await, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_current_draft_lock_wait_session_revocation_and_timeout_do_not_expose_state() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (view, history) = saved(&server, &cookie).await;
    let observation_path = current_path(&view);
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(view["task_id"].as_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    let (reply, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(query(&server, &cookie, &observation_path), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE wait_event_type='Lock' AND query LIKE 'SELECT * FROM tasks WHERE task_id=$1 AND owner_id=$2 FOR UPDATE%'")
                    .fetch_one(&db.admin).await.unwrap();
                if waiting > 0 { break; }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(server.call(Method::DELETE, "/api/sim/session", Some("{}".into()),
                Some(&cookie), None, None, true).await.status, StatusCode::NO_CONTENT);
            tx.commit().await.unwrap();
        })
    }).await.unwrap();
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    assert!(reply.json.get("draft").is_none());
    let fresh = server.login().await;
    assert_eq!(
        query(&server, &fresh, &current_path(&view)).await.status,
        StatusCode::OK
    );
    let slow = Server::with_draft(
        &db,
        "user_001",
        Duration::from_millis(40),
        Duration::from_secs(120),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(60),
                Duration::from_secs(60),
                300,
                "current-draft-timeout-test-v1",
            )
            .unwrap(),
        ),
    )
    .await;
    let slow_cookie = slow.login().await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(view["task_id"].as_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    let timed = query(&slow, &slow_cookie, &current_path(&view)).await;
    assert_eq!(timed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(timed.json["error"]["task_id"], view["task_id"]);
    tx.commit().await.unwrap();
    assert_eq!(
        query(&server, &fresh, &receipt_path(&view)).await.json["receipt"],
        history["receipt"]
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(slow.calls(), 0);
    assert_eq!(claim_count(&db).await, 1);
    slow.stop();
    server.stop();
    db.finish().await;
}

struct LifecycleClock;
impl MockDraftClock for LifecycleClock {
    fn now_seconds(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }
}

/// Only in-process contract-10 tools; this does not enable persistent HTTP lifecycle writes.
async fn lifecycle(
    server: &Server,
    cookie: &str,
    view: &Value,
    draft: &Value,
    action: MockDraftAction,
    runtime_version: u32,
) {
    let sessions = server.state.0.sessions.clone();
    let identity = server
        .state
        .identity(&HeaderMap::from_iter([(
            header::COOKIE,
            HeaderValue::from_str(cookie).unwrap(),
        )]))
        .unwrap_or_else(|_| panic!("test session unavailable"));
    let store = server.state.0.draft.as_ref().unwrap().store.clone();
    let authority = Arc::new(MockDraftAuthority::new(
        sessions.clone(),
        Arc::new(LifecycleClock),
    ));
    let mut tools = ToolRegistry::new();
    let mut policies = DomainPolicyRegistry::new();
    let mut catalog = ToolSecurityCatalog::new();
    let (tool, policy) = match action {
        MockDraftAction::Modify => (
            MockDraftLifecycleTool::modify(store.clone(), authority.clone()),
            MockDraftLifecyclePolicy::modify(store),
        ),
        MockDraftAction::Cancel => (
            MockDraftLifecycleTool::cancel(store.clone(), authority.clone()),
            MockDraftLifecyclePolicy::cancel(store),
        ),
        _ => unreachable!(),
    };
    tools.register(Arc::new(tool)).unwrap();
    let version = PolicyVersion::parse(MOCK_DRAFT_LIFECYCLE_POLICY_VERSION).unwrap();
    policies
        .register(Arc::new(policy), version.clone(), MOCK_DRAFT_SCHEMA_VERSION)
        .unwrap();
    catalog
        .register(ToolSecurityProfile {
            tool_name: ToolName::parse(action.tool_name()).unwrap(),
            tool_schema_version: MOCK_DRAFT_SCHEMA_VERSION,
            policy_version: version,
            minimum_level: SecurityLevel::Green,
        })
        .unwrap();
    let runtime = RuntimeBootstrap::build_mock_draft(
        tools,
        policies,
        catalog,
        sessions,
        Arc::new(safeflow_runtime::InMemoryAuditRecorder::new()),
        authority.clone(),
    )
    .unwrap();
    let mut arguments =
        json!({"draft_id":draft["draft_id"], "expected_version":draft["draft_version"]});
    if action == MockDraftAction::Modify {
        arguments["changes"] = json!({"amount":"180.00"});
    }
    let request = authority
        .request_for_action(
            action,
            identity.reference,
            TaskId::parse(view["task_id"].as_str().unwrap()).unwrap(),
            NodeId::parse(format!("isolated_lifecycle_{runtime_version}")).unwrap(),
            runtime_version,
            arguments,
            60,
        )
        .unwrap();
    let confirmation = authority.record_explicit_confirmation(&request).unwrap();
    let permit = authority.issue(&confirmation).unwrap();
    runtime
        .runtime()
        .execute_mock_draft(request, &permit)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_current_draft_observes_existing_in_process_modify_and_cancel_not_historical_version(
) {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let (view, history) = saved(&server, &cookie).await;
    let initial = query(&server, &cookie, &current_path(&view)).await;
    lifecycle(
        &server,
        &cookie,
        &view,
        &initial.json["draft"],
        MockDraftAction::Modify,
        2,
    )
    .await;
    let modified = query(&server, &cookie, &current_path(&view)).await;
    assert_eq!(modified.status, StatusCode::OK, "{}", modified.json);
    assert_eq!(modified.json["draft"]["draft_version"], 2);
    assert_eq!(modified.json["draft"]["status"], "READY_TO_CONFIRM");
    assert_eq!(
        modified.json["draft"]["expires_at"],
        initial.json["draft"]["expires_at"]
    );
    lifecycle(
        &server,
        &cookie,
        &view,
        &modified.json["draft"],
        MockDraftAction::Cancel,
        3,
    )
    .await;
    let cancelled = query(&server, &cookie, &current_path(&view)).await;
    assert_eq!(cancelled.status, StatusCode::OK, "{}", cancelled.json);
    assert_eq!(cancelled.json["draft"]["draft_version"], 2);
    assert_eq!(cancelled.json["draft"]["status"], "CANCELLED");
    assert_eq!(
        cancelled.json["draft"]["business_operation_id"],
        history["receipt"]["business_operation_id"]
    );
    assert_eq!(
        query(&server, &cookie, &receipt_path(&view)).await.json["receipt"],
        history["receipt"]
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(claim_count(&db).await, 1);
    assert_eq!(cancelled.json["lifecycle_execution_enabled"], false);
    server.stop();
    db.finish().await;
}
