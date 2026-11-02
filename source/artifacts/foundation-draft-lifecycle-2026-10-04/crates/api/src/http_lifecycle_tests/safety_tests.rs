use super::*;

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_get_head_perimeter_expired_create_and_lock_wait_session() {
    let db = Db::new().await;
    let server = Server::with_draft(
        &db,
        "user_001",
        Duration::from_secs(30),
        Duration::from_secs(120),
        Some(
            DraftConfirmationConfig::new(
                Duration::from_secs(2),
                Duration::from_secs(60),
                300,
                "isolated-short-confirmation-v2",
            )
            .unwrap()
            .with_lifecycle_v2()
            .unwrap(),
        ),
    )
    .await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &first).await;
    let snapshot: Value = sqlx::query_scalar(
        "SELECT row_to_json(f)::jsonb FROM security_decision_time_fence f WHERE scope=1",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    let audits = count(&db, "audit_events").await;
    assert_eq!(
        query(&server, &cookie, &task_path(&first)).await.json,
        prepared
    );
    let head = server
        .call(
            Method::HEAD,
            &operation_path(&first),
            None,
            Some(&cookie),
            None,
            None,
            false,
        )
        .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.json.is_null());
    assert_eq!(head.headers[header::CACHE_CONTROL], "no-store");
    let after: Value = sqlx::query_scalar(
        "SELECT row_to_json(f)::jsonb FROM security_decision_time_fence f WHERE scope=1",
    )
    .fetch_one(&db.scoped)
    .await
    .unwrap();
    assert_eq!(snapshot, after);
    assert_eq!(count(&db, "audit_events").await, audits);
    assert_eq!(
        server
            .call(
                Method::POST,
                "/api/sim/draft-lifecycles",
                Some(scope().to_string()),
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
                Method::GET,
                &task_path(&first),
                None,
                Some(&cookie),
                None,
                Some("http://invalid.example"),
                false
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        query(
            &server,
            &cookie,
            &format!("{}?action=CONFIRM", task_path(&first))
        )
        .await
        .status,
        StatusCode::BAD_REQUEST
    );
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", operation_path(&first)),
            confirmation(&prepared, "CONFIRM")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/prepare", operation_path(&first)),
            scope()
        )
        .await
        .status,
        StatusCode::OK
    );
    let expired = ok(query(&server, &cookie, &task_path(&first)).await);
    let replacement = select(
        &server,
        &cookie,
        &expired,
        "CREATE_MOCK_DRAFT",
        first["parameters"].clone(),
    )
    .await;
    let mut held = db.scoped.begin().await.unwrap();
    sqlx::query("SELECT task_id FROM tasks WHERE task_id=$1 FOR UPDATE")
        .bind(first["task_id"].as_str().unwrap())
        .fetch_one(&mut *held)
        .await
        .unwrap();
    let address = server.address;
    let request_cookie = cookie.clone();
    let path = format!("{}/prepare", operation_path(&replacement));
    let pending = tokio::spawn(async move {
        Server::call_at(
            address,
            Method::POST,
            &path,
            Some(scope().to_string()),
            Some(&request_cookie),
            None,
            None,
            true,
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let parsed = Cookie::parse(cookie).unwrap();
    server
        .state
        .0
        .sessions
        .revoke(&token_reference(parsed.value()).unwrap())
        .unwrap();
    held.rollback().await.unwrap();
    assert_eq!(pending.await.unwrap().status, StatusCode::UNAUTHORIZED);
    assert_eq!(server.calls(), 0);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_strict_nested_changes_versions_and_route_isolation() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let parsed = Cookie::parse(&cookie).unwrap();
    let reference = token_reference(parsed.value()).unwrap();
    let legacy = safeflow_storage::PersistenceService::for_trusted_backend(
        db.repo.clone(),
        server.state.0.sessions.clone(),
    );
    assert_eq!(
        legacy
            .recover(
                &reference,
                &TaskId::parse(first["task_id"].as_str().unwrap()).unwrap(),
            )
            .await,
        Err(StorageError::RuntimeContinuationRequired)
    );
    let (_, created) = run(&server, &cookie, &first).await;
    let path = format!("{}/actions", task_path(&created));
    let duplicate=selection(&created,"MODIFY_MOCK_DRAFT",json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":{"amount":"300.00"}})).to_string().replace("\"amount\":\"300.00\"","\"amount\":\"200.00\",\"\\u0061mount\":\"300.00\"");
    assert_eq!(
        server
            .call(
                Method::POST,
                &path,
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
    for changes in [
        json!({}),
        Value::Null,
        json!({"amount":null}),
        json!({"amount":"300.00","unknown":"x"}),
        json!({"amount":300.0}),
        json!({"amount":{"value":"300.00"}}),
    ] {
        let input = selection(
            &created,
            "MODIFY_MOCK_DRAFT",
            json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1,"changes":changes}),
        );
        assert_ne!(
            command(&server, &cookie, &path, input).await.status,
            StatusCode::OK
        );
    }
    for field in ["task_revision", "plan_version", "facts_version"] {
        let mut bad = selection(
            &created,
            "CANCEL_MOCK_DRAFT",
            json!({"draft_id":created["receipt"]["draft_id"],"expected_version":1}),
        );
        bad[field] = json!(bad[field].as_u64().unwrap() + 1);
        assert_ne!(
            command(&server, &cookie, &path, bad).await.status,
            StatusCode::OK
        );
    }
    let mut extra = scope();
    extra["owner"] = json!("user_002");
    assert_eq!(
        command(&server, &cookie, "/api/sim/draft-lifecycles", extra)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        command(
            &server,
            &cookie,
            "/api/sim/draft-lifecycles",
            json!({"contract_version":1,"scope":SCOPE})
        )
        .await
        .status,
        StatusCode::NOT_IMPLEMENTED
    );
    let old = ok(command(&server, &cookie, "/api/sim/draft-confirmations", json!({})).await);
    assert_ne!(
        query(&server, &cookie, &task_path(&old)).await.status,
        StatusCode::OK
    );
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!(
                "/api/sim/draft-confirmations/{}/prepare",
                first["task_id"].as_str().unwrap()
            ),
            json!({})
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_ne!(
        query(
            &server,
            &cookie,
            &format!(
                "/api/sim/draft-executions/{}/operations/{}",
                first["task_id"].as_str().unwrap(),
                first["operation_id"].as_str().unwrap()
            )
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_ne!(command(&server,&cookie,"/api/chat",json!({"kind":"CONTINUE","task_id":created["task_id"],"waiting_node_id":"decision_1","task_revision":created["task_revision"],"plan_version":1,"facts_version":7})).await.status,StatusCode::OK);
    assert_eq!(server.calls(), 1);
    assert_eq!(count(&db, "mock_draft_lifecycle_plans").await, 1);
    let disabled = Server::new(
        &db,
        "user_001",
        Duration::from_secs(15),
        Duration::from_secs(120),
    )
    .await;
    assert_eq!(
        command(
            &disabled,
            &disabled.login().await,
            "/api/sim/draft-lifecycles",
            scope()
        )
        .await
        .status,
        StatusCode::NOT_IMPLEMENTED
    );
    disabled.stop();
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_confirm_reject_scope_session_and_expiry_never_execute() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &first).await;
    assert_eq!(
        prepare(&server, &cookie, &first).await["confirmation"],
        prepared["confirmation"]
    );
    for field in ["scope", "confirmation_ref", "display_ticket"] {
        let mut wrong = confirmation(&prepared, "CONFIRM");
        wrong[field] = json!(if field == "display_ticket" {
            "lifecycle_00000000000000000000000000000000"
        } else {
            "WRONG"
        });
        assert_ne!(
            command(
                &server,
                &cookie,
                &format!("{}/confirm", operation_path(&first)),
                wrong
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    let fresh = server.login().await;
    assert_ne!(
        command(
            &server,
            &fresh,
            &format!("{}/confirm", operation_path(&first)),
            confirmation(&prepared, "CONFIRM")
        )
        .await
        .status,
        StatusCode::OK
    );
    let new_prepared = prepare(&server, &fresh, &first).await;
    let rejected = ok(command(
        &server,
        &fresh,
        &format!("{}/confirm", operation_path(&first)),
        confirmation(&new_prepared, "REJECT"),
    )
    .await);
    assert_eq!(rejected["operation_state"], "DRAFT");
    assert_eq!(rejected["task_state"], "RUNNING");
    assert_ne!(
        command(
            &server,
            &fresh,
            &format!("{}/execute", operation_path(&first)),
            execution(&new_prepared)
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(server.calls(), 0);
    let replacement = select(
        &server,
        &fresh,
        &rejected,
        "CREATE_MOCK_DRAFT",
        first["parameters"].clone(),
    )
    .await;
    let prepared = prepare(&server, &fresh, &replacement).await;
    let confirmed = confirm(&server, &fresh, &prepared).await;
    sqlx::query("UPDATE server_confirmations SET prepared_at=prepared_at-interval '10 minutes',confirmed_at=confirmed_at-interval '10 minutes',expires_at=expires_at-interval '10 minutes' WHERE confirmation_ref=$1").bind(confirmed["confirmation"]["confirmation_ref"].as_str().unwrap()).execute(&db.scoped).await.unwrap();
    assert_ne!(
        command(
            &server,
            &fresh,
            &format!("{}/execute", operation_path(&confirmed)),
            execution(&confirmed)
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(server.calls(), 0);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 0);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_claim_rollback_and_settlement_failure_keep_unknown_barrier() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &first).await;
    let confirmed = confirm(&server, &cookie, &prepared).await;
    db.scoped.execute("CREATE FUNCTION reject_lifecycle_claim() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'isolated claim failure'; END $$; CREATE TRIGGER reject_lifecycle_claim BEFORE INSERT ON mock_draft_lifecycle_claims FOR EACH ROW EXECUTE FUNCTION reject_lifecycle_claim();").await.unwrap();
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/execute", operation_path(&confirmed)),
            execution(&confirmed)
        )
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(server.calls(), 0);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 0);
    db.scoped.execute("DROP TRIGGER reject_lifecycle_claim ON mock_draft_lifecycle_claims; CREATE FUNCTION reject_lifecycle_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.status='SUCCEEDED' THEN RAISE EXCEPTION 'isolated settlement failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_lifecycle_settlement BEFORE UPDATE ON mock_draft_lifecycle_claims FOR EACH ROW EXECUTE FUNCTION reject_lifecycle_settlement();").await.unwrap();
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/execute", operation_path(&confirmed)),
            execution(&confirmed)
        )
        .await
        .status,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(server.calls(), 1);
    let original = ok(query(&server, &cookie, &operation_path(&first)).await);
    assert_eq!(original["execution_status"], "RESULT_UNKNOWN");
    assert_eq!(original["next_action"]["kind"], "QUERY_ORIGINAL_OPERATION");
    assert!(original["receipt"].is_null());
    let actual = ok(query(
        &server,
        &cookie,
        &format!("{}/current-draft", operation_path(&first)),
    )
    .await);
    assert_eq!(actual["draft"]["draft_version"], 1);
    for action in [
        "CREATE_MOCK_DRAFT",
        "MODIFY_MOCK_DRAFT",
        "CANCEL_MOCK_DRAFT",
    ] {
        assert_ne!(
            command(
                &server,
                &cookie,
                &format!("{}/actions", task_path(&first)),
                selection(&original, action, first["parameters"].clone())
            )
            .await
            .status,
            StatusCode::OK
        );
    }
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/prepare", operation_path(&first)),
            scope()
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(
        execute(&server, &cookie, &confirmed).await["execution_status"],
        "RESULT_UNKNOWN"
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_corrupt_history_sources_and_audits_fail_closed() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    for sql in [
        "UPDATE mock_draft_lifecycle_claims SET source=jsonb_set(source,'{session_binding_version}','999') WHERE task_id=$1",
        "UPDATE mock_draft_lifecycle_claims SET receipt=jsonb_set(receipt,'{draft_version}','99') WHERE task_id=$1",
        "UPDATE mock_draft_lifecycle_plans SET completed_prefix='[\"bogus\"]' WHERE task_id=$1",
        "UPDATE mock_draft_lifecycle_actions SET request=jsonb_set(request,'{parameters,amount}','\"999.00\"') WHERE task_id=$1",
        "DELETE FROM audit_events WHERE task_id=$1 AND kind='CHECKPOINT_SAVED'",
        "UPDATE audit_events SET details='{}' WHERE task_id=$1 AND kind='OPERATION_TRANSITIONED'",
        "UPDATE server_confirmations SET arguments_hash=repeat('0',64) WHERE task_id=$1",
        "UPDATE security_decisions SET assembly_metadata='{}' WHERE task_id=$1",
        "DELETE FROM audit_events WHERE task_id=$1 AND kind='EXPLICIT_CONFIRMATION_RECORDED'",
        "DELETE FROM audit_events WHERE task_id=$1 AND kind='SECURITY_DECISION_RECORDED'",
        "DELETE FROM audit_events WHERE task_id=$1 AND kind='CONFIRMATION_PREPARED'",
    ] {
        let first=create(&server,&cookie).await; let (_,created)=run(&server,&cookie,&first).await;
        let mut tx = db.scoped.begin().await.unwrap();
        if sql.starts_with("DELETE FROM audit_events") {
            // Deliberate corruption must also remove the FK-protected delivery reference.
            sqlx::query("ALTER TABLE public_audit_outbox DISABLE TRIGGER public_audit_outbox_immutable").execute(&mut *tx).await.unwrap();
            let remove_outbox = sql.replacen("DELETE FROM audit_events", "DELETE FROM public_audit_outbox WHERE event_id IN (SELECT event_id FROM audit_events", 1) + ")";
            sqlx::query(&remove_outbox).bind(first["task_id"].as_str().unwrap()).execute(&mut *tx).await.unwrap();
            sqlx::query("ALTER TABLE public_audit_outbox ENABLE TRIGGER public_audit_outbox_immutable").execute(&mut *tx).await.unwrap();
        }
        sqlx::query(sql).bind(first["task_id"].as_str().unwrap()).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        assert_ne!(query(&server,&cookie,&operation_path(&created)).await.status,StatusCode::OK,"corruption accepted: {sql}");
        assert_ne!(query(&server,&cookie,&format!("/api/tasks/{}",first["task_id"].as_str().unwrap())).await.status,StatusCode::OK);
    }
    assert_eq!(server.calls(), 11);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_unclaimed_replacement_replay_conflict_and_old_ticket() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let first = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &first).await;
    let confirmed = confirm(&server, &cookie, &prepared).await;
    let request = selection(
        &confirmed,
        "CREATE_MOCK_DRAFT",
        confirmed["parameters"].clone(),
    );
    let replacement = ok(command(
        &server,
        &cookie,
        &format!("{}/actions", task_path(&first)),
        request.clone(),
    )
    .await);
    assert_eq!(replacement["plan_version"], 2);
    assert_ne!(replacement["operation_id"], first["operation_id"]);
    let replay = ok(command(
        &server,
        &cookie,
        &format!("{}/actions", task_path(&first)),
        request.clone(),
    )
    .await);
    assert_eq!(replay["operation_id"], replacement["operation_id"]);
    assert_eq!(replay["replayed"], true);
    let mut crossed = request;
    crossed["parameters"]["amount"] = json!("300.00");
    assert_eq!(
        command(
            &server,
            &cookie,
            &format!("{}/actions", task_path(&first)),
            crossed
        )
        .await
        .status,
        StatusCode::CONFLICT
    );
    let old = ok(query(&server, &cookie, &operation_path(&first)).await);
    assert_eq!(old["operation_state"], "CANCELLED");
    assert!(old["receipt"].is_null());
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/confirm", operation_path(&first)),
            confirmation(&prepared, "CONFIRM")
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/execute", operation_path(&first)),
            execution(&confirmed)
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(server.calls(), 0);
    let (_, created) = run(&server, &cookie, &replacement).await;
    assert_eq!(created["receipt"]["draft_version"], 1);
    assert_ne!(
        command(
            &server,
            &cookie,
            &format!("{}/actions", task_path(&created)),
            selection(&created, "CREATE_MOCK_DRAFT", first["parameters"].clone())
        )
        .await
        .status,
        StatusCode::OK
    );
    assert_eq!(server.calls(), 1);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_100_execution_requests_claim_once_and_key_conflicts() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    let initial = create(&server, &cookie).await;
    let prepared = prepare(&server, &cookie, &initial).await;
    let confirmed = confirm(&server, &cookie, &prepared).await;
    let path = format!("{}/execute", operation_path(&confirmed));
    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..100 {
        let address = server.address;
        let cookie = cookie.clone();
        let path = path.clone();
        let raw = execution(&confirmed).to_string();
        requests.spawn(async move {
            Server::call_at(
                address,
                Method::POST,
                &path,
                Some(raw),
                Some(&cookie),
                None,
                None,
                true,
            )
            .await
        });
    }
    while let Some(reply) = requests.join_next().await {
        let view = ok(reply.unwrap());
        assert_eq!(view["operation_id"], confirmed["operation_id"]);
        assert!(
            ["SUCCEEDED", "RESULT_UNKNOWN"].contains(&view["execution_status"].as_str().unwrap())
        );
    }
    assert_eq!(server.calls(), 1);
    assert_eq!(count(&db, "mock_draft_lifecycle_claims").await, 1);
    let mut bad = execution(&confirmed);
    bad["facts_version"] = json!(8);
    assert_eq!(
        command(&server, &cookie, &path, bad).await.status,
        StatusCode::CONFLICT
    );
    let fresh = server.login().await;
    let replay = execute(&server, &fresh, &confirmed).await;
    assert_eq!(replay["execution_status"], "SUCCEEDED");
    assert_eq!(server.calls(), 1);
    server.stop();
    db.finish().await;
}

#[tokio::test]
#[ignore = "requires isolated PostgreSQL and real loopback HTTP"]
async fn real_http_lifecycle_replacement_versus_execution_is_serialized() {
    let db = Db::new().await;
    let server = server(&db).await;
    let cookie = server.login().await;
    for _ in 0..8 {
        let first = create(&server, &cookie).await;
        let prepared = prepare(&server, &cookie, &first).await;
        let confirmed = confirm(&server, &cookie, &prepared).await;
        let calls = server.calls();
        let execution_path = format!("{}/execute", operation_path(&confirmed));
        let action_path = format!("{}/actions", task_path(&confirmed));
        let (a, b) = tokio::join!(
            command(&server, &cookie, &execution_path, execution(&confirmed)),
            command(
                &server,
                &cookie,
                &action_path,
                selection(
                    &confirmed,
                    "CREATE_MOCK_DRAFT",
                    confirmed["parameters"].clone()
                )
            )
        );
        assert_ne!(
            a.status == StatusCode::OK,
            b.status == StatusCode::OK,
            "only one competing command can win: {} {}",
            a.json,
            b.json
        );
        if a.status == StatusCode::OK {
            assert_eq!(a.json["execution_status"], "SUCCEEDED");
            assert_eq!(server.calls(), calls + 1);
        } else {
            assert_eq!(b.json["plan_version"], 2);
            assert_eq!(server.calls(), calls);
        }
    }
    server.stop();
    db.finish().await;
}
