use super::*;
#[path = "http_draft_tests/execution_tests.rs"]
mod execution_tests;
use safeflow_contracts::{AgentPlan, PlanNode, ToolName, MOCK_DRAFT_CREATE_TOOL};

async fn server(db: &Db) -> Server {
    Server::with_draft(
        db,
        "user_001",
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
    .await
}
async fn command(server: &Server, cookie: &str, path: &str, input: Value) -> Reply {
    server
        .call(
            Method::POST,
            path,
            Some(input.to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await
}
async fn create(server: &Server, cookie: &str) -> Value {
    let reply = command(server, cookie, "/api/sim/draft-confirmations", json!({})).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    assert_eq!(reply.json["task_state"], "RUNNING");
    assert_eq!(reply.json["operation_state"], "DRAFT");
    assert_eq!(reply.json["task_revision"], 2);
    assert_eq!(reply.json["operation_revision"], 1);
    assert_eq!(reply.json["next_action"], json!({"kind":"CONFIRM"}));
    assert_eq!(
        reply.json["preflight"],
        json!({"evidence_kind":"PREFLIGHT_ONLY",
        "pre_tool_outcome":"READY_TO_CONFIRM","pre_auth_outcome":"READY_TO_CONFIRM",
        "required_gates":["EXPLICIT_CONFIRMATION"]})
    );
    assert_eq!(reply.json.as_object().unwrap().len(), 16);
    for field in [
        "owner",
        "owner_id",
        "session",
        "binding",
        "audit",
        "permit",
        "business_result",
        "auth_grant",
        "decision",
        "assembly_metadata",
    ] {
        assert!(reply.json.get(field).is_none());
    }
    reply.json
}
fn path(view: &Value) -> String {
    format!(
        "/api/sim/draft-confirmations/{}",
        view["task_id"].as_str().unwrap()
    )
}
fn action_path(view: &Value) -> String {
    format!(
        "/api/operations/{}/confirm",
        view["operation_id"].as_str().unwrap()
    )
}
fn action(view: &Value, action: &str) -> Value {
    json!({"scope":"MOCK_DRAFT_CONFIRMATION_ONLY","task_id":view["task_id"],
        "confirmation_ref":view["confirmation"]["confirmation_ref"],"display_ticket":view["display_ticket"],"action":action})
}
async fn query(server: &Server, cookie: &str, path: &str) -> Reply {
    server
        .call(Method::GET, path, None, Some(cookie), None, None, false)
        .await
}
async fn count(db: &Db, table: &str) -> i64 {
    assert!([
        "tasks",
        "operations",
        "server_confirmations",
        "security_decisions",
        "audit_events",
        "execution_permits",
        "planning_confirmation_displays"
    ]
    .contains(&table));
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}
async fn assert_no_execution(db: &Db, server: &Server) {
    assert_eq!(server.calls(), 0);
    assert_eq!(count(db, "execution_permits").await, 0);
    let states: Vec<String> = sqlx::query_scalar("SELECT state FROM operations")
        .fetch_all(&db.scoped)
        .await
        .unwrap();
    assert!(states.iter().all(|s| s == "DRAFT"));
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_frozen_display_confirmation_replay_and_pure_get() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let second = create(&server, &cookie).await;
    let mut crossed = action(&first, "CONFIRM");
    crossed["display_ticket"] = second["display_ticket"].clone();
    assert_eq!(
        command(&server, &cookie, &action_path(&first), crossed)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let mut crossed = action(&first, "CONFIRM");
    crossed["confirmation_ref"] = second["confirmation"]["confirmation_ref"].clone();
    assert_eq!(
        command(&server, &cookie, &action_path(&first), crossed)
            .await
            .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&second),
            action(&first, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let audits = count(&db, "audit_events").await;
    let fence: Value = sqlx::query_scalar(
        "SELECT row_to_json(f)::jsonb FROM security_decision_time_fence f WHERE scope=1",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(query(&server, &cookie, &path(&first)).await.json, first);
    assert_eq!(count(&db, "audit_events").await, audits);
    let after: Value = sqlx::query_scalar(
        "SELECT row_to_json(f)::jsonb FROM security_decision_time_fence f WHERE scope=1",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(fence, after, "GET must not write the time fence");
    let replay_prepare = command(
        &server,
        &cookie,
        &format!("{}/prepare", path(&first)),
        json!({}),
    )
    .await;
    assert_eq!(replay_prepare.json, first);
    assert_eq!(count(&db, "audit_events").await, audits);
    let confirmed = command(
        &server,
        &cookie,
        &action_path(&first),
        action(&first, "CONFIRM"),
    )
    .await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.json);
    assert_eq!(confirmed.json["confirmation"]["status"], "CONFIRMED");
    assert!(confirmed.json["confirmation"]["confirmed_at"].is_string());
    assert_eq!(confirmed.json["parameters"], first["parameters"]);
    assert_eq!(confirmed.json["preflight"], first["preflight"]);
    assert_eq!(confirmed.json["next_action"], json!({"kind":"NONE"}));
    assert!(confirmed.json["display_ticket"].is_null());
    let replay = command(
        &server,
        &cookie,
        &action_path(&first),
        action(&first, "CONFIRM"),
    )
    .await;
    assert_eq!(replay.json, confirmed.json);
    assert_eq!(count(&db, "audit_events").await, audits + 1);
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&first),
            action(&first, "REJECT")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        query(&server, &cookie, &path(&first)).await.json,
        confirmed.json
    );
    let mut tampered = action(&first, "CONFIRM");
    tampered["display_ticket"] = json!("display_00000000000000000000000000000000");
    assert_eq!(
        command(&server, &cookie, &action_path(&first), tampered)
            .await
            .status,
        StatusCode::CONFLICT
    );
    let mut extra = action(&first, "CONFIRM");
    extra["parameters"] = json!({"amount":"1.00"});
    assert_eq!(
        command(&server, &cookie, &action_path(&first), extra)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&first),
            json!({"scope":"FORMAL_TRANSFER"})
        )
        .await
        .status,
        StatusCode::NOT_IMPLEMENTED
    );
    let duplicate = action(&first, "CONFIRM").to_string().replacen(
        "{",
        "{\"scope\":\"MOCK_DRAFT_CONFIRMATION_ONLY\",",
        1,
    );
    assert_eq!(
        server
            .call(
                Method::POST,
                &action_path(&first),
                Some(duplicate),
                Some(&cookie),
                None,
                None,
                true
            )
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_reject_replay_and_concurrent_opposing_actions() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let rejected = command(
        &server,
        &cookie,
        &action_path(&first),
        action(&first, "REJECT"),
    )
    .await;
    assert_eq!(rejected.status, StatusCode::OK);
    assert_eq!(rejected.json["confirmation"]["status"], "REVOKED");
    assert!(rejected.json["confirmation"]["revoked_at"].is_string());
    assert_eq!(rejected.json["preflight"], first["preflight"]);
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&first),
            action(&first, "REJECT")
        )
        .await
        .json,
        rejected.json
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&first),
            action(&first, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/prepare", path(&first)),
            json!({})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let second = create(&server, &cookie).await;
    let endpoint = action_path(&second);
    let (confirmed, revoked) = tokio::join!(
        command(&server, &cookie, &endpoint, action(&second, "CONFIRM")),
        command(&server, &cookie, &endpoint, action(&second, "REJECT"))
    );
    assert!(
        matches!(
            (confirmed.status, revoked.status),
            (StatusCode::OK, StatusCode::CONFLICT) | (StatusCode::CONFLICT, StatusCode::OK)
        ),
        "{} / {}",
        confirmed.json,
        revoked.json
    );
    let decisions: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE task_id=$1 AND kind IN ('EXPLICIT_CONFIRMATION_RECORDED','CONFIRMATION_REVOKED')")
        .bind(second["task_id"].as_str().unwrap()).fetch_one(&db.scoped).await.unwrap();
    assert_eq!(decisions, 1);
    let third = create(&server, &cookie).await;
    let endpoint = action_path(&third);
    let (a, b) = tokio::join!(
        command(&server, &cookie, &endpoint, action(&third, "CONFIRM")),
        command(&server, &cookie, &endpoint, action(&third, "CONFIRM"))
    );
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(a.json, b.json);
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_relogin_prepare_separate_assembly_preserves_readonly_task() {
    let db = Db::new().await;
    let old_server = Server::new(
        &db,
        "user_001",
        Duration::from_secs(15),
        Duration::from_secs(120),
    )
    .await;
    let old_cookie = old_server.login().await;
    let old_task = old_server.new_task(&old_cookie).await;
    old_server.stop();
    let server = server(&db).await;
    let cookie = server.login().await;
    let old_path = format!("/api/tasks/{}", old_task["task_id"].as_str().unwrap());
    let recovered = query(&server, &cookie, &old_path).await;
    assert_eq!(recovered.status, StatusCode::OK, "{}", recovered.json);
    assert_eq!(recovered.json["next_action"], old_task["next_action"]);
    assert_eq!(
        server.continue_task(&cookie, &recovered.json).await.status,
        StatusCode::OK
    );
    let first = create(&server, &cookie).await;
    let fresh = server.login().await;
    let audit_count = count(&db, "audit_events").await;
    let original = query(&server, &fresh, &path(&first)).await;
    assert_eq!(original.status, StatusCode::OK);
    assert_eq!(original.json["next_action"], json!({"kind":"PREPARE"}));
    assert!(
        original.json["confirmation"].is_null()
            && original.json["preflight"].is_null()
            && original.json["display_ticket"].is_null()
    );
    assert_eq!(count(&db, "audit_events").await, audit_count);
    assert_eq!(
        command(
            &server,
            &fresh,
            &action_path(&first),
            action(&first, "CONFIRM")
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    let renewed = command(
        &server,
        &fresh,
        &format!("{}/prepare", path(&first)),
        json!({}),
    )
    .await;
    assert_eq!(renewed.status, StatusCode::OK, "{}", renewed.json);
    assert_eq!(renewed.json["task_id"], first["task_id"]);
    assert_eq!(renewed.json["operation_id"], first["operation_id"]);
    assert_ne!(renewed.json["display_ticket"], first["display_ticket"]);
    assert_ne!(
        renewed.json["confirmation"]["confirmation_ref"],
        first["confirmation"]["confirmation_ref"]
    );
    assert_eq!(count(&db, "tasks").await, 2);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_provenance_forgery_and_snapshot_changes_are_rejected() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let token = Cookie::parse(cookie.clone()).unwrap();
    let session = token_reference(token.value()).unwrap();
    let persistence =
        PersistenceService::for_trusted_backend(db.repo.clone(), server.state.0.sessions.clone());
    let id = new_task_id();
    let plan = AgentPlan::new(
        id.clone(),
        1,
        vec![PlanNode::Tool {
            id: NodeId::parse("prepare_draft").unwrap(),
            tool: ToolName::parse(MOCK_DRAFT_CREATE_TOOL).unwrap(),
            arguments: json!({"payer_account_id":"account_001",
            "payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}),
            depends_on: vec![],
        }],
    )
    .unwrap();
    persistence.create_task(&session, plan, 1).await.unwrap();
    let endpoint = format!("/api/sim/draft-confirmations/{id}");
    assert_eq!(
        query(&server, &cookie, &endpoint).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        command(&server, &cookie, &format!("{endpoint}/prepare"), json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(count(&db, "operations").await, 0);
    let first = create(&server, &cookie).await;
    let id = TaskId::parse(first["task_id"].as_str().unwrap()).unwrap();
    persistence
        .save_checkpoint(
            &session,
            &id,
            CheckpointChange {
                expected_revision: 2,
                event: None,
                checkpoint: RecoveryPoint::AtNode {
                    node_id: NodeId::parse("prepare_draft").unwrap(),
                },
                completed_nodes: vec![],
                facts_version: 8,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        query(&server, &cookie, &path(&first)).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            &action_path(&first),
            action(&first, "CONFIRM")
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_query_rechecks_decision_expiry_and_revocation_after_lock_wait() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(first["task_id"].as_str().unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let endpoint = path(&first);
    let pending = query(&server, &cookie, &endpoint);
    let expire = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sqlx::query(
            "UPDATE security_decisions SET expires_at_ms=evaluated_at_ms+1 WHERE task_id=$1",
        )
        .bind(first["task_id"].as_str().unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
    };
    let (reply, ()) = tokio::join!(pending, expire);
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.json);
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/prepare", path(&first)),
            json!({})
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let second = create(&server, &cookie).await;
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(second["task_id"].as_str().unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let endpoint = path(&second);
    let pending = query(&server, &cookie, &endpoint);
    let revoke = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        sqlx::query("UPDATE security_decisions SET status='REVOKED',ended_at_ms=evaluated_at_ms+1 WHERE task_id=$1")
            .bind(second["task_id"].as_str().unwrap()).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    };
    let (reply, ()) = tokio::join!(pending, revoke);
    assert_eq!(reply.status, StatusCode::CONFLICT);
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_partial_storage_failures_keep_original_ids_and_resume_stages() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    db.scoped.execute("CREATE FUNCTION fail_confirmation_stage() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated stage failure'; END $$").await.unwrap();
    db.scoped.execute("CREATE TRIGGER fail_operation BEFORE INSERT ON operations FOR EACH ROW EXECUTE FUNCTION fail_confirmation_stage()").await.unwrap();
    let failed = command(&server, &cookie, "/api/sim/draft-confirmations", json!({})).await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    let id = failed.json["error"]["task_id"].as_str().unwrap();
    let endpoint = format!("/api/sim/draft-confirmations/{id}");
    let initial = query(&server, &cookie, &endpoint).await;
    assert_eq!(initial.status, StatusCode::OK);
    assert_eq!(initial.json["next_action"], json!({"kind":"PREPARE"}));
    assert!(
        initial.json["operation_id"].is_null()
            && initial.json["operation_state"].is_null()
            && initial.json["operation_revision"].is_null()
    );
    db.scoped
        .execute("DROP TRIGGER fail_operation ON operations")
        .await
        .unwrap();
    db.scoped.execute("CREATE TRIGGER fail_pre_auth BEFORE INSERT ON security_decisions FOR EACH ROW WHEN (NEW.phase='PRE_AUTH') EXECUTE FUNCTION fail_confirmation_stage()").await.unwrap();
    let failed = command(&server, &cookie, &format!("{endpoint}/prepare"), json!({})).await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(failed.json["error"]["task_id"], id);
    let interrupted = query(&server, &cookie, &endpoint).await;
    assert_eq!(interrupted.status, StatusCode::OK);
    assert_eq!(interrupted.json["next_action"], json!({"kind":"PREPARE"}));
    assert_eq!(interrupted.json["operation_state"], "DRAFT");
    assert!(interrupted.json["preflight"].is_null() && interrupted.json["confirmation"].is_null());
    let first_pre_tool: (String, i64, i64) = sqlx::query_as("SELECT stored_decision_id,evaluated_at_ms,expires_at_ms FROM security_decisions WHERE task_id=$1 AND phase='PRE_TOOL'")
        .bind(id).fetch_one(&db.scoped).await.unwrap();
    db.scoped
        .execute("DROP TRIGGER fail_pre_auth ON security_decisions")
        .await
        .unwrap();
    db.scoped.execute("CREATE TRIGGER fail_display BEFORE UPDATE ON planning_confirmation_displays FOR EACH ROW EXECUTE FUNCTION fail_confirmation_stage()").await.unwrap();
    let failed = command(&server, &cookie, &format!("{endpoint}/prepare"), json!({})).await;
    assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
    let prepared: (String, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "SELECT confirmation_ref,expires_at FROM server_confirmations WHERE task_id=$1",
    )
    .bind(id)
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    db.scoped
        .execute("DROP TRIGGER fail_display ON planning_confirmation_displays")
        .await
        .unwrap();
    let recovered = command(&server, &cookie, &format!("{endpoint}/prepare"), json!({})).await;
    assert_eq!(recovered.status, StatusCode::OK, "{}", recovered.json);
    assert_eq!(recovered.json["task_id"], id);
    assert_eq!(
        recovered.json["operation_id"],
        interrupted.json["operation_id"]
    );
    assert_eq!(
        recovered.json["confirmation"]["confirmation_ref"],
        prepared.0
    );
    let after: (String, i64, i64) = sqlx::query_as("SELECT stored_decision_id,evaluated_at_ms,expires_at_ms FROM security_decisions WHERE task_id=$1 AND phase='PRE_TOOL'")
        .bind(id).fetch_one(&db.scoped).await.unwrap();
    assert_eq!(after, first_pre_tool);
    let expiry: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT expires_at FROM server_confirmations WHERE task_id=$1")
            .bind(id)
            .fetch_one(&db.scoped)
            .await
            .unwrap();
    assert_eq!(expiry, prepared.1);
    assert_eq!(count(&db, "tasks").await, 1);
    assert_eq!(count(&db, "operations").await, 1);
    assert_eq!(count(&db, "security_decisions").await, 2);
    assert_eq!(count(&db, "server_confirmations").await, 1);
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_draft_confirmed_query_expires_while_waiting_for_task_lock() {
    let db = Db::new().await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(15),
        Duration::from_secs(120),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(60),
                Duration::from_secs(2),
                300,
                "isolated-http-confirmation-v1",
            )
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let confirmed = command(
        &server,
        &cookie,
        &action_path(&first),
        action(&first, "CONFIRM"),
    )
    .await;
    assert_eq!(confirmed.status, StatusCode::OK);
    let mut tx = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(first["task_id"].as_str().unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let endpoint = path(&first);
    let pending = query(&server, &cookie, &endpoint);
    let release = async {
        tokio::time::sleep(Duration::from_millis(2100)).await;
        tx.commit().await.unwrap();
    };
    let (reply, ()) = tokio::join!(pending, release);
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.json);
    assert_eq!(reply.json["error"]["task_id"], first["task_id"]);
    assert_no_execution(&db, &server).await;
    server.stop();
    db.finish().await;
}
