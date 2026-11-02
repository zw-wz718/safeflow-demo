use super::*;
#[path = "current_tests.rs"]
mod current_tests;

fn input(view: &Value) -> Value {
    json!({"scope":"MOCK_DRAFT_EXECUTION_ONLY","operation_id":view["operation_id"],
        "confirmation_ref":view["confirmation"]["confirmation_ref"],"task_revision":view["task_revision"],
        "operation_revision":view["operation_revision"],"plan_version":view["plan_version"],"facts_version":view["facts_version"]})
}
fn execute_path(view: &Value) -> String {
    format!("{}/execute", path(view))
}
fn receipt_path(view: &Value) -> String {
    format!(
        "/api/sim/draft-executions/{}/operations/{}",
        view["task_id"].as_str().unwrap(),
        view["operation_id"].as_str().unwrap()
    )
}
async fn confirmed(server: &Server, cookie: &str) -> Value {
    let view = create(server, cookie).await;
    let reply = command(
        server,
        cookie,
        &action_path(&view),
        action(&view, "CONFIRM"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    assert_eq!(server.calls(), 0, "confirmation must not invoke Tool");
    reply.json
}
async fn claim_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM mock_draft_execution_claims")
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_persistent_draft_execution_actual_tool_replay_query_and_restart() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let view = confirmed(&server, &cookie).await;
    assert_eq!(
        query(&server, &cookie, &receipt_path(&view)).await.status,
        StatusCode::NOT_FOUND
    );
    let reply = command(&server, &cookie, &execute_path(&view), input(&view)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    assert_eq!(reply.json["execution_status"], "COMPLETED");
    assert_eq!(reply.json["operation_id"], view["operation_id"]);
    assert_eq!(reply.json["task_state"], "COMPLETED");
    assert_eq!(reply.json["operation_state"], "SUCCEEDED");
    assert_eq!(reply.json["receipt_scope"], "HISTORICAL_EXECUTION_RECEIPT");
    assert_eq!(reply.json["current_draft_verified"], false);
    assert_eq!(reply.json["replayed"], false);
    assert_ne!(
        reply.json["receipt"]["business_operation_id"],
        view["operation_id"]
    );
    assert_eq!(reply.json["receipt"]["draft_version"], 1);
    assert_eq!(server.calls(), 1);
    assert_eq!(claim_count(&db).await, 1);
    assert_eq!(count(&db, "execution_permits").await, 0);
    let replay = command(&server, &cookie, &execute_path(&view), input(&view)).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.json);
    assert_eq!(replay.json["receipt"], reply.json["receipt"]);
    assert_eq!(replay.json["replayed"], true);
    let reads = query(&server, &cookie, &receipt_path(&view)).await;
    assert_eq!(reads.json, replay.json);
    assert_eq!(server.calls(), 1);
    let public = query(
        &server,
        &cookie,
        &format!("/api/tasks/{}", view["task_id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.json);
    assert_eq!(public.json["operation_state"], "SUCCEEDED");
    assert_eq!(public.json["task_state"], "COMPLETED");
    let restarted = super::server(&db).await;
    let fresh = restarted.login().await;
    let history = query(&restarted, &fresh, &receipt_path(&view)).await;
    assert_eq!(history.status, StatusCode::OK, "{}", history.json);
    assert_eq!(history.json, reads.json);
    assert_eq!(restarted.calls(), 0);
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
                "isolated-http-confirmation-v1",
            )
            .unwrap(),
        ),
    )
    .await;
    assert_eq!(
        query(&other, &other.login().await, &receipt_path(&view))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    restarted.stop();
    other.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_persistent_draft_execution_confirmation_sources_and_parameter_tampering_do_not_claim(
) {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let view = create(&server, &cookie).await;
    assert_ne!(
        command(&server, &cookie, &execute_path(&view), input(&view))
            .await
            .status,
        StatusCode::OK
    );
    let reply = command(
        &server,
        &cookie,
        &action_path(&view),
        action(&view, "CONFIRM"),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK);
    for (field, value) in [
        ("task_revision", json!(999)),
        ("operation_revision", json!(2)),
        ("plan_version", json!(2)),
        ("facts_version", json!(2)),
        ("operation_id", json!("wrong_operation")),
        ("confirmation_ref", json!("wrong_confirmation")),
        ("verified", json!(true)),
        ("parameters", json!({"amount":"1.00"})),
    ] {
        let mut command_input = input(&view);
        command_input[field] = value;
        assert_ne!(
            command(&server, &cookie, &execute_path(&view), command_input)
                .await
                .status,
            StatusCode::OK
        );
    }
    assert_eq!(server.calls(), 0);
    assert_eq!(claim_count(&db).await, 0);
    sqlx::query(
        "UPDATE security_decisions SET status='REVOKED',ended_at_ms=expires_at_ms WHERE task_id=$1",
    )
    .bind(view["task_id"].as_str().unwrap())
    .execute(&db.scoped)
    .await
    .unwrap();
    assert_ne!(
        command(&server, &cookie, &execute_path(&view), input(&view))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(server.calls(), 0);
    assert_eq!(claim_count(&db).await, 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_persistent_draft_execution_concurrent_calls_are_one_and_corrupt_history_is_rejected(
) {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let view = confirmed(&server, &cookie).await;
    let execution = execute_path(&view);
    let (a, b) = tokio::join!(
        command(&server, &cookie, &execution, input(&view)),
        command(&server, &cookie, &execution, input(&view))
    );
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(b.status, StatusCode::OK, "{}", b.json);
    let settled = query(&server, &cookie, &receipt_path(&view)).await;
    assert_eq!(settled.status, StatusCode::OK, "{}", settled.json);
    assert_eq!(settled.json["execution_status"], "COMPLETED");
    assert!(a.json["receipt"].is_object() || b.json["receipt"].is_object());
    for reply in [&a, &b] {
        if reply.json["execution_status"] == "COMPLETED" {
            assert_eq!(reply.json["receipt"], settled.json["receipt"]);
        } else {
            assert_eq!(
                reply.json["execution_status"],
                "REQUIRES_ORIGINAL_OPERATION_REVIEW"
            );
            assert_eq!(reply.json["receipt"], Value::Null);
            assert_eq!(
                reply.json["next_action"]["kind"],
                "QUERY_ORIGINAL_OPERATION"
            );
        }
        assert_eq!(reply.json["operation_id"], view["operation_id"]);
    }
    assert_eq!(server.calls(), 1);
    assert_eq!(claim_count(&db).await, 1);
    sqlx::query("UPDATE operations SET facts_version=facts_version+1 WHERE task_id=$1")
        .bind(view["task_id"].as_str().unwrap())
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_ne!(
        query(
            &server,
            &cookie,
            &format!("{}/current-draft", receipt_path(&view))
        )
        .await
        .status,
        StatusCode::OK,
        "current Store reads must not bypass corrupt persistent history"
    );
    assert_ne!(
        query(&server, &cookie, &receipt_path(&view)).await.status,
        StatusCode::OK
    );
    assert_ne!(
        command(&server, &cookie, &execute_path(&view), input(&view))
            .await
            .status,
        StatusCode::OK
    );
    assert_ne!(
        query(
            &server,
            &cookie,
            &format!("/api/tasks/{}", view["task_id"].as_str().unwrap()),
        )
        .await
        .status,
        StatusCode::OK,
        "generic task query must not swallow corrupt execution history"
    );
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_persistent_draft_execution_sql_settlement_failure_keeps_original_claim() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let view = confirmed(&server, &cookie).await;
    db.scoped.execute("CREATE FUNCTION reject_draft_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='COMPLETED' THEN RAISE EXCEPTION 'isolated settlement failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_draft_settlement BEFORE UPDATE ON mock_draft_execution_claims FOR EACH ROW EXECUTE FUNCTION reject_draft_settlement();").await.unwrap();
    assert_eq!(
        command(&server, &cookie, &execute_path(&view), input(&view))
            .await
            .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(claim_count(&db).await, 1);
    let original = query(&server, &cookie, &receipt_path(&view)).await;
    assert_eq!(original.status, StatusCode::OK, "{}", original.json);
    assert_eq!(
        original.json["execution_status"],
        "REQUIRES_ORIGINAL_OPERATION_REVIEW"
    );
    assert_eq!(original.json["receipt"], Value::Null);
    assert_eq!(original.json["operation_state"], "DRAFT");
    let current = query(
        &server,
        &cookie,
        &format!("{}/current-draft", receipt_path(&view)),
    )
    .await;
    assert_eq!(current.status, StatusCode::OK, "{}", current.json);
    assert_eq!(current.json["draft"]["status"], "READY_TO_CONFIRM");
    assert_eq!(current.json["current_draft_verified"], true);
    assert_eq!(current.json["lifecycle_execution_enabled"], false);
    assert_eq!(
        server.calls(),
        1,
        "observation never retries an unresolved claim"
    );
    let public = query(
        &server,
        &cookie,
        &format!("/api/tasks/{}", view["task_id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(public.status, StatusCode::OK, "{}", public.json);
    assert_eq!(public.json["task_state"], "RUNNING");
    assert_eq!(public.json["operation_state"], "DRAFT");
    assert_eq!(public.json["next_action"]["kind"], "REVIEW_ORIGINAL_TASK");
    let replay = command(&server, &cookie, &execute_path(&view), input(&view)).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.json, original.json);
    assert_eq!(
        server.calls(),
        1,
        "SQL rollback must not retry in-memory business"
    );
    server.stop();
    db.finish().await;
}
