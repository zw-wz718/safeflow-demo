use super::super::tests::{host, prepared, runtime, setup, verified, Clock, Sessions};
use super::*;
async fn restricted() -> (
    PgUnifiedFictionalSource,
    PgUnifiedFictionalSource,
    Arc<Sessions>,
    SessionRef,
    Arc<Clock>,
) {
    restricted_injected(false).await
}
async fn restricted_injected(
    inject: bool,
) -> (
    PgUnifiedFictionalSource,
    PgUnifiedFictionalSource,
    Arc<Sessions>,
    SessionRef,
    Arc<Clock>,
) {
    let (admin, sessions, s, clock) = setup().await;
    if inject {
        let db = admin.repository_for_isolated_backend();
        sqlx::raw_sql("CREATE TABLE step07_injection(stage text NOT NULL); INSERT INTO step07_injection VALUES('off'); CREATE FUNCTION step07_reject() RETURNS trigger LANGUAGE plpgsql AS $$ DECLARE stage text; eligible boolean:=true; BEGIN SELECT i.stage INTO stage FROM step07_injection i; IF TG_TABLE_NAME='tasks' THEN eligible:=NEW.state='COMPLETED'; ELSIF TG_TABLE_NAME='operations' THEN eligible:=NEW.state='SUCCEEDED'; ELSIF TG_TABLE_NAME='execution_permits' THEN eligible:=NEW.status='CONSUMED'; ELSIF TG_TABLE_NAME='audit_events' THEN eligible:=NEW.kind='CARD_FREEZE_OPERATION_RECORDED'; ELSIF TG_TABLE_NAME='public_audit_outbox' THEN eligible:=NEW.envelope->'public_event'->>'kind'='CARD_FREEZE_OPERATION_RECORDED'; ELSIF TG_TABLE_NAME='public_card_attempts_v1' THEN eligible:=NEW.state='BUSINESS_COMMITTED'; END IF; IF eligible AND stage=TG_TABLE_NAME THEN RAISE EXCEPTION 'owned-write-stage-failure'; END IF; IF stage='final_guard' AND TG_TABLE_NAME='public_card_audit_links_v1' THEN UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id='account_001'; END IF; RETURN NEW; END $$; CREATE FUNCTION step07_deferred() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM step07_injection WHERE stage='deferred') THEN RAISE EXCEPTION 'owned-deferred-failure'; END IF; RETURN NEW; END $$;") .execute(&db.pool).await.unwrap();
        for table in [
            "card_locks_v1",
            "card_freeze_operations_v1",
            "card_freeze_reasons_v1",
            "card_freeze_receipts_v1",
            "public_card_results_v1",
            "public_card_audit_links_v1",
            "audit_events",
            "public_audit_outbox",
            "operations",
            "tasks",
            "execution_permits",
            "public_execution_requests",
            "public_card_attempts_v1",
        ] {
            sqlx::raw_sql(&format!("CREATE TRIGGER step07_failure BEFORE INSERT OR UPDATE ON {table} FOR EACH ROW EXECUTE FUNCTION step07_reject()" )).execute(&db.pool).await.unwrap();
        }
        sqlx::raw_sql("CREATE CONSTRAINT TRIGGER step07_deferred AFTER INSERT ON public_card_results_v1 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION step07_deferred()" ).execute(&db.pool).await.unwrap();
    }
    let deploy = CardFinalDeployment::for_isolated_backend(admin.clone());
    deploy.activate_exact(&[], &[]).await.unwrap();
    let role = format!("sf_card_{}", uuid::Uuid::new_v4().simple());
    let password = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    deploy
        .register_new_login(&role, Zeroizing::new(password.clone()))
        .await
        .unwrap();
    if inject {
        sqlx::raw_sql(&format!("GRANT SELECT ON step07_injection TO \"{role}\""))
            .execute(&admin.repository_for_isolated_backend().pool)
            .await
            .unwrap();
    }
    let url = std::env::var("SAFEFLOW_TEST_DATABASE_URL").unwrap();
    let url = format!(
        "postgresql://{role}:{password}@{}",
        url.split_once('@').unwrap().1
    );
    let repo = PgRepository::connect_existing(&url, "sf_unified_card")
        .await
        .unwrap();
    let user: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(&repo.pool)
        .await
        .unwrap();
    assert_eq!(user, role);
    let source = PgUnifiedFictionalSource::open(
        repo,
        UNIFIED_FICTIONAL_SOURCE,
        crate::UNIFIED_FICTIONAL_REVISION,
    )
    .await
    .unwrap();
    (admin, source, sessions, s, clock)
}
async fn ready(service: &CardSafetyService<'_>, s: &SessionRef) -> CardSafetyView {
    let v = prepared(service, s).await;
    verified(service, s, &v).await;
    service.issue_permit(s, &v.operation_id).await.unwrap();
    v
}
async fn count(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}
#[test]
fn exact_domain_details() {
    for effect in ["CREATED", "REUSED_ACTIVE"] {
        assert!(checked_details(&details(effect)).is_ok());
    }
    let mut v = details("CREATED");
    v["schema_version"] = json!(1.0);
    assert!(checked_details(&v).is_err());
    let mut v = details("CREATED");
    v["card_id"] = json!("private");
    assert!(checked_details(&v).is_err());
    assert!(checked_details(&details("UNKNOWN")).is_err());
}
#[tokio::test]
async fn real_created_reused_replay_and_concurrency() {
    let (admin, source, sessions, s, clock) = restricted().await;
    let rt = runtime(sessions.clone());
    let service = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock);
    let v = ready(&service, &s).await;
    let first = service.execute_freeze(&s, &v.operation_id).await.unwrap();
    assert_eq!(first.state, "RECORDED");
    assert_eq!(first.original_effect.as_deref(), Some("CREATED"));
    assert_eq!(first.original_lock_active, Some(true));
    assert_eq!(first.account_outgoing_restricted, Some(true));
    let old = serde_json::to_value(&first).unwrap();
    assert_eq!(
        serde_json::to_value(service.execute_freeze(&s, &v.operation_id).await.unwrap()).unwrap(),
        old
    );
    let v2 = ready(&service, &s).await;
    let (a, b) = tokio::join!(
        service.execute_freeze(&s, &v2.operation_id),
        service.execute_freeze(&s, &v2.operation_id)
    );
    assert!(a.is_ok(), "{:?}", a.err());
    assert!(b.is_ok(), "{:?}", b.err());
    let r = service
        .query_freeze_original(&s, &v2.operation_id)
        .await
        .unwrap();
    assert_eq!(r.original_effect.as_deref(), Some("REUSED_ACTIVE"));
    let db = admin.repository_for_isolated_backend();
    for table in [
        "card_freeze_operations_v1",
        "card_freeze_reasons_v1",
        "card_freeze_receipts_v1",
        "public_card_results_v1",
        "public_card_audit_links_v1",
    ] {
        assert_eq!(count(&db.pool, table).await, 2, "{table}");
    }
    assert_eq!(count(&db.pool, "card_locks_v1").await, 1);
    let snapshot = db
        .recover(&UserId::parse("user_001").unwrap(), &v.task_id, &|| Ok(()))
        .await
        .unwrap();
    assert!(snapshot
        .audit
        .iter()
        .any(|a| a.kind == PersistentAuditKind::CardFreezeOperationRecorded));
    let wrong = sessions.add("another_owner", "user_002", 1);
    assert!(service
        .query_freeze_original(&wrong, &v.operation_id)
        .await
        .is_err());
    let fresh = sessions.add("new_valid_session", "user_001", 2);
    assert_eq!(
        serde_json::to_value(
            service
                .query_freeze_original(&fresh, &v.operation_id)
                .await
                .unwrap()
        )
        .unwrap(),
        old
    );
    for table in ["transfer_ledger_v1", "transfer_fund_orders_v1"] {
        assert_eq!(count(&db.pool, table).await, 0);
    }
    let ack: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM public_audit_outbox WHERE acknowledged_at IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(ack, 0);
    println!("SCRAM restricted role: Created, original replay, concurrent independently confirmed ReusedActive, strict typed recovery, new-session read-only original, no funds/ACK");
    source.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn whole_transaction_failures_and_explicit_retry() {
    let (admin, source, sessions, s, clock) = restricted_injected(true).await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    let db = admin.repository_for_isolated_backend();
    // All test-only SQL triggers were registered BEFORE object activation.
    // Only admin changes this data row; no DDL drift or product callback.
    for (index, table) in [
        "card_locks_v1",
        "card_freeze_operations_v1",
        "card_freeze_reasons_v1",
        "card_freeze_receipts_v1",
        "public_card_results_v1",
        "public_card_audit_links_v1",
        "audit_events",
        "public_audit_outbox",
        "operations",
        "tasks",
        "execution_permits",
        "public_execution_requests",
        "public_card_attempts_v1",
        "deferred",
        "final_guard",
    ]
    .iter()
    .enumerate()
    {
        let v = ready(&service, &s).await;
        sqlx::query("UPDATE step07_injection SET stage=$1")
            .bind(table)
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(
            service.execute_freeze(&s, &v.operation_id).await.is_err(),
            "{table}"
        );
        sqlx::query("UPDATE step07_injection SET stage='off'")
            .execute(&db.pool)
            .await
            .unwrap();
        let state = service
            .query_freeze_original(&s, &v.operation_id)
            .await
            .unwrap();
        assert_eq!(state.state, "ROLLBACK_VERIFIED", "{table}");
        assert_eq!(
            count(&db.pool, "card_freeze_operations_v1").await,
            index as i64
        );
        assert_eq!(
            service
                .execute_freeze(&s, &v.operation_id)
                .await
                .unwrap()
                .state,
            "ROLLBACK_VERIFIED"
        );
        assert!(service
            .retry_rolled_back_freeze(&s, &v.operation_id)
            .await
            .is_err());
        service.issue_permit(&s, &v.operation_id).await.unwrap();
        assert_eq!(
            service
                .retry_rolled_back_freeze(&s, &v.operation_id)
                .await
                .unwrap()
                .state,
            "RECORDED"
        );
        let history:Vec<(i64,String)>=sqlx::query_as("SELECT card_attempt_no,status FROM execution_permits WHERE operation_id=$1 ORDER BY card_attempt_no").bind(v.operation_id.as_str()).fetch_all(&db.pool).await.unwrap();
        assert_eq!(history, vec![(0, "REVOKED".into()), (1, "CONSUMED".into())]);
    }
    println!("15 real SQL write-stage/final-guard/deferred failures: whole rollback, durable server proof, no automatic execution, explicit same O with new permit; all old permit tombstones kept");
    source.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn admitted_orphan_is_unknown_and_never_reexecuted() {
    let (admin, source, sessions, s, clock) = restricted().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    let v = ready(&service, &s).await;
    let token = service
        .admit(&s, &v.operation_id, false)
        .await
        .unwrap()
        .unwrap();
    drop(token);
    assert_eq!(
        service
            .query_freeze_original(&s, &v.operation_id)
            .await
            .unwrap()
            .state,
        "UNKNOWN"
    );
    for retry in [false, true] {
        assert_eq!(
            service
                .execute_freeze_command(&s, &v.operation_id, retry)
                .await
                .unwrap()
                .state,
            "UNKNOWN"
        );
    }
    let db = admin.repository_for_isolated_backend();
    assert_eq!(count(&db.pool, "card_locks_v1").await, 0);
    assert_eq!(count(&db.pool, "public_card_results_v1").await, 0);
    println!("lost admission token / absent member / READY / explicit retry all retain UNKNOWN original, zero business writes");
    source.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn current_gates_and_writer_withdrawal() {
    let (admin, source, sessions, s, clock) = restricted().await;
    let rt = runtime(sessions.clone());
    let service = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let missing = prepared(&service, &s).await;
    assert!(service
        .execute_freeze(&s, &missing.operation_id)
        .await
        .is_err());
    assert_eq!(
        service
            .query_freeze_original(&s, &missing.operation_id)
            .await
            .unwrap()
            .state,
        "PREPARED_ONLY"
    );
    let v = ready(&service, &s).await;
    let wrong = sessions.add("wrong_final_owner", "user_002", 1);
    assert!(service
        .execute_freeze(&wrong, &v.operation_id)
        .await
        .is_err());
    clock.add(60000);
    assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
    assert_eq!(
        service
            .query_freeze_original(&s, &v.operation_id)
            .await
            .unwrap()
            .state,
        "ROLLBACK_VERIFIED"
    );
    let stale = ready(&service, &s).await;
    let db = admin.repository_for_isolated_backend();
    sqlx::query(
        "UPDATE card_account_versions_v1 SET revision=revision+1 WHERE account_id='account_001'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(service
        .execute_freeze(&s, &stale.operation_id)
        .await
        .is_err());
    assert_eq!(count(&db.pool, "card_locks_v1").await, 0);
    let valid = ready(&service, &s).await;
    assert_eq!(
        service
            .execute_freeze(&s, &valid.operation_id)
            .await
            .unwrap()
            .state,
        "RECORDED"
    );
    let original = serde_json::to_value(
        service
            .query_freeze_original(&s, &valid.operation_id)
            .await
            .unwrap(),
    )
    .unwrap();
    CardFinalDeployment::for_isolated_backend(admin.clone())
        .disable_writer()
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            service
                .query_freeze_original(&s, &valid.operation_id)
                .await
                .unwrap()
        )
        .unwrap(),
        original
    );
    assert!(service
        .prepare(
            &s,
            &ResourceId::parse("account_001").unwrap(),
            "demo_card_salary_v1",
            &uuid::Uuid::new_v4().to_string()
        )
        .await
        .is_err());
    assert!(
        sqlx::query("UPDATE public_card_final_deployment_v1 SET active=true,epoch=epoch+1")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(sqlx::query("UPDATE card_locks_v1 SET state='RELEASED'")
        .execute(&db.pool)
        .await
        .is_err());
    for q in [
        "CREATE TABLE forbidden(x int)",
        "CREATE TEMP TABLE forbidden(x int)",
        "DELETE FROM public_card_results_v1",
        "UPDATE transfer_accounts_v1 SET booked_cents=booked_cents-1",
        "INSERT INTO transfer_ledger_v1(operation_id) VALUES('forbidden')",
    ] {
        assert!(
            sqlx::query(q)
                .execute(&source.repository_for_isolated_backend().pool)
                .await
                .is_err(),
            "{q}"
        );
    }
    println!("no permit, wrong owner, equality expiry, stale facts, one-way disabled writer, original read preserved; DDL/TEMP/delete/balance/ledger denied under actual SCRAM final role");
    source.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn exact_legacy_set_and_mixed_writer_denied() {
    let (source, sessions, s, clock) = setup().await;
    let rt = runtime(sessions.clone());
    let service =
        CardSafetyService::for_isolated_backend(source.clone(), sessions, rt.runtime(), host())
            .unwrap()
            .with_clock(clock);
    let v = prepared(&service, &s).await;
    let db = source.repository_for_isolated_backend();
    let input: Input = sqlx::query_scalar::<_, Json<Input>>(
        "SELECT input FROM public_card_safety_requests_v1 WHERE operation_id=$1",
    )
    .bind(v.operation_id.as_str())
    .fetch_one(&db.pool)
    .await
    .unwrap()
    .0;
    let mut read = db.subscription_read_pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED READ ONLY")
        .execute(&mut *read)
        .await
        .unwrap();
    let preview =
        PreparedFreeze::read_on_connection(&mut read, "user_001", "account_001", &input.command())
            .await
            .unwrap();
    read.commit().await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    sqlx::query("SELECT txid_current()")
        .execute(&mut *tx)
        .await
        .unwrap();
    {
        let mut member = CardFreezeBackend::borrow(&mut tx, "user_001", "account_001")
            .await
            .unwrap();
        member
            .execute_prepared_freeze_handoff(&preview, &input.command())
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let deploy = CardFinalDeployment::for_isolated_backend(source.clone());
    assert!(deploy.activate_exact(&[], &[]).await.is_err());
    deploy
        .activate_exact(
            &[input.operation_record_id.clone()],
            &[v.operation_id.clone()],
        )
        .await
        .unwrap();
    assert_eq!(
        service
            .query_freeze_original(&s, &v.operation_id)
            .await
            .unwrap()
            .state,
        "LEGACY_UNLINKED"
    );
    assert_eq!(count(&db.pool, "public_card_results_v1").await, 0);
    assert_eq!(count(&db.pool, "public_card_audit_links_v1").await, 0);
    assert!(sqlx::query("INSERT INTO public_card_legacy_operations_v1 SELECT operation_id,to_jsonb(q) FROM public_card_safety_requests_v1 q").execute(&db.pool).await.is_err());
    assert!(sqlx::query("UPDATE card_locks_v1 SET state='RELEASED'")
        .execute(&db.pool)
        .await
        .is_err());
    // Even privileged old invoker is unregistered: no return to member-only saves.
    assert!(sqlx::query("INSERT INTO card_locks_v1(lock_id,card_id,source,state) VALUES('old_writer','demo_card_salary_v1','RISK_CONTROL','ACTIVE')").execute(&db.pool).await.is_err());
    println!("exact nonempty member/public legacy sets; unmatched history LEGACY_UNLINKED, zero fabricated events; sealed sets and old unregistered member/lock writes rejected");
    db.close().await;
}
async fn denied_final_gate(mode: &str) {
    let (admin, source, sessions, s, clock) = restricted().await;
    let rt = runtime(sessions.clone());
    let service = CardSafetyService::for_isolated_backend(
        source.clone(),
        sessions.clone(),
        rt.runtime(),
        host(),
    )
    .unwrap()
    .with_clock(clock.clone());
    let v = ready(&service, &s).await;
    match mode {
        "host" => {
            let reboot = CardSafetyService::for_isolated_backend(
                source.clone(),
                sessions.clone(),
                rt.runtime(),
                host(),
            )
            .unwrap()
            .with_clock(clock);
            assert!(reboot.execute_freeze(&s, &v.operation_id).await.is_err());
        }
        "session" => {
            sessions.add("owner_session", "user_001", 2);
            assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
        }
        "epoch" => {
            service
                .revoke_account_for_native_controller(
                    "isolated-unit-controller-secret-not-http",
                    &UserId::parse("user_001").unwrap(),
                    &ResourceId::parse("account_001").unwrap(),
                )
                .await
                .unwrap();
            assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
        }
        "source" => {
            admin.deactivate_for_isolated_backend().await.unwrap();
            assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
        }
        "catalog_aba" => {
            let pool = &admin.repository_for_isolated_backend().pool;
            sqlx::query(
                "UPDATE card_catalog_v1 SET last_four='9876' WHERE card_id='demo_card_salary_v1'",
            )
            .execute(pool)
            .await
            .unwrap();
            sqlx::query(
                "UPDATE card_catalog_v1 SET last_four='6628' WHERE card_id='demo_card_salary_v1'",
            )
            .execute(pool)
            .await
            .unwrap();
            assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
        }
        "objects" => {
            sqlx::raw_sql("CREATE FUNCTION step07_foreign_object() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$").execute(&admin.repository_for_isolated_backend().pool).await.unwrap();
            assert!(service.execute_freeze(&s, &v.operation_id).await.is_err());
            assert!(service
                .query_freeze_original(&s, &v.operation_id)
                .await
                .is_err());
        }
        _ => panic!("unknown test gate"),
    }
    let db = admin.repository_for_isolated_backend();
    for table in [
        "card_locks_v1",
        "card_freeze_operations_v1",
        "public_card_results_v1",
        "public_card_audit_links_v1",
        "public_execution_requests",
        "transfer_ledger_v1",
        "transfer_fund_orders_v1",
    ] {
        assert_eq!(count(&db.pool, table).await, 0, "{mode}: {table}");
    }
    println!("final execute rejects {mode}, zero member/public/funds writes");
    source.repository_for_isolated_backend().close().await;
    db.close().await;
}
#[tokio::test]
async fn final_host_replacement_denied() {
    denied_final_gate("host").await;
}
#[tokio::test]
async fn final_session_rebinding_denied() {
    denied_final_gate("session").await;
}
#[tokio::test]
async fn final_account_epoch_denied() {
    denied_final_gate("epoch").await;
}
#[tokio::test]
async fn final_source_withdrawal_denied() {
    denied_final_gate("source").await;
}
#[tokio::test]
async fn final_catalog_aba_denied() {
    denied_final_gate("catalog_aba").await;
}
#[tokio::test]
async fn final_object_replacement_denied() {
    denied_final_gate("objects").await;
}
