use super::*;
use safeflow_contracts::{ExecutionContext, ResourceId};
use safeflow_transfer_integration::{DraftStatus, StoredDraft};

fn identity(server: &Server, cookie: &str) -> super::super::super::Identity {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    server
        .state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("actual API session required"))
}

async fn actual(server: &Server, cookie: &str, view: &Value) -> StoredDraft {
    let identity = identity(server, cookie);
    let context = ExecutionContext {
        task_id: serde_json::from_value(view["task_id"].clone()).unwrap(),
        node_id: NodeId::parse("bridge_readback").unwrap(),
        operation_id: serde_json::from_value(view["operation_id"].clone()).unwrap(),
        authenticated_user_id: identity.binding.user_id,
        plan_version: view["plan_version"].as_u64().unwrap() as u32,
        idempotency_key: None,
    };
    server
        .state
        .0
        .draft
        .as_ref()
        .unwrap()
        .store
        .read_created_for_task(&context)
        .await
        .unwrap()
        .unwrap()
}

async fn assert_sql_actual(server: &Server, cookie: &str, view: &Value) -> StoredDraft {
    let expected = actual(server, cookie, view).await;
    let identity = identity(server, cookie);
    let saved = server
        .state
        .transfer_sql_reader_for_backend()
        .draft(&identity.reference, &expected.draft_id)
        .await
        .unwrap();
    assert_eq!(
        saved, expected,
        "includes full balance/account/payee metadata, not just ToolResult fields"
    );
    assert_eq!(saved.user_id.as_str(), "user_001");
    assert_eq!(
        serde_json::to_value(&saved.task_id).unwrap(),
        view["task_id"]
    );
    assert_eq!(
        serde_json::to_value(&saved.operation_id).unwrap(),
        view["receipt"]["business_operation_id"]
    );
    saved
}

async fn sql_drafts(db: &Db) -> Value {
    sqlx::query_scalar("SELECT coalesce(jsonb_agg(to_jsonb(d) ORDER BY owner_id,draft_id),'[]') FROM transfer_drafts_v1 d")
        .fetch_one(&db.scoped).await.unwrap()
}

