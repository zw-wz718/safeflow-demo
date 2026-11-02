use super::process_tests::ProcessServer;
use super::*;
use safeflow_contracts::{ExecutionContext, OperationId, ResourceId};

async fn snapshot(db: &Db) -> Value {
    sqlx::query_scalar("SELECT to_jsonb(d) FROM transfer_drafts_v1 d")
        .fetch_one(&db.scoped)
        .await
        .unwrap()
}

async fn process_run(process: &ProcessServer, cookie: &str, view: &Value) -> (Value, Value) {
    let prepared = ok(process
        .call(
            Method::POST,
            &format!("{}/prepare", operation_path(view)),
            Some(scope()),
            Some(cookie),
        )
        .await);
    let confirmed = ok(process
        .call(
            Method::POST,
            &format!("{}/confirm", operation_path(view)),
            Some(confirmation(&prepared, "CONFIRM")),
            Some(cookie),
        )
        .await);
    let saved = ok(process
        .call(
            Method::POST,
            &format!("{}/execute", operation_path(view)),
            Some(execution(&confirmed)),
            Some(cookie),
        )
        .await);
    assert_eq!(saved["execution_status"], "SUCCEEDED");
    (confirmed, saved)
}

fn sample(sample: &str, previous: Option<&Value>) -> Value {
    let mut input = json!({"sample":sample,"selection":{"account_id":"account_001","payee_query":"小李","payee_id":"payee_008"}});
    if let Some(previous) = previous {
        input["context"] = json!({"task_id":previous["task_id"],"task_revision":previous["task_revision"],"plan_version":previous["plan_version"],"facts_version":previous["facts_version"],"selection_ticket":previous["selection_ticket"]});
    }
    input
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, actual four-process Agent lifecycle"]
async fn restart_agent_real_process_modify_cancel_preserves_identity_deadline_and_replays() {
    let db = Db::new().await;
    let mut previous = None;
    let mut original: Option<Value> = None;
    let mut confirmations: Vec<(Value, Value)> = Vec::new();
    let mut expired_cookie: Option<String> = None;
    for (step, name, version, amount, status) in [
        (0, "CREATE_200", 1, "200.00", "READY"),
        (1, "MODIFY_300", 2, "300.00", "READY"),
        (2, "CANCEL_DRAFT", 2, "300.00", "CANCELLED"),
    ] {
        let process = ProcessServer::start(&db).await;
        let cookie = process.login().await;
        if let (Some(old), Some(view)) = (&expired_cookie, &previous) {
            assert_eq!(
                process
                    .call(Method::GET, &operation_path(view), None, Some(old))
                    .await
                    .status,
                StatusCode::UNAUTHORIZED
            );
        }
        let proposed = ok(process
            .call(
                Method::POST,
                "/api/agent/samples",
                Some(sample(name, previous.as_ref())),
                Some(&cookie),
            )
            .await);
        assert_eq!(proposed["kind"], "DRAFT");
        if step > 0 {
            let (old, _) = &confirmations[step - 1];
            let replay = ok(process
                .call(
                    Method::POST,
                    &format!("{}/execute", operation_path(old)),
                    Some(execution(old)),
                    Some(&cookie),
                )
                .await);
            assert_eq!(replay["replayed"], true);
        }
        let (confirmed, saved) = process_run(&process, &cookie, &proposed["draft"]).await;
        let sql = snapshot(&db).await;
        assert_eq!(sql["business_version"], version);
        assert_eq!(sql["status"], status);
        assert_eq!(sql["details"]["amount"], amount);
        assert_eq!(sql["draft_id"], saved["receipt"]["draft_id"]);
        assert_eq!(
            sql["operation_id"],
            saved["receipt"]["business_operation_id"]
        );
        assert_eq!(sql["task_id"], saved["task_id"]);
        if let Some(first) = &original {
            for field in [
                "draft_id",
                "operation_id",
                "task_id",
                "owner_id",
                "expires_at",
            ] {
                assert_eq!(sql[field], first[field], "original {field}");
            }
            assert_eq!(
                saved["receipt"]["draft_id"],
                previous.as_ref().unwrap()["receipt"]["draft_id"]
            );
        } else {
            original = Some(sql);
        }
        confirmations.push((confirmed, saved.clone()));
        previous = Some(saved);
        expired_cookie = Some(cookie);
        process.kill();
    }
    let process = ProcessServer::start(&db).await;
    let cookie = process.login().await;
    let frozen = snapshot(&db).await;
    for (confirmed, saved) in confirmations {
        let history = ok(process
            .call(Method::GET, &operation_path(&saved), None, Some(&cookie))
            .await);
        assert_eq!(history["receipt"], saved["receipt"]);
        let replay = ok(process
            .call(
                Method::POST,
                &format!("{}/execute", operation_path(&confirmed)),
                Some(execution(&confirmed)),
                Some(&cookie),
            )
            .await);
        assert_eq!(replay["receipt"], saved["receipt"]);
        assert_eq!(replay["replayed"], true);
    }
    assert_eq!(snapshot(&db).await, frozen);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 3);
    assert_eq!(count(&db, "execution_permits").await, 0);
    process.kill();
    db.finish().await;
}

