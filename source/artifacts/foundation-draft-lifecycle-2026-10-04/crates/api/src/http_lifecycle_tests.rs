use super::*;
#[path = "http_lifecycle_tests/payment_tests.rs"]
mod payment_tests;
#[path = "http_lifecycle_tests/process_tests.rs"]
mod process_tests;
#[path = "http_lifecycle_tests/restart_adapter.rs"]
mod restart_adapter;
#[path = "http_lifecycle_tests/safety_tests.rs"]
mod safety_tests;
#[path = "http_lifecycle_tests/sql_bridge_tests.rs"]
mod sql_bridge_tests;

const SCOPE: &str = "MOCK_DRAFT_LIFECYCLE_ONLY";

async fn server(db: &Db) -> Server {
    Server::with_draft(
        db,
        "user_001",
        Duration::from_secs(60),
        Duration::from_secs(600),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(300),
                Duration::from_secs(300),
                600,
                "isolated-http-lifecycle-v2",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await
}
fn scope() -> Value {
    json!({"contract_version":2,"scope":SCOPE})
}
fn task_path(view: &Value) -> String {
    format!(
        "/api/sim/draft-lifecycles/{}",
        view["task_id"].as_str().unwrap()
    )
}
pub(super) fn operation_path(view: &Value) -> String {
    format!(
        "{}/operations/{}",
        task_path(view),
        view["operation_id"].as_str().unwrap()
    )
}
pub(super) async fn command(server: &Server, cookie: &str, path: &str, input: Value) -> Reply {
    let reply = server
        .call(
            Method::POST,
            path,
            Some(input.to_string()),
            Some(cookie),
            None,
            None,
            true,
        )
        .await;
    eprintln!("lifecycle command {path}: {}", reply.status);
    reply
}
async fn query(server: &Server, cookie: &str, path: &str) -> Reply {
    server
        .call(Method::GET, path, None, Some(cookie), None, None, false)
        .await
}
fn ok(reply: Reply) -> Value {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.json);
    reply.json
}
pub(super) async fn create(server: &Server, cookie: &str) -> Value {
    ok(command(server, cookie, "/api/sim/draft-lifecycles", scope()).await)
}
pub(super) async fn prepare(server: &Server, cookie: &str, view: &Value) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/prepare", operation_path(view)),
        scope(),
    )
    .await)
}
fn confirmation(view: &Value, action: &str) -> Value {
    json!({"contract_version":2,"scope":SCOPE,"confirmation_ref":view["confirmation"]["confirmation_ref"],
        "display_ticket":view["display_ticket"],"action":action})
}
pub(super) async fn confirm(server: &Server, cookie: &str, view: &Value) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/confirm", operation_path(view)),
        confirmation(view, "CONFIRM"),
    )
    .await)
}
pub(super) fn execution(view: &Value) -> Value {
    json!({"contract_version":2,"scope":SCOPE,"confirmation_ref":view["confirmation"]["confirmation_ref"],
        "task_revision":view["task_revision"],"operation_revision":view["operation_revision"],
        "plan_version":view["plan_version"],"facts_version":view["facts_version"]})
}
async fn execute(server: &Server, cookie: &str, view: &Value) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/execute", operation_path(view)),
        execution(view),
    )
    .await)
}
fn selection(view: &Value, action: &str, parameters: Value) -> Value {
    json!({"contract_version":2,"scope":SCOPE,"selection_ticket":view["selection_ticket"],
        "task_revision":view["task_revision"],"plan_version":view["plan_version"],"facts_version":view["facts_version"],
        "action":action,"parameters":parameters})
}
async fn select(
    server: &Server,
    cookie: &str,
    view: &Value,
    action: &str,
    parameters: Value,
) -> Value {
    ok(command(
        server,
        cookie,
        &format!("{}/actions", task_path(view)),
        selection(view, action, parameters),
    )
    .await)
}
async fn run(server: &Server, cookie: &str, view: &Value) -> (Value, Value) {
    let prepared = prepare(server, cookie, view).await;
    assert_eq!(prepared["next_action"]["kind"], "CONFIRM");
    let confirmed = confirm(server, cookie, &prepared).await;
    assert_eq!(confirmed["next_action"]["kind"], "EXECUTE");
    let executed = execute(server, cookie, &confirmed).await;
    assert_eq!(executed["execution_status"], "SUCCEEDED");
    (confirmed, executed)
}
async fn count(db: &Db, table: &str) -> i64 {
    assert!([
        "mock_draft_lifecycle_claims",
        "audit_events",
        "execution_permits",
        "mock_draft_execution_claims",
        "mock_draft_lifecycle_plans",
        "mock_draft_lifecycle_actions",
        "server_confirmations",
        "security_decisions"
    ]
    .contains(&table));
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_create_modify_cancel_and_historical_receipts() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    assert_eq!(initial["plan_version"], 1);
    assert_eq!(initial["task_revision"], 1);
    assert!(initial["observed_draft"].is_null());
    let (create_confirmation, created) = run(&server, &cookie, &initial).await;
    assert_eq!(created["task_state"], "WAITING_USER");
    assert_eq!(created["next_action"]["kind"], "SELECT_ACTION");
    let current = ok(query(
        &server,
        &cookie,
        &format!("{}/current-draft", operation_path(&created)),
    )
    .await);
    assert_eq!(current["draft"]["draft_version"], 1);
    let modify=select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
    assert_eq!(modify["task_id"], initial["task_id"]);
    assert_eq!(modify["plan_version"], 2);
    assert_eq!(modify["target_parameters"]["amount"], "300.00");
    let (_, modified) = run(&server, &cookie, &modify).await;
    assert_eq!(modified["task_state"], "WAITING_USER");
    assert_eq!(modified["receipt"]["draft_version"], 2);
    let cancel = select(
        &server,
        &cookie,
        &modified,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":modified["receipt"]["draft_id"],"expected_version":2}),
    )
    .await;
    let (_, cancelled) = run(&server, &cookie, &cancel).await;
    assert_eq!(cancelled["task_state"], "CANCELLED");
    assert_eq!(cancelled["operation_state"], "SUCCEEDED");
    assert_eq!(cancelled["plan_version"], 3);
    assert_eq!(cancelled["task_revision"], 6);
    assert_eq!(cancelled["next_action"]["kind"], "FINISHED");
    assert_eq!(cancelled["receipt"]["draft_version"], 2);
    for field in ["draft_id", "business_operation_id"] {
        assert_eq!(created["receipt"][field], modified["receipt"][field]);
        assert_eq!(created["receipt"][field], cancelled["receipt"][field]);
    }
    assert_ne!(created["operation_id"], modified["operation_id"]);
    assert_ne!(modified["operation_id"], cancelled["operation_id"]);
    assert_eq!(server.calls(), 3);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 3);
    assert_eq!(count(&db, "execution_permits").await, 0);
    assert!(!server
        .state
        .0
        .audit
        .events()
        .unwrap()
        .iter()
        .any(|event| event.kind == AuditEventKind::TaskCompleted));
    let before = count(&db, "audit_events").await;
    for old in [&created, &modified, &cancelled] {
        let history = ok(query(&server, &cookie, &operation_path(old)).await);
        assert_eq!(history["receipt"], old["receipt"]);
        assert_eq!(history["task_state"], "CANCELLED");
        assert_eq!(history["current_draft_verified"], false);
        let observation = ok(query(
            &server,
            &cookie,
            &format!("{}/current-draft", operation_path(old)),
        )
        .await);
        assert_eq!(observation["draft"]["draft_version"], 2);
        assert_eq!(
            observation["draft"]["expires_at"],
            current["draft"]["expires_at"]
        );
        assert_eq!(observation["draft"]["status"], "CANCELLED");
    }
    let replay = execute(&server, &cookie, &create_confirmation).await;
    assert_eq!(replay["receipt"], created["receipt"]);
    assert_eq!(replay["replayed"], true);
    let ordinary = ok(query(
        &server,
        &cookie,
        &format!("/api/tasks/{}", initial["task_id"].as_str().unwrap()),
    )
    .await);
    assert_ne!(ordinary["next_action"]["kind"], "CONTINUE");
    assert_eq!(count(&db, "audit_events").await, before);
    assert_eq!(server.calls(), 3);
    server.stop();
    db.finish().await;
}