fn draft_snapshot(draft: &StoredDraft) -> Value {
    json!({"draft_id":draft.draft_id,"operation_id":draft.operation_id,"task_id":draft.task_id,
        "user_id":draft.user_id,"business_version":draft.plan_version,"status":format!("{:?}",draft.status),
        "expires_at":draft.expires_at,"details":draft.details})
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn sql_bridge_actual_three_actions_and_original_replays_do_not_resave() {
    let db = Db::new().await;
    db.scoped.execute("CREATE TABLE bridge_write_probe(id int); CREATE FUNCTION bridge_probe() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO bridge_write_probe VALUES(1); RETURN NEW; END $$; CREATE TRIGGER bridge_probe AFTER INSERT OR UPDATE ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION bridge_probe()").await.unwrap();
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    let (create_c, created) = run(&server, &cookie, &initial).await;
    let first = assert_sql_actual(&server, &cookie, &created).await;
    assert_eq!(first.plan_version, 1);
    let modify = select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":first.draft_id,"expected_version":1,"changes":{"amount":"300.00","remark":""}})).await;
    let (modify_c, modified) = run(&server, &cookie, &modify).await;
    let second = assert_sql_actual(&server, &cookie, &modified).await;
    assert_eq!(second.plan_version, 2);
    assert_eq!(second.details.as_ref().unwrap().amount.as_str(), "300.00");
    let cancel = select(
        &server,
        &cookie,
        &modified,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":first.draft_id,"expected_version":2}),
    )
    .await;
    let (cancel_c, cancelled) = run(&server, &cookie, &cancel).await;
    let third = assert_sql_actual(&server, &cookie, &cancelled).await;
    assert_eq!(third.status, DraftStatus::Cancelled);
    assert_eq!(third.plan_version, 2);
    assert_eq!(cancelled["plan_version"], 3);
    assert_eq!(third.expires_at, first.expires_at);
    assert_eq!(third.details, second.details);
    let writes: i64 = sqlx::query_scalar("SELECT count(*) FROM bridge_write_probe")
        .fetch_one(&db.scoped)
        .await
        .unwrap();
    assert_eq!(writes, 3);
    let frozen = sql_drafts(&db).await;
    db.scoped.execute("CREATE FUNCTION bridge_reject_save() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'replay attempted SQL save'; END $$; CREATE TRIGGER bridge_reject_save BEFORE INSERT OR UPDATE ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION bridge_reject_save()").await.unwrap();
    for (confirmed, original) in [
        (&create_c, &created),
        (&modify_c, &modified),
        (&cancel_c, &cancelled),
    ] {
        let replay = execute(&server, &cookie, confirmed).await;
        assert_eq!(replay["replayed"], true);
        assert_eq!(replay["receipt"], original["receipt"]);
    }
    assert_eq!(sql_drafts(&db).await, frozen);
    assert_eq!(server.calls(), 3);
    assert_eq!(count(&db, "execution_permits").await, 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn sql_bridge_save_settlement_audit_and_commit_failures_leave_claim_without_success() {
    for mode in 0..4 {
        let db = Db::new().await;
        let server = server(&db).await;
        let cookie = server.login().await;
        let initial = create(&server, &cookie).await;
        let selected = if mode == 0 {
            initial
        } else {
            let (_, created) = run(&server, &cookie, &initial).await;
            select(&server,&cookie,&created,if mode==2 {"CANCEL_MOCK_DRAFT"} else {"MODIFY_MOCK_DRAFT"},
                if mode==2 {json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1})}
                else {json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})}).await
        };
        let prepared = prepare(&server, &cookie, &selected).await;
        let confirmed = confirm(&server, &cookie, &prepared).await;
        let before = sql_drafts(&db).await;
        let calls = server.calls();
        db.scoped.execute("CREATE FUNCTION bridge_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated bridge failure'; END $$").await.unwrap();
        db.scoped.execute(match mode {
            0 => "CREATE TRIGGER bridge_fail BEFORE INSERT ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION bridge_fail()",
            1 => "CREATE TRIGGER bridge_fail BEFORE UPDATE ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION bridge_fail()",
            2 => "CREATE TRIGGER bridge_fail AFTER INSERT ON audit_events FOR EACH ROW WHEN (NEW.kind='OPERATION_TRANSITIONED') EXECUTE FUNCTION bridge_fail()",
            _ => "CREATE CONSTRAINT TRIGGER bridge_fail AFTER UPDATE ON mock_draft_lifecycle_claims DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.status='SUCCEEDED') EXECUTE FUNCTION bridge_fail()",
        }).await.unwrap();
        let failed = command(
            &server,
            &cookie,
            &format!("{}/execute", operation_path(&confirmed)),
            execution(&confirmed),
        )
        .await;
        assert_eq!(
            failed.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "mode {mode}: {}",
            failed.json
        );
        assert_eq!(sql_drafts(&db).await, before);
        let memory = actual(&server, &cookie, &confirmed).await;
        assert_eq!(
            memory.status,
            if mode == 2 {
                DraftStatus::Cancelled
            } else {
                DraftStatus::ReadyToConfirm
            }
        );
        assert_eq!(
            memory.plan_version,
            if mode == 0 || mode == 2 { 1 } else { 2 }
        );
        assert_eq!(
            server.calls(),
            calls + 1,
            "memory action did happen and is not DB rollback"
        );
        let original = ok(query(&server, &cookie, &operation_path(&confirmed)).await);
        assert_eq!(original["execution_status"], "RESULT_UNKNOWN");
        assert_eq!(original["operation_state"], "DRAFT");
        assert!(original["receipt"].is_null());
        let success_audits: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE operation_id=$1 AND kind='OPERATION_TRANSITIONED'")
            .bind(confirmed["operation_id"].as_str().unwrap()).fetch_one(&db.scoped).await.unwrap();
        assert_eq!(success_audits, 0);
        // Remove the injected fault: the original claim still forbids a second call/save.
        db.scoped
            .execute(match mode {
                0 | 1 => "DROP TRIGGER bridge_fail ON transfer_drafts_v1",
                2 => "DROP TRIGGER bridge_fail ON audit_events",
                _ => "DROP TRIGGER bridge_fail ON mock_draft_lifecycle_claims",
            })
            .await
            .unwrap();
        assert_eq!(
            execute(&server, &cookie, &confirmed).await["execution_status"],
            "RESULT_UNKNOWN"
        );
        assert_eq!(server.calls(), calls + 1);
        assert_eq!(sql_drafts(&db).await, before);
        let denied = command(&server,&cookie,&format!("{}/actions",task_path(&original)),selection(&original,"MODIFY_MOCK_DRAFT",json!({"draft_id":memory.draft_id,"expected_version":memory.plan_version,"changes":{"amount":"400.00"}}))).await;
        assert_ne!(denied.status, StatusCode::OK);
        server.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn sql_bridge_divergent_or_missing_sql_prevents_tool_and_old_memory_backfill() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    let (_, created) = run(&server, &cookie, &initial).await;
    let memory = actual(&server, &cookie, &created).await;
    let input = selection(
        &created,
        "MODIFY_MOCK_DRAFT",
        json!({"draft_id":memory.draft_id,"expected_version":1,"changes":{"amount":"300.00"}}),
    );
    sqlx::query("UPDATE transfer_drafts_v1 SET business_version=2")
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/actions", task_path(&created)),
            input.clone()
        )
        .await
        .status,
        StatusCode::OK
    );
    sqlx::query("DELETE FROM transfer_drafts_v1")
        .execute(&db.scoped)
        .await
        .unwrap();
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/actions", task_path(&created)),
            input
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(sql_drafts(&db).await, json!([]));
    assert_eq!(server.calls(), 1);
    assert_eq!(actual(&server, &cookie, &created).await, memory);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and rebuilt ApiState"]