fn identity(server: &Server, cookie: &str) -> crate::http::Identity {
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, cookie.parse().unwrap());
    server
        .state
        .identity(&headers)
        .unwrap_or_else(|_| panic!("actual session required"))
}

async fn memory_empty(server: &Server, cookie: &str, view: &Value) {
    let identity = identity(server, cookie);
    let context = ExecutionContext {
        task_id: serde_json::from_value(view["task_id"].clone()).unwrap(),
        node_id: NodeId::parse("restore_probe").unwrap(),
        operation_id: serde_json::from_value(view["operation_id"].clone()).unwrap(),
        authenticated_user_id: identity.binding.user_id,
        plan_version: view["plan_version"].as_u64().unwrap() as u32,
        idempotency_key: None,
    };
    assert!(server
        .state
        .0
        .draft
        .as_ref()
        .unwrap()
        .store
        .read_created_for_task(&context)
        .await
        .unwrap()
        .is_none());
    assert_eq!(server.calls(), 0);
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, invalid durable restore sources"]
async fn restart_rejects_foreign_task_version_submitted_expired_incomplete_and_old_confirmation() {
    for mutation in [
        "UPDATE transfer_drafts_v1 SET owner_id='user_002'",
        "UPDATE transfer_drafts_v1 SET task_id='task_other'",
        "UPDATE transfer_drafts_v1 SET business_version=2",
        "UPDATE transfer_drafts_v1 SET submitted=true",
        "UPDATE transfer_drafts_v1 SET expires_at=1",
        "UPDATE transfer_drafts_v1 SET status='NEEDS_INFORMATION'",
        "UPDATE transfer_drafts_v1 SET details=NULL,status='NEEDS_INFORMATION'",
    ] {
        let db = Db::new().await;
        let first = server(&db).await;
        let cookie = first.login().await;
        let initial = create(&first, &cookie).await;
        let (_, saved) = run(&first, &cookie, &initial).await;
        let pending = select(&first, &cookie, &saved, "MODIFY_MOCK_DRAFT", json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
        let prepared = prepare(&first, &cookie, &pending).await;
        let old = confirm(&first, &cookie, &prepared).await;
        first.stop();
        db.scoped.execute(mutation).await.unwrap();
        let frozen = snapshot(&db).await;
        let second = server(&db).await;
        let cookie = second.login().await;
        let denied = command(
            &second,
            &cookie,
            &format!("{}/execute", operation_path(&old)),
            execution(&old),
        )
        .await;
        assert_ne!(denied.status, StatusCode::OK, "old session confirmation");
        let denied = query(
            &second,
            &cookie,
            &format!("{}/current-draft", operation_path(&saved)),
        )
        .await;
        assert_ne!(denied.status, StatusCode::OK, "{mutation}");
        memory_empty(&second, &cookie, &saved).await;
        assert_eq!(snapshot(&db).await, frozen);
        assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
        second.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, valid restore still requires a fresh confirmation"]
async fn restart_old_confirmation_cannot_authorize_restored_action_but_new_confirmation_can() {
    let db = Db::new().await;
    let first = server(&db).await;
    let cookie = first.login().await;
    let initial = create(&first, &cookie).await;
    let (_, saved) = run(&first, &cookie, &initial).await;
    let selected = select(&first, &cookie, &saved, "MODIFY_MOCK_DRAFT", json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
    let prepared = prepare(&first, &cookie, &selected).await;
    let old = confirm(&first, &cookie, &prepared).await;
    first.stop();
    let second = server(&db).await;
    let cookie = second.login().await;
    assert_ne!(
        command(
            &second,
            &cookie,
            &format!("{}/execute", operation_path(&old)),
            execution(&old)
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(second.calls(), 0);
    assert_eq!(snapshot(&db).await["business_version"], 1);
    let prepared = prepare(&second, &cookie, &selected).await;
    assert_ne!(
        prepared["confirmation"]["confirmation_ref"],
        old["confirmation"]["confirmation_ref"]
    );
    let confirmed = confirm(&second, &cookie, &prepared).await;
    let modified = execute(&second, &cookie, &confirmed).await;
    assert_eq!(modified["receipt"]["draft_version"], 2);
    assert_eq!(second.calls(), 1);
    second.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, legacy source and new explicit query"]
async fn restart_missing_query_fails_then_explicit_query_is_retained_for_next_restart() {
    let db = Db::new().await;
    let first = server(&db).await;
    let cookie = first.login().await;
    let who = identity(&first, &cookie);
    let id = TaskId::parse(format!("legacy_{}", uuid::Uuid::new_v4().simple())).unwrap();
    let context = ExecutionContext {
        task_id: id.clone(),
        node_id: NodeId::parse("legacy_source").unwrap(),
        operation_id: OperationId::parse(format!("legacy_op_{}", uuid::Uuid::new_v4().simple()))
            .unwrap(),
        authenticated_user_id: who.binding.user_id.clone(),
        plan_version: 1,
        idempotency_key: None,
    };
    first
        .state
        .0
        .draft
        .as_ref()
        .unwrap()
        .store
        .find_candidates(&context, "小李")
        .await
        .unwrap();
    // A valid pre-adapter public history, without any retained search words.
    crate::http::draft_lifecycle::service(&first.state).unwrap_or_else(|_| panic!("service"))
        .create(&who.reference, &id, json!({"payer_account_id":"account_001","payee_id":"payee_008","amount":"200.00","currency":"CNY","remark":"","replaces":null}),1).await.unwrap();
    let initial = ok(query(&first, &cookie, &format!("/api/sim/draft-lifecycles/{id}")).await);
    let (_, saved) = run(&first, &cookie, &initial).await;
    first.stop();
    let second = server(&db).await;
    let cookie = second.login().await;
    let current_path = format!("{}/current-draft", operation_path(&saved));
    assert_eq!(
        query(&second, &cookie, &current_path).await.status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    memory_empty(&second, &cookie, &saved).await;
    let who = identity(&second, &cookie);
    let service = crate::http::draft_lifecycle::service(&second.state)
        .unwrap_or_else(|_| panic!("service"))
        .with_payee_query_for_backend(ResourceId::parse("payee_008").unwrap(), "小李".into())
        .unwrap();
    let selected = service.select(&who.reference,&id,safeflow_storage::LifecycleSelection {
        selection_ticket:saved["selection_ticket"].as_str().unwrap().into(),
        task_revision:saved["task_revision"].as_u64().unwrap(),
        plan_version:1,facts_version:saved["facts_version"].as_u64().unwrap(),
        action:safeflow_contracts::MockDraftAction::Modify,
        parameters:json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}}),
    }).await.unwrap();
    let selected = ok(query(
        &second,
        &cookie,
        &format!(
            "/api/sim/draft-lifecycles/{}/operations/{}",
            id, selected.operation.operation_id
        ),
    )
    .await);
    let (_, modified) = run(&second, &cookie, &selected).await;
    drop(service);
    second.stop();
    let third = server(&db).await;
    let cookie = third.login().await;
    let current = ok(query(
        &third,
        &cookie,
        &format!("{}/current-draft", operation_path(&modified)),
    )
    .await);
    assert_eq!(current["draft"]["draft_version"], 2);
    let cancel = select(
        &third,
        &cookie,
        &modified,
        "CANCEL_MOCK_DRAFT",
        json!({"draft_id":modified["receipt"]["draft_id"],"expected_version":2}),
    )
    .await;
    run(&third, &cookie, &cancel).await;
    assert_eq!(third.calls(), 1);
    third.stop();
    db.finish().await;
}

async fn wait_blocked(db: &Db, pid: i32) {
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
    .expect("restore must reach actual SQL lock wait");
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, deterministic restore lock race"]
async fn restart_sql_lock_wait_rechecks_changed_version_and_revoked_session() {
    for revoke in [false, true] {
        let db = Db::new().await;
        let first = server(&db).await;
        let cookie = first.login().await;
        let initial = create(&first, &cookie).await;
        let (_, saved) = run(&first, &cookie, &initial).await;
        first.stop();
        let second = server(&db).await;
        let cookie = second.login().await;
        let who = identity(&second, &cookie);
        let mut held = db.scoped.begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *held)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(
                json!([
                    "transfer-draft-v1",
                    "user_001",
                    saved["receipt"]["draft_id"]
                ])
                .to_string(),
            )
            .execute(&mut *held)
            .await
            .unwrap();
        let address = second.address;
        let path = format!("{}/current-draft", operation_path(&saved));
        let request_cookie = cookie.clone();
        let job = tokio::spawn(async move {
            Server::call_at(
                address,
                Method::GET,
                &path,
                None,
                Some(&request_cookie),
                None,
                None,
                false,
            )
            .await
        });
        wait_blocked(&db, pid).await;
        memory_empty(&second, &cookie, &saved).await;
        if revoke {
            second.state.0.sessions.revoke(&who.reference).unwrap();
        } else {
            sqlx::query("UPDATE transfer_drafts_v1 SET business_version=2")
                .execute(&mut *held)
                .await
                .unwrap();
        }
        held.commit().await.unwrap();
        assert_ne!(job.await.unwrap().status, StatusCode::OK);
        let fresh = second.login().await;
        memory_empty(&second, &fresh, &saved).await;
        assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
        second.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, restored modification SQL/public/Outbox/commit failures"]
async fn restart_failed_restored_action_never_reexecutes_after_next_rebuild() {
    for mode in 0..4 {
        let db = Db::new().await;
        let first = server(&db).await;
        let cookie = first.login().await;
        let initial = create(&first, &cookie).await;
        let (_, saved) = run(&first, &cookie, &initial).await;
        first.stop();
        let second = server(&db).await;
        let cookie = second.login().await;
        let selected=select(&second,&cookie,&saved,"MODIFY_MOCK_DRAFT",json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
        let prepared = prepare(&second, &cookie, &selected).await;
        let confirmed = confirm(&second, &cookie, &prepared).await;
        let frozen = snapshot(&db).await;
        db.scoped.execute("CREATE FUNCTION restore_fail() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated restart settlement failure'; END $$").await.unwrap();
        db.scoped.execute(match mode {
            0=>"CREATE TRIGGER restore_fail BEFORE UPDATE ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION restore_fail()",
            1=>"CREATE TRIGGER restore_fail BEFORE UPDATE ON mock_draft_lifecycle_claims FOR EACH ROW WHEN (NEW.status='SUCCEEDED') EXECUTE FUNCTION restore_fail()",
            2=>"CREATE TRIGGER restore_fail BEFORE INSERT ON public_audit_outbox FOR EACH ROW WHEN (NEW.envelope #>> '{public_event,kind}'='OPERATION_TRANSITIONED') EXECUTE FUNCTION restore_fail()",
            _=>"CREATE CONSTRAINT TRIGGER restore_fail AFTER UPDATE ON mock_draft_lifecycle_claims DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.status='SUCCEEDED') EXECUTE FUNCTION restore_fail()",
        }).await.unwrap();
        assert_eq!(
            command(
                &second,
                &cookie,
                &format!("{}/execute", operation_path(&confirmed)),
                execution(&confirmed)
            )
            .await
            .status,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            second.calls(),
            1,
            "restored business memory changed before failed settlement"
        );
        assert_eq!(snapshot(&db).await, frozen);
        second.stop();
        db.scoped
            .execute(match mode {
                0 => "DROP TRIGGER restore_fail ON transfer_drafts_v1",
                1 | 3 => "DROP TRIGGER restore_fail ON mock_draft_lifecycle_claims",
                _ => "DROP TRIGGER restore_fail ON public_audit_outbox",
            })
            .await
            .unwrap();
        let third = server(&db).await;
        let cookie = third.login().await;
        let unknown = execute(&third, &cookie, &confirmed).await;
        assert_eq!(unknown["execution_status"], "RESULT_UNKNOWN");
        assert!(unknown["receipt"].is_null());
        assert_ne!(
            query(
                &third,
                &cookie,
                &format!("{}/current-draft", operation_path(&confirmed))
            )
            .await
            .status,
            StatusCode::OK
        );
        assert_ne!(
            command(
                &third,
                &cookie,
                &format!("{}/actions", task_path(&unknown)),
                selection(
                    &unknown,
                    "CANCEL_MOCK_DRAFT",
                    json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1})
                )
            )
            .await
            .status,
            StatusCode::OK
        );
        memory_empty(&third, &cookie, &confirmed).await;
        assert_eq!(snapshot(&db).await, frozen);
        assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 2);
        third.stop();
        db.finish().await;
    }
}

#[tokio::test]
#[ignore = "isolated PostgreSQL, locks survive restored Tool through SQL/public settlement"]
async fn restart_restored_modification_blocks_concurrent_stale_sql_until_commit() {
    let db = Db::new().await;
    let first = server(&db).await;
    let cookie = first.login().await;
    let initial = create(&first, &cookie).await;
    let (_, saved) = run(&first, &cookie, &initial).await;
    first.stop();
    let second = server(&db).await;
    let cookie = second.login().await;
    let selected = select(&second, &cookie, &saved, "MODIFY_MOCK_DRAFT", json!({"draft_id":saved["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).await;
    let prepared = prepare(&second, &cookie, &selected).await;
    let confirmed = confirm(&second, &cookie, &prepared).await;
    let key =
        i64::from_le_bytes(*uuid::Uuid::new_v4().as_bytes().first_chunk::<8>().unwrap()) & i64::MAX;
    let mut held = db.scoped.begin().await.unwrap();
    let holder: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *held)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(key)
        .execute(&mut *held)
        .await
        .unwrap();
    db.scoped.execute(format!("CREATE FUNCTION restore_wait() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}::bigint); RETURN NEW; END $$; CREATE TRIGGER restore_wait BEFORE UPDATE ON transfer_drafts_v1 FOR EACH ROW EXECUTE FUNCTION restore_wait()").as_str()).await.unwrap();
    let address = second.address;
    let path = format!("{}/execute", operation_path(&confirmed));
    let input = execution(&confirmed).to_string();
    let request_cookie = cookie.clone();
    let job = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &path,
            Some(input),
            Some(&request_cookie),
            None,
            None,
            true,
        )
        .await
    });
    wait_blocked(&db, holder).await;
    assert_eq!(
        second.calls(),
        1,
        "actual restored Tool ran before save trigger"
    );
    let executor: i32 = sqlx::query_scalar(
        "SELECT pid FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)) LIMIT 1",
    )
    .bind(holder)
    .fetch_one(&db.admin)
    .await
    .unwrap();
    let pool = db.scoped.clone();
    let stale = tokio::spawn(async move {
        sqlx::query("UPDATE transfer_drafts_v1 SET business_version=99 WHERE business_version=1")
            .execute(&pool)
            .await
            .unwrap()
            .rows_affected()
    });
    wait_blocked(&db, executor).await;
    assert!(!stale.is_finished());
    held.rollback().await.unwrap();
    let modified = ok(job.await.unwrap());
    assert_eq!(modified["execution_status"], "SUCCEEDED");
    assert_eq!(
        stale.await.unwrap(),
        0,
        "stale writer observes committed version 2"
    );
    let sql = snapshot(&db).await;
    assert_eq!(sql["business_version"], 2);
    assert_eq!(sql["details"]["amount"], "300.00");
    assert_eq!(modified["receipt"]["draft_version"], 2);
    second.stop();
    db.finish().await;
}