async fn sql_bridge_rebuilt_api_reads_sql_receipt_and_restores_for_edit() {
    let db = Db::new().await;
    let first = server(&db).await;
    let cookie = first.login().await;
    let initial = create(&first, &cookie).await;
    let (confirmed, created) = run(&first, &cookie, &initial).await;
    let saved = assert_sql_actual(&first, &cookie, &created).await;
    let probe = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "http::tests::lifecycle_tests::sql_bridge_tests::sql_bridge_read_process_child",
            "--ignored",
            "--nocapture",
        ])
        .env("SAFEFLOW_SQL_BRIDGE_SCHEMA", &db.namespace)
        .env(
            "SAFEFLOW_SQL_BRIDGE_DRAFT",
            draft_snapshot(&saved).to_string(),
        )
        .env("SAFEFLOW_SQL_BRIDGE_VIEW", created.to_string())
        .output()
        .unwrap();
    assert!(
        probe.status.success(),
        "fresh read process failed: {} {}",
        String::from_utf8_lossy(&probe.stdout),
        String::from_utf8_lossy(&probe.stderr)
    );
    assert!(String::from_utf8_lossy(&probe.stdout).contains("FRESH_SQL_AND_PUBLIC_RECEIPT_READ_OK"));
    first.stop();
    let second = server(&db).await;
    let fresh = second.login().await;
    let identity = identity(&second, &fresh);
    assert_eq!(
        second
            .state
            .transfer_sql_reader_for_backend()
            .draft(&identity.reference, &saved.draft_id)
            .await
            .unwrap(),
        saved
    );
    let history = ok(query(&second, &fresh, &operation_path(&created)).await);
    assert_eq!(history["receipt"], created["receipt"]);
    assert_eq!(
        execute(&second, &fresh, &confirmed).await["receipt"],
        created["receipt"]
    );
    for action in ["MODIFY_MOCK_DRAFT"] {
        let parameters = if action == "MODIFY_MOCK_DRAFT" {
            json!({"draft_id":saved.draft_id,"expected_version":1,"changes":{"amount":"300.00"}})
        } else {
            json!({"draft_id":saved.draft_id,"expected_version":1})
        };
        assert_eq!(
            command(
                &second,
                &fresh,
                &format!("{}/actions", task_path(&history)),
                selection(&history, action, parameters)
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    assert_eq!(second.calls(), 0);
    assert_eq!(
        second
            .state
            .transfer_sql_reader_for_backend()
            .draft(&identity.reference, &saved.draft_id)
            .await
            .unwrap(),
        saved
    );
    let absent = ResourceId::parse("no-automatic-replacement-draft").unwrap();
    assert_eq!(
        second
            .state
            .transfer_sql_reader_for_backend()
            .draft(&identity.reference, &absent)
            .await
            .unwrap_err(),
        StorageError::NotFound
    );
    second.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "parent owns isolated schema and supplies expected actual snapshot"]
async fn sql_bridge_read_process_child() {
    let schema = std::env::var("SAFEFLOW_SQL_BRIDGE_SCHEMA").expect("parent required");
    let suffix = schema
        .strip_prefix("sf_http_test_")
        .expect("isolated API schema only");
    assert_eq!(suffix.len(), 32);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    let repo = PgRepository::connect_existing(
        &std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap(),
        &schema,
    )
    .await
    .unwrap();
    let expected: Value =
        serde_json::from_str(&std::env::var("SAFEFLOW_SQL_BRIDGE_DRAFT").unwrap()).unwrap();
    let view: Value =
        serde_json::from_str(&std::env::var("SAFEFLOW_SQL_BRIDGE_VIEW").unwrap()).unwrap();
    let mut config = ApiConfig::local_simulation(Zeroizing::new(SECRET.into()));
    config.draft_confirmation = Some(
        DraftConfirmationConfig::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            600,
            "isolated-http-lifecycle-v2",
        )
        .unwrap()
        .with_lifecycle_v2()
        .unwrap(),
    );
    let state = ApiState::for_project_simulation(repo.clone(), config).unwrap();
    let cookie = state.0.sessions.issue(SECRET, None).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        cookie.split(';').next().unwrap().parse().unwrap(),
    );
    let identity = state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("new process API session required"));
    let id = serde_json::from_value(expected["draft_id"].clone()).unwrap();
    let actual = state
        .transfer_sql_reader_for_backend()
        .draft(&identity.reference, &id)
        .await
        .unwrap();
    assert_eq!(draft_snapshot(&actual), expected);
    let task = serde_json::from_value(view["task_id"].clone()).unwrap();
    let op = serde_json::from_value(view["operation_id"].clone()).unwrap();
    let history = crate::http::draft_lifecycle::service(&state)
        .unwrap_or_else(|_| panic!("lifecycle assembly required"))
        .query(&identity.reference, &task, Some(&op))
        .await
        .unwrap();
    let receipt = history.receipt.unwrap();
    let serialized = serde_json::to_value(receipt).unwrap();
    for (field, expected) in view["receipt"].as_object().unwrap() {
        assert_eq!(&serialized[field], expected, "public receipt field {field}");
    }
    assert!(state.0.audit.events().unwrap().is_empty());
    repo.close().await;
    println!("FRESH_SQL_AND_PUBLIC_RECEIPT_READ_OK");
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and deterministic SQL wait/session revoke"]
async fn sql_bridge_session_revoked_during_real_save_wait_rolls_back_success() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &initial).await;
    let confirmed = confirm(&server, &cookie, &prepared).await;
    let identity = identity(&server, &cookie);
    let mut held = db.scoped.begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    // The save trigger blocks only after Runtime has completed and full readback was taken.
    let key =
        i64::from_le_bytes(*uuid::Uuid::new_v4().as_bytes().first_chunk::<8>().unwrap()) & i64::MAX;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    db.scoped.execute(format!("CREATE FUNCTION bridge_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}::bigint); RETURN NEW; END $$; CREATE TRIGGER bridge_wait BEFORE INSERT ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION bridge_wait()").as_str()).await.unwrap();
    let address = server.address;
    let request_cookie = cookie.clone();
    let path = format!("{}/execute", operation_path(&confirmed));
    let request = execution(&confirmed).to_string();
    let job = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &path,
            Some(request),
            Some(&request_cookie),
            None,
            None,
            true,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let blocked: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",
            )
            .bind(pid)
            .fetch_one(&db.admin)
            .await
            .unwrap();
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual SQL save did not block");
    assert_eq!(server.calls(), 1);
    server.state.0.sessions.revoke(&identity.reference).unwrap();
    held.rollback().await.unwrap();
    assert_ne!(job.await.unwrap().status, StatusCode::OK);
    assert_eq!(sql_drafts(&db).await, json!([]));
    let fresh = server.login().await;
    let original = ok(query(&server, &fresh, &operation_path(&confirmed)).await);
    assert_eq!(original["execution_status"], "RESULT_UNKNOWN");
    assert!(original["receipt"].is_null());
    assert_eq!(
        execute(&server, &fresh, &confirmed).await["execution_status"],
        "RESULT_UNKNOWN"
    );
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and actual lifecycle HTTP"]
async fn sql_bridge_version_change_after_confirmation_and_foreign_owner_never_reach_tool() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    let (_, created) = run(&server, &cookie, &initial).await;
    let selected=select(&server,&cookie,&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
    let prepared = prepare(&server, &cookie, &selected).await;
    let confirmed = confirm(&server, &cookie, &prepared).await;
    sqlx::query("UPDATE transfer_drafts_v1 SET business_version=2")
        .execute(&db.scoped)
        .await
        .unwrap();
    let conflict = command(
        &server,
        &cookie,
        &format!("{}/execute", operation_path(&confirmed)),
        execution(&confirmed),
    )
    .await;
    assert_eq!(conflict.status, StatusCode::CONFLICT);
    assert_eq!(server.calls(), 1);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    let foreign = Server::with_draft(
        &db,
        "user_002",
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
    .await;
    let other = foreign.login().await;
    let denied = command(
        &foreign,
        &other,
        &format!("{}/execute", operation_path(&confirmed)),
        execution(&confirmed),
    )
    .await;
    assert_ne!(denied.status, StatusCode::OK);
    assert_eq!(foreign.calls(), 0);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    server.stop();
    foreign.stop();
    db.finish().await;
}
